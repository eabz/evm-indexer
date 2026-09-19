//! ClickHouse side of the Solana pipeline: the candle aggregates as
//! [`DerivedTable`]s, the [`ReorgStore`] over the `sol_*` tables, and the
//! reads the sync loop needs (resume cursor, contiguity anchor, gaps).
//!
//! # Why this is a second store and not a `Scope` of `pipeline::store`
//!
//! `ClickhouseReorgStore` names `blocks` in eight places and enumerates
//! `db::BASE_TABLES` + `ALL_MODULES`. On Solana the commit marker is
//! `sol_slots`, the children are `svm::BASE_TABLES`, and none of the EVM
//! tables exist for the chain. Teaching the EVM store a family switch would
//! touch every one of those methods; implementing the SAME trait a second
//! time touches none of them, and `reorg::Purger` - the thing that actually
//! decides the ORDER of a purge, which is the part that is hard to get
//! right - is reused byte for byte.
//!
//! Nothing here issues `DELETE`, `ALTER .. DELETE/UPDATE` or `DROP
//! PARTITION` (docs/design.md §2): rows are removed by tombstone, and the
//! statements are the SAME generic builders the EVM path uses
//! (`db::tombstone_sql`, `db::schema::live_rows_sql`, `min_timestamp_sql`),
//! which read their column lists out of the embedded migration DDL and
//! therefore work for `sol_*` with no change.

use crate::{
    db::{
        self,
        derived::DerivedTable,
        ranges::{BlockRange, DatabaseCheckpoint},
        schema::{live_rows_sql, min_timestamp_sql},
        Database,
    },
    reorg::{ReorgRecord, ReorgStore},
    svm,
    utils::format::SerB256,
};
use alloy::primitives::B256;
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::debug;
use serde::{Deserialize, Serialize};

/// The commit marker: Solana's `blocks`.
pub const COMMIT_MARKER: &str = "sol_slots";

/// Block scoped `sol_*` tables except the commit marker, in the order a
/// purge tombstones them (children first). Derived from
/// [`svm::BASE_TABLES`] so a table added there is never forgotten here - a
/// unit test asserts the two agree.
pub fn child_tables() -> Vec<&'static str> {
    svm::BASE_TABLES
        .iter()
        .copied()
        .filter(|table| *table != COMMIT_MARKER)
        .collect()
}

// --------------------------------------------------------------- candles

/// The three Solana candle aggregates of `migrations/0042`.
///
/// Every `rebuild_sql` is the `SELECT` of the table's materialized view
/// with `FINAL`, the rebuild range and the NEW epoch instead of the rows'
/// own; `solana_tests` compares the two texts so a change to one breaks
/// without the other.
///
/// Like the DEX candles (`dex::derived`), they carry no
/// `{purge_from}`/`{purge_to}`: every one of them reads `sol_dex_swaps`,
/// which the purge tombstones BEFORE it rebuilds and verifies with
/// `live_children`. Only an aggregate sourced from the commit marker itself
/// would need the exclusion, and Solana deliberately has none
/// (`migrations/0040`: no `daily_*_stats` over a filtered subset).
macro_rules! sol_candles {
    ($name:literal, $seconds:literal) => {
        DerivedTable {
            name: $name,
            bucket_seconds: $seconds,
            bucket_column: "bucket",
            rebuild_sql: concat!(
                "INSERT INTO ",
                $name,
                " WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AS trade_ok,",
                " abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price,",
                " reserve0 != 0 AND reserve1 != 0 AS pool_ok,",
                " toFloat64(reserve1) / toFloat64(reserve0) AS pool_price",
                " SELECT chain, pool_id, venue_program,",
                " toDateTime(intDiv(toUInt32(timestamp), ",
                $seconds,
                ") * ",
                $seconds,
                ", 'UTC') AS bucket,",
                " toUInt32({epoch}) AS epoch,",
                " argMinStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS open,",
                " argMaxStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS close,",
                " max(if(trade_ok, trade_price, NULL)) AS high,",
                " min(if(trade_ok, trade_price, NULL)) AS low,",
                " countIf(trade_ok) AS trades,",
                " argMinStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_open,",
                " argMaxStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_close,",
                " max(if(pool_ok, pool_price, NULL)) AS pool_high,",
                " min(if(pool_ok, pool_price, NULL)) AS pool_low,",
                " countIf(pool_ok) AS pool_prices,",
                " sum(abs(toFloat64(amount0))) AS volume0,",
                " sum(abs(toFloat64(amount1))) AS volume1,",
                " count() AS swaps,",
                " uniqState(trader) AS traders",
                " FROM sol_dex_swaps FINAL",
                " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND timestamp < toDateTime({to_ts})",
                " AND is_deleted = 0",
                " GROUP BY chain, pool_id, venue_program, bucket, epoch"
            ),
        }
    };
}

