//! ClickHouse side of the Solana pipeline: the candle aggregates as
//! [`DerivedTable`]s, the [`ReorgStore`] over the `sol_*` tables, and the
//! reads the sync loop needs (resume cursor, contiguity anchor, gaps).
//!
//! # Why this is a second store and not a `Scope` of `pipeline::store`
//!
//! `ClickhouseReorgStore` names `blocks` in eight places and enumerates
//! `core::BASE_TABLES` + `ALL_MODULES`. On Solana the commit marker is
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
    db::format::SerB256,
    db::{
        self,
        derived::DerivedTable,
        ranges::{BlockRange, DatabaseCheckpoint},
        schema::{live_rows_sql, min_timestamp_sql, tombstone_sql_where},
        Database,
    },
    launchpads,
    reorg::{ReorgRecord, ReorgStore},
    svm::{self, derived::SOL_DERIVED},
};
use alloy::primitives::B256;
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::debug;
use serde::{Deserialize, Serialize};

/// The commit marker: Solana's `blocks`.
pub const COMMIT_MARKER: &str = "sol_slots";

/// `sol_token_balances` is written by the Solana flush and is deliberately
/// NEITHER a purge child NOR a seeded version table. Both would be wrong,
/// for the same reason.
///
/// Its `_version` is **the POSITION** - `(slot, tx_index)` packed - not the
/// flush's clock. `SolLaunchpadRows::set_version` skips it on purpose, so
/// that "the newest observation of an account wins a merge on its own and a
/// replayed range cannot move a balance backwards". It is a latest-value
/// projection keyed on `(chain, mint, owner, account)`, not an append log.
///
/// Two consequences, both found by
/// `a_flush_killed_before_the_commit_marker_is_healed_on_restart` on a real
/// ClickHouse:
///
/// * **It cannot be tombstoned.** A tombstone carries `db::next_version()`,
///   a unix-millisecond clock around 1.8e12; a position is around 1.7e18.
///   The tombstone loses the `ReplacingMergeTree` merge every time, so
///   `tombstone_children` would never see zero live rows and the purge
///   would fail with `TombstonesNotConverging`.
/// * **It must not seed the version counter.** `Database::seed_version`
///   takes `max(_version)` over the tables it is given to stop a host whose
///   clock stepped back from undercutting stored rows. Fed a POSITION it
///   pushes the global counter to ~1.7e18, and every later purge then
///   stamps tombstones that outrank the positions of the rows the
///   re-stream writes - which is exactly how the healed index came out
///   missing its newest balances.
///
/// Nothing is lost by leaving it out: the range is always re-streamed after
/// a gap heal, and re-observing an account's balance is precisely how this
/// table is meant to be corrected. See the note to `solana-launchpads` in
/// the final report.
pub const SOL_TOKEN_BALANCES: &str = "sol_token_balances";

/// Every block scoped table a Solana flush writes EXCEPT the commit
/// marker, in the order a purge tombstones them: children first.
///
/// Three families, and the order between them matters only in that the
/// marker is last:
///
/// * the SHARED `launchpad_*` tables. The Solana launchpad decoder writes
///   the very same chain-neutral rows the EVM one does, so a Solana purge
///   must tombstone ONLY the rows of `chain = 1399811149` - which the
///   `chain = {chain}` predicate does by itself. `launchpads::BASE_TABLES`
///   is where their order is defined and this follows it.
/// * the `sol_*` DEX tables of [`svm::BASE_TABLES`], marker excluded.
///
/// `sol_tokens`, `sol_launchpad_configs` and `sol_dex_programs` are NOT
/// here: they are chain state, exactly like the EVM `tokens` table. A
/// mint's decimals and a curve config do not change with a fork. Neither
/// is `sol_token_balances`, for a sharper reason - see the note on
/// [`SOL_TOKEN_BALANCES`].
pub fn child_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = svm::SHARED_BASE_TABLES.to_vec();
    tables.extend(
        svm::BASE_TABLES
            .iter()
            .copied()
            .filter(|table| *table != COMMIT_MARKER),
    );
    tables
}

