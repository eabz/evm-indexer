//! The background workers (token metadata, DEX pools) and their ClickHouse
//! side. Nothing in here is on the commit path: the pipeline only ever
//! calls the non-blocking `discover`s, the workers resolve over RPC and
//! insert their rows themselves, and a database backfill finds whatever
//! the live path missed (docs/design.md, sections 4 and 5).

use crate::{
    db::{models::token::DatabaseToken, Database},
    dex::{
        self, DexPool, MissingPoolSource, PoolCandidate, PoolSink,
        PoolWorker, PoolWorkerOptions, PoolWorkerStats,
    },
    metrics::{Metrics, WorkerStatsSnapshot},
    pipeline::modules::{EnabledModules, ModuleRows},
    tokens::{
        multicall::EthCaller, MissingTokenSource, TokenSink, TokenStandard,
        TokenWorker, TokenWorkerOptions, TokenWorkerStats,
    },
    utils::format::{SerAddress, SerB256},
};
use alloy::primitives::{Address, B256};
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::{info, warn};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::task::JoinHandle;

/// How often the workers' statistics are copied into the metrics.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

/// Pool ids per `known_pools` query (`IN (...)` list).
const KNOWN_POOLS_CHUNK: usize = 1_000;

/// The ClickHouse side of both workers.
#[derive(Clone)]
pub struct ClickhouseWorkerStore {
    db: Database,
    /// Include the tokens of `dex_pools` in the token backfill.
    dex: bool,
    /// Rotates the order of the token backfill, see [`missing_tokens_sql`].
    salt: Arc<AtomicU64>,
}

impl ClickhouseWorkerStore {
    pub fn new(db: Database, dex: bool) -> Self {
        Self { db, dex, salt: Arc::new(AtomicU64::new(0)) }
    }
}

/// Token addresses referenced by stored data without a `tokens` row.
///
/// Cost: this runs for the lifetime of a multi-billion-row database, so it
/// never touches a transfer table. It reads `seen_tokens` (one row per
/// token, fed by materialized views of the three transfer tables,
/// migration 0005) and `dex_pools_by_token`, both partitioned by chain and
/// a few million rows at most, and anti-joins them against the chain's
/// `tokens` keys.
///
/// Order: deterministic for a given `salt` (a hash of the address), and
/// the salt changes with every call. A fixed order would put the same
/// addresses first forever; addresses that can not be resolved right now
/// (no code yet, RPC trouble) would then starve everything behind them.
/// No `FINAL` anywhere: duplicates collapse in the `GROUP BY`, `tokens` is
/// never tombstoned, and resolving the token of a reorged-out transfer is
/// harmless.
pub fn missing_tokens_sql(
    chain: u64,
    dex: bool,
    salt: u64,
    limit: usize,
) -> String {
    let pools = if dex {
        format!(
            " UNION ALL SELECT token AS address, 'ERC20' AS type \
             FROM dex_pools_by_token WHERE chain = {chain} \
             AND source != 'unresolved'"
        )
    } else {
        String::new()
    };

    format!(
        "SELECT address, any(type) AS type FROM (\
         SELECT address, toString(type) AS type FROM seen_tokens \
         WHERE chain = {chain}{pools}) \
         WHERE address NOT IN (\
         SELECT address FROM tokens WHERE chain = {chain}) \
         AND address NOT IN (\
         unhex('0000000000000000000000000000000000000000'), \
         unhex('eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee')) \
         GROUP BY address \
         ORDER BY cityHash64(address, {salt}) \
         LIMIT {limit}"
    )
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct MissingTokenRow {
    #[serde_as(as = "SerAddress")]
    address: Address,
    r#type: String,
}

fn standard_of(label: &str) -> TokenStandard {
    match label {
        "ERC721" => TokenStandard::Erc721,
        "ERC1155" => TokenStandard::Erc1155,
        _ => TokenStandard::Erc20,
    }
}

impl TokenSink for ClickhouseWorkerStore {
    fn insert_tokens<'a>(
        &'a self,
        rows: &'a [DatabaseToken],
    ) -> BoxFuture<'a, Result<()>> {
        // The normal insert path (retries, metrics). Idempotent: `tokens`
        // is a ReplacingMergeTree keyed by (chain, address).
        Box::pin(self.db.insert_rows("tokens", rows))
    }
}

