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
    predictions::{
        self, MissingVenueSource, PredictionVenue, VenueCandidate,
        VenueSink, VenueWorker, VenueWorkerOptions, VenueWorkerStats,
    },
    tokens::{
        multicall::EthCaller, MissingTokenSource, TokenSink,
        TokenStandard, TokenWorker, TokenWorkerOptions, TokenWorkerStats,
    },
    utils::format::{id32, SerAddress, SerB256, SerId32},
};
use alloy::primitives::{Address, B256};
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::{info, warn};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
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
}

impl ClickhouseWorkerStore {
    pub fn new(db: Database, dex: bool) -> Self {
        Self { db, dex }
    }
}

/// `AND <column> > <cursor>`, where `column` is an EXPRESSION yielding the
/// 20 address bytes (a `FixedString(20)` column, or `substring(id, 13, 20)`
/// of a 32 byte identity column). Comparing a padded 32 byte id with a 20
/// byte literal is always false, which silently dropped every page after
/// the first.
fn after_predicate(column: &str, after: Option<Address>) -> String {
    after
        .map(|after| {
            format!(
                " AND {column} > unhex('{}')",
                hex_of(after.as_slice())
            )
        })
        .unwrap_or_default()
}

/// Token addresses referenced by stored data without a `tokens` row, by
/// address, after the cursor.
///
/// Cost: this runs for the lifetime of a multi-billion-row database, so it
/// never touches a transfer table. It reads `seen_tokens` (one row per
/// token, fed by materialized views of the three transfer tables,
/// migration 0005) and `dex_pools_by_token`, both partitioned by chain,
/// sorted by address and a few million rows at most, and anti-joins them
/// against the chain's `tokens` keys.
///
/// Order: stable (`ORDER BY address`), paged by the worker with the
/// `after` cursor, so tokens that can not be resolved right now never hide
/// the ones behind them. No `FINAL` anywhere: duplicates collapse in the
/// `GROUP BY`, `tokens` is never tombstoned, and resolving the token of a
/// reorged-out transfer is harmless.
pub fn missing_tokens_sql(
    chain: u64,
    dex: bool,
    after: Option<Address>,
    limit: usize,
) -> String {
    let seen_after = after_predicate("address", after);

    // `dex_pools_by_token.token` is a 32 byte identity column
    // (docs/design.md section 13) while `seen_tokens.address` and
    // `tokens.address` are `FixedString(20)`. A `UNION ALL` of the two
    // does not error - ClickHouse widens both to `String` - and everything
    // downstream then breaks quietly: the anti-join against `tokens` never
    // matches, the zero / 0xee..ee exclusions miss, the cursor comparison
    // is always false, and the rows arrive length prefixed while the Rust
    // row reads 20 raw bytes (the RowBinary stream desynchronises).
    //
    // So the DEX leg is narrowed to the EVM ids (12 leading zero bytes)
    // and unpadded here. A Solana mint is NOT truncated into an address:
    // it is left out, and its decimals arrive with the SVM data.
    let pools = if dex {
        format!(
            " UNION ALL SELECT toFixedString(substring(token, 13, 20), \
             20) AS address, 'ERC20' AS type FROM dex_pools_by_token \
             WHERE chain = {chain} AND source != 'unresolved' \
             AND substring(token, 1, 12) = toFixedString('', 12){}",
            after_predicate(
                "toFixedString(substring(token, 13, 20), 20)",
                after,
            )
        )
    } else {
        String::new()
    };

    format!(
        "SELECT address, any(type) AS type FROM (\
         SELECT address, toString(type) AS type FROM seen_tokens \
         WHERE chain = {chain}{seen_after}{pools}) \
         WHERE address NOT IN (\
         SELECT address FROM tokens WHERE chain = {chain}{seen_after}) \
         AND address NOT IN (\
         unhex('0000000000000000000000000000000000000000'), \
         unhex('eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee')) \
         GROUP BY address \
         ORDER BY address ASC \
         LIMIT {limit}"
    )
}

