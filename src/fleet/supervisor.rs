//! One tokio task per chain, all in one process (docs/design.md §15).
//!
//! # What it does, and what it deliberately does not
//!
//! Every task calls the SAME entry point `indexer run` calls
//! (`pipeline::run_with` / `pipeline::solana::run_with`). Nothing about a
//! chain's safety changes because it shares a process: it takes its own
//! lease, keeps its own epoch, writes its own tombstones and flushes
//! through its own writer. The supervisor adds exactly four things:
//!
//! 1. it starts and stops those tasks,
//! 2. it restarts a task that failed, with a backoff capped at
//!    [`MAX_RESTART_BACKOFF`], so one broken chain never takes the others
//!    down and never hammers a provider,
//! 3. it treats "another process holds this chain's lease" as a STATE and
//!    not as a failure, so the fleet waits quietly instead of looping,
//! 4. it remembers what the owner wanted in `fleet_chains`.
//!
//! **Stopping a chain is the graceful path `ctrl-c` already takes.** The
//! pipeline's `Runtime::shutdown` is a future; `indexer run` gives it the
//! process signal and the fleet gives it a per-chain handle. The chain
//! therefore stops the way it always has - final flush, workers down, lease
//! released - and the other chains never notice.
//!
//! **Nothing here removes data.** There is no purge, no drop and no delete
//! anywhere in fleet mode, by construction: the only thing the supervisor
//! can do to a chain is start it, stop it, or change the options it starts
//! with.

use super::{
    budgets::Budgets,
    chains::{DesiredChain, ForeignChain},
    status::{human_duration, ChainStatus, Event, LiveView},
};
use crate::{
    configs::{ChainSettings, Desired, FleetConfig, SettingError},
    metrics::Metrics,
    pipeline::{
        lease::LeaseOptions,
        status::{ChainState, StatusSink},
    },
};
use anyhow::Result;
use futures::future::BoxFuture;
use log::{info, warn};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

/// First wait after a chain failed; it doubles from here.
pub const FIRST_RESTART_BACKOFF: Duration = Duration::from_secs(2);

/// Cap of the wait between restarts (docs/design.md section 15).
pub const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(300);

/// How often a chain whose lease another process holds looks again. A
/// fixed, unhurried interval, NOT a growing backoff: this is a state to
/// wait out, and the moment the other process stops we want to take over
/// without a ten minute delay.
pub const LEASE_RECHECK: Duration = Duration::from_secs(30);

/// `2s, 4s, 8s ... 5min`.
pub fn restart_backoff(failures: u32) -> Duration {
    FIRST_RESTART_BACKOFF
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_RESTART_BACKOFF)
}

/// What actually runs one chain. The production implementation calls the
/// pipeline; the supervisor's own tests substitute a fake one, so restart,
/// backoff, cancellation and "running elsewhere" are tested without
/// HyperSync or ClickHouse.
pub trait ChainRunner: Send + Sync + 'static {
    fn run(
        &self,
        chain: u64,
        settings: ChainSettings,
        flush_rows: usize,
        metrics: Metrics,
        status: StatusSink,
        shutdown: BoxFuture<'static, ()>,
    ) -> BoxFuture<'static, Result<()>>;

    /// Rejects a settings map the pipeline could not be started with,
    /// through the CLI's own parser. Called before anything is stored, so
    /// the panel refuses a bad setting instead of a chain crash-looping on
    /// it.
    fn validate(
        &self,
        chain: u64,
        settings: &ChainSettings,
    ) -> Result<(), SettingError>;
}

/// Where the desired state is kept between runs. ClickHouse in production
/// (`super::chains`); an in-memory one in the tests.
pub trait DesiredStore: Send + Sync + 'static {
    fn load(&self) -> BoxFuture<'_, Result<Vec<DesiredChain>>>;
    fn save(&self, chain: DesiredChain) -> BoxFuture<'_, Result<()>>;
    /// Chains a DIFFERENT process is indexing into the same database.
    fn foreign(
        &self,
        mine: Vec<u64>,
    ) -> BoxFuture<'_, Result<Vec<ForeignChain>>>;
}

