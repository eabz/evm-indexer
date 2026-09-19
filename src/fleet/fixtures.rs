//! Fakes shared by the supervisor tests and the control panel tests.
//!
//! The point of them is that the whole of fleet mode - start, stop,
//! restart, backoff, "running elsewhere", the panel and its
//! authentication - can be driven without HyperSync, without ClickHouse and
//! without a clock, in the same spirit as `pipeline::sync_tests`, which
//! fakes a `BlockSource` to drive the real sync loop.

use super::{
    chains::{DesiredChain, ForeignChain},
    supervisor::{ChainRunner, DesiredStore, Supervisor},
};
use crate::{
    configs::{ChainSettings, Desired, FleetConfig, SettingError},
    metrics::Metrics,
    pipeline::status::StatusSink,
};
use anyhow::{anyhow, Result};
use futures::future::BoxFuture;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

/// What a fake chain does when it is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    /// Runs until it is cancelled, then returns `Ok` - the graceful path.
    Indexes,
    /// Fails at once with an ordinary error.
    Fails,
    /// Fails at once because another process holds the chain's lease.
    LeaseHeldElsewhere,
    /// Returns `Ok` at once, as `--end-block` would.
    Finishes,
}

#[derive(Default)]
struct RunnerState {
    behaviour: BTreeMap<u64, Behaviour>,
    /// Chains that are inside a `run` call right now.
    live: BTreeMap<u64, bool>,
    /// The flush_rows the supervisor handed each chain last time.
    flush_rows: BTreeMap<u64, usize>,
    settings: BTreeMap<u64, ChainSettings>,
}

/// A [`ChainRunner`] that does exactly what the test tells it to.
pub struct FakeRunner {
    /// Behind an `Arc` so the future `run` returns can keep a handle to it
    /// and mark the chain as finished when it ends.
    state: Arc<Mutex<RunnerState>>,
    /// How often `run` was entered, per chain, since the fleet started.
    starts: Mutex<BTreeMap<u64, u64>>,
    total_starts: AtomicU64,
    /// Settings are judged by the REAL validator, so the panel tests see
    /// the same refusals a real fleet would give.
    config: FleetConfig,
}

impl FakeRunner {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Arc::default(),
            starts: Mutex::default(),
            total_starts: AtomicU64::new(0),
            config: config(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RunnerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn behave(&self, chain: u64, behaviour: Behaviour) {
        self.lock().behaviour.insert(chain, behaviour);
    }

    pub fn starts(&self, chain: u64) -> u64 {
        self.starts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&chain)
            .copied()
            .unwrap_or(0)
    }

    pub fn total_starts(&self) -> u64 {
        self.total_starts.load(Ordering::SeqCst)
    }

    /// Is the chain inside a `run` call at this instant?
    pub fn is_live(&self, chain: u64) -> bool {
        self.lock().live.get(&chain).copied().unwrap_or(false)
    }

    pub fn flush_rows(&self, chain: u64) -> Option<usize> {
        self.lock().flush_rows.get(&chain).copied()
    }

    /// The settings the chain was last STARTED with, which is how the
    /// tests prove that a settings change only applies on the next start.
    pub fn started_with(&self, chain: u64) -> Option<ChainSettings> {
        self.lock().settings.get(&chain).cloned()
    }
}

impl ChainRunner for FakeRunner {
    fn validate(
        &self,
        chain: u64,
        settings: &ChainSettings,
    ) -> Result<(), SettingError> {
        // Exactly what the real runner does: the command line parser
        // decides, so nothing about validation is faked.
        self.config.chain_config(chain, settings).map(|_| ())
    }

    fn run(
        &self,
        chain: u64,
        settings: ChainSettings,
        flush_rows: usize,
        metrics: Metrics,
        _status: StatusSink,
        shutdown: BoxFuture<'static, ()>,
    ) -> BoxFuture<'static, Result<()>> {
        let state = self.state.clone();

        let behaviour = {
            let mut locked = self.lock();
            locked.flush_rows.insert(chain, flush_rows);
            locked.settings.insert(chain, settings);
            locked.live.insert(chain, true);
            locked
                .behaviour
                .get(&chain)
                .copied()
                .unwrap_or(Behaviour::Indexes)
        };

        *self
            .starts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(chain)
            .or_insert(0) += 1;
        self.total_starts.fetch_add(1, Ordering::SeqCst);

