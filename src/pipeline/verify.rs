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
//!
//! # Where a verification starts
//!
//! At the chain's **coverage floor** (docs/design.md section 16), not at
//! block 0. The floor is what the database promises; below it there is
//! nothing and there is meant to be nothing. Checking from 0 made every
//! healthy database report the whole of history as one missing range and
//! print `PROBLEMS FOUND` for ever - which is the fastest way to teach an
//! operator to ignore the one command that tells them the truth.
//!
//! An explicit `--start-block` still means exactly what it says, including
//! below the floor: the blocks down there really are missing, the report
//! says so, and it names the floor so the number reads as a choice rather
//! than as damage.

use crate::{
    coverage,
    db::{
        ranges::{
            checkpoints_sql, contiguous_until, subtract_ranges, BlockRange,
        },
        Database,
    },
    pipeline::{
        modules::{range_predicate, ALL_MODULES},
        store::{ClickhouseReorgStore, Scope},
    },
    reorg::ReorgStore,
};
use anyhow::{Context, Result};
use std::fmt;

/// Gap ranges listed (and checked against the checkpoints) at most.
const MAX_GAPS_REPORTED: usize = 1_000;

/// Blocks per orphan query: bounds the `NOT IN` set of block numbers.
const ORPHAN_CHUNK_BLOCKS: u64 = 1_000_000;

/// Gap-free parts of the range the aggregate cross-check looks at, at
/// most: it costs one query per part and per aggregate. The rest is
/// reported as "not checked" rather than silently left out.
const MAX_AGGREGATE_PARTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReport {
    pub table: &'static str,
    /// Distinct heights with live rows and no live block.
    pub blocks: u64,
    pub first_block: u64,
    pub last_block: u64,
}

/// One aggregate that disagrees with the base table it is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateReport {
    pub view: &'static str,
    pub base: &'static str,
    pub days_checked: u64,
    pub days_wrong: u64,
    /// Start of the first UTC day that disagrees (unix seconds).
    pub first_wrong_day: u32,
    /// What the view says, and what the base table says, over the days
    /// that disagree.
    pub view_rows: i64,
    pub base_rows: i64,
}

impl AggregateReport {
    /// Adds what another gap-free part of the range found for the SAME
    /// view: one line per view, whichever part it went wrong in.
    fn merge(&mut self, other: Self) {
        self.days_checked += other.days_checked;
        self.days_wrong += other.days_wrong;
        self.first_wrong_day =
            self.first_wrong_day.min(other.first_wrong_day);
        self.view_rows += other.view_rows;
        self.base_rows += other.base_rows;
    }
}

/// An aggregate whose row count must equal a plain count over its base
/// table, per UTC day: the check that catches a DOUBLED aggregate, which
/// is what the whole epoch machinery exists to prevent and what nothing
/// verified.
///
/// Only aggregates whose grouping PARTITIONS the base rows qualify (every
/// row contributes to exactly one bucket of exactly one group). Bucket
/// width must be one day, so the comparison needs no bucket arithmetic.
struct AggregateCheck {
    view: &'static str,
    /// Column of the view holding the row count.
    count: &'static str,
    bucket: &'static str,
    base: &'static str,
    /// The same restriction the materialized view applies, if any.
    filter: Option<&'static str>,
}