struct Entry {
    desired: Desired,
    settings: ChainSettings,
    status: Arc<ChainStatus>,
    /// Bumped to cancel the attempt that is running right now. The chain's
    /// task turns the current value into the `shutdown` future it hands
    /// the pipeline, which is the same seam `ctrl-c` uses.
    cancel: watch::Sender<u64>,
    /// False = do not start another attempt after this one ends.
    keep_running: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl Entry {
    fn cancel_current(&self) {
        self.cancel.send_modify(|generation| *generation += 1);
    }
}

/// One process, many chains.
pub struct Supervisor {
    config: FleetConfig,
    runner: Arc<dyn ChainRunner>,
    store: Arc<dyn DesiredStore>,
    budgets: Arc<Budgets>,
    lease: LeaseOptions,
    chains: Mutex<BTreeMap<u64, Entry>>,
    /// Set once, when the whole process is going down: every chain's
    /// shutdown future resolves and no chain is started again.
    stopping: watch::Sender<bool>,
}

impl Supervisor {
    pub fn new(
        config: FleetConfig,
        runner: Arc<dyn ChainRunner>,
        store: Arc<dyn DesiredStore>,
    ) -> Arc<Self> {
        let budgets = Arc::new(Budgets::new(
            config.max_inflight_mb,
            config.solana_queries_per_minute,
        ));

        Self::with_budgets(config, runner, store, budgets)
    }

