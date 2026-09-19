//! ClickHouse side of the reorg logic (`crate::reorg`): what is stored, and
//! the insert-only steps of a purge. The ORDER of the steps is decided by
//! `reorg::Purger`, never here; every method is safe to repeat.
//!
//! Nothing in this file issues `DELETE`, `ALTER .. DELETE/UPDATE` or `DROP
//! PARTITION` (docs/design.md, section 2): rows are removed by tombstone.

use crate::{
    db::{
        self,
        derived::{DerivedTable, CORE_DERIVED},
        ranges::DatabaseCheckpoint,
        schema::{live_rows_sql, min_timestamp_sql},
        Database,
    },
    pipeline::modules::{
        plain_rebuild, range_predicate, ModuleSpec, Rebuild, ALL_MODULES,
    },
    reorg::{ReorgRecord, ReorgStore},
    utils::format::SerB256,
};
use alloy::primitives::B256;
use anyhow::{Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::debug;
use serde::{Deserialize, Serialize};

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
    fork_block: u64,
    old_head: u64,
    #[serde_as(as = "SerB256")]
    old_hash: B256,
    #[serde_as(as = "SerB256")]
    new_hash: B256,
    depth: u64,
    rows_tombstoned: u64,
    reason: String,
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
                    db::BASE_TABLES
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
                spec.derived
                    .iter()
                    .map(|table| (table, spec.rebuild_sql, in_scope)),
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

            for child in self.children() {
                // No FINAL on the child: tombstoned rows are the only
                // trace of a heal that died half way. FINAL on `blocks`:
                // a tombstoned block is not there.
                let sql = format!(
                    "SELECT toUInt64(count()) FROM (\
                     SELECT `{column}` AS n FROM `{table}` WHERE {predicate} \
                     AND `{column}` NOT IN (\
                     SELECT number FROM blocks FINAL WHERE chain = {chain} \
                     AND number >= {from}{upper}) LIMIT 1)",
                    column = child.block_column(),
                    table = child.table,
                    predicate = child.predicate(chain, from, to),
                );

                if self.count(&sql).await? > 0 {
                    debug!(
                        "Orphan rows in '{}' for blocks [{from}, {to:?}).",
                        child.table
                    );
                    return Ok(true);
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

            let mut queries: Vec<String> = self
                .children()
                .iter()
                .map(|child| {
                    format!(
                        "SELECT toUInt64(count()), toUInt32(min(timestamp)) \
                         FROM `{}` WHERE {}",
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
                        "SELECT toUInt64(count()), toUInt32(min(timestamp))",
                    ),
                );
            }

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
                fork_block: record.fork_block,
                old_head: record.old_head,
                old_hash: record.old_hash,
                new_hash: record.new_hash,
                depth: record.depth,
                rows_tombstoned: record.rows_tombstoned,
                reason: record.reason.to_string(),
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
            // Exclusive end of the rebuild: the newest row there is.
            // No FINAL: an upper bound is all that is needed.
            let newest: u32 = self
                .db
                .db
                .query(&format!(
                    "SELECT toUInt32(max(timestamp)) FROM blocks \
                     WHERE chain = {chain}"
                ))
                .fetch_one()
                .await
                .context("query the newest block timestamp")?;
            let to_ts = newest.max(from_ts).saturating_add(1);

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
