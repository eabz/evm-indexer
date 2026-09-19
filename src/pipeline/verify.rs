//! `indexer verify`: is what is stored for a chain consistent? Read only.
//!
//! Three checks over `[start_block, end_block)`:
//!
//! 1. **Gaps**: blocks without a live `blocks` row (the commit marker).
//! 2. **Orphan children**: live rows (transactions, logs, transfers, module
//!    rows) at a height without a live `blocks` row. They are what a flush
//!    that died before its `blocks` insert leaves behind; the indexer purges
//!    them on its next start (gap heal), until then the aggregates of those
//!    days may count them.
//! 3. **Checkpoints**: a live checkpoint must never claim a missing block.

use crate::{
    db::{
        self,
        ranges::{checkpoints_sql, contiguous_until, BlockRange},
        Database,
    },
    pipeline::modules::{range_predicate, ALL_MODULES},
};
use anyhow::{Context, Result};
use std::fmt;

/// Gap ranges listed (and checked against the checkpoints) at most.
const MAX_GAPS_REPORTED: usize = 1_000;

/// Blocks per orphan query: bounds the `NOT IN` set of block numbers.
const ORPHAN_CHUNK_BLOCKS: u64 = 1_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReport {
    pub table: &'static str,
    /// Distinct heights with live rows and no live block.
    pub blocks: u64,
    pub first_block: u64,
    pub last_block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub chain: u64,
    pub range: BlockRange,
    /// Live `blocks` rows in the range.
    pub indexed_blocks: u64,
    pub gaps: Vec<BlockRange>,
    /// More gaps exist than are listed.
    pub gaps_truncated: bool,
    pub orphans: Vec<OrphanReport>,
    /// Gap ranges a live checkpoint claims to cover.
    pub checkpoint_conflicts: Vec<BlockRange>,
    /// Block up to which the checkpoints cover the range without a hole.
    pub checkpoint_resume: u64,
    pub epoch: u32,
}

impl VerifyReport {
    pub fn is_consistent(&self) -> bool {
        self.gaps.is_empty()
            && self.orphans.is_empty()
            && self.checkpoint_conflicts.is_empty()
    }
}

impl fmt::Display for VerifyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Chain {}, blocks {}: {} of {} blocks indexed, epoch {}.",
            self.chain,
            self.range,
            self.indexed_blocks,
            self.range.len(),
            self.epoch
        )?;

        if self.gaps.is_empty() {
            writeln!(f, "Gaps: none.")?;
        } else {
            let missing: u64 = self.gaps.iter().map(BlockRange::len).sum();
            writeln!(
                f,
                "Gaps: {}{} range(s), {missing} block(s) missing:",
                self.gaps.len(),
                if self.gaps_truncated { "+" } else { "" }
            )?;
            for gap in self.gaps.iter().take(20) {
                writeln!(f, "  {gap}")?;
            }
            if self.gaps.len() > 20 {
                writeln!(f, "  ... and {} more", self.gaps.len() - 20)?;
            }
        }

        if self.orphans.is_empty() {
            writeln!(f, "Orphan rows (rows without their block): none.")?;
        } else {
            writeln!(
                f,
                "Orphan rows (left by an interrupted write; purged \
                 automatically when `indexer run` next starts):"
            )?;
            for orphan in &self.orphans {
                writeln!(
                    f,
                    "  {}: {} block height(s) between {} and {}",
                    orphan.table,
                    orphan.blocks,
                    orphan.first_block,
                    orphan.last_block
                )?;
            }
        }

        if self.checkpoint_conflicts.is_empty() {
            writeln!(
                f,
                "Checkpoints: consistent (contiguous up to block {}).",
                self.checkpoint_resume
            )?;
        } else {
            writeln!(
                f,
                "Checkpoints: {} range(s) are claimed by a checkpoint but \
                 have missing blocks:",
                self.checkpoint_conflicts.len()
            )?;
            for range in self.checkpoint_conflicts.iter().take(20) {
                writeln!(f, "  {range}")?;
            }
        }

        write!(
            f,
            "Result: {}",
            if self.is_consistent() {
                "CONSISTENT"
            } else {
                "PROBLEMS FOUND"
            }
        )
    }
}

/// Every block scoped child table: `(table, block column, predicate)`.
fn child_tables(
    chain: u64,
    range: BlockRange,
) -> Vec<(&'static str, &'static str, String)> {
    let mut tables = Vec::new();

    for table in db::BASE_TABLES.iter().filter(|t| **t != "blocks") {
        tables.push((
            *table,
            "block_number",
            format!(
                "chain = {chain} AND block_number >= {} AND \
                 block_number < {}",
                range.from, range.to
            ),
        ));
    }

    for spec in ALL_MODULES {
        for table in spec.base_tables {
            tables.push((
                *table,
                (spec.block_column)(table),
                range_predicate(
                    spec,
                    table,
                    chain,
                    range.from,
                    Some(range.to),
                ),
            ));
        }
    }

    tables
}