const AGGREGATE_CHECKS: &[AggregateCheck] = &[
    AggregateCheck {
        view: "daily_block_stats_v",
        count: "blocks",
        bucket: "day",
        base: "blocks",
        filter: None,
    },
    AggregateCheck {
        view: "daily_transaction_stats_v",
        count: "transactions",
        bucket: "day",
        base: "transactions",
        filter: None,
    },
    AggregateCheck {
        view: "daily_erc20_transfer_stats_v",
        count: "transfers",
        bucket: "day",
        base: "erc20_transfers",
        filter: None,
    },
    // MODULE: one aggregate per module whose grouping partitions its base
    // table. `dex_candles_1d` groups by (pool, emitter) and skips the two
    // protocols whose amounts are not a price.
    AggregateCheck {
        view: "dex_candles_1d_v",
        count: "swaps",
        bucket: "bucket",
        base: "dex_swaps",
        filter: Some("protocol NOT IN ('balancer_v2', 'curve')"),
    },
];

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
    /// Aggregates that disagree with their base table, over the complete
    /// UTC days of the GAP-FREE parts of the range (an incomplete day is
    /// not a wrong day).
    pub aggregates: Vec<AggregateReport>,
    /// The aggregate cross-check did not run, and why.
    pub aggregates_skipped: Option<&'static str>,
    /// Gap-free parts of the range the aggregate cross-check did not get
    /// to ([`MAX_AGGREGATE_PARTS`]).
    pub aggregate_parts_skipped: usize,
    /// The next `indexer run` will purge and re-index something: exactly
    /// what the gap heal looks for, asked with the SAME query. The orphan
    /// list above reads live rows (`FINAL`) only, so a heal that died half
    /// way - tombstoned rows nothing has settled - is invisible there.
    pub heal_pending: bool,
    pub epoch: u32,
    /// The one line an owner reads: what this database promises for this
    /// chain (docs/design.md section 16). `None` when no coverage floor has
    /// been stored yet, which is a database no `indexer run` has ever
    /// started against.
    pub coverage: Option<String>,
    /// The operator asked for a start BELOW the coverage floor, and this is
    /// the floor. Everything between the two is missing on purpose, so the
    /// report says whose choice that was rather than leaving a reader to
    /// conclude the database is damaged.
    pub below_floor: Option<u64>,
}

impl VerifyReport {
    /// Nothing found that is wrong with what is stored.
    ///
    /// `heal_pending` belongs here and was missing
    /// (docs/review-round-4.md, MINOR 14): the next `indexer run` will
    /// purge something, which means rows are tombstoned that nothing has
    /// settled - and the aggregates of those days COUNT them right now.
    /// The data IS wrong until the heal runs, even though the operator
    /// need do nothing about it.
    pub fn is_consistent(&self) -> bool {
        self.gaps.is_empty()
            && self.orphans.is_empty()
            && self.checkpoint_conflicts.is_empty()
            && self.aggregates.is_empty()
            && !self.heal_pending
    }

