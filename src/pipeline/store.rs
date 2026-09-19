//! ClickHouse side of the reorg logic (`crate::reorg`): what is stored, and
//! the insert-only steps of a purge. The ORDER of the steps is decided by
//! `reorg::Purger`, never here; every method is safe to repeat.
//!
//! Nothing in this file issues `DELETE`, `ALTER .. DELETE/UPDATE` or `DROP
//! PARTITION` (docs/design.md, section 2): rows are removed by tombstone.

use crate::{
    core::CORE_DERIVED,
    db::format::SerB256,
    db::{
        self,
        derived::DerivedTable,
        ranges::DatabaseCheckpoint,
        schema::{
            live_rows_sql, min_timestamp_sql, tombstone_sql_where,
            view_targets,
        },
        Database,
    },
    pipeline::modules::{
        plain_rebuild, range_predicate, ModuleSpec, Rebuild, ALL_MODULES,
    },
    reorg::{ReorgRecord, ReorgStore},
};
use alloy::primitives::B256;
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::{debug, warn};
use serde::{Deserialize, Serialize};

/// How many completed purges [`ReorgStore::has_orphan_children`] reads to
/// decide whether a tombstone is settled debris.
///
/// `reorgs` is insert only and nothing compacts it: two rows per purge on
/// a chain with frequent tip reorgs. The query only reads the purges whose
/// block range overlaps the range being checked, newest tombstone version
/// first, and stops here.
const MAX_SETTLED_PURGES: usize = 1_000;

/// Which tables a purge reaches.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    /// Everything block scoped: a rollback or a gap heal.
    Chain,
    /// One module's tables (`PurgeReason::Redecode`, `indexer backfill`):
    /// `blocks`, checkpoints and every other table are left alone.
    Module(&'static ModuleSpec),
}

#[derive(Clone)]
pub struct ClickhouseReorgStore {
    db: Database,
    scope: Scope,
}

/// A child table of the scope and how to address its rows by block.
struct Child {
    table: &'static str,
    spec: Option<&'static ModuleSpec>,
}

impl Child {
    fn predicate(&self, chain: u64, from: u64, to: Option<u64>) -> String {
        match self.spec {
            Some(spec) => {
                range_predicate(spec, self.table, chain, from, to)
            }
            None => {
                let mut predicate = format!(
                    "chain = {chain} AND `block_number` >= {from}"
                );
                if let Some(to) = to {
                    predicate
                        .push_str(&format!(" AND `block_number` < {to}"));
                }
                predicate
            }
        }
    }

    fn block_column(&self) -> &'static str {
        match self.spec {
            Some(spec) => (spec.block_column)(self.table),
            None => "block_number",
        }
    }

    fn tombstone_sql(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> Result<String> {
        match self.spec {
            Some(spec) => {
                (spec.tombstone_sql)(self.table, chain, from, to, version)
            }
            None => {
                db::tombstone_sql(self.table, chain, from, to, version)
            }
        }
    }
}

/// A read-path side table of the scope, with the base table whose rows it
/// mirrors.
///
/// Side tables are fed by materialized views that pass `_version` and
/// `is_deleted` through, so a tombstone in the base table normally kills
/// the side rows for free. "Normally": if the base insert lands and the
/// push into one of its views does not (a failure between the parts, a
/// process killed mid insert), the base row is dead and the side row stays
/// alive FOR EVER - nothing else ever rewrites it. So a purge verifies the
/// side tables too and repairs them by tombstoning them directly, which is
/// safe because they are `ReplacingMergeTree(_version, is_deleted)` over
/// `chain` + a block column like everything else.
struct Side {
    table: &'static str,
    /// Column of the SIDE table holding the block number; it may differ
    /// from the base table's (`block_lookup.block_number` mirrors
    /// `blocks.number`, `dex_pools_by_token.block_number` mirrors
    /// `dex_pools.created_block`).
    block_column: &'static str,
    /// The extra predicate of the BASE table (`dex_pools`: only rows that
    /// came from an event, never the RPC resolver's). The side table
    /// carries the same column, which a unit test asserts.
    filter: Option<&'static str>,
}