    /// [`Self::new`] over budgets the caller already owns, so the
    /// supervisor and the thing that starts the chains share ONE Solana
    /// query allowance instead of one each.
    pub fn with_budgets(
        config: FleetConfig,
        runner: Arc<dyn ChainRunner>,
        store: Arc<dyn DesiredStore>,
        budgets: Arc<Budgets>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            runner,
            store,
            budgets,
            lease: LeaseOptions::default(),
            chains: Mutex::new(BTreeMap::new()),
            stopping: watch::channel(false).0,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Entry>> {
        self.chains.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn config(&self) -> &FleetConfig {
        &self.config
    }

    /// Reads `fleet_chains` ONCE, adds the chains named on the command
    /// line, and starts everything whose desired state is `running`.
    ///
    /// From here on the table is never read again: the supervisor's memory
    /// is the authority (ClickHouse has no read-your-writes).
    pub async fn load_and_start(self: &Arc<Self>) -> Result<()> {
        let stored = self.store.load().await?;

        for chain in stored {
            self.insert(chain.chain, chain.desired, chain.settings);
        }

        // `--chain` on the command line: a chain the table does not know
        // yet. It never overwrites settings the table already holds.
        for chain in self.config.chains.clone() {
            if self.lock().contains_key(&chain) {
                continue;
            }
            let entry =
                self.insert(chain, Desired::Running, ChainSettings::new());
            if let Err(e) = self.store.save(entry).await {
                warn!("Could not remember chain {chain}: {e:#}");
            }
        }

        let to_start: Vec<u64> = self
            .lock()
            .iter()
            .filter(|(_, entry)| entry.desired == Desired::Running)
            .map(|(chain, _)| *chain)
            .collect();

        if to_start.is_empty() {
            info!(
                "No chain is set to run yet. Add one in the control panel, \
                 or start the fleet with --chain <id>."
            );
        }

        for chain in to_start {
            self.spawn(chain);
        }

        Ok(())
    }

    /// Creates (or replaces the settings of) an entry. Does not start it.
    fn insert(
        &self,
        chain: u64,
        desired: Desired,
        settings: ChainSettings,
    ) -> DesiredChain {
        let mut chains = self.lock();

        let entry = chains.entry(chain).or_insert_with(|| Entry {
            desired,
            settings: settings.clone(),
            status: ChainStatus::new(
                chain,
                // Always enabled: the panel reads the same handle
                // `/metrics` renders, so a fleet without --metrics-addr
                // still has numbers to show.
                Metrics::new(chain, READY_STALENESS),
            ),
            cancel: watch::channel(0).0,
            keep_running: Arc::new(AtomicBool::new(false)),
            task: None,
        });

        entry.desired = desired;
        entry.settings = settings;

        DesiredChain {
            chain,
            desired: entry.desired,
            settings: entry.settings.clone(),
        }
    }

    /// The shutdown future one attempt gets: it resolves when this chain is
    /// cancelled OR the process is going down. This is the ONLY way a chain
    /// is stopped, and it is the graceful one.
    fn shutdown_future(&self, entry: &Entry) -> BoxFuture<'static, ()> {
        let mut cancel = entry.cancel.subscribe();
        let generation = *cancel.borrow();
        let mut stopping = self.stopping.subscribe();

        Box::pin(async move {
            tokio::select! {
                _ = cancel.wait_for(|now| *now != generation) => {}
                _ = stopping.wait_for(|stop| *stop) => {}
            }
        })
    }

    /// Starts the chain's task if it is not running.
    fn spawn(self: &Arc<Self>, chain: u64) {
        let mut chains = self.lock();
        let Some(entry) = chains.get_mut(&chain) else { return };

        if entry.task.as_ref().is_some_and(|task| !task.is_finished()) {
            return;
        }

        entry.keep_running.store(true, Ordering::SeqCst);
        let keep_running = entry.keep_running.clone();
        let status = entry.status.clone();
        let supervisor = self.clone();

        entry.task = Some(tokio::spawn(async move {
            supervisor.run_chain(chain, keep_running, status).await;
        }));
    }

    /// The restart loop of ONE chain.
    async fn run_chain(
        self: Arc<Self>,
        chain: u64,
        keep_running: Arc<AtomicBool>,
        status: Arc<ChainStatus>,
    ) {
        let mut failures: u32 = 0;

        while keep_running.load(Ordering::SeqCst)
            && !*self.stopping.borrow()
        {
            let Some((settings, shutdown, flush_rows, generation)) =
                self.attempt_inputs(chain)
            else {
                break;
            };

            status.set_state(ChainState::Starting);

            let result = self
                .runner
                .run(
                    chain,
                    settings,
                    flush_rows,
                    status.metrics(),
                    StatusSink::new(status.clone()),
                    shutdown,
                )
                .await;

            // The owner stopped it (or the process is going down) while it
            // was running: that is not a failure and not a restart.
            if !keep_running.load(Ordering::SeqCst)
                || *self.stopping.borrow()
            {
                break;
            }

            // The pipeline returns `Ok` both when it reached `--end-block`
            // and when its shutdown future resolved, so the two are told
            // apart by the cancellation counter: a Restart bumps it, and
            // the chain starts again at once with no backoff, because
            // nothing failed.
            if self.cancel_generation(chain) != Some(generation) {
                failures = 0;
                continue;
            }

            let wait = match result {
                // `--end-block` was reached, or the pipeline returned on a
                // cancellation that raced the check above.
                Ok(()) => {
                    status.set_state(ChainState::Stopped);
                    status.record_command(
                        "stop",
                        "Finished: the chain reached the block it was told \
                         to stop at."
                            .to_string(),
                    );
                    break;
                }
                // A STATE, not an error loop: another process owns the
                // chain. Wait a fixed, short interval and look again.
                Err(e) if is_lease_held(&e) => {
                    status.set_state(ChainState::RunningElsewhere);
                    LEASE_RECHECK
                }
                Err(e) => {
                    failures = failures.saturating_add(1);
                    let message = crate::tokens::redact::redact_urls(
                        &format!("{e:#}"),
                    );
                    warn!("Chain {chain} stopped: {message}");
                    status.record_error(&message);
                    status.set_state(ChainState::Failed);
                    restart_backoff(failures)
                }
            };

            if !matches!(status.state(), ChainState::RunningElsewhere) {
                status.record_restart(wait);
            }

            // A stop during the wait must take effect at once.
            let mut stopping = self.stopping.subscribe();
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopping.wait_for(|stop| *stop) => break,
                _ = wait_for_stop(keep_running.clone()) => break,
            }
        }

        status.set_state(ChainState::Stopped);
    }

    /// The cancellation counter of a chain right now.
    fn cancel_generation(&self, chain: u64) -> Option<u64> {
        let chains = self.lock();
        let generation = *chains.get(&chain)?.cancel.borrow();
        Some(generation)
    }