/// Tokens whose stored row is blank and older than `older_than_ms` (unix
/// ms; `tokens._version` is the insert time), by address after the cursor.
/// `FINAL`: a blank row that was replaced by a good one is not blank.
pub fn blank_tokens_sql(
    chain: u64,
    after: Option<Address>,
    limit: usize,
    older_than_ms: u64,
) -> String {
    format!(
        "SELECT address, toString(type) AS type FROM tokens FINAL \
         WHERE chain = {chain}{} AND name = '' AND symbol = '' \
         AND decimals = 0 AND _version < {older_than_ms} \
         ORDER BY address ASC LIMIT {limit}",
        after_predicate("address", after)
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

impl ClickhouseWorkerStore {
    async fn token_listing(
        &self,
        sql: String,
        what: &'static str,
    ) -> Result<Vec<(Address, TokenStandard)>> {
        let rows = self
            .db
            .db
            .query(&sql)
            .fetch_all::<MissingTokenRow>()
            .await
            .context(what)?;

        Ok(rows
            .into_iter()
            .map(|row| (row.address, standard_of(&row.r#type)))
            .collect())
    }
}

impl MissingTokenSource for ClickhouseWorkerStore {
    fn missing_tokens<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(Address, TokenStandard)>>> {
        self.missing_tokens_after(None, limit)
    }

    fn missing_tokens_after<'a>(
        &'a self,
        after: Option<Address>,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(Address, TokenStandard)>>> {
        Box::pin(self.token_listing(
            missing_tokens_sql(self.db.chain_id, self.dex, after, limit),
            "query tokens without metadata",
        ))
    }

    fn blank_tokens<'a>(
        &'a self,
        after: Option<Address>,
        limit: usize,
        older_than: Duration,
    ) -> BoxFuture<'a, Result<Vec<(Address, TokenStandard)>>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or_default();
        let older_than_ms =
            now_ms.saturating_sub(older_than.as_millis() as u64);

        Box::pin(self.token_listing(
            blank_tokens_sql(
                self.db.chain_id,
                after,
                limit,
                older_than_ms,
            ),
            "query blank tokens",
        ))
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
    // `dex_pools.emitter` is FixedString(32) (docs/design.md section 13).
    #[serde_as(as = "SerId32")]
    emitter: Address,
    protocol: String,
    attempts: u32,
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
                    .map(|id| {
                        format!("unhex('{}')", hex_of(id.as_slice()))
                    })
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
                        attempts: row.attempts,
                    })
                })
                .collect())
        })
    }
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct ExchangeRow {
    // `prediction_venues.exchange` is FixedString(32).
    #[serde_as(as = "SerId32")]
    exchange: Address,
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct MissingVenueRow {
    // `prediction_trades.exchange` is FixedString(32).
    #[serde_as(as = "SerId32")]
    exchange: Address,
    protocol: String,
}

impl VenueSink for ClickhouseWorkerStore {
    fn known_venues<'a>(
        &'a self,
        exchanges: &'a [Address],
    ) -> BoxFuture<'a, Result<HashSet<Address>>> {
        Box::pin(async move {
            let mut known = HashSet::new();

            for chunk in exchanges.chunks(KNOWN_POOLS_CHUNK) {
                // The column is FixedString(32): a 40 hex literal is a
                // 20 byte value and never matches.
                let ids: Vec<String> = chunk
                    .iter()
                    .map(|a| {
                        format!("unhex('{}')", hex_of(id32(*a).as_slice()))
                    })
                    .collect();

                let sql = format!(
                    "SELECT DISTINCT exchange FROM prediction_venues \
                     WHERE chain = {} AND exchange IN ({})",
                    self.db.chain_id,
                    ids.join(", ")
                );

                let rows = self
                    .db
                    .db
                    .query(&sql)
                    .fetch_all::<ExchangeRow>()
                    .await
                    .context("query known venues")?;

                known.extend(rows.into_iter().map(|row| row.exchange));
            }

            Ok(known)
        })
    }

    fn insert_venues<'a>(
        &'a self,
        rows: &'a [PredictionVenue],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.db.insert_rows("prediction_venues", rows))
    }
}