/// Read-path side tables a Solana flush feeds through a materialized view.
///
/// `svm::SIDE_TABLES` is empty - the `sol_*` tables have no side tables -
/// but the SHARED `launchpad_*` tables do, and their views fire on the
/// Solana rows exactly as on the EVM ones. Without this the purge would
/// leave a `launchpad_trades_by_token` row of a trade that never happened
/// alive for ever: nothing else ever rewrites a side row once its base row
/// is dead.
pub fn side_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = svm::SIDE_TABLES.to_vec();
    tables.extend_from_slice(launchpads::SIDE_TABLES);
    tables
}

/// Every aggregate a Solana purge must repair: the `sol_*` candles plus
/// the SHARED launchpad aggregates, which the Solana launchpad decoder
/// feeds with the same rows the EVM one does.
///
/// The core, DEX and prediction aggregates are deliberately NOT here. The
/// epoch and the validity rule are per CHAIN, so a `reorgs` row of this
/// chain hides contributions in every aggregate - but those three are fed
/// only from tables the Solana pipeline never writes (`blocks`,
/// `transactions`, `dex_swaps`, `prediction_*`), so for
/// `chain = 1399811149` they hold no row to hide and no row to rebuild.
/// `every_aggregate_the_solana_flush_feeds_is_repaired` is the test that
/// keeps that true: it derives the set from the tables the writer actually
/// inserts, so a future Solana front end for `dex_swaps` (the TODO(merge)
/// of migration 0041) breaks the test instead of silently zeroing a chart.
pub fn repaired_derived() -> Vec<&'static DerivedTable> {
    SOL_DERIVED
        .iter()
        .chain(launchpads::LAUNCHPADS_DERIVED.iter())
        .collect()
}