    /// Everything one attempt needs, read under the lock in one go.
    #[allow(clippy::type_complexity)]
    fn attempt_inputs(
        &self,
        chain: u64,
    ) -> Option<(ChainSettings, BoxFuture<'static, ()>, usize, u64)> {
        let chains = self.lock();
        let entry = chains.get(&chain)?;

        // The memory cap is split over the chains that are meant to be
        // running right now, so one more chain means smaller batches
        // everywhere rather than a bigger process.
        let running = chains
            .values()
            .filter(|entry| entry.desired == Desired::Running)
            .count();

        let generation = *entry.cancel.borrow();

        let inputs = (
            entry.settings.clone(),
            self.shutdown_future(entry),
            self.budgets.flush_rows(running),
            generation,
        );

        Some(inputs)
    }

    pub fn budgets(&self) -> Arc<Budgets> {
        self.budgets.clone()
    }

    // ------------------------------------------------- panel commands

    /// Adds a chain. Refuses a settings map the command line would refuse,
    /// and refuses a chain that is already in the fleet.
    pub async fn add(
        self: &Arc<Self>,
        chain: u64,
        settings: ChainSettings,
    ) -> Result<(), CommandError> {
        self.runner.validate(chain, &settings)?;

        if self.lock().contains_key(&chain) {
            return Err(CommandError::AlreadyThere(chain));
        }

        let stored = self.insert(chain, Desired::Running, settings);
        self.remember(stored).await;

        if let Some(entry) = self.lock().get(&chain) {
            entry.status.record_command(
                "start",
                "Added to the fleet and started.".to_string(),
            );
        }
        self.spawn(chain);
        info!("Chain {chain} was added to the fleet and started.");
        Ok(())
    }

    /// New settings, applied the NEXT time the chain starts. Never
    /// restarts it behind the owner's back: the panel says so and offers
    /// the restart button.
    pub async fn update_settings(
        self: &Arc<Self>,
        chain: u64,
        settings: ChainSettings,
    ) -> Result<(), CommandError> {
        self.runner.validate(chain, &settings)?;

        let stored = {
            let mut chains = self.lock();
            let entry = chains
                .get_mut(&chain)
                .ok_or(CommandError::NoSuchChain(chain))?;
            entry.settings = settings;
            entry.status.record_command(
                "settings",
                "Settings changed. They take effect the next time this \
                 chain starts."
                    .to_string(),
            );
            DesiredChain {
                chain,
                desired: entry.desired,
                settings: entry.settings.clone(),
            }
        };

        self.remember(stored).await;
        Ok(())
    }

    pub async fn start(
        self: &Arc<Self>,
        chain: u64,
    ) -> Result<(), CommandError> {
        let stored = {
            let mut chains = self.lock();
            let entry = chains
                .get_mut(&chain)
                .ok_or(CommandError::NoSuchChain(chain))?;
            entry.desired = Desired::Running;
            entry.status.record_command(
                "start",
                "Started by the owner.".to_string(),
            );
            DesiredChain {
                chain,
                desired: Desired::Running,
                settings: entry.settings.clone(),
            }
        };

        self.remember(stored).await;
        self.spawn(chain);
        Ok(())
    }

    /// The graceful stop: the chain flushes what it buffered and releases
    /// its lease, exactly as it would on `ctrl-c`. No data is touched.
    pub async fn stop(
        self: &Arc<Self>,
        chain: u64,
    ) -> Result<(), CommandError> {
        let stored = {
            let mut chains = self.lock();
            let entry = chains
                .get_mut(&chain)
                .ok_or(CommandError::NoSuchChain(chain))?;
            entry.desired = Desired::Stopped;
            entry.keep_running.store(false, Ordering::SeqCst);
            entry.cancel_current();
            entry.status.record_command(
                "stop",
                "Stopping: the chain writes what it has buffered and lets \
                 go of the chain, then stops."
                    .to_string(),
            );
            DesiredChain {
                chain,
                desired: Desired::Stopped,
                settings: entry.settings.clone(),
            }
        };

        self.remember(stored).await;
        Ok(())
    }