/// Runs the checks. `end_block` 0 = up to the highest indexed block.
pub async fn verify(
    db: &Database,
    start_block: u64,
    end_block: u64,
) -> Result<VerifyReport> {
    let chain = db.chain_id;

    let end = if end_block > 0 {
        end_block
    } else {
        db.stored_head().await?.map_or(start_block, |head| head + 1)
    };
    let range = BlockRange::new(start_block, end.max(start_block));

    // 1. Gaps.
    let mut gaps: Vec<BlockRange> = Vec::new();
    let mut gaps_truncated = false;
    let mut cursor = range.from;

    while cursor < range.to {
        let missing =
            db.missing_ranges(BlockRange::new(cursor, range.to)).await?;
        gaps.extend(missing.ranges);

        if gaps.len() >= MAX_GAPS_REPORTED {
            gaps_truncated = missing.covered_until < range.to
                || gaps.len() > MAX_GAPS_REPORTED;
            gaps.truncate(MAX_GAPS_REPORTED);
            break;
        }
        if missing.covered_until <= cursor {
            break;
        }
        cursor = missing.covered_until;
    }

    let missing_blocks: u64 = gaps.iter().map(BlockRange::len).sum();
    let indexed_blocks = if gaps_truncated {
        db.db
            .query(&format!(
                "SELECT toUInt64(count()) FROM blocks FINAL WHERE chain = \
                 {chain} AND number >= {} AND number < {}",
                range.from, range.to
            ))
            .fetch_one::<u64>()
            .await
            .context("count indexed blocks")?
    } else {
        range.len().saturating_sub(missing_blocks)
    };

    // 2. Orphan children, in bounded chunks.
    let mut orphans: Vec<OrphanReport> = Vec::new();
    let mut from = range.from;

    while from < range.to {
        let chunk = BlockRange::new(
            from,
            from.saturating_add(ORPHAN_CHUNK_BLOCKS).min(range.to),
        );

        for (table, column, predicate) in child_tables(chain, chunk) {
            let sql = format!(
                "SELECT toUInt64(count()), toUInt64(min(n)), \
                 toUInt64(max(n)) FROM (\
                 SELECT DISTINCT `{column}` AS n FROM `{table}` FINAL \
                 WHERE {predicate}) WHERE n NOT IN (\
                 SELECT number FROM blocks FINAL WHERE chain = {chain} \
                 AND number >= {} AND number < {})",
                chunk.from, chunk.to
            );

            let (blocks, first, last): (u64, u64, u64) =
                db.db.query(&sql).fetch_one().await.with_context(
                    || format!("orphan check of '{table}'"),
                )?;

            if blocks == 0 {
                continue;
            }

            match orphans.iter_mut().find(|o| o.table == table) {
                Some(report) => {
                    report.blocks += blocks;
                    report.first_block = report.first_block.min(first);
                    report.last_block = report.last_block.max(last);
                }
                None => orphans.push(OrphanReport {
                    table,
                    blocks,
                    first_block: first,
                    last_block: last,
                }),
            }
        }

        from = chunk.to;
    }

    // 3. Checkpoints never claim a missing block.
    let mut checkpoint_conflicts = Vec::new();

    for gap in &gaps {
        let claimed: u64 = db
            .db
            .query(&format!(
                "SELECT toUInt64(count()) FROM checkpoints FINAL \
                 WHERE chain = {chain} AND from_block < {} AND \
                 to_block > {}",
                gap.to, gap.from
            ))
            .fetch_one()
            .await
            .context("checkpoint check")?;

        if claimed > 0 {
            checkpoint_conflicts.push(*gap);
        }
    }

    let checkpoint_resume = resume_point(db, range.from).await?;

    Ok(VerifyReport {
        chain,
        range,
        indexed_blocks,
        gaps,
        gaps_truncated,
        orphans,
        checkpoint_conflicts,
        checkpoint_resume,
        epoch: db.current_epoch().await?,
    })
}

/// Block up to which the live checkpoints cover `start` onwards without a
/// hole (docs/design.md, section 3).
pub async fn resume_point(db: &Database, start: u64) -> Result<u64> {
    let mut cursor = db
        .db
        .query(&checkpoints_sql(db.chain_id, start))
        .fetch::<(u64, u64)>()
        .context("query checkpoints")?;

    let mut until = start;

    // Streamed: one row per flush adds up over the years.
    while let Some((from, to)) =
        cursor.next().await.context("read checkpoints")?
    {
        let next = contiguous_until(until, [(from, to)]);
        if next == until && from > until {
            break;
        }
        until = next;
    }

    Ok(until)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> VerifyReport {
        VerifyReport {
            chain: 1,
            range: BlockRange::new(0, 100),
            indexed_blocks: 100,
            gaps: vec![],
            gaps_truncated: false,
            orphans: vec![],
            checkpoint_conflicts: vec![],
            checkpoint_resume: 100,
            epoch: 0,
        }
    }

    #[test]
    fn a_clean_report_is_consistent() {
        let report = report();
        assert!(report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("Gaps: none."));
        assert!(text.ends_with("Result: CONSISTENT"));
    }

    #[test]
    fn any_finding_makes_it_inconsistent() {
        let gap = VerifyReport {
            gaps: vec![BlockRange::new(10, 20)],
            ..report()
        };
        assert!(!gap.is_consistent());
        assert!(gap.to_string().contains("10 block(s) missing"));

        let orphan = VerifyReport {
            orphans: vec![OrphanReport {
                table: "logs",
                blocks: 3,
                first_block: 5,
                last_block: 7,
            }],
            ..report()
        };
        assert!(!orphan.is_consistent());
        assert!(orphan.to_string().contains("logs: 3 block height(s)"));

        let checkpoint = VerifyReport {
            checkpoint_conflicts: vec![BlockRange::new(10, 20)],
            ..report()
        };
        assert!(!checkpoint.is_consistent());
        assert!(checkpoint.to_string().ends_with("PROBLEMS FOUND"));
    }

    #[test]
    fn every_block_scoped_child_table_is_checked() {
        let tables: Vec<&str> = child_tables(1, BlockRange::new(0, 10))
            .into_iter()
            .map(|(table, _, _)| table)
            .collect();

        assert!(!tables.contains(&"blocks"));
        for expected in [
            "logs",
            "transactions",
            "dex_swaps",
            "dex_pools",
            "prediction_trades",
        ] {
            assert!(tables.contains(&expected), "{expected}");
        }
    }
}