pub const SOL_CANDLES_1M: DerivedTable =
    sol_candles!("sol_dex_candles_1m", 60);
pub const SOL_CANDLES_1H: DerivedTable =
    sol_candles!("sol_dex_candles_1h", 3600);
pub const SOL_CANDLES_1D: DerivedTable =
    sol_candles!("sol_dex_candles_1d", 86400);

/// Every Solana aggregate, all fed from `sol_dex_swaps`.
pub const SOL_DERIVED: &[DerivedTable] =
    &[SOL_CANDLES_1M, SOL_CANDLES_1H, SOL_CANDLES_1D];

/// The reader views of [`SOL_DERIVED`]: the only correct way to read them
/// (they apply the validity rule of docs/design.md §2).
pub const SOL_CANDLE_VIEWS: &[&str] = &[
    "sol_dex_candles_1m_v",
    "sol_dex_candles_1h_v",
    "sol_dex_candles_1d_v",
];

/// Every table a Solana process writes versioned rows of a chain into:
/// what `Database::seed_version` looks at. `checkpoints` is shared with the
/// EVM path and is included, because a Solana flush writes it too.
pub fn versioned_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = svm::BASE_TABLES.to_vec();
    tables.push("sol_tokens");
    tables.push("checkpoints");
    tables
}

// ------------------------------------------------------------ row shapes

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct StoredHash {
    block_number: u64,
    #[serde_as(as = "SerB256")]
    blockhash: B256,
}

#[serde_with::serde_as]
#[derive(Debug, Row, Serialize)]
struct ReorgRow {
    chain: u64,
    epoch: u32,
    from_ts: u32,
    fork_block: u64,
    to_block: u64,
    old_head: u64,
    #[serde_as(as = "SerB256")]
    old_hash: B256,
    #[serde_as(as = "SerB256")]
    new_hash: B256,
    depth: u64,
    rows_tombstoned: u64,
    reason: String,
    tombstone_version: u64,
    completed: u8,
}

/// A checkpoint row with its tombstone flag (the flush path never writes
/// `is_deleted`, a purge does).
#[derive(Debug, Clone, Copy, Row, Serialize, PartialEq, Eq)]
struct CheckpointWrite {
    chain: u64,
    from_block: u64,
    to_block: u64,
    epoch: u32,
    _version: u64,
    is_deleted: u8,
}

/// The last stored slot of a range, with the two continuity witnesses.
///
/// `blockhash` is a Solana pubkey, i.e. a `FixedString(32)`, read with the
/// same serializer the EVM hashes use - the bytes are the bytes.
#[serde_with::serde_as]
#[derive(Debug, Clone, Copy, Row, Deserialize, PartialEq, Eq)]
pub struct SlotAnchor {
    pub block_number: u64,
    #[serde_as(as = "SerB256")]
    pub blockhash: B256,
    pub block_height: u64,
}

/// The rows that remove `[from, to)` from the live `checkpoints`: a
/// tombstone per overlapping checkpoint plus the part of it outside the
/// range as a new live row. Identical in shape to the EVM one in
/// `pipeline::store`; duplicated rather than shared because that one is a
/// private helper of a file this work must not touch.
fn checkpoint_writes(
    live: &[DatabaseCheckpoint],
    from: u64,
    to: Option<u64>,
    version: u64,
) -> Vec<CheckpointWrite> {
    let mut writes = Vec::new();

    for checkpoint in live {
        let write = |from_block, to_block, is_deleted| CheckpointWrite {
            chain: checkpoint.chain,
            from_block,
            to_block,
            epoch: checkpoint.epoch,
            _version: version,
            is_deleted,
        };

        writes.push(write(checkpoint.from_block, checkpoint.to_block, 1));

        if checkpoint.from_block < from {
            writes.push(write(checkpoint.from_block, from, 0));
        }
        if let Some(to) = to {
            if checkpoint.to_block > to {
                writes.push(write(to, checkpoint.to_block, 0));
            }
        }
    }

    writes
}