    /// Stop and start again: the same graceful stop, then a new attempt
    /// with no backoff (the owner asked, nothing failed).
    pub async fn restart(
        self: &Arc<Self>,
        chain: u64,
    ) -> Result<(), CommandError> {
        let stored = {
            let mut chains = self.lock();
            let entry = chains
                .get_mut(&chain)
                .ok_or(CommandError::NoSuchChain(chain))?;
            entry.desired = Desired::Running;
            entry.keep_running.store(true, Ordering::SeqCst);
            // Cancels the attempt that is running; the task's loop sees
            // `keep_running` still true and starts a new one.
            entry.cancel_current();
            entry.status.record_command(
                "restart",
                "Restarted by the owner.".to_string(),
            );
            DesiredChain {
                chain,
                desired: Desired::Running,
                settings: entry.settings.clone(),
            }
        };

        self.remember(stored).await;
        // If nothing was running, the loop is not there to start again.
        self.spawn(chain);
        Ok(())
    }

    /// A failed write to `fleet_chains` costs the NEXT start's memory and
    /// nothing else: the command already took effect in this process.
    async fn remember(&self, chain: DesiredChain) {
        let id = chain.chain;
        if let Err(e) = self.store.save(chain).await {
            warn!(
                "Chain {id}: the change took effect, but it could not be \
                 written to fleet_chains ({e:#}). The next start of this \
                 process will not remember it."
            );
        }
    }

    // ------------------------------------------------------- reading

    /// Every chain this process manages, plus the ones another process is
    /// indexing into the same database (read-only).
    pub async fn views(&self) -> Vec<ChainView> {
        let mut views: Vec<ChainView> = self
            .lock()
            .iter()
            .map(|(chain, entry)| ChainView {
                chain: *chain,
                name: chain_name(*chain),
                managed: true,
                desired: entry.desired.as_str(),
                settings: redact_settings(&entry.settings),
                live: entry.status.view(),
                host: None,
            })
            .collect();

        let mine: Vec<u64> = views.iter().map(|view| view.chain).collect();

        match self.store.foreign(mine).await {
            Ok(foreign) => {
                views.extend(foreign.into_iter().map(|other| ChainView {
                    chain: other.chain,
                    name: chain_name(other.chain),
                    managed: false,
                    desired: "running",
                    settings: BTreeMap::new(),
                    live: LiveView {
                        state: ChainState::RunningElsewhere.as_str(),
                        state_text: ChainState::RunningElsewhere.plain(),
                        ..LiveView::default()
                    },
                    host: Some(other.host),
                }))
            }
            Err(e) => warn!(
                "Could not look for chains indexed by another process: \
                 {e:#}"
            ),
        }

        views.sort_by_key(|view| view.chain);
        views
    }

    pub fn events(&self, chain: u64) -> Option<Vec<Event>> {
        Some(self.lock().get(&chain)?.status.events())
    }

    pub fn knows(&self, chain: u64) -> bool {
        self.lock().contains_key(&chain)
    }

    /// Every chain's metrics handle, for the fleet's one `/metrics`.
    pub fn metrics_handles(&self) -> Vec<(u64, Metrics)> {
        self.lock()
            .iter()
            .map(|(chain, entry)| (*chain, entry.status.metrics()))
            .collect()
    }

    /// The handles of the chains that are SUPPOSED to be running, for
    /// `/readyz`: a chain the owner stopped is not a reason to report the
    /// process as unhealthy.
    pub fn running_metrics_handles(&self) -> Vec<(u64, Metrics)> {
        self.lock()
            .iter()
            .filter(|(_, entry)| entry.desired == Desired::Running)
            .map(|(chain, entry)| (*chain, entry.status.metrics()))
            .collect()
    }

    pub fn lease_ttl(&self) -> Duration {
        self.lease.ttl
    }