    /// Did the aggregate cross-check - the only one that can find a
    /// DOUBLED aggregate - cover the whole range?
    ///
    /// It is skipped where no complete UTC day can be compared (a range
    /// shorter than a day, or a gap-free part shorter than a day), which
    /// is not a fault of the data but must not read as "checked and fine"
    /// either: the verdict line says so.
    pub fn fully_checked(&self) -> bool {
        self.aggregates_skipped.is_none()
            && self.aggregate_parts_skipped == 0
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

        // The promise first: everything below is detail about whether it
        // is being kept.
        if let Some(coverage) = &self.coverage {
            writeln!(f, "{coverage}")?;
        }

        if let Some(floor) = self.below_floor {
            writeln!(
                f,
                "--start-block {} is BELOW this chain's coverage floor \
                 (block {floor}). Blocks [{}, {floor}) were never promised \
                 and are listed as missing below, which is what was asked \
                 for. Without --start-block the check starts at the floor.",
                self.range.from, self.range.from
            )?;
        }

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
            writeln!(
                f,
                "Orphan rows (rows without their block): none{}.",
                if self.heal_pending {
                    " that are still live, but a gap heal is pending                      (tombstoned rows of a purge that did not finish)"
                } else {
                    ""
                }
            )?;
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

        match (&self.aggregates_skipped, self.aggregates.is_empty()) {
            (Some(why), _) => {
                writeln!(f, "Aggregates: not checked ({why}).")?
            }
            (None, true) if self.aggregate_parts_skipped > 0 => writeln!(
                f,
                "Aggregates: they agree with the base tables over the \
                 gap-free parts that were checked; {} more gap-free \
                 part(s) were not (verify them one by one with \
                 --start-block / --end-block).",
                self.aggregate_parts_skipped
            )?,
            (None, true) => writeln!(
                f,
                "Aggregates: they agree with the base tables."
            )?,
            (None, false) => {
                writeln!(
                    f,
                    "Aggregates DISAGREE with the base tables (a range                      counted twice, or a repair that did not finish).                      Re-run the affected days with `indexer backfill`, or                      re-index them:"
                )?;
                for wrong in &self.aggregates {
                    writeln!(
                        f,
                        "  {}: {} of {} day(s) wrong, first at unix time                          {}; those days say {} instead of {} ({})",
                        wrong.view,
                        wrong.days_wrong,
                        wrong.days_checked,
                        wrong.first_wrong_day,
                        wrong.view_rows,
                        wrong.base_rows,
                        wrong.base
                    )?;
                }
            }
        }

        if self.checkpoint_conflicts.is_empty() {
            // FROM the start of the range, not "up to N" from an implied
            // zero: the range starts at the coverage floor, and a line
            // that says "contiguous up to block 0" about a healthy chain
            // reads as "nothing is covered".
            writeln!(
                f,
                "Checkpoints: consistent (contiguous from block {} to \
                 block {}).",
                self.range.from, self.checkpoint_resume
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
            match (self.is_consistent(), self.fully_checked()) {
                (false, _) => "PROBLEMS FOUND",
                (true, false) => "CONSISTENT, NOT FULLY CHECKED",
                (true, true) => "CONSISTENT",
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

    for table in
        crate::core::BASE_TABLES.iter().filter(|t| **t != "blocks")
    {
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

/// The stored coverage floor of this chain, or `None`.
///
/// Never fatal: a database from before floors existed has no
/// `chain_coverage` row (and, on the oldest ones, no table), and that is a
/// reason to check from 0 rather than a reason to refuse to check at all.
async fn stored_floor(db: &Database) -> Option<u64> {
    match coverage::store::stored(db).await {
        Ok(floor) => floor.map(|floor| floor.block),
        Err(e) => {
            log::debug!("could not read the coverage floor: {e:#}");
            None
        }
    }
}

/// Where a verification starts: what the operator asked for, or - when
/// they asked for nothing - this chain's coverage floor.
///
/// This is the whole of the "verify starts at the floor" rule, in one
/// place, so both families and the fleet answer it the same way.
pub async fn start_of(db: &Database, explicit: Option<u64>) -> u64 {
    match explicit {
        Some(block) => block,
        None => stored_floor(db).await.unwrap_or(0),
    }
}

/// The lowest STORED block (or Solana slot) of `date` or later, for a
/// `--start-date` / `--from-date` that has to become a number.
///
/// Deliberately a question about what is stored rather than a binary
/// search over the source's headers (`coverage::resolve`): `verify` and
/// `backfill` read this database and nothing else, so asking the source
/// would be a network call to answer a question the database already
/// knows. `None` means nothing is stored on or after that day.
pub async fn block_at_date(
    db: &Database,
    date: coverage::date::Date,
) -> Result<Option<u64>> {
    let solana = db.chain_id == crate::pipeline::solana::SOLANA_CHAIN_ID;
    let (table, column) = if solana {
        ("sol_slots", "block_number")
    } else {
        ("blocks", "number")
    };

    let sql = format!(
        "SELECT toUInt64(count()), toUInt64(min(`{column}`)) \
         FROM `{table}` FINAL WHERE chain = {} \
         AND timestamp >= toDateTime({})",
        db.chain_id,
        date.midnight()
    );

    let (found, lowest) =
        db.db.query(&sql).fetch_one::<(u64, u64)>().await.with_context(
            || format!("find the first stored block of {date} or later"),
        )?;

    Ok((found > 0).then_some(lowest))
}

/// Runs the checks.
///
/// `start_block` `None` = start at the coverage floor (see the module
/// header); `end_block` 0 = up to the highest indexed block.
pub async fn verify(
    db: &Database,
    start_block: Option<u64>,
    end_block: u64,
) -> Result<VerifyReport> {
    let chain = db.chain_id;

    let floor = stored_floor(db).await;
    let start_block = start_block.unwrap_or(floor.unwrap_or(0));
    let below_floor = floor.filter(|floor| start_block < *floor);

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

    // 2. Orphan children, in bounded chunks. Without an explicit end the
    //    check is open ended: rows ABOVE the highest block are exactly
    //    what a flush that died before its `blocks` insert leaves behind.
    let mut orphans: Vec<OrphanReport> = Vec::new();
    let mut from = range.from;
    let orphans_to = if end_block > 0 { range.to } else { u64::MAX };

    while from < orphans_to {
        let chunk = if from >= range.to {
            // Above the head: no block there, one open ended look.
            BlockRange::new(from, u64::MAX)
        } else {
            BlockRange::new(
                from,
                from.saturating_add(ORPHAN_CHUNK_BLOCKS).min(range.to),
            )
        };

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

    // 3b. What will the next start do? The orphan list above is over LIVE
    //     rows; the heal detector deliberately also counts tombstoned ones
    //     that no completed purge settled, so a purge can be pending while
    //     the list is empty.
    let heal_pending = ClickhouseReorgStore::new(db.clone(), Scope::Chain)
        .has_orphan_children(chain, range.from, None)
        .await
        .context("gap heal check")?;

    // 4. Do the aggregates still say what the base tables say?
    //
    //    This is the corruption the epoch machinery exists to prevent - a
    //    materialized view only ever ADDS, so a range written twice counts
    //    twice - and nothing checked it: the base tables read perfectly
    //    while every total is wrong, which is worse than missing data.
    //
    //    Only over days that are COMPLETE: a partial day disagrees for a
    //    legitimate reason. So the check runs over the GAP-FREE parts of
    //    the range, and the edge days of each part are left out.
    //
    //    It used to be skipped outright as soon as the range had ANY gap,
    //    i.e. essentially always while a backfill is in progress - so the
    //    one check that finds a doubled aggregate was almost never on
    //    (docs/review-round-4.md, MINOR 14). A complete day of a gap-free
    //    part holds only blocks of that part, so the comparison is exact.
    let (mut aggregates, mut aggregates_skipped) = (Vec::new(), None);
    let mut aggregate_parts_skipped = 0;
    let mut days_checked = 0;

    if gaps_truncated {
        aggregates_skipped = Some(
            "the range has more gaps than can be listed, so its gap-free \
             parts are not known",
        );
    } else {
        // THE DAY OF THE FLOOR IS COMPARED, from the floor onwards.
        //
        // It is a partial day - the floor lands in the middle of one - but
        // the comparison is still exact, and that is the only thing that
        // matters here: the view counts every stored row of the day, the
        // base table is read from `range.from` up, and on a floor-to-head
        // database there IS nothing stored below the floor. Skipping it
        // instead would leave a Solana chain (whose floor is the head)
        // with nothing to check for its first two days, which is exactly
        // when a doubled write is most likely.
        //
        // The one case where the day would NOT be comparable is a start
        // below the oldest stored block's day with rows beneath it - an
        // explicit `--start-block` in the middle of the data. Hence the
        // question asked here is "is anything stored below where we
        // start", not "did we start at zero". The Solana twin
        // (`pipeline::solana_verify`) decides this identically.
        let nothing_below_the_start = coverage::store::lowest_stored(db)
            .await
            .unwrap_or(None)
            .is_none_or(|lowest| range.from <= lowest);

        let parts = subtract_ranges(&[range], &gaps);
        aggregate_parts_skipped =
            parts.len().saturating_sub(MAX_AGGREGATE_PARTS);

        for part in parts.iter().take(MAX_AGGREGATE_PARTS) {
            // Only the FIRST part starts where the range does; every later
            // one starts after a gap, and the blocks below it in that day
            // are counted by the view but not by the base side.
            let first_day_complete =
                part.from == range.from && nothing_below_the_start;

            let Some((first_day, last_day)) =
                complete_days(db, *part, first_day_complete).await?
            else {
                continue;
            };
            days_checked += 1;

            for check in AGGREGATE_CHECKS {
                let Some(report) =
                    check.run(db, *part, first_day, last_day).await?
                else {
                    continue;
                };

                // One line per view, whichever part it went wrong in.
                match aggregates.iter_mut().find(
                    |seen: &&mut AggregateReport| seen.view == report.view,
                ) {
                    Some(seen) => seen.merge(report),
                    None => aggregates.push(report),
                }
            }
        }

        if days_checked == 0 {
            aggregates_skipped = Some(
                "no gap-free part of the range holds a complete UTC day",
            );
        }
    }

    Ok(VerifyReport {
        chain,
        range,
        indexed_blocks,
        gaps,
        gaps_truncated,
        orphans,
        checkpoint_conflicts,
        checkpoint_resume,
        aggregates,
        aggregates_skipped,
        aggregate_parts_skipped,
        heal_pending,
        epoch: db.current_epoch().await?,
        coverage: coverage_line(db).await,
        below_floor,
    })
}

/// "gap-free from DATE (block N) to DATE (block M)", or what is missing
/// (docs/design.md section 16).
///
/// Never fatal and never a reason to fail a verification: an older database
/// may have no floor at all, and a sentence nobody can print is not a
/// problem with the data.
async fn coverage_line(db: &Database) -> Option<String> {
    let coverage = match coverage::store::coverage(db).await {
        Ok(coverage) => coverage?,
        Err(e) => {
            log::debug!("could not read the coverage of the chain: {e:#}");
            return None;
        }
    };

    // The date of the last covered block: one primary-key point read, and
    // only when there IS a covered block to name.
    let covered_to_date = if coverage.is_empty() {
        None
    } else {
        block_date(db, coverage.covered_to_block.saturating_sub(1)).await
    };

    let stored_head = db.stored_head().await.ok().flatten();

    Some(coverage::store::sentence(
        &coverage,
        coverage::store::unit_of(db.chain_id),
        covered_to_date.as_deref(),
        stored_head,
    ))
}

/// The UTC day one stored block was mined on. A point lookup on the
/// primary key, so it costs nothing next to the rest of a verification.
async fn block_date(db: &Database, number: u64) -> Option<String> {
    Some(coverage::date::format(i64::from(
        block_timestamp(db, number).await?,
    )))
}

/// The timestamp of one stored block, or `None` when it is not stored or
/// carries no block time (a 0 is a MISSING time, never 1970).
pub async fn block_timestamp(db: &Database, number: u64) -> Option<u32> {
    let sql = format!(
        "SELECT toUInt32(timestamp) FROM blocks FINAL \
         WHERE chain = {} AND number = {number}",
        db.chain_id
    );

    db.db
        .query(&sql)
        .fetch_all::<u32>()
        .await
        .ok()?
        .into_iter()
        .next()
        .filter(|timestamp| *timestamp > 0)
}

/// `[first, last)`: the UTC days of `range` whose stored rows can be
/// compared with the aggregates exactly.
///
/// The day of the LAST stored block is always left out: the range ends
/// inside it, so blocks of that day above `range.to` are counted by the
/// view and not by the base side. The day of the FIRST stored block is
/// left out unless `first_day_complete`, which the caller sets when
/// nothing is stored below `range.from` - the coverage floor being the
/// ordinary case.
async fn complete_days(
    db: &Database,
    range: BlockRange,
    first_day_complete: bool,
) -> Result<Option<(u32, u32)>> {
    const DAY: u32 = 86_400;

    let (blocks, low, high): (u64, u32, u32) = db
        .db
        .query(&format!(
            "SELECT toUInt64(count()), toUInt32(min(timestamp)),              toUInt32(max(timestamp)) FROM blocks FINAL WHERE chain = {}              AND number >= {} AND number < {}",
            db.chain_id, range.from, range.to
        ))
        .fetch_one()
        .await
        .context("timestamp span of the verified range")?;

    if blocks == 0 {
        return Ok(None);
    }

    // `low` is the first stored block's timestamp. Its day is comparable
    // when nothing is stored below it inside this range.
    let first = if first_day_complete {
        low - low % DAY
    } else {
        low - low % DAY + DAY
    };
    let last = high - high % DAY;

    Ok((first < last).then_some((first, last)))
}

impl AggregateCheck {
    /// `None` when the aggregate and its base table agree on every
    /// complete day.
    async fn run(
        &self,
        db: &Database,
        range: BlockRange,
        first_day: u32,
        last_day: u32,
    ) -> Result<Option<AggregateReport>> {
        let chain = db.chain_id;
        let block_column =
            if self.base == "blocks" { "number" } else { "block_number" };
        let filter = self
            .filter
            .map(|filter| format!(" AND {filter}"))
            .unwrap_or_default();

        // One row per day from each side, subtracted. Cheap: both sides
        // are keyed by chain and read one day range.
        let sql = format!(
            "SELECT toUInt64(count()), toUInt64(countIf(delta != 0)),              toUInt32(ifNull(min(if(delta != 0, day, NULL)), 0)),              toInt64(sumIf(in_view, delta != 0)),              toInt64(sumIf(in_base, delta != 0)) FROM (             SELECT day, sum(agg) AS in_view, sum(base) AS in_base,              sum(agg) - sum(base) AS delta FROM (             SELECT toUInt32(`{bucket}`) AS day, toInt64(`{count}`) AS agg,              toInt64(0) AS base FROM `{view}`              WHERE chain = {chain} AND `{bucket}` >= toDateTime({first_day})              AND `{bucket}` < toDateTime({last_day})              UNION ALL              SELECT intDiv(toUInt32(timestamp), 86400) * 86400 AS day,              toInt64(0) AS agg, toInt64(count()) AS base FROM `{base}` FINAL              WHERE chain = {chain} AND is_deleted = 0              AND timestamp >= toDateTime({first_day})              AND timestamp < toDateTime({last_day})              AND `{block_column}` >= {from} AND `{block_column}` < {to}             {filter} GROUP BY day) GROUP BY day)",
            bucket = self.bucket,
            count = self.count,
            view = self.view,
            base = self.base,
            from = range.from,
            to = range.to,
        );

        let (
            days_checked,
            days_wrong,
            first_wrong_day,
            view_rows,
            base_rows,
        ): (u64, u64, u32, i64, i64) =
            db.db.query(&sql).fetch_one().await.with_context(|| {
                format!("aggregate check of '{}'", self.view)
            })?;

        Ok((days_wrong > 0).then_some(AggregateReport {
            view: self.view,
            base: self.base,
            days_checked,
            days_wrong,
            first_wrong_day,
            view_rows,
            base_rows,
        }))
    }
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
            aggregates: vec![],
            aggregates_skipped: None,
            aggregate_parts_skipped: 0,
            heal_pending: false,
            epoch: 0,
            coverage: None,
            below_floor: None,
        }
    }

    /// The one line the owner is actually reading for
    /// (docs/design.md section 16): it comes FIRST, before the detail.
    #[test]
    fn the_coverage_promise_is_the_first_thing_verify_prints() {
        let mut with_floor = report();
        with_floor.coverage = Some(
            "Coverage: gap-free from 2024-03-01 (block 500) to 2025-09-19 \
             (block 40000000)."
                .to_string(),
        );

        let text = with_floor.to_string();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].starts_with("Coverage: gap-free from"), "{text}");
        assert!(lines[2].starts_with("Gaps:"), "{text}");

        // A database with no floor stored simply does not print the line,
        // rather than printing an empty or invented one.
        let quiet = report().to_string();
        assert!(!quiet.contains("Coverage:"), "{quiet}");
    }

    /// The checkpoint line used to say "contiguous up to block 0" on a
    /// perfectly healthy chain whose floor is a year back, because the
    /// verification started at 0. It now says where it started FROM.
    #[test]
    fn the_checkpoint_line_says_the_range_it_covered() {
        let floored = VerifyReport {
            range: BlockRange::new(23_399_283, 23_412_807),
            checkpoint_resume: 23_412_807,
            ..report()
        };

        let text = floored.to_string();
        assert!(
            text.contains(
                "Checkpoints: consistent (contiguous from block 23399283 \
                 to block 23412807)."
            ),
            "{text}"
        );
        assert!(!text.contains("up to block 0"), "{text}");
    }

    /// A start below the floor is honoured, and the hole it exposes is
    /// explained rather than left to look like damage.
    #[test]
    fn a_start_below_the_floor_names_the_floor() {
        let below = VerifyReport {
            range: BlockRange::new(0, 1_000),
            gaps: vec![BlockRange::new(0, 500)],
            below_floor: Some(500),
            ..report()
        };

        assert!(!below.is_consistent());
        let text = below.to_string();
        assert!(
            text.contains("BELOW this chain's coverage floor"),
            "{text}"
        );
        assert!(text.contains("block 500"), "{text}");
        assert!(text.contains("[0, 500)"), "{text}");

        // And the note is absent when the start IS the floor.
        assert!(!report().to_string().contains("BELOW this chain's"));
    }

    #[test]
    fn a_doubled_aggregate_is_inconsistent_and_says_which_days() {
        let mut report = report();
        report.aggregates.push(AggregateReport {
            view: "daily_transaction_stats_v",
            base: "transactions",
            days_checked: 30,
            days_wrong: 2,
            first_wrong_day: 1_700_000_000,
            view_rows: 120,
            base_rows: 60,
        });

        assert!(!report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("Aggregates DISAGREE"), "{text}");
        assert!(text.contains("2 of 30 day(s) wrong"), "{text}");
        assert!(text.contains("120 instead of 60"), "{text}");
        assert!(text.ends_with("Result: PROBLEMS FOUND"));
    }

    /// "Not checked" must not read as "consistent", and neither must "the
    /// next start will purge something" (docs/review-round-4.md,
    /// MINOR 14): both used to leave the verdict at CONSISTENT.
    #[test]
    fn what_was_not_checked_is_not_reported_as_consistent() {
        // Nothing is WRONG, but the one check that finds a doubled
        // aggregate did not run: the verdict has to say so.
        let skipped = VerifyReport {
            aggregates_skipped: Some("the range has gaps"),
            ..report()
        };

        assert!(skipped.is_consistent());
        assert!(!skipped.fully_checked());
        let text = skipped.to_string();
        assert!(
            text.contains("Aggregates: not checked (the range has gaps)"),
            "{text}"
        );
        assert!(text.ends_with("Result: CONSISTENT, NOT FULLY CHECKED"));

        // Gap-free parts the check did not get to.
        let partial =
            VerifyReport { aggregate_parts_skipped: 3, ..report() };
        assert!(!partial.fully_checked());
        assert!(
            partial.to_string().contains("3 more gap-free part(s)"),
            "{partial}"
        );

        // A pending heal IS wrong right now: the rows are tombstoned but
        // nothing settled them, so the aggregates still count them.
        let pending = VerifyReport { heal_pending: true, ..report() };
        assert!(!pending.is_consistent());
        assert!(
            pending.to_string().contains("a gap heal is pending"),
            "{pending}"
        );
    }

    /// One line per view, whichever gap-free part it went wrong in.
    #[test]
    fn a_view_that_is_wrong_in_two_parts_is_reported_once() {
        let wrong = |first_wrong_day| AggregateReport {
            view: "daily_block_stats_v",
            base: "blocks",
            days_checked: 10,
            days_wrong: 1,
            first_wrong_day,
            view_rows: 20,
            base_rows: 10,
        };

        let mut report = wrong(1_700_086_400);
        report.merge(wrong(1_700_000_000));

        assert_eq!(report.days_checked, 20);
        assert_eq!(report.days_wrong, 2);
        assert_eq!(report.first_wrong_day, 1_700_000_000);
        assert_eq!((report.view_rows, report.base_rows), (40, 20));
    }

    /// Only aggregates whose grouping partitions their base table can be
    /// compared row for row, and the comparison is per UTC day.
    #[test]
    fn every_aggregate_check_names_a_daily_view_of_a_known_base_table() {
        use crate::pipeline::modules::ALL_MODULES;

        let bases: Vec<&str> = crate::core::BASE_TABLES
            .iter()
            .copied()
            .chain(
                ALL_MODULES
                    .iter()
                    .flat_map(|spec| spec.base_tables.iter().copied()),
            )
            .collect();

        for check in AGGREGATE_CHECKS {
            assert!(bases.contains(&check.base), "{}", check.base);
            assert!(check.view.ends_with("_v"), "{}", check.view);
            assert!(
                crate::db::schema::has_column(check.base, "timestamp"),
                "{}",
                check.base
            );
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