impl MissingVenueSource for ClickhouseWorkerStore {
    fn missing_venues<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<VenueCandidate>>> {
        Box::pin(async move {
            let sql = predictions::MISSING_VENUES_SQL
                .replace("{chain}", &self.db.chain_id.to_string())
                .replace("{limit}", &limit.to_string());

            let rows = self
                .db
                .db
                .query(&sql)
                .fetch_all::<MissingVenueRow>()
                .await
                .context("query venues without a prediction_venues row")?;

            Ok(rows
                .into_iter()
                .filter_map(|row| {
                    Some(VenueCandidate {
                        exchange: row.exchange,
                        protocol: row.protocol.parse().ok()?,
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
pub fn token_stats_snapshot(
    stats: &TokenWorkerStats,
) -> WorkerStatsSnapshot {
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
        unconfirmed: stats.unconfirmed,
        blank_rechecked: stats.blank_rechecked,
        blank_healed: stats.blank_healed,
        cache_hits: stats.cache_hits,
        cache_misses: stats.cache_misses,
        breaker_open: stats.breaker_open,
        endpoints_total: stats.endpoints_total as u64,
        endpoints_healthy: stats.endpoints_healthy as u64,
        endpoints_distrusted: stats.endpoints_distrusted as u64,
    }
}

/// `dex::PoolWorkerStats` as the metrics see it (`resolved` and `negative`
/// are already disjoint there). The pool worker shares the token worker's
/// RPC caller, whose endpoint health is reported by the `tokens` series.
pub fn pool_stats_snapshot(
    stats: &PoolWorkerStats,
) -> WorkerStatsSnapshot {
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
        ..Default::default()
    }
}

/// `predictions::VenueWorkerStats` as the metrics see it.
pub fn venue_stats_snapshot(
    stats: &VenueWorkerStats,
) -> WorkerStatsSnapshot {
    WorkerStatsSnapshot {
        queue_depth: stats.queue_depth as u64,
        resolved: stats.resolved,
        negative: stats.negative,
        dropped: stats.dropped,
        inserted: stats.inserted,
        insert_failures: stats.insert_failures,
        rpc_failures: stats.rpc_failures,
        backfill_found: stats.backfill_found,
        backfill_failures: stats.backfill_failures,
        cache_hits: stats.already_known,
        cache_misses: stats.queued,
        breaker_open: stats.breaker_open,
        ..Default::default()
    }
}

/// Tunables of both workers (the defaults in production).
#[derive(Debug, Clone, Default)]
pub struct WorkerOptions {
    pub tokens: TokenWorkerOptions,
    pub pools: PoolWorkerOptions,
    pub venues: VenueWorkerOptions,
}

/// Handles of the background workers.
pub struct Workers {
    tokens: TokenWorker,
    tokens_task: JoinHandle<()>,
    /// `None` with `--no-dex`.
    pools: Option<(PoolWorker, JoinHandle<()>)>,
    /// `None` with `--no-predictions`.
    venues: Option<(VenueWorker, JoinHandle<()>)>,
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
        let store =
            Arc::new(ClickhouseWorkerStore::new(db.clone(), enabled.dex));

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

        let venues = enabled.predictions.then(|| {
            VenueWorker::spawn(
                db.chain_id,
                caller.clone(),
                store.clone(),
                Some(store.clone()),
                options.venues,
            )
        });

        let stats_task = {
            let tokens = tokens.clone();
            let pools = pools.as_ref().map(|(worker, _)| worker.clone());
            let venues = venues.as_ref().map(|(worker, _)| worker.clone());
            let rpc = caller.is_some();

            tokio::spawn(async move {
                // A disabled worker has no series at all.
                if !metrics.is_enabled() || !rpc {
                    return;
                }

                let mut tick = tokio::time::interval(STATS_INTERVAL);
                loop {
                    tick.tick().await;
                    metrics.set_token_stats(token_stats_snapshot(
                        &tokens.stats(),
                    ));
                    if let Some(pools) = &pools {
                        metrics.set_pool_stats(pool_stats_snapshot(
                            &pools.stats(),
                        ));
                    }
                    if let Some(venues) = &venues {
                        metrics.set_venue_stats(venue_stats_snapshot(
                            &venues.stats(),
                        ));
                    }
                }
            })
        };

        Ok(Self { tokens, tokens_task, pools, venues, stats_task })
    }

    /// Cheap handle for the code that discovers (the sync loop, the sink).
    pub fn discovery(&self) -> Discovery {
        Discovery {
            tokens: self.tokens.clone(),
            pools: self.pools.as_ref().map(|(worker, _)| worker.clone()),
            venues: self.venues.as_ref().map(|(worker, _)| worker.clone()),
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

        if let Some((worker, task)) = self.venues {
            worker.shutdown().await;
            if let Err(e) = task.await {
                warn!("Venue worker task ended abnormally: {e}");
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
    venues: Option<VenueWorker>,
}

impl Discovery {
    /// Token contracts seen by transform. Never awaits.
    pub fn tokens_seen(&self, seen: &HashMap<Address, TokenStandard>) {
        if !seen.is_empty() {
            self.tokens.discover(seen);
        }
    }

    /// Highest block handed to the writer / the chain head: lets the RPC
    /// layer reject nodes that are behind the indexer. One atomic store.
    pub fn set_head(&self, block: u64) {
        self.tokens.set_head(block);
    }

    /// Module rows that were just stored: pools that traded go to the pool
    /// resolver (creation events are forgeable, the contract is asked
    /// either way); same for prediction market venues.
    pub fn stored(&self, modules: &ModuleRows) {
        if let Some(pools) = &self.pools {
            if !modules.dex.is_empty() {
                pools.discover(&modules.dex.pool_candidates());
            }
        }

        if let Some(venues) = &self.venues {
            if !modules.predictions.is_empty() {
                venues.discover(&modules.predictions.venue_candidates());
            }
        }
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
            let sql = missing_tokens_sql(137, dex, None, 500);

            assert!(sql.contains("FROM seen_tokens WHERE chain = 137"));
            assert!(sql.contains("FROM tokens WHERE chain = 137"));
            assert!(sql.contains("ORDER BY address ASC LIMIT 500"));
            assert_eq!(sql.contains("dex_pools_by_token"), dex);

            for big in ["erc20_transfers", "erc721_transfers", "logs"] {
                assert!(!sql.contains(big), "{big}: {sql}");
            }
        }
    }

    #[test]
    fn token_listings_are_paged_by_address() {
        let after = Address::repeat_byte(0xab);
        let cursor = format!("> unhex('{}')", "ab".repeat(20));

        let sql = missing_tokens_sql(1, true, Some(after), 10);
        assert_eq!(sql.matches(&format!("address {cursor}")).count(), 2);
        // The DEX leg reads a 32 byte identity column: the cursor compares
        // the UNPADDED 20 bytes, or it would never match.
        assert_eq!(
            sql.matches(&format!(
                "toFixedString(substring(token, 13, 20), 20) {cursor}"
            ))
            .count(),
            1
        );
        assert!(sql
            .contains("substring(token, 1, 12) = toFixedString('', 12)"));

        let sql = blank_tokens_sql(1, Some(after), 10, 1_700_000_000_000);
        assert!(sql.contains(&format!("address {cursor}")));
        assert!(sql.contains("FROM tokens FINAL"));
        assert!(sql.contains("name = '' AND symbol = '' AND decimals = 0"));
        assert!(sql.contains("_version < 1700000000000"));
        assert!(sql.contains("ORDER BY address ASC LIMIT 10"));
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