    /// One task for the whole fleet: blocks per second and reorg events,
    /// read from the counters the pipeline already keeps.
    pub async fn sample_forever(self: Arc<Self>) {
        let mut stopping = self.stopping.subscribe();
        let mut tick =
            tokio::time::interval(super::status::SAMPLE_INTERVAL);

        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let statuses: Vec<Arc<ChainStatus>> = self
                        .lock()
                        .values()
                        .map(|entry| entry.status.clone())
                        .collect();
                    for status in statuses {
                        status.sample();
                    }
                }
                _ = stopping.wait_for(|stop| *stop) => return,
            }
        }
    }

    /// Stops every chain the graceful way and waits for the tasks.
    pub async fn shutdown(&self) {
        let _ = self.stopping.send(true);

        let tasks: Vec<JoinHandle<()>> = {
            let mut chains = self.lock();
            chains
                .values_mut()
                .filter_map(|entry| {
                    entry.keep_running.store(false, Ordering::SeqCst);
                    entry.cancel_current();
                    entry.task.take()
                })
                .collect()
        };

        for task in tasks {
            let _ = task.await;
        }
    }
}

/// Resolves when the chain is told not to start again. Polling, because the
/// flag is shared with the command handlers, which hold no channel.
async fn wait_for_stop(keep_running: Arc<AtomicBool>) {
    while keep_running.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `/readyz` staleness of a chain's metrics handle. The same value the
/// single-chain pipeline uses.
const READY_STALENESS: Duration = Duration::from_secs(120);

fn is_lease_held(error: &anyhow::Error) -> bool {
    crate::pipeline::lease::LeaseHeldElsewhere::is_cause_of(error)
}

/// The only name this project gives a chain id (`configs::NAMED_CHAINS`).
fn chain_name(chain: u64) -> Option<&'static str> {
    (chain == crate::pipeline::solana::SOLANA_CHAIN_ID).then_some("solana")
}

/// What a value is replaced by when the browser may know THAT it is set
/// but not what it is.
pub const SET_MARKER: &str = "<set>";

/// What the browser is allowed to see of a chain's settings.
///
/// No endpoint and no secret is editable from the panel any anymore
/// (review MAJOR 4), so in normal operation there is nothing here to hide.
/// The guard stays for two cases, and it hides rather than redacts:
///
/// * a setting this build does not know - a row written by an older
///   version, which can still name the HyperSync endpoint or the RPC urls;
/// * any setting a future edit marks `secret`.
///
/// It replaces the value with a fixed marker instead of running
/// `redact_urls` over it, because `redact_urls` only matches
/// `scheme://...`: a QuickNode- or Alchemy-style key pasted WITHOUT the
/// scheme (`host.example/TOKEN-abcdef/`) went to the browser in clear
/// (review MINOR 2). A marker cannot leak by omission.
pub fn redact_settings(settings: &ChainSettings) -> ChainSettings {
    settings
        .iter()
        .map(|(key, value)| {
            let known = crate::configs::CHAIN_SETTINGS
                .iter()
                .find(|setting| setting.name == key);

            let hide = match known {
                Some(setting) => setting.secret,
                // Unknown to this build: assume the worst about it.
                None => true,
            };

            let value = if hide && !value.trim().is_empty() {
                SET_MARKER.to_string()
            } else {
                value.clone()
            };

            (key.clone(), value)
        })
        .collect()
}

/// The process-wide settings, as the panel is allowed to see them: whether
/// each one is set, and never what it is.
///
/// These are the endpoints and credentials the review's MAJOR 4 was about.
/// They come from the fleet process's own flags and environment and the
/// panel cannot change any of them; it shows them so the owner can see how
/// the process was started without having to read a compose file.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessView {
    /// `<set>` - never the url, which carries a password.
    pub database: &'static str,
    /// `<set>` or `not set`.
    pub hypersync_token: &'static str,
    pub rpc: &'static str,
    pub redis: &'static str,
    pub metrics_addr: Option<String>,
    /// Megabytes the whole fleet may buffer before writing.
    pub max_inflight_mb: u64,
    /// Metered Solana queries a minute, shared by every Solana chain.
    pub solana_queries_per_minute: u32,
    /// Always true, so the page can say so plainly.
    pub read_only: bool,
}

fn shown(value: Option<&str>) -> &'static str {
    match value {
        Some(value) if !value.trim().is_empty() => SET_MARKER,
        _ => "not set",
    }
}