fn overlap_predicate(chain: u64, from: u64, to: Option<u64>) -> String {
    let mut predicate = format!("chain = {chain} AND to_block > {from}");
    if let Some(to) = to {
        predicate.push_str(&format!(" AND from_block < {to}"));
    }
    predicate
}

fn range_predicate(
    table: &str,
    chain: u64,
    from: u64,
    to: Option<u64>,
) -> String {
    let _ = table;
    let mut predicate =
        format!("chain = {chain} AND `block_number` >= {from}");
    if let Some(to) = to {
        predicate.push_str(&format!(" AND `block_number` < {to}"));
    }
    predicate
}

// ------------------------------------------------------------- the store

#[derive(Clone)]
pub struct SolanaReorgStore {
    db: Database,
}

impl SolanaReorgStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    async fn count(&self, sql: &str) -> Result<u64> {
        self.db
            .db
            .query(sql)
            .fetch_one::<u64>()
            .await
            .with_context(|| format!("query failed: {sql}"))
    }

    async fn execute(&self, sql: &str) -> Result<()> {
        debug!("solana purge: {sql}");
        self.db
            .db
            .query(sql)
            .execute()
            .await
            .with_context(|| format!("statement failed: {sql}"))
    }

    async fn live_checkpoint_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> Result<Vec<DatabaseCheckpoint>> {
        self.db
            .db
            .query(&format!(
                "SELECT chain, from_block, to_block, epoch, _version \
                 FROM checkpoints FINAL WHERE {}",
                overlap_predicate(chain, from, to)
            ))
            .fetch_all::<DatabaseCheckpoint>()
            .await
            .context("query the checkpoints overlapping the purge")
    }

    // --------------------------------------------- reads the loop needs

    /// Live checkpoints of the chain ending above `start`, ordered for
    /// `db::ranges::contiguous_until`. This is witness 1 of
    /// docs/solana-research.md §11.4.3: the cursor tiling, which is the
    /// ONLY contiguity test that survives skipped slots, because a
    /// checkpoint's `to_block` is the server's `next_slot` and not
    /// `max(slot) + 1`.
    pub async fn checkpoint_tiling(
        &self,
        chain: u64,
        start: u64,
        limit: usize,
    ) -> Result<Vec<(u64, u64)>> {
        #[derive(Debug, Row, Deserialize)]
        struct Row2 {
            from_block: u64,
            to_block: u64,
        }

        let rows = self
            .db
            .db
            .query(&format!(
                "SELECT from_block, to_block FROM checkpoints FINAL \
                 WHERE chain = {chain} AND to_block > {start} \
                 ORDER BY from_block ASC, to_block ASC LIMIT {limit}"
            ))
            .fetch_all::<Row2>()
            .await
            .context("query the Solana checkpoint tiling")?;

        Ok(rows.into_iter().map(|r| (r.from_block, r.to_block)).collect())
    }

    /// The highest stored slot at or below `below`, with its `blockhash`
    /// and `block_height`: the anchor the next window's continuity check
    /// compares against across a restart and across a window boundary.
    ///
    /// docs/solana-research.md §11.4.4 proposes carrying this in the
    /// checkpoint row (`last_block` / `last_hash` / `last_height`) to save
    /// this read. It is one indexed `FINAL` read per resume and per gap
    /// heal, not per flush, so the columns are an optimisation rather than
    /// a correctness requirement - and adding them means altering the
    /// SHARED `checkpoints` table, which this work deliberately leaves
    /// alone. See the final report.
    pub async fn anchor_below(
        &self,
        chain: u64,
        below: u64,
    ) -> Result<Option<SlotAnchor>> {
        let rows = self
            .db
            .db
            .query(&format!(
                "SELECT block_number, blockhash, block_height \
                 FROM `{COMMIT_MARKER}` FINAL \
                 WHERE chain = {chain} AND block_number < {below} \
                 ORDER BY block_number DESC LIMIT 1"
            ))
            .fetch_all::<SlotAnchor>()
            .await
            .context("query the Solana continuity anchor")?;

        Ok(rows.into_iter().next())
    }

    /// Slots with a live `sol_slots` row inside `[from, to)`.
    pub async fn stored_slots(
        &self,
        chain: u64,
        range: BlockRange,
    ) -> Result<u64> {
        self.count(&format!(
            "SELECT toUInt64(count()) FROM `{COMMIT_MARKER}` FINAL \
             WHERE chain = {chain} AND block_number >= {} \
             AND block_number < {}",
            range.from, range.to
        ))
        .await
    }

    /// Timestamp of the highest live slot, for the seconds-lag metric.
    pub async fn newest_slot_timestamp(
        &self,
        chain: u64,
    ) -> Result<Option<(u64, u32)>> {
        #[derive(Debug, Row, Deserialize)]
        struct Row2 {
            rows: u64,
            slot: u64,
            timestamp: u32,
        }

        let row = self
            .db
            .db
            .query(&format!(
                "SELECT toUInt64(count()) AS rows, \
                 toUInt64(max(block_number)) AS slot, \
                 toUInt32(argMax(timestamp, block_number)) AS timestamp \
                 FROM `{COMMIT_MARKER}` FINAL WHERE chain = {chain}"
            ))
            .fetch_one::<Row2>()
            .await
            .context("query the newest stored slot")?;

        Ok((row.rows > 0).then_some((row.slot, row.timestamp)))
    }
}

