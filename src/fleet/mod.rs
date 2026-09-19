//! `indexer fleet`: one process, many chains, with a control panel
//! (docs/design.md section 15).
//!
//! ```text
//!   migrations (ONCE, before any chain starts)
//!     -> fleet_chains, read ONCE: which chains, which settings
//!        -> one tokio task per chain, each calling the SAME
//!           pipeline::run_with / pipeline::solana::run_with that
//!           `indexer run` calls
//!             -> per-chain cancellation through Runtime::shutdown,
//!                which is the graceful ctrl-c path
//!             -> restart with exponential backoff, capped at 5 minutes
//!     -> one /metrics for every chain, each series labelled `chain`
//!     -> the control panel (src/admin), off unless ADMIN_PASSWORD is set
//! ```
//!
//! `indexer run` is not touched by any of this. It builds its own metrics
//! handle, serves its own endpoint, uses a no-op status sink and gives the
//! pipeline the process signal as its shutdown future, exactly as before.
//!
//! See `README.md` next to this file.

pub mod budgets;
pub mod chains;
pub mod metrics;
pub mod status;
pub mod supervisor;

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod integration_tests;

use crate::{
    configs::{ChainSettings, FleetConfig, SettingError},
    db::{migrate, Database},
    metrics::Metrics,
    pipeline::{
        self,
        lease::LeaseOptions,
        solana::{SolanaRuntime, SOLANA_CHAIN_ID},
        status::StatusSink,
        workers::WorkerOptions,
        Runtime,
    },
    source::{evm::Source, solana::SolanaSource},
    tokens,
};
use anyhow::{Context, Result};
use budgets::Budgets;
use chains::{DesiredChain, ForeignChain};
use futures::future::BoxFuture;
use log::{error, info};
use metrics::FleetExposition;
use std::sync::Arc;
use supervisor::{ChainRunner, DesiredStore, Supervisor};

/// The chain id the fleet's own bookkeeping connection uses. It reads and
/// writes `fleet_chains` and `indexer_instances`, neither of which is
/// scoped to one chain, and it never writes chain data.
const BOOKKEEPING_CHAIN: u64 = 0;

/// Runs the fleet until the process is asked to stop.
pub async fn run(config: FleetConfig) -> Result<()> {
    // Once, before any chain starts: fifty chains racing the same
    // migrations is exactly what the migrator's concurrency notes describe,
    // and there is no reason to make them do it inside one process.
    if config.no_migrate {
        info!("Skipping schema migrations (--no-migrate).");
    } else {
        migrate::run(&config.database_url)
            .await
            .context("apply schema migrations")?;
    }

    let db = Database::new(&config.database_url, BOOKKEEPING_CHAIN)
        .await
        .context("connect to the database")?;

    // ONE set of provider budgets for the process: the supervisor splits
    // the memory cap with it, the runner hands its Solana half to every
    // Solana chain.
    let budgets = Arc::new(Budgets::new(
        config.max_inflight_mb,
        config.solana_queries_per_minute,
    ));

    let runner =
        Arc::new(PipelineRunner::new(config.clone(), budgets.clone()));
    let store = Arc::new(ClickhouseStore {
        db: db.clone(),
        lease_ttl_ms: u64::try_from(
            LeaseOptions::default().ttl.as_millis(),
        )
        .unwrap_or(35_000),
    });

    let supervisor =
        Supervisor::with_budgets(config.clone(), runner, store, budgets);
    supervisor.load_and_start().await?;

    tokio::spawn(supervisor.clone().sample_forever());

    // ONE endpoint for every chain. The hand-rolled server of `indexer
    // run` is reused; only what it renders is different.
    let metrics_server = match config.metrics_addr {
        Some(addr) => {
            let server = crate::metrics::bind_exposition(
                addr,
                FleetExposition::new(&supervisor),
            )
            .await
            .with_context(|| format!("bind --metrics-addr {addr}"))?;
            info!(
                "Serving metrics for every chain on http://{}/metrics.",
                server.local_addr()?
            );
            Some(server)
        }
        None => None,
    };

    let (stop_servers, servers_stopped) =
        tokio::sync::watch::channel(false);

    if let Some(server) = metrics_server {
        let mut stopped = servers_stopped.clone();
        tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            };
            if let Err(e) = server.run(shutdown).await {
                error!("Metrics server stopped: {e:#}");
            }
        });
    }

    // The control panel. Off - not even bound - when no password is set.
    let admin = crate::admin::start(supervisor.clone(), {
        let mut stopped = servers_stopped.clone();
        Box::pin(async move {
            let _ = stopped.wait_for(|stop| *stop).await;
        })
    })
    .await?;

    pipeline::shutdown_signal().await;
    info!("Shutdown requested; stopping every chain.");

    supervisor.shutdown().await;
    let _ = stop_servers.send(true);
    if let Some(admin) = admin {
        let _ = admin.await;
    }

    Ok(())
}