/// Every table a Solana process writes versioned rows of a chain into:
/// what `Database::seed_version` looks at. `checkpoints` is shared with the
/// EVM path and is included, because a Solana flush writes it too.
pub fn versioned_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = child_tables();
    tables.push(COMMIT_MARKER);
    tables.push("sol_tokens");
    tables.push("sol_launchpad_configs");
    tables.push("checkpoints");
    // NOT `sol_token_balances`: see the note on [`SOL_TOKEN_BALANCES`].
    // Its `_version` is a position, and seeding a clock from it poisons
    // every later purge.

    // `sol_dex_programs` is deliberately NOT here although the flush can
    // write it. `Database::seed_version` reads
    // `max(_version) WHERE chain = ?`, and that table has no `chain`
    // column: it is a GLOBAL registry keyed on `program_id`, because a
    // program id means the same thing on every network. Listing it makes
    // the seed query fail outright (found by the acceptance run).
    //
    // Nothing is lost. The seed exists so a host whose clock stepped back
    // cannot hand out a `_version` below a stored one and have a purge's
    // tombstones beat the rows re-streamed afterwards. `sol_dex_programs`
    // is never purged and never tombstoned - it is an operator's
    // judgement, not slot data - so there is no tombstone for a low
    // version to win against.
    debug_assert!(!crate::db::schema::has_column(
        "sol_dex_programs",
        "chain"
    ));

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
    /// Exclusive end of the repaired bucket range. NOT optional: the
    /// column defaults to `from_ts + 1 day`, so leaving it out would hide
    /// one day and rebuild the real window - every bucket in between would
    /// keep counting a stale epoch. `epoch_floor_v` only raises the floor
    /// inside `[from_ts, to_ts)`.
    to_ts: u32,
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

    /// Both ends of the timestamp window the purge invalidated, over ALL
    /// row versions.
    ///
    /// No `FINAL`, deliberately: a tombstone keeps its row's timestamp, so
    /// a purge that died half way computes the SAME window when it runs
    /// again. With `FINAL` the window would shrink between attempts and a
    /// bucket could end up hidden by the validity rule but never rebuilt.
    fn timestamp_span(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<Option<(u32, u32)>>> {
        Box::pin(async move {
            let mut span: Option<(u32, u32)> = None;

            const COUNTED: &str =
                "SELECT toUInt64(count()), toUInt32(min(timestamp)), \
                 toUInt32(max(timestamp))";

            let mut queries: Vec<String> = child_tables()
                .iter()
                .map(|table| {
                    format!(
                        "{COUNTED} FROM `{table}` WHERE {}",
                        range_predicate(table, chain, from, to)
                    )
                })
                .collect();

            // A purged range of slots that matched nothing has no child
            // row at all, and its `sol_slots` rows still decide the
            // window - the same reason the EVM store reads `blocks` here.
            queries.push(
                min_timestamp_sql(COMMIT_MARKER, chain, from, to)
                    .replace("SELECT toUInt32(min(timestamp))", COUNTED),
            );

            for sql in queries {
                let (rows, min, max): (u64, u32, u32) = self
                    .db
                    .db
                    .query(&sql)
                    .fetch_one()
                    .await
                    .with_context(|| format!("query failed: {sql}"))?;

                if rows > 0 {
                    span = Some(match span {
                        Some((low, high)) => (low.min(min), high.max(max)),
                        None => (min, max),
                    });
                }
            }

            Ok(span)
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

    /// Orphans a lost materialized-view push left in a side table.
    ///
    /// The `sol_*` tables have none, but the SHARED `launchpad_*` tables
    /// do and their views fire on the Solana rows exactly as on the EVM
    /// ones. A side row is written ONLY by the view of its base row, so
    /// once the base row is dead nothing ever rewrites it: without this
    /// check a `launchpad_trades_by_token` row of a trade that never
    /// happened would stay alive for ever.
    fn live_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut live = 0;
            for table in side_tables() {
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

    /// Tombstones them directly, with the same statement shape the base
    /// tables use. Only ever a REPAIR: in the normal case the views
    /// already did it and this writes nothing.
    fn tombstone_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut tombstoned = 0;

            for table in side_tables() {
                let predicate = range_predicate(table, chain, from, to);

                let live = self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{table}` FINAL \
                         WHERE {predicate}"
                    ))
                    .await?;

                if live == 0 {
                    continue;
                }

                debug!(
                    "solana purge: repairing {live} orphaned row(s) in the \
                     side table '{table}' (a materialized view push was \
                     lost)."
                );

                self.execute(&tombstone_sql_where(
                    table, &predicate, version,
                )?)
                .await?;
                tombstoned += live;
            }

            Ok(tombstoned)
        })
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
                to_ts: record.to_ts,
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

    /// Bucket repair over exactly `[from_ts, to_ts)`.
    ///
    /// The window comes from the purge, which read both ends over ALL row
    /// versions of the purged range, and it is exactly what this purge's
    /// `reorgs` row hides. The two must never disagree: a bucket the
    /// validity rule hides but the repair does not cover reads as empty
    /// for ever. So nothing here re-derives an upper bound of its own -
    /// the previous version of this method queried `max(timestamp)` of the
    /// whole chain and rebuilt to the head, which repaired far more than
    /// was hidden and, at the tip, raced the writer.
    fn rebuild_derived(
        &self,
        chain: u64,
        from_ts: u32,
        to_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Every aggregate here reads a CHILD table - `sol_dex_swaps`
            // for the candles, `launchpad_trades` / `_tokens` /
            // `_graduations` / `_creator_fees` for the launchpad ones -
            // and the purge tombstoned and verified all of them with
            // `live_children` before this step. None reads the commit
            // marker, whose rows are still alive at this point, so there
            // is no block range to exclude. Same reasoning as the EVM DEX
            // and launchpad aggregates.
            let _ = (purged_from, purged_to);

            for table in repaired_derived() {
                for sql in table.rebuild_statements(
                    chain,
                    from_ts,
                    to_ts,
                    epoch,
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
            vec![
                // The SHARED launchpad tables, in the module's own order.
                "launchpad_creator_fees",
                "launchpad_graduations",
                "launchpad_trades",
                "launchpad_tokens",
                // The sol_* DEX tables, marker excluded.
                "sol_dex_swaps",
                "sol_transactions",
            ]
        );
        // Children before the marker, exactly as the purge tombstones and
        // the flush inserts them.
        assert!(!child_tables().contains(&COMMIT_MARKER));
        // Chain state is never a child: no purge may touch it.
        for state in
            ["sol_tokens", "sol_dex_programs", "sol_launchpad_configs"]
        {
            assert!(!child_tables().contains(&state), "{state}");
        }
    }

    /// Every table the Solana flush writes must be reachable by a purge,
    /// or be chain state on purpose. A launchpad table added to
    /// `svm::SHARED_BASE_TABLES` and forgotten here would leave rows of a
    /// rolled-back range alive for ever.
    #[test]
    fn every_block_scoped_table_the_flush_writes_is_a_child() {
        let children = child_tables();

        for table in svm::SHARED_BASE_TABLES {
            assert!(children.contains(table), "{table}");
        }
        for table in svm::BASE_TABLES {
            assert!(
                children.contains(table) || *table == COMMIT_MARKER,
                "{table}"
            );
        }
        // ... but NOT the holder projection, whose `_version` is a
        // position: a clock-versioned tombstone could never win against
        // it, and the purge would fail to converge.
        assert!(!children.contains(&SOL_TOKEN_BALANCES));
        assert!(!versioned_tables().contains(&SOL_TOKEN_BALANCES));
    }

    /// The side tables of the SHARED launchpad tables are fed by views
    /// that fire on the Solana rows too, so the purge has to repair them.
    #[test]
    fn the_launchpad_side_tables_are_repaired() {
        let sides = side_tables();

        assert!(svm::SIDE_TABLES.is_empty(), "the sol_* tables have none");
        for table in launchpads::SIDE_TABLES {
            assert!(sides.contains(table), "{table}");
        }
        // A side table is never also a child: children are tombstoned
        // directly, side tables only as a repair.
        for side in &sides {
            assert!(!child_tables().contains(side), "{side}");
        }
    }

    /// The repaired set must be exactly the aggregates fed from a table
    /// the Solana flush writes.
    ///
    /// This is what keeps "the core, DEX and prediction aggregates hold no
    /// Solana row" honest. A future Solana front end for `dex_swaps` (the
    /// TODO(merge) of migration 0041) would start feeding `dex_candles_*`
    /// and this test fails instead of a chart silently zeroing.
    #[test]
    fn every_aggregate_the_solana_flush_feeds_is_repaired() {
        use crate::{core::CORE_DERIVED, dex, predictions};

        let written: Vec<&str> = child_tables();
        let repaired: Vec<&str> =
            repaired_derived().iter().map(|t| t.name).collect();

        let feeds_solana = |table: &DerivedTable| {
            written.iter().any(|source| {
                table.rebuild_sql.contains(&format!("FROM {source} "))
                    || table
                        .rebuild_sql
                        .contains(&format!("FROM `{source}` "))
            })
        };

        for table in CORE_DERIVED
            .iter()
            .chain(dex::derived::DEX_DERIVED)
            .chain(predictions::derived::PREDICTIONS_DERIVED)
            .chain(launchpads::LAUNCHPADS_DERIVED)
            .chain(SOL_DERIVED)
        {
            assert_eq!(
                feeds_solana(table),
                repaired.contains(&table.name),
                "'{}' is fed from a table the Solana flush writes: {}, \
                 but repaired_derived() says {}",
                table.name,
                feeds_solana(table),
                repaired.contains(&table.name),
            );
        }
    }

    /// Every table the flush writes has to keep a deduplication log, or a
    /// retried insert double counts through its materialized views.
    #[test]
    fn every_table_the_solana_flush_writes_keeps_a_deduplication_log() {
        use crate::db::schema::split_sql_statements;

        const SETTING: &str = "non_replicated_deduplication_window";

        let statements: Vec<String> = db::migrate::embedded()
            .unwrap()
            .iter()
            .flat_map(|migration| split_sql_statements(&migration.sql))
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| s.contains(SETTING))
            .collect();

        let protected = |table: &str| {
            statements.iter().any(|s| {
                s.starts_with(&format!("ALTER TABLE {table} "))
                    || s.contains(&format!(
                        "CREATE TABLE IF NOT EXISTS {table} ("
                    ))
            })
        };

        let mut required = versioned_tables();
        required.extend(side_tables());
        required.extend(repaired_derived().iter().map(|t| t.name));

        let missing: Vec<&str> = required
            .into_iter()
            .filter(|table| !protected(table))
            .collect();

        assert!(
            missing.is_empty(),
            "tables a Solana flush writes (or feeds through a view) \
             without `{SETTING}`: {missing:?}"
        );
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