impl ProcessView {
    pub fn of(config: &FleetConfig) -> Self {
        Self {
            database: shown(Some(&config.database_url)),
            hypersync_token: shown(Some(&config.hypersync_token)),
            rpc: shown(config.rpc_url.as_deref()),
            redis: shown(config.redis_url.as_deref()),
            // An ip:port is not a secret and is useful to see.
            metrics_addr: config.metrics_addr.map(|addr| addr.to_string()),
            max_inflight_mb: config.max_inflight_mb,
            solana_queries_per_minute: config.solana_queries_per_minute,
            read_only: true,
        }
    }
}

/// One row of `GET /api/chains`.
#[derive(Debug, Clone, Serialize)]
pub struct ChainView {
    pub chain: u64,
    /// `solana`, or nothing: EVM chains are known by their id.
    pub name: Option<&'static str>,
    /// False = another process indexes this chain. The panel shows it
    /// read-only and offers no button for it.
    pub managed: bool,
    pub desired: &'static str,
    /// Secrets replaced, never the values themselves.
    pub settings: ChainSettings,
    /// The host the other process runs on, for a chain that is not ours.
    pub host: Option<String>,
    #[serde(flatten)]
    pub live: LiveView,
}

/// Why a panel command was refused. Every variant is safe to show a
/// browser.
#[derive(Debug)]
pub enum CommandError {
    NoSuchChain(u64),
    AlreadyThere(u64),
    Settings(SettingError),
}

impl From<SettingError> for CommandError {
    fn from(error: SettingError) -> Self {
        Self::Settings(error)
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchChain(chain) => write!(
                f,
                "chain {chain} is not in this fleet. Add it first."
            ),
            Self::AlreadyThere(chain) => {
                write!(f, "chain {chain} is already in this fleet.")
            }
            Self::Settings(error) => write!(f, "{error}"),
        }
    }
}

/// The wait a restart announces, in words ("2 seconds", "5 minutes").
pub fn backoff_in_words(failures: u32) -> String {
    human_duration(restart_backoff(failures))
}

#[cfg(test)]
mod backoff_tests {
    use super::*;

    #[test]
    fn the_wait_doubles_and_stops_at_five_minutes() {
        assert_eq!(restart_backoff(1), Duration::from_secs(2));
        assert_eq!(restart_backoff(2), Duration::from_secs(4));
        assert_eq!(restart_backoff(3), Duration::from_secs(8));
        assert_eq!(restart_backoff(8), Duration::from_secs(256));
        assert_eq!(restart_backoff(9), MAX_RESTART_BACKOFF);
        // No overflow, ever: a chain that failed for a week still waits
        // five minutes and not an eternity.
        assert_eq!(restart_backoff(u32::MAX), MAX_RESTART_BACKOFF);
        assert_eq!(backoff_in_words(1), "2 seconds");
    }

    #[test]
    fn a_lease_held_elsewhere_is_rechecked_soon_not_backed_off() {
        assert!(LEASE_RECHECK < MAX_RESTART_BACKOFF);
    }

    /// Review MINOR 2: a key pasted WITHOUT a scheme used to slip through,
    /// because the url redactor only matched `scheme://...`. A fixed marker
    /// cannot leak by omission.
    #[test]
    fn a_value_the_browser_may_not_see_is_replaced_wholesale() {
        let settings: ChainSettings = [
            // Not a setting this build knows: a row from an older version.
            ("rpc", "eth-mainnet.example/v2/hunter2-key"),
            ("hypersync-url", "host.example/TOKEN-abcdef123456/"),
            // One it does.
            ("confirmations", "12"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let shown = redact_settings(&settings);

        assert_eq!(shown["rpc"], SET_MARKER);
        assert_eq!(shown["hypersync-url"], SET_MARKER);
        for value in shown.values() {
            assert!(!value.contains("hunter2-key"), "{shown:?}");
            assert!(!value.contains("TOKEN-abcdef123456"), "{shown:?}");
        }

        // What the owner IS allowed to see is untouched.
        assert_eq!(shown["confirmations"], "12");
    }
}