        // A chain that indexes looks like one: the head moves, so the
        // panel's numbers and the sampler have something real to read.
        metrics.set_head(1_000);
        metrics.set_indexed_height(1_000);
        metrics.set_ready(true);

        Box::pin(async move {
            let result = match behaviour {
                Behaviour::Fails => {
                    Err(anyhow!("the source is unreachable"))
                }
                Behaviour::LeaseHeldElsewhere => {
                    Err(crate::pipeline::lease::LeaseHeldElsewhere {
                        chain,
                        role: "run".to_string(),
                        by: "instance abcdef12 on another-host"
                            .to_string(),
                    }
                    .into())
                }
                Behaviour::Finishes => Ok(()),
                // The graceful path: run until the supervisor's
                // cancellation handle resolves, exactly as the real
                // pipeline does on ctrl-c.
                Behaviour::Indexes => {
                    shutdown.await;
                    Ok(())
                }
            };

            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .live
                .insert(chain, false);

            result
        })
    }
}

/// A [`DesiredStore`] in a `Mutex`, so the round trip can be watched
/// without a database. The ClickHouse one is covered by
/// `fleet::integration_tests`.
#[derive(Default)]
pub struct MemoryStore {
    rows: Mutex<BTreeMap<u64, DesiredChain>>,
    foreign: Mutex<Vec<ForeignChain>>,
    /// Every write fails: the panel must still work.
    pub broken: std::sync::atomic::AtomicBool,
}

impl MemoryStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn seed(&self, chain: DesiredChain) {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(chain.chain, chain);
    }

    pub fn stored(&self, chain: u64) -> Option<DesiredChain> {
        self.rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&chain)
            .cloned()
    }

    pub fn set_foreign(&self, chains: Vec<ForeignChain>) {
        *self.foreign.lock().unwrap_or_else(|e| e.into_inner()) = chains;
    }
}

impl DesiredStore for MemoryStore {
    fn load(&self) -> BoxFuture<'_, Result<Vec<DesiredChain>>> {
        Box::pin(async move {
            Ok(self
                .rows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect())
        })
    }

    fn save(&self, chain: DesiredChain) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if self.broken.load(Ordering::SeqCst) {
                return Err(anyhow!("ClickHouse is down"));
            }
            self.rows
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(chain.chain, chain);
            Ok(())
        })
    }

    fn foreign(
        &self,
        mine: Vec<u64>,
    ) -> BoxFuture<'_, Result<Vec<ForeignChain>>> {
        Box::pin(async move {
            Ok(self
                .foreign
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|other| !mine.contains(&other.chain))
                .cloned()
                .collect())
        })
    }
}

/// A fleet configuration with nothing real in it.
pub fn config() -> FleetConfig {
    FleetConfig {
        database_url: "http://default:pw@localhost:8123/indexer"
            .to_string(),
        hypersync_token: "00000000-0000-0000-0000-000000000000"
            .to_string(),
        rpc_url: None,
        redis_url: None,
        metrics_addr: None,
        admin_addr: "127.0.0.1:0".parse().unwrap(),
        admin_allow_remote: false,
        admin_secure_cookie: false,
        admin_trust_forwarded_proto: false,
        admin_trusted_proxy: None,
        admin_hosts: Vec::new(),
        chains: Vec::new(),
        max_inflight_mb: 1_024,
        solana_queries_per_minute: 25,
        no_migrate: true,
        debug: false,
    }
}

pub fn settings(pairs: &[(&str, &str)]) -> ChainSettings {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

pub fn desired(chain: u64, desired: Desired) -> DesiredChain {
    DesiredChain { chain, desired, settings: ChainSettings::new() }
}

/// A supervisor over the fakes, with the chains already seeded.
pub fn fleet(
    chains: &[u64],
) -> (Arc<Supervisor>, Arc<FakeRunner>, Arc<MemoryStore>) {
    let runner = FakeRunner::new();
    let store = MemoryStore::new();

    for chain in chains {
        store.seed(desired(*chain, Desired::Running));
    }

    let supervisor =
        Supervisor::new(config(), runner.clone(), store.clone());

    (supervisor, runner, store)
}

/// Waits (in virtual or real time) until `condition` holds, or gives up.
///
/// Every test below uses it instead of a fixed sleep: the supervisor's work
/// happens in spawned tasks, so "has it happened yet" is the only honest
/// question.
pub async fn until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..2_000 {
        if condition() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {what}");
}