impl Side {
    fn predicate(&self, chain: u64, from: u64, to: Option<u64>) -> String {
        let column = self.block_column;
        let mut predicate =
            format!("chain = {chain} AND `{column}` >= {from}");

        if let Some(to) = to {
            predicate.push_str(&format!(" AND `{column}` < {to}"));
        }
        if let Some(filter) = self.filter {
            predicate.push_str(&format!(" AND {filter}"));
        }

        predicate
    }
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct StoredHash {
    number: u64,
    #[serde_as(as = "SerB256")]
    hash: B256,
}

#[serde_with::serde_as]
#[derive(Debug, Row, Serialize)]
struct ReorgRow {
    chain: u64,
    epoch: u32,
    from_ts: u32,
    /// Exclusive end of the repaired bucket range: `epoch_floor_v` only
    /// raises the floor inside `[from_ts, to_ts)`.
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
    /// 0 = the purge armed the validity rule, 1 = it finished. Two rows
    /// per purge, never collapsed (see migration 0004).
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

/// The rows that remove `[from, to)` from the live `checkpoints`: a
/// tombstone per overlapping checkpoint, plus the part of it outside the
/// range as a new live row (so a checkpoint never claims a purged block
/// and never forgets a surviving one).
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

impl ClickhouseReorgStore {
    pub fn new(db: Database, scope: Scope) -> Self {
        Self { db, scope }
    }

    /// Child tables (everything block scoped except `blocks`) in the order
    /// a purge tombstones them.
    fn children(&self) -> Vec<Child> {
        let of_module = |spec: &'static ModuleSpec| {
            spec.base_tables
                .iter()
                .map(move |table| Child { table, spec: Some(spec) })
        };

        match self.scope {
            Scope::Module(spec) => of_module(spec).collect(),
            Scope::Chain => ALL_MODULES
                .iter()
                .flat_map(|spec| of_module(spec))
                .chain(
                    crate::core::BASE_TABLES
                        .iter()
                        .filter(|table| **table != "blocks")
                        .map(|table| Child { table, spec: None }),
                )
                .collect(),
        }
    }

    fn touches_blocks(&self) -> bool {
        matches!(self.scope, Scope::Chain)
    }

    /// Every side table of the scope: the targets the embedded migrations
    /// declare a materialized view of a base table of the scope for, kept
    /// down to the ones listed as side tables in Rust.
    ///
    /// The migrations are the single source of truth for WHICH table
    /// mirrors which (like the column lists in `db::schema`); the Rust
    /// lists say which of those targets is a side table and not an
    /// aggregate (an `AggregatingMergeTree` has no `is_deleted` and is
    /// repaired per epoch instead).
    fn side_tables(&self) -> Vec<Side> {
        let mut sides = Vec::new();

        let mut of_base =
            |base: &'static str, spec: Option<&'static ModuleSpec>| {
                let declared: &[&'static str] = match spec {
                    Some(spec) => spec.side_tables,
                    None => crate::core::SIDE_TABLES,
                };
                let filter =
                    spec.and_then(|spec| (spec.purge_filter)(base));

                let Some(targets) = view_targets().get(base) else {
                    return;
                };

                for target in targets {
                    let Some(table) = declared
                        .iter()
                        .find(|side| *side == target)
                        .copied()
                    else {
                        continue;
                    };

                    // `nft_transfers_by_account` mirrors both ERC-721 and
                    // ERC-1155 transfers: one repair covers both.
                    if sides.iter().any(|side: &Side| side.table == table)
                    {
                        continue;
                    }

                    sides.push(Side {
                        table,
                        block_column: match spec {
                            Some(spec) => (spec.block_column)(table),
                            None => db::block_number_column(table),
                        },
                        filter,
                    });
                }
            };

        for child in self.children() {
            of_base(child.table, child.spec);
        }
        if self.touches_blocks() {
            of_base("blocks", None);
        }

        sides
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
        debug!("purge: {sql}");
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
            .context("query overlapping checkpoints")
    }