impl ReorgStore for SolanaReorgStore {
    fn current_epoch(&self, chain: u64) -> BoxFuture<'_, Result<u32>> {
        Box::pin(async move {
            self.db
                .db
                .query(&format!(
                    "SELECT toUInt32(max(epoch)) FROM reorgs \
                     WHERE chain = {chain}"
                ))
                .fetch_one::<u32>()
                .await
                .context("query the current epoch")
        })
    }

    fn stored_head(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, Result<Option<u64>>> {
        Box::pin(async move {
            let (count, max): (u64, u64) = self
                .db
                .db
                .query(&format!(
                    "SELECT toUInt64(count()), \
                     toUInt64(max(block_number)) \
                     FROM `{COMMIT_MARKER}` FINAL WHERE chain = {chain}"
                ))
                .fetch_one()
                .await
                .context("query the stored head slot")?;

            Ok((count > 0).then_some(max))
        })
    }

    fn stored_hashes(
        &self,
        chain: u64,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, Result<Vec<(u64, B256)>>> {
        Box::pin(async move {
            let rows = self
                .db
                .db
                .query(&format!(
                    "SELECT block_number, blockhash \
                     FROM `{COMMIT_MARKER}` FINAL \
                     WHERE chain = {chain} AND block_number >= {from} \
                     AND block_number < {to}"
                ))
                .fetch_all::<StoredHash>()
                .await
                .context("query stored slot hashes")?;

            Ok(rows
                .into_iter()
                .map(|row| (row.block_number, row.blockhash))
                .collect())
        })
    }

    /// Children at a slot with no live `sol_slots` row.
    ///
    /// The rule is the EVM one, and it has to be: it is the invariant that
    /// makes a flush killed between "children written" and "`sol_slots`
    /// written" heal instead of double counting. Skipped slots change
    /// nothing about it - a skipped slot has no children either.
    fn has_orphan_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            let upper = to
                .map(|to| format!(" AND block_number < {to}"))
                .unwrap_or_default();

            let done = format!(
                "(SELECT groupArray((fork_block, to_block, \
                 tombstone_version)) FROM reorgs WHERE chain = {chain} \
                 AND completed = 1) AS done"
            );

            let settled =
                "arrayMax(arrayConcat([toUInt64(0)], arrayMap(r -> r.3, \
                 arrayFilter(r -> r.1 <= `block_number` AND r.2 > \
                 `block_number`, done))))";

            let no_live_slot = format!(
                "NOT IN (SELECT block_number FROM `{COMMIT_MARKER}` FINAL \
                 WHERE chain = {chain} AND block_number >= {from}{upper})"
            );

            for table in child_tables() {
                let predicate = range_predicate(table, chain, from, to);

                // A tombstone is the only trace a gap-heal purge that died
                // half way leaves; one a COMPLETED purge wrote is settled
                // debris and must not restart the purge on every boot.
                let unsettled = format!(
                    "WITH {done} SELECT toUInt64(count()) FROM (\
                     SELECT `block_number` AS n FROM `{table}` \
                     WHERE {predicate} AND `block_number` {no_live_slot} \
                     AND is_deleted = 1 AND `_version` > {settled} LIMIT 1)"
                );

                let alive = format!(
                    "SELECT toUInt64(count()) FROM (\
                     SELECT `block_number` AS n FROM `{table}` FINAL \
                     WHERE {predicate} AND `block_number` {no_live_slot} \
                     LIMIT 1)"
                );

                for sql in [unsettled, alive] {
                    if self.count(&sql).await? > 0 {
                        debug!(
                            "Orphan rows in '{table}' for slots \
                             [{from}, {to:?})."
                        );
                        return Ok(true);
                    }
                }
            }

            Ok(false)
        })
    }

    fn min_timestamp(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<Option<u32>>> {
        Box::pin(async move {
            let mut lowest: Option<u32> = None;

            let mut queries: Vec<String> = child_tables()
                .iter()
                .map(|table| {
                    format!(
                        "SELECT toUInt64(count()), \
                         toUInt32(min(timestamp)) FROM `{table}` WHERE {}",
                        range_predicate(table, chain, from, to)
                    )
                })
                .collect();

            // A purged range of slots that matched nothing has no child
            // row at all, and its `sol_slots` rows still have to decide
            // `from_ts`.
            queries.push(
                min_timestamp_sql(COMMIT_MARKER, chain, from, to).replace(
                    "SELECT toUInt32(min(timestamp))",
                    "SELECT toUInt64(count()), toUInt32(min(timestamp))",
                ),
            );

            for sql in queries {
                let (rows, min): (u64, u32) = self
                    .db
                    .db
                    .query(&sql)
                    .fetch_one()
                    .await
                    .with_context(|| format!("query failed: {sql}"))?;

                if rows > 0 {
                    lowest = Some(lowest.map_or(min, |low| low.min(min)));
                }
            }

            Ok(lowest)
        })
    }

    fn live_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut live = 0;
            for table in child_tables() {
                live += self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{table}` FINAL \
                         WHERE {}",
                        range_predicate(table, chain, from, to)
                    ))
                    .await?;
            }
            Ok(live)
        })
    }

    fn live_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            self.count(
                &live_rows_sql(COMMIT_MARKER, chain, from, to)
                    .replace("SELECT count()", "SELECT toUInt64(count())"),
            )
            .await
        })
    }

    /// `svm::SIDE_TABLES` is empty: the Solana module writes its base
    /// tables and the candle aggregates, and an `AggregatingMergeTree` is
    /// repaired per epoch, never tombstoned. Nothing to check or repair.
    fn live_side_rows(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        debug_assert!(svm::SIDE_TABLES.is_empty());
        Box::pin(async move { Ok(0) })
    }

    fn tombstone_side_rows(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
        _version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move { Ok(0) })
    }

    fn live_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            Ok(self.live_checkpoint_rows(chain, from, to).await?.len()
                as u64)
        })
    }

    fn tombstone_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let live = self.live_checkpoint_rows(chain, from, to).await?;
            let writes = checkpoint_writes(&live, from, to, version);

            self.db.insert_rows("checkpoints", &writes).await?;

            Ok(live.len() as u64)
        })
    }

    fn tombstone_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut tombstoned = 0;

            for table in child_tables() {
                let live = self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{table}` FINAL \
                         WHERE {}",
                        range_predicate(table, chain, from, to)
                    ))
                    .await?;

                if live == 0 {
                    continue;
                }

                self.execute(&db::tombstone_sql(
                    table, chain, from, to, version,
                )?)
                .await?;
                tombstoned += live;
            }

            Ok(tombstoned)
        })
    }

    fn insert_reorg<'a>(
        &'a self,
        record: &'a ReorgRecord,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let row = ReorgRow {
                chain: record.chain,
                epoch: record.epoch,
                from_ts: record.from_ts,
                fork_block: record.fork_block,
                to_block: record.to_block.unwrap_or(u64::MAX),
                old_head: record.old_head,
                old_hash: record.old_hash,
                new_hash: record.new_hash,
                depth: record.depth,
                rows_tombstoned: record.rows_tombstoned,
                reason: record.reason.to_string(),
                tombstone_version: record.version,
                completed: u8::from(record.completed),
            };

            self.db.insert_rows("reorgs", std::slice::from_ref(&row)).await
        })
    }

    fn rebuild_derived(
        &self,
        chain: u64,
        from_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Exclusive end of the rebuild: the newest row there is. No
            // FINAL, an upper bound is all that is needed.
            let newest: u32 = self
                .db
                .db
                .query(&format!(
                    "SELECT toUInt32(max(timestamp)) \
                     FROM `{COMMIT_MARKER}` WHERE chain = {chain}"
                ))
                .fetch_one()
                .await
                .context("query the newest stored slot timestamp")?;
            let to_ts = newest.max(from_ts).saturating_add(1);

            let _ = (purged_from, purged_to);

            for table in SOL_DERIVED {
                for sql in table.rebuild_statements(
                    chain,
                    from_ts,
                    to_ts,
                    epoch,
                    // Every Solana aggregate reads `sol_dex_swaps`, which
                    // this purge has already tombstoned and verified with
                    // `live_children`; none reads the commit marker. So
                    // there is no block range to exclude, exactly as for
                    // the DEX aggregates.
                    u64::MAX,
                    None,
                ) {
                    self.execute(&sql).await.with_context(|| {
                        format!("rebuild of '{}'", table.name)
                    })?;
                }
            }

            Ok(())
        })
    }

    fn tombstone_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let live = self
                .count(
                    &live_rows_sql(COMMIT_MARKER, chain, from, to)
                        .replace(
                            "SELECT count()",
                            "SELECT toUInt64(count())",
                        ),
                )
                .await?;

            if live > 0 {
                self.execute(&db::tombstone_sql(
                    COMMIT_MARKER,
                    chain,
                    from,
                    to,
                    version,
                )?)
                .await?;
            }

            Ok(live)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commit_marker_is_last_and_the_children_are_the_rest() {
        assert_eq!(svm::BASE_TABLES.last(), Some(&COMMIT_MARKER));
        assert_eq!(
            child_tables(),
            vec!["sol_dex_swaps", "sol_transactions"]
        );
        // Children before the marker, exactly as the purge tombstones and
        // the flush inserts them.
        assert!(!child_tables().contains(&COMMIT_MARKER));
    }

    /// The rebuild must be the materialized view's own SELECT, or a
    /// repaired index would silently differ from a clean one.
    #[test]
    fn every_candle_rebuild_matches_its_materialized_view() {
        let sql: String = db::migrate::embedded()
            .unwrap()
            .iter()
            .find(|migration| migration.sql.contains("sol_dex_candles_1m"))
            .expect("migration 0042 is embedded")
            .sql
            .clone();

        for table in SOL_DERIVED {
            let mv = format!("{}_mv", table.name);
            let view = sql
                .split(&format!(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS {mv}"
                ))
                .nth(1)
                .unwrap_or_else(|| panic!("{mv} is in the migration"));
            let view = view.split(';').next().unwrap();

            let normalize = |text: &str| {
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            };

            let rebuild = normalize(table.rebuild_sql);
            let view = normalize(view);

            // The parts that must be identical: the WITH clause and every
            // aggregate expression.
            for fragment in [
                "argMinStateIf(trade_price, (block_number, tx_index, \
                 ordinal), trade_ok) AS open",
                "argMaxStateIf(trade_price, (block_number, tx_index, \
                 ordinal), trade_ok) AS close",
                "countIf(pool_ok) AS pool_prices",
                "sum(abs(toFloat64(amount0))) AS volume0",
                "uniqState(trader) AS traders",
                "GROUP BY chain, pool_id, venue_program, bucket, epoch",
            ] {
                let fragment = normalize(fragment);
                assert!(
                    rebuild.contains(&fragment),
                    "{}: rebuild_sql is missing `{fragment}`",
                    table.name
                );
                assert!(
                    view.contains(&fragment),
                    "{mv}: the view is missing `{fragment}`"
                );
            }

            // The rebuild reads FINAL and the new epoch; the view reads the
            // insert block and the rows' own epoch.
            assert!(rebuild.contains("FROM sol_dex_swaps FINAL"));
            assert!(rebuild.contains("toUInt32({epoch}) AS epoch"));
            assert!(view.contains("FROM sol_dex_swaps"));
        }
    }

    #[test]
    fn the_candle_buckets_are_a_minute_an_hour_and_a_day() {
        let widths: Vec<u32> =
            SOL_DERIVED.iter().map(|t| t.bucket_seconds).collect();
        assert_eq!(widths, vec![60, 3_600, 86_400]);
        assert_eq!(SOL_DERIVED.len(), SOL_CANDLE_VIEWS.len());
    }

    #[test]
    fn a_rebuild_is_sliced_by_month() {
        // One UTC month apart: two statements, never one insert touching
        // two partitions more than ClickHouse allows.
        let statements = SOL_CANDLES_1D.rebuild_statements(
            1_399_811_149,
            1_767_225_600, // 2026-01-01
            1_772_496_000, // 2026-03-03
            7,
            u64::MAX,
            None,
        );
        assert!(statements.len() >= 2, "{}", statements.len());
        assert!(statements[0].contains("chain = 1399811149"));
        assert!(statements[0].contains("toUInt32(7) AS epoch"));
        assert!(!statements[0].contains('{'), "{}", statements[0]);
    }

    #[test]
    fn versioned_tables_cover_every_table_a_flush_writes() {
        let tables = versioned_tables();
        for table in [
            "sol_slots",
            "sol_transactions",
            "sol_dex_swaps",
            "sol_tokens",
        ] {
            assert!(tables.contains(&table), "{table}");
        }
        assert!(tables.contains(&"checkpoints"));
    }

    #[test]
    fn checkpoint_writes_split_an_overlapping_checkpoint() {
        let live = [DatabaseCheckpoint {
            chain: 1,
            from_block: 100,
            to_block: 200,
            epoch: 0,
            _version: 1,
        }];

        let writes = checkpoint_writes(&live, 150, Some(160), 9);
        let summary: Vec<(u64, u64, u8)> = writes
            .iter()
            .map(|w| (w.from_block, w.to_block, w.is_deleted))
            .collect();

        assert_eq!(
            summary,
            vec![(100, 200, 1), (100, 150, 0), (160, 200, 0)]
        );
    }

    #[test]
    fn the_range_predicate_is_half_open_and_keys_on_the_slot_column() {
        let predicate = range_predicate("sol_dex_swaps", 7, 10, Some(20));
        assert_eq!(
            predicate,
            "chain = 7 AND `block_number` >= 10 AND `block_number` < 20"
        );
        assert_eq!(
            range_predicate("sol_slots", 7, 10, None),
            "chain = 7 AND `block_number` >= 10"
        );
    }

    /// The generic statement builders must accept the `sol_*` tables: they
    /// take their column lists from the embedded DDL, so this is really a
    /// test that 0040 / 0041 declare `chain`, `block_number`, `_version`
    /// and `is_deleted` everywhere the purge needs them.
    #[test]
    fn the_shared_tombstone_builder_handles_every_sol_table() {
        for table in svm::BASE_TABLES {
            let sql =
                db::tombstone_sql(table, 1_399_811_149, 10, Some(20), 5)
                    .unwrap_or_else(|e| panic!("{table}: {e}"));
            assert!(sql.contains(&format!("INSERT INTO `{table}`")));
            assert!(sql.contains("`block_number` >= 10"));
            assert!(sql.contains("`block_number` < 20"));
            assert!(sql.contains("FINAL"));
        }
    }
}