impl MissingTokenSource for ClickhouseWorkerStore {
    fn missing_tokens<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(Address, TokenStandard)>>> {
        Box::pin(async move {
            let salt = self.salt.fetch_add(1, Ordering::Relaxed);
            let sql =
                missing_tokens_sql(self.db.chain_id, self.dex, salt, limit);

            let rows = self
                .db
                .db
                .query(&sql)
                .fetch_all::<MissingTokenRow>()
                .await
                .context("query tokens without metadata")?;

            Ok(rows
                .into_iter()
                .map(|row| (row.address, standard_of(&row.r#type)))
                .collect())
        })
    }
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct PoolIdRow {
    #[serde_as(as = "SerB256")]
    pool_id: B256,
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct MissingPoolRow {
    #[serde_as(as = "SerB256")]
    pool_id: B256,
    #[serde_as(as = "SerAddress")]
    emitter: Address,
    protocol: String,
}

impl PoolSink for ClickhouseWorkerStore {
    fn known_pools<'a>(
        &'a self,
        pool_ids: &'a [B256],
    ) -> BoxFuture<'a, Result<HashSet<B256>>> {
        Box::pin(async move {
            let mut known = HashSet::new();

            for chunk in pool_ids.chunks(KNOWN_POOLS_CHUNK) {
                let ids: Vec<String> = chunk
                    .iter()
                    .map(|id| format!("unhex('{}')", hex_of(id.as_slice())))
                    .collect();

                // `FINAL`: a tombstoned pool is not known.
                let sql = format!(
                    "SELECT DISTINCT pool_id FROM dex_pools FINAL \
                     WHERE chain = {} AND pool_id IN ({})",
                    self.db.chain_id,
                    ids.join(", ")
                );

                let rows = self
                    .db
                    .db
                    .query(&sql)
                    .fetch_all::<PoolIdRow>()
                    .await
                    .context("query known pools")?;

                known.extend(rows.into_iter().map(|row| row.pool_id));
            }

            Ok(known)
        })
    }

    fn insert_pools<'a>(
        &'a self,
        rows: &'a [DexPool],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.db.insert_rows(dex::POOLS_TABLE, rows))
    }
}

impl MissingPoolSource for ClickhouseWorkerStore {
    fn missing_pools<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<PoolCandidate>>> {
        Box::pin(async move {
            let sql = dex::MISSING_POOLS_SQL
                .replace("{chain}", &self.db.chain_id.to_string())
                .replace("{limit}", &limit.to_string());

            let rows = self
                .db
                .db
                .query(&sql)
                .fetch_all::<MissingPoolRow>()
                .await
                .context("query pools without a dex_pools row")?;

            Ok(rows
                .into_iter()
                .filter_map(|row| {
                    // A family this binary does not know: skip, never fail.
                    let protocol = row.protocol.parse().ok()?;
                    Some(PoolCandidate {
                        pool_id: row.pool_id,
                        address: row.emitter,
                        protocol,
                    })
                })
                .collect())
        })
    }
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `tokens::TokenWorkerStats` as the metrics see it.
///
/// `TokenWorkerStats::resolved` counts every row the RPC produced,
/// INCLUDING the negative ones; the metric `resolver_resolved_total` means
/// "with metadata", so the negatives are taken out here and reported only
/// as `resolver_negative_total`.
pub fn token_stats_snapshot(stats: &TokenWorkerStats) -> WorkerStatsSnapshot {
    WorkerStatsSnapshot {
        queue_depth: stats.queue_depth as u64,
        resolved: stats.resolved.saturating_sub(stats.negative),
        negative: stats.negative,
        codeless: stats.codeless,
        dropped: stats.dropped,
        inserted: stats.inserted,
        insert_failures: stats.insert_failures,
        rpc_failures: stats.rpc_failures,
        backfill_found: stats.backfill_found,
        backfill_failures: stats.backfill_failures,
        cache_hits: stats.cache_hits,
        cache_misses: stats.cache_misses,
        breaker_open: stats.breaker_open,
        endpoints_total: stats.endpoints_total as u64,
        endpoints_healthy: stats.endpoints_healthy as u64,
    }
}

/// `dex::PoolWorkerStats` as the metrics see it (`resolved` and `negative`
/// are already disjoint there). The pool worker shares the token worker's
/// RPC caller, whose endpoint health is reported by the `tokens` series.
pub fn pool_stats_snapshot(stats: &PoolWorkerStats) -> WorkerStatsSnapshot {
    WorkerStatsSnapshot {
        queue_depth: stats.queue_depth as u64,
        resolved: stats.resolved,
        negative: stats.negative,
        codeless: stats.codeless,
        dropped: stats.dropped,
        inserted: stats.inserted,
        insert_failures: stats.insert_failures,
        rpc_failures: stats.rpc_failures,
        backfill_found: stats.backfill_found,
        backfill_failures: stats.backfill_failures,
        cache_hits: stats.already_known,
        cache_misses: stats.queued,
        breaker_open: stats.breaker_open,
        endpoints_total: 0,
        endpoints_healthy: 0,
    }
}

/// Tunables of both workers (the defaults in production).
#[derive(Debug, Clone, Default)]
pub struct WorkerOptions {
    pub tokens: TokenWorkerOptions,
    pub pools: PoolWorkerOptions,
}

/// Handles of the background workers.
pub struct Workers {
    tokens: TokenWorker,
    tokens_task: JoinHandle<()>,
    /// `None` with `--no-dex`.
    pools: Option<(PoolWorker, JoinHandle<()>)>,
    stats_task: JoinHandle<()>,
}

impl Workers {
    /// Starts the workers over ONE shared RPC caller (`None` = `--rpc
    /// none`: both are inert and nothing is resolved). Sync, no I/O.
    pub fn spawn(
        db: &Database,
        caller: Option<Arc<dyn EthCaller>>,
        redis_url: Option<&str>,
        enabled: EnabledModules,
        metrics: Metrics,
        options: WorkerOptions,
    ) -> Result<Self> {
        let store = Arc::new(ClickhouseWorkerStore::new(db.clone(), enabled.dex));

        if caller.is_none() {
            info!(
                "No RPC: token metadata{} will not be resolved.",
                if enabled.dex { " and DEX pool tokens" } else { "" }
            );
        }

        let (tokens, tokens_task) = TokenWorker::spawn(
            db.chain_id,
            caller.clone(),
            redis_url,
            store.clone(),
            Some(store.clone()),
            options.tokens,
        )
        .context("start the token worker")?;

        let pools = enabled.dex.then(|| {
            let (worker, task) = PoolWorker::spawn(
                db.chain_id,
                caller.clone(),
                store.clone(),
                Some(store.clone()),
                options.pools,
            );
            worker.set_epoch(db.epoch());
            (worker, task)
        });

        let stats_task = {
            let tokens = tokens.clone();
            let pools = pools.as_ref().map(|(worker, _)| worker.clone());
            let rpc = caller.is_some();

            tokio::spawn(async move {
                // A disabled worker has no series at all.
                if !metrics.is_enabled() || !rpc {
                    return;
                }

                let mut tick = tokio::time::interval(STATS_INTERVAL);
                loop {
                    tick.tick().await;
                    metrics
                        .set_token_stats(token_stats_snapshot(&tokens.stats()));
                    if let Some(pools) = &pools {
                        metrics.set_pool_stats(pool_stats_snapshot(
                            &pools.stats(),
                        ));
                    }
                }
            })
        };

        Ok(Self { tokens, tokens_task, pools, stats_task })
    }

    /// Cheap handle for the code that discovers (the sync loop, the sink).
    pub fn discovery(&self) -> Discovery {
        Discovery {
            tokens: self.tokens.clone(),
            pools: self.pools.as_ref().map(|(worker, _)| worker.clone()),
        }
    }

    pub fn token_stats(&self) -> TokenWorkerStats {
        self.tokens.stats()
    }

    pub fn pool_stats(&self) -> Option<PoolWorkerStats> {
        self.pools.as_ref().map(|(worker, _)| worker.stats())
    }

    /// Stops the workers. Call AFTER the final flush: the order of a
    /// graceful shutdown is stop the stream -> final flush -> this.
    pub async fn shutdown(self) {
        self.stats_task.abort();

        self.tokens.shutdown().await;
        if let Err(e) = self.tokens_task.await {
            warn!("Token worker task ended abnormally: {e}");
        }

        if let Some((worker, task)) = self.pools {
            worker.shutdown().await;
            if let Err(e) = task.await {
                warn!("Pool worker task ended abnormally: {e}");
            }
        }
    }
}

/// What the pipeline tells the workers. Every method is synchronous and
/// non-blocking (bounded queues, drop-on-full: the database backfill finds
/// what was dropped).
#[derive(Clone)]
pub struct Discovery {
    tokens: TokenWorker,
    pools: Option<PoolWorker>,
}

impl Discovery {
    /// Token contracts seen by transform. Never awaits.
    pub fn tokens_seen(&self, seen: &HashMap<Address, TokenStandard>) {
        if !seen.is_empty() {
            self.tokens.discover(seen);
        }
    }

    /// Module rows that were just stored: pools that traded without a
    /// creation event in the batch go to the pool resolver, pools created
    /// in it are marked as known.
    pub fn stored(&self, modules: &ModuleRows) {
        let Some(pools) = &self.pools else { return };

        if modules.dex.is_empty() {
            return;
        }

        pools.mark_known(modules.dex.pools.iter().map(|pool| pool.pool_id));
        pools.discover(&modules.dex.pool_candidates());
    }

    /// A purge adopted a new epoch: resolver rows written from now on
    /// carry it.
    pub fn set_epoch(&self, epoch: u32) {
        if let Some(pools) = &self.pools {
            pools.set_epoch(epoch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_backfill_never_reads_a_transfer_table() {
        for dex in [false, true] {
            let sql = missing_tokens_sql(137, dex, 7, 500);

            assert!(sql.contains("FROM seen_tokens WHERE chain = 137"));
            assert!(sql.contains("FROM tokens WHERE chain = 137"));
            assert!(sql.contains("LIMIT 500"));
            assert!(sql.contains("cityHash64(address, 7)"));
            assert_eq!(sql.contains("dex_pools_by_token"), dex);

            for big in ["erc20_transfers", "erc721_transfers", "logs"] {
                assert!(!sql.contains(big), "{big}: {sql}");
            }
        }
    }

    #[test]
    fn negatives_are_not_counted_twice_in_the_token_series() {
        let stats = TokenWorkerStats {
            // 10 rows produced by the RPC, 3 of them negative.
            resolved: 10,
            negative: 3,
            codeless: 2,
            inserted: 10,
            backfill_found: 4,
            endpoints_total: 5,
            endpoints_healthy: 4,
            ..Default::default()
        };

        let snapshot = token_stats_snapshot(&stats);

        assert_eq!(snapshot.resolved, 7);
        assert_eq!(snapshot.negative, 3);
        assert_eq!(snapshot.resolved + snapshot.negative, stats.resolved);
        assert_eq!(snapshot.codeless, 2);
        assert_eq!(snapshot.backfill_found, 4);
        assert_eq!(
            (snapshot.endpoints_total, snapshot.endpoints_healthy),
            (5, 4)
        );

        // The pool worker's counters are already disjoint.
        let pools = pool_stats_snapshot(&PoolWorkerStats {
            resolved: 10,
            negative: 3,
            ..Default::default()
        });
        assert_eq!((pools.resolved, pools.negative), (10, 3));
    }

    #[test]
    fn standards_round_trip_through_the_type_label() {
        for standard in [
            TokenStandard::Erc20,
            TokenStandard::Erc721,
            TokenStandard::Erc1155,
        ] {
            assert_eq!(standard_of(standard.as_str()), standard);
        }
        assert_eq!(standard_of("garbage"), TokenStandard::Erc20);
    }
}