    /// Every aggregate of the chain: how to rebuild it, and whether it
    /// belongs to the scope.
    fn derived(&self) -> Vec<(&'static DerivedTable, RebuildFn, bool)> {
        let mut tables: Vec<(&'static DerivedTable, RebuildFn, bool)> =
            CORE_DERIVED
                .iter()
                .map(|table| {
                    (
                        table,
                        plain_rebuild as RebuildFn,
                        matches!(self.scope, Scope::Chain),
                    )
                })
                .collect();

        for spec in ALL_MODULES {
            let in_scope = match self.scope {
                Scope::Chain => true,
                Scope::Module(scoped) => scoped.name == spec.name,
            };
            tables.extend(
                spec.derived.iter().map(|table| {
                    (table, spec.rebuild_statements, in_scope)
                }),
            );
        }

        tables
    }
}

type RebuildFn = fn(&DerivedTable, &Rebuild) -> Vec<String>;

impl ReorgStore for ClickhouseReorgStore {
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
                    "SELECT toUInt64(count()), toUInt64(max(number)) \
                     FROM blocks FINAL WHERE chain = {chain}"
                ))
                .fetch_one()
                .await
                .context("query the stored head")?;

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
                    "SELECT number, hash FROM blocks FINAL \
                     WHERE chain = {chain} AND number >= {from} \
                     AND number < {to}"
                ))
                .fetch_all::<StoredHash>()
                .await
                .context("query stored block hashes")?;

            Ok(rows
                .into_iter()
                .map(|row| (row.number, row.hash))
                .collect())
        })
    }

    fn has_orphan_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async move {
            let upper = to
                .map(|to| format!(" AND number < {to}"))
                .unwrap_or_default();

            // The purges of this chain that FINISHED, as (first block,
            // exclusive last block, the `_version` they stamped on their
            // tombstones).
            //
            // `reason != 'redecode'`: a MODULE purge (`indexer backfill
            // --module X`) only ever touched that module's tables, and
            // its repair window is the timestamp span of THOSE rows - it
            // can be far narrower in time than its block range. Counting
            // it here let it settle core orphans it never repaired: the
            // heal was skipped, the tombstoned core rows stayed in the
            // aggregates, and the range was counted again when it was
            // streamed a second time (docs/review-round-4.md, MAJOR 5).
            //
            // Only the purges whose block range can cover a row of
            // [from, to) are read, newest tombstones first: a chain with
            // frequent tip reorgs collects two `reorgs` rows per purge
            // and nothing compacts them. Leaving an old row out can only
            // LOWER a block's settled version, i.e. heal once too often
            // (safe); it can never hide an unfinished purge.
            let overlaps = to
                .map(|to| format!(" AND fork_block < {to}"))
                .unwrap_or_default();

            let done = format!(
                "(SELECT groupArray((fork_block, to_block, \
                 tombstone_version)) FROM (SELECT fork_block, to_block, \
                 tombstone_version FROM reorgs WHERE chain = {chain} \
                 AND completed = 1 AND reason != 'redecode' \
                 AND to_block > {from}{overlaps} \
                 ORDER BY tombstone_version DESC \
                 LIMIT {MAX_SETTLED_PURGES})) AS done"
            );

            // Highest tombstone version a completed purge of this block
            // wrote, 0 when there is none. A tombstone at or below it is
            // that purge's debris; a NEWER one was written by a purge that
            // died half way, and its rows are still counted by the
            // aggregates.
            let settled = |column: &str| {
                format!(
                    "arrayMax(arrayConcat([toUInt64(0)], arrayMap(r -> \
                     r.3, arrayFilter(r -> r.1 <= `{column}` AND r.2 > \
                     `{column}`, done))))"
                )
            };

            let no_live_block = format!(
                "NOT IN (SELECT number FROM blocks FINAL \
                 WHERE chain = {chain} AND number >= {from}{upper})"
            );

            for child in self.children() {
                let column = child.block_column();
                let table = child.table;
                let predicate = child.predicate(chain, from, to);

                // (1) No FINAL: a tombstone is the only trace a gap-heal
                //     purge that died half way leaves behind. One a
                //     COMPLETED purge wrote is settled.
                let unsettled = format!(
                    "WITH {done} SELECT toUInt64(count()) FROM (\
                     SELECT `{column}` AS n FROM `{table}` \
                     WHERE {predicate} AND `{column}` {no_live_block} \
                     AND is_deleted = 1 AND `_version` > {} LIMIT 1)",
                    settled(column)
                );

                // (2) A LIVE row always counts, wherever it is: it is a
                //     flush that died before its `blocks` insert, and its
                //     contributions are in the aggregates.
                let alive = format!(
                    "SELECT toUInt64(count()) FROM (\
                     SELECT `{column}` AS n FROM `{table}` FINAL \
                     WHERE {predicate} AND `{column}` {no_live_block} \
                     LIMIT 1)"
                );

                for sql in [unsettled, alive] {
                    if self.count(&sql).await? > 0 {
                        debug!(
                            "Orphan rows in '{table}' for blocks \
                             [{from}, {to:?})."
                        );
                        return Ok(true);
                    }
                }
            }

            Ok(false)
        })
    }

    fn timestamp_span(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<Option<(u32, u32)>>> {
        Box::pin(async move {
            // `minIf(timestamp > 0)`: a timestamp of 0 is a MISSING block
            // time, not a block time of 1970, and taking it as the start
            // of the repair window hides every aggregate bucket of the
            // chain (docs/review-round-4.md, MAJOR 6). It reads 0 when
            // the table has no row with a real timestamp in the range,
            // which is why the row count and the zero count come too.
            const COUNTED: &str =
                "SELECT toUInt64(count()), \
                 toUInt32(minIf(timestamp, timestamp > 0)), \
                 toUInt32(max(timestamp)), \
                 toUInt64(countIf(timestamp = 0))";

            // (smallest real timestamp, largest timestamp seen).
            let mut low: Option<u32> = None;
            let mut high: Option<u32> = None;
            let mut missing = 0u64;

            let mut queries: Vec<String> = self
                .children()
                .iter()
                .map(|child| {
                    format!(
                        "{COUNTED} FROM `{}` WHERE {}",
                        child.table,
                        child.predicate(chain, from, to)
                    )
                })
                .collect();

            if self.touches_blocks() {
                // An orphaned range of EMPTY blocks has no child row, yet
                // the block aggregates need repair.
                queries.push(
                    min_timestamp_sql("blocks", chain, from, to).replace(
                        "SELECT toUInt32(min(timestamp))",
                        COUNTED,
                    ),
                );
            }

            for sql in queries {
                let (rows, min, max, zeros): (u64, u32, u32, u64) = self
                    .db
                    .db
                    .query(&sql)
                    .fetch_one()
                    .await
                    .with_context(|| format!("query failed: {sql}"))?;

                if rows == 0 {
                    continue;
                }

                missing += zeros;
                high = Some(high.map_or(max, |high: u32| high.max(max)));
                if min > 0 {
                    low = Some(low.map_or(min, |low: u32| low.min(min)));
                }
            }

            if missing > 0 {
                warn!(
                    "Chain {chain}: {missing} stored row(s) of blocks \
                     [{from}, {to:?}) have `timestamp` 0, which is not a \
                     block time but a missing one. They are left out of \
                     the repair window of this purge (they would set it \
                     to every day since 1970 and hide every aggregate of \
                     the chain until the rebuild finished). Fix the \
                     source: a block time it does not report is being \
                     stored as 0."
                );
            }

            // Every row of the range has timestamp 0: day 0 really is the
            // only bucket they contributed to.
            Ok(high.map(|high| (low.unwrap_or(0).min(high), high)))
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
            for child in self.children() {
                live += self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{}` FINAL WHERE {}",
                        child.table,
                        child.predicate(chain, from, to)
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
            if !self.touches_blocks() {
                return Ok(0);
            }
            self.count(
                &live_rows_sql("blocks", chain, from, to)
                    .replace("SELECT count()", "SELECT toUInt64(count())"),
            )
            .await
        })
    }

    fn live_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            if !self.touches_blocks() {
                return Ok(0);
            }
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
            if !self.touches_blocks() {
                return Ok(0);
            }

            let live = self.live_checkpoint_rows(chain, from, to).await?;
            let writes = checkpoint_writes(&live, from, to, version);

            // One insert: the tombstones and the surviving remainders
            // become visible together.
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

            for child in self.children() {
                let live = self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{}` FINAL WHERE {}",
                        child.table,
                        child.predicate(chain, from, to)
                    ))
                    .await?;

                if live == 0 {
                    continue;
                }

                self.execute(
                    &child.tombstone_sql(chain, from, to, version)?,
                )
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
            // `[from_ts, to_ts)` is what the `reorgs` row of this purge
            // hides and therefore exactly what has to be rebuilt - not
            // "from from_ts to the head". Both come from the purge, which
            // read them over ALL row versions of the purged range.

            // EVERY aggregate of the chain: the epoch and the validity rule
            // are per chain, so the new `reorgs` row hides every older
            // contribution from `from_ts` on, whoever wrote it.
            for (table, rebuild, in_scope) in self.derived() {
                // Out of scope (a module re-decode): the rows of the range
                // stay alive and nobody writes them again, so the rebuild
                // has to count them: exclude an empty block range.
                let (purged_from, purged_to) = if in_scope {
                    (purged_from, purged_to)
                } else {
                    (u64::MAX, None)
                };

                let statements = rebuild(
                    table,
                    &Rebuild {
                        chain,
                        from_ts,
                        to_ts,
                        epoch,
                        purged_from,
                        purged_to,
                    },
                );

                for sql in statements {
                    self.execute(&sql).await.with_context(|| {
                        format!("rebuild of '{}'", table.name)
                    })?;
                }
            }

            Ok(())
        })
    }

    fn live_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut live = 0;
            for side in self.side_tables() {
                live += self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{}` FINAL WHERE {}",
                        side.table,
                        side.predicate(chain, from, to)
                    ))
                    .await?;
            }
            Ok(live)
        })
    }

    fn tombstone_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let mut tombstoned = 0;

            for side in self.side_tables() {
                let predicate = side.predicate(chain, from, to);

                let live = self
                    .count(&format!(
                        "SELECT toUInt64(count()) FROM `{}` FINAL WHERE \
                         {predicate}",
                        side.table
                    ))
                    .await?;

                if live == 0 {
                    continue;
                }

                debug!(
                    "purge: repairing {live} orphaned row(s) in the side \
                     table '{}' (a materialized view push was lost).",
                    side.table
                );

                self.execute(&tombstone_sql_where(
                    side.table, &predicate, version,
                )?)
                .await?;
                tombstoned += live;
            }

            Ok(tombstoned)
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
            if !self.touches_blocks() {
                return Ok(0);
            }

            let live = self
                .count(
                    &live_rows_sql("blocks", chain, from, to).replace(
                        "SELECT count()",
                        "SELECT toUInt64(count())",
                    ),
                )
                .await?;

            if live > 0 {
                self.execute(&db::tombstone_sql(
                    "blocks", chain, from, to, version,
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

    fn checkpoint(from_block: u64, to_block: u64) -> DatabaseCheckpoint {
        DatabaseCheckpoint {
            chain: 1,
            from_block,
            to_block,
            epoch: 2,
            _version: 10,
        }
    }

    fn summary(writes: &[CheckpointWrite]) -> Vec<(u64, u64, u8)> {
        writes
            .iter()
            .map(|w| (w.from_block, w.to_block, w.is_deleted))
            .collect()
    }

    #[test]
    fn a_purge_splits_the_checkpoints_it_overlaps() {
        // Open ended purge from 15: [10, 20) keeps [10, 15).
        let writes =
            checkpoint_writes(&[checkpoint(10, 20)], 15, None, 99);
        assert_eq!(summary(&writes), vec![(10, 20, 1), (10, 15, 0)]);
        assert!(writes.iter().all(|w| w._version == 99 && w.epoch == 2));

        // Bounded purge inside one checkpoint: both ends survive.
        let writes =
            checkpoint_writes(&[checkpoint(10, 20)], 12, Some(14), 99);
        assert_eq!(
            summary(&writes),
            vec![(10, 20, 1), (10, 12, 0), (14, 20, 0)]
        );

        // Fully covered: only the tombstone.
        let writes =
            checkpoint_writes(&[checkpoint(10, 20)], 0, Some(50), 99);
        assert_eq!(summary(&writes), vec![(10, 20, 1)]);

        assert!(checkpoint_writes(&[], 0, None, 1).is_empty());
    }

    /// The base -> side mapping comes from the migration DDL, so a view
    /// that is renamed, retargeted or forgotten is caught here instead of
    /// leaving permanent orphans behind after the next reorg.
    #[test]
    fn every_declared_side_table_is_covered_by_the_purge() {
        use crate::db::schema::has_column;

        let store = |scope| ClickhouseReorgStore {
            db: Database::offline(1),
            scope,
        };

        let chain = store(Scope::Chain);
        let mut found: Vec<&str> =
            chain.side_tables().iter().map(|side| side.table).collect();
        found.sort_unstable();

        let mut declared: Vec<&str> = crate::core::SIDE_TABLES.to_vec();
        for spec in ALL_MODULES {
            declared.extend_from_slice(spec.side_tables);
        }
        declared.sort_unstable();

        assert_eq!(
            found, declared,
            "a side table has no materialized view of a base table in the \
             embedded migrations (renamed? fed from another side table?), \
             or a view feeds a table nobody declared. A purge would leave \
             its rows alive for ever."
        );

        // Every column the repair's predicate names really exists there.
        for side in chain.side_tables() {
            assert!(
                has_column(side.table, "chain")
                    && has_column(side.table, side.block_column),
                "{}",
                side.table
            );
            if let Some(filter) = side.filter {
                let column =
                    filter.split_whitespace().next().unwrap_or_default();
                assert!(
                    has_column(side.table, column),
                    "the purge filter of the base table of '{}' is \
                     `{filter}`, but it has no '{column}' column: the \
                     repair would tombstone rows the base table keeps",
                    side.table
                );
            }
        }

        // A module scoped purge only ever reaches its own read path.
        for spec in ALL_MODULES {
            let mut scoped: Vec<&str> = store(Scope::Module(spec))
                .side_tables()
                .iter()
                .map(|side| side.table)
                .collect();
            scoped.sort_unstable();

            let mut expected = spec.side_tables.to_vec();
            expected.sort_unstable();
            assert_eq!(scoped, expected, "{}", spec.name);
        }
    }

    #[test]
    fn the_side_predicate_uses_the_side_block_column_and_the_base_filter()
    {
        let sides = ClickhouseReorgStore {
            db: Database::offline(1),
            scope: Scope::Chain,
        }
        .side_tables();

        let side = |name: &str| {
            sides.iter().find(|side| side.table == name).unwrap()
        };

        // `block_lookup` mirrors `blocks`, whose block column is `number`.
        assert_eq!(
            side("block_lookup").predicate(1, 10, Some(20)),
            "chain = 1 AND `block_number` >= 10 AND `block_number` < 20"
        );
        // `dex_pools_by_token` mirrors `dex_pools`, whose rows may also
        // come from the RPC resolver (`created_block` 0): the filter of
        // the base table travels with it.
        assert_eq!(
            side("dex_pools_by_token").predicate(7, 0, None),
            "chain = 7 AND `block_number` >= 0 AND source = 'event'"
        );
        assert_eq!(
            side("tx_lookup").predicate(1, 5, None),
            "chain = 1 AND `block_number` >= 5"
        );
    }

    #[test]
    fn overlap_predicate_is_half_open() {
        assert_eq!(
            overlap_predicate(1, 10, Some(20)),
            "chain = 1 AND to_block > 10 AND from_block < 20"
        );
        assert_eq!(
            overlap_predicate(1, 10, None),
            "chain = 1 AND to_block > 10"
        );
    }
}