// ------------------------------------------------- the production runner

/// Starts a chain through the same entry points `indexer run` uses.
struct PipelineRunner {
    config: FleetConfig,
    budgets: Arc<Budgets>,
}

impl PipelineRunner {
    fn new(config: FleetConfig, budgets: Arc<Budgets>) -> Self {
        Self { config, budgets }
    }
}

impl ChainRunner for PipelineRunner {
    fn validate(
        &self,
        chain: u64,
        settings: &ChainSettings,
    ) -> Result<(), SettingError> {
        self.config.chain_config(chain, settings).map(|_| ())
    }

    fn run(
        &self,
        chain: u64,
        settings: ChainSettings,
        flush_rows: usize,
        metrics: Metrics,
        status: StatusSink,
        shutdown: BoxFuture<'static, ()>,
    ) -> BoxFuture<'static, Result<()>> {
        let fleet = self.config.clone();
        let budgets = self.budgets.clone();

        Box::pin(async move {
            let mut config = fleet
                .chain_config(chain, &settings)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            // The fleet's share of the memory cap never RAISES what the
            // chain asked for, it only lowers it.
            config.flush_rows = config.flush_rows.min(flush_rows);

            if chain == SOLANA_CHAIN_ID {
                let source = SolanaSource::new(
                    config.hypersync_url.as_deref(),
                    &config.hypersync_token,
                )?;

                let mut runtime = SolanaRuntime::new(source);
                runtime.shutdown = shutdown;
                runtime.metrics = Some(metrics);
                runtime.status = status;
                // One token, one budget: every Solana chain in the process
                // draws from the same allowance.
                runtime.budget = Some(budgets.solana());

                pipeline::solana::run_with(config, runtime).await
            } else {
                let source = Source::new(
                    config.chain_id,
                    config.hypersync_url.as_deref(),
                    &config.hypersync_token,
                )?;

                if config.hypersync_url.is_some() {
                    source.verify_chain_id(config.chain_id).await?;
                }

                let caller = tokens::build_caller_shared(
                    config.chain_id,
                    config.rpc_url.as_deref(),
                    config.redis_url.as_deref(),
                )
                .await
                .context("set up the RPC endpoints (--rpc)")?;

                let runtime = Runtime {
                    canonical: Arc::new(source.clone()),
                    source,
                    caller,
                    workers: WorkerOptions::default(),
                    lease: LeaseOptions::default(),
                    shutdown,
                    metrics: Some(metrics),
                    status,
                };

                pipeline::run_with(config, runtime).await
            }
        })
    }
}

// ------------------------------------------------ the desired state store

struct ClickhouseStore {
    db: Database,
    lease_ttl_ms: u64,
}

impl DesiredStore for ClickhouseStore {
    fn load(&self) -> BoxFuture<'_, Result<Vec<DesiredChain>>> {
        Box::pin(chains::load(&self.db))
    }

    fn save(&self, chain: DesiredChain) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { chains::save(&self.db, &chain).await })
    }

    fn foreign(
        &self,
        mine: Vec<u64>,
    ) -> BoxFuture<'_, Result<Vec<ForeignChain>>> {
        Box::pin(async move {
            chains::foreign(&self.db, self.lease_ttl_ms, &mine).await
        })
    }
}

/// Logged once at start, so an operator who never opens the panel still
/// sees what the process is doing.
pub fn describe(config: &FleetConfig) -> String {
    let mut lines = vec![format!(
        "Fleet mode: one process, many chains. Memory cap {} MB, Solana \
         query budget {}/minute (shared).",
        config.max_inflight_mb, config.solana_queries_per_minute
    )];

    match config.metrics_addr {
        Some(addr) => lines.push(format!(
            "Metrics for every chain: http://{addr}/metrics"
        )),
        None => lines.push(
            "Metrics are off (--metrics-addr is unset).".to_string(),
        ),
    }

    if std::env::var_os(crate::configs::ADMIN_PASSWORD_ENV).is_some() {
        lines.push(format!(
            "Control panel: http://{}/ (password from {})",
            config.admin_addr,
            crate::configs::ADMIN_PASSWORD_ENV
        ));
    } else {
        lines.push(format!(
            "Control panel is OFF: set {} to switch it on.",
            crate::configs::ADMIN_PASSWORD_ENV
        ));
    }

    lines.join(" ")
}
