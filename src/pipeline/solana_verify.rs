//! `indexer verify --chain solana`: is what is stored for Solana
//! consistent? Read only, and it costs no Envio budget.
//!
//! # Why the EVM checks can not simply be pointed at `sol_slots`
//!
//! `pipeline::verify` check 1 is "every block number in the range has a
//! live `blocks` row". On Solana **most slots have no block and that is
//! normal**, so that query would report the whole chain as one long gap,
//! for ever. The Solana checks are therefore the five of
//! the Solana venue research §11.4.5:
//!
//! | # | Check | What only IT can catch |
//! |---|---|---|
//! | 1 | **Cursor tiling.** The live checkpoints tile `[start, end)` with no hole | slots we never ASKED FOR. Nothing in the data can show this: an unasked slot and a skipped slot both have no row |
//! | 2 | **Height chain.** `S.block_height == P.block_height + 1` for consecutive stored slots | a LOST produced block, without false-positiving on skipped slots. `block_height` counts blocks, not slots |
//! | 3 | **Parent chain.** `S.parent_slot == P.slot` and `S.parent_blockhash == P.blockhash` | a WRONG block. Redundant with 2 for the missing case, which is the point |
//! | 4 | **Orphan children.** Every live `sol_transactions` / `sol_dex_swaps` row's slot has a live `sol_slots` row | a flush that died between its children and its commit marker. In practice this is the one that fires |
//! | 5 | **Candles agree with `sol_dex_swaps`** per complete UTC day | a range counted TWICE. The base tables read perfectly while every total is wrong, which is worse than missing data |
//!
//! §11.4.5 lists a sixth, optional, nightly check: re-fetch 100 random
//! stored slots and compare per-slot row counts, which is the only defence
//! against Envio re-ingesting a slot with different `transaction_index`
//! values. It is the only check that costs metered queries and needs the
//! network, so it is deliberately not part of a read-only `verify`; see the
//! final report.
//!
//! # Where a verification starts
//!
//! At the chain's **coverage floor** (docs/design.md section 16), which on
//! Solana is the head slot of the chain's first start. Below it nothing was
//! ever asked for, so a check from slot 0 reported hundreds of millions of
//! slots as "never asked for" and printed `PROBLEMS FOUND` about a database
//! with nothing wrong with it. An explicit `--start-block` still means what
//! it says, floor or no floor. The rule and its one helper are shared with
//! the EVM twin (`pipeline::verify::start_of`).

use crate::{
    db::{ranges::BlockRange, Database},
    pipeline::{
        solana::holes,
        solana_store::{child_tables, SolanaReorgStore, COMMIT_MARKER},
    },
    reorg::ReorgStore,
    svm::derived::SOL_CANDLE_FILTER,
};
use anyhow::{Context, Result};
use clickhouse::Row;
use serde::Deserialize;
use std::fmt;

/// Holes listed at most.
const MAX_HOLES_REPORTED: usize = 1_000;

/// Continuity breaks listed at most.
const MAX_BREAKS_REPORTED: usize = 20;

/// One break in the height or parent chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuityBreak {
    pub slot: u64,
    pub previous_slot: u64,
    pub block_height: u64,
    pub previous_height: u64,
    pub parent_slot: u64,
    /// True when `block_height` itself does not chain: a produced block is
    /// missing. False means the heights chain but the identity does not.
    pub height_broken: bool,
    pub parent_hash_broken: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanReport {
    pub table: &'static str,
    pub slots: u64,
    pub first_slot: u64,
    pub last_slot: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandleReport {
    pub view: &'static str,
    pub days_checked: u64,
    pub days_wrong: u64,
    pub first_wrong_day: u32,
    pub view_swaps: i64,
    pub base_swaps: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolanaVerifyReport {
    pub chain: u64,
    pub range: BlockRange,
    /// Live `sol_slots` rows in the range.
    pub stored_slots: u64,
    /// `range.len() - stored_slots - missing`: slots the chain produced no
    /// block for. NORMAL, and reported so an operator can see the rate.
    pub skipped_slots: u64,
    /// Check 1: slots inside the range no live checkpoint claims.
    pub unasked: Vec<BlockRange>,
    pub unasked_truncated: bool,
    /// Checks 2 and 3.
    pub breaks: Vec<ContinuityBreak>,
    pub breaks_total: u64,
    /// Check 4.
    pub orphans: Vec<OrphanReport>,
    /// A gap heal is pending: the next `indexer run` will purge something.
    pub heal_pending: bool,
    /// Check 5.
    pub candles: Vec<CandleReport>,
    pub candles_skipped: Option<&'static str>,
    pub epoch: u32,
    /// The operator asked for a start BELOW the coverage floor, and this is
    /// the floor. The slots down there were never asked for on purpose.
    pub below_floor: Option<u64>,
}

impl SolanaVerifyReport {
    /// Nothing found that is wrong with what is stored.
    ///
    /// `heal_pending` counts, exactly as it does on the EVM side
    /// (review round 4, MINOR 14): a repair that is armed and not
    /// completed means rows are tombstoned that nothing has settled, and
    /// the aggregates of those days still count them. The operator need
    /// do nothing about it - the next `indexer run` purges and re-indexes
    /// the range - but the numbers ARE wrong until it does, and a
    /// verifier that says "consistent" about them is lying.
    pub fn is_consistent(&self) -> bool {
        self.unasked.is_empty()
            && self.breaks.is_empty()
            && self.orphans.is_empty()
            && self.candles.is_empty()
            && !self.heal_pending
    }

    /// Did the candle cross-check - the only one that can find a DOUBLED
    /// range - actually run?
    ///
    /// It is skipped where there is nothing it could compare (a tiling
    /// with holes, a chain that does not close, a range shorter than one
    /// complete UTC day). That is not a fault of the data, but it must
    /// not read as "checked and fine" either, so the verdict line says so.
    pub fn fully_checked(&self) -> bool {
        self.candles_skipped.is_none()
    }
}

impl fmt::Display for SolanaVerifyReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Chain {} (Solana), slots {}: {} slot(s) stored, {} skipped \
             (no block was produced - that is normal on Solana), epoch {}.",
            self.chain,
            self.range,
            self.stored_slots,
            self.skipped_slots,
            self.epoch
        )?;

        if let Some(floor) = self.below_floor {
            writeln!(
                f,
                "--start-block {} is BELOW this chain's coverage floor \
                 (slot {floor}). Slots [{}, {floor}) were never asked for \
                 on purpose, and are listed below because that is what was \
                 asked for. Without --start-block the check starts at the \
                 floor.",
                self.range.from, self.range.from
            )?;
        }

        if self.unasked.is_empty() {
            writeln!(
                f,
                "Cursor tiling: complete. Every slot of the range was \
                 asked for and served."
            )?;
        } else {
            let slots: u64 =
                self.unasked.iter().map(BlockRange::len).sum();
            writeln!(
                f,
                "Cursor tiling: {}{} range(s), {slots} slot(s) were never \
                 asked for (no checkpoint covers them):",
                self.unasked.len(),
                if self.unasked_truncated { "+" } else { "" }
            )?;
            for range in self.unasked.iter().take(20) {
                writeln!(f, "  {range}")?;
            }
            if self.unasked.len() > 20 {
                writeln!(f, "  ... and {} more", self.unasked.len() - 20)?;
            }
        }

        if self.breaks.is_empty() {
            writeln!(
                f,
                "Continuity: the block_height and parent chains close over \
                 every stored slot."
            )?;
        } else {
            writeln!(
                f,
                "Continuity: {} break(s). This is NOT a skipped slot - \
                 block_height counts produced blocks and increases by \
                 exactly 1 per block:",
                self.breaks_total
            )?;
            for b in &self.breaks {
                writeln!(
                    f,
                    "  slot {} after slot {}: height {} after {} ({}){}",
                    b.slot,
                    b.previous_slot,
                    b.block_height,
                    b.previous_height,
                    if b.height_broken {
                        "a produced block is MISSING"
                    } else {
                        "heights chain"
                    },
                    if b.parent_hash_broken {
                        ", and its parent_blockhash is not the stored one \
                         (a DIFFERENT block)"
                    } else if b.parent_slot != b.previous_slot {
                        ", and its parent_slot is not the stored one"
                    } else {
                        ""
                    }
                )?;
            }
        }

        if self.orphans.is_empty() {
            writeln!(
                f,
                "Orphan rows (rows without their slot): none{}.",
                if self.heal_pending {
                    " that are still live, but a gap heal is pending \
                     (tombstoned rows of a purge that did not finish)"
                } else {
                    ""
                }
            )?;
        } else {
            writeln!(
                f,
                "Orphan rows (left by a flush that died before its \
                 `{COMMIT_MARKER}` insert; purged automatically when \
                 `indexer run` next starts):"
            )?;
            for orphan in &self.orphans {
                writeln!(
                    f,
                    "  {}: {} slot(s) between {} and {}",
                    orphan.table,
                    orphan.slots,
                    orphan.first_slot,
                    orphan.last_slot
                )?;
            }
        }

        match (&self.candles_skipped, self.candles.is_empty()) {
            (Some(why), _) => {
                writeln!(f, "Candles: not checked ({why}).")?
            }
            (None, true) => {
                writeln!(f, "Candles: they agree with `sol_dex_swaps`.")?
            }
            (None, false) => {
                writeln!(
                    f,
                    "Candles DISAGREE with `sol_dex_swaps` (a range \
                     counted twice, or a repair that did not finish):"
                )?;
                for wrong in &self.candles {
                    writeln!(
                        f,
                        "  {}: {} of {} day(s) wrong, first at unix time \
                         {}; those days say {} swaps instead of {}",
                        wrong.view,
                        wrong.days_wrong,
                        wrong.days_checked,
                        wrong.first_wrong_day,
                        wrong.view_swaps,
                        wrong.base_swaps
                    )?;
                }
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

#[derive(Debug, Row, Deserialize)]
struct BreakRow {
    slot: u64,
    previous_slot: u64,
    block_height: u64,
    previous_height: u64,
    parent_slot: u64,
    height_broken: u8,
    parent_hash_broken: u8,
}

/// Runs the checks.
///
/// `start_slot` `None` = start at the coverage floor (see the module
/// header); `end_slot` 0 = up to the highest stored slot.
pub async fn verify(
    db: &Database,
    start_slot: Option<u64>,
    end_slot: u64,
) -> Result<SolanaVerifyReport> {
    let chain = db.chain_id;
    let store = SolanaReorgStore::new(db.clone());

    let floor = match crate::coverage::store::stored(db).await {
        Ok(floor) => floor.map(|floor| floor.block),
        Err(e) => {
            log::debug!("could not read the coverage floor: {e:#}");
            None
        }
    };
    let start_slot = start_slot.unwrap_or(floor.unwrap_or(0));
    let below_floor = floor.filter(|floor| start_slot < *floor);

    let end = if end_slot > 0 {
        end_slot
    } else {
        store.stored_head(chain).await?.map_or(start_slot, |head| head + 1)
    };
    let range = BlockRange::new(start_slot, end.max(start_slot));

    // ---- check 1: the cursor tiling.
    //
    // This is the ONLY check that can see a slot we never asked for, and
    // it is why `to_block` must be the server's `next_slot`: inside a
    // served window a missing integer is a skipped slot, so nothing in the
    // data distinguishes the two.
    let tiling =
        store.checkpoint_tiling(chain, range.from, Some(range.to)).await?;
    let mut unasked = holes(range, &tiling, MAX_HOLES_REPORTED + 1).ranges;
    let unasked_truncated = unasked.len() > MAX_HOLES_REPORTED;
    unasked.truncate(MAX_HOLES_REPORTED);

    let stored_slots = store.stored_slots(chain, range).await?;
    let unasked_slots: u64 = unasked.iter().map(BlockRange::len).sum();
    let skipped_slots = range
        .len()
        .saturating_sub(stored_slots)
        .saturating_sub(unasked_slots);

    // ---- checks 2 and 3: the height and parent chains.
    //
    // One window pass over the stored slots: every row is compared with
    // the slot stored below it. Slots the chain skipped are simply not
    // rows, which is exactly why `block_height` is the witness and
    // `slot + 1` is not.
    let breaks_sql = format!(
        "SELECT slot, previous_slot, block_height, previous_height, \
         parent_slot, \
         toUInt8(block_height != previous_height + 1) AS height_broken, \
         toUInt8(parent_blockhash != previous_hash) AS parent_hash_broken \
         FROM ( \
           SELECT toUInt64(block_number) AS slot, \
                  toUInt64(block_height) AS block_height, \
                  toUInt64(parent_slot) AS parent_slot, \
                  parent_blockhash, \
                  lagInFrame(toUInt64(block_number)) OVER w AS previous_slot, \
                  lagInFrame(toUInt64(block_height)) OVER w AS previous_height, \
                  lagInFrame(blockhash) OVER w AS previous_hash, \
                  row_number() OVER w AS position \
           FROM `{COMMIT_MARKER}` FINAL \
           WHERE chain = {chain} AND block_number >= {from} \
             AND block_number < {to} \
           WINDOW w AS (ORDER BY block_number ASC \
                        ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
         ) \
         WHERE position > 1 AND (block_height != previous_height + 1 \
           OR parent_slot != previous_slot \
           OR parent_blockhash != previous_hash) \
         ORDER BY slot ASC",
        from = range.from,
        to = range.to,
    );

    let rows: Vec<BreakRow> = db
        .db
        .query(&format!("{breaks_sql} LIMIT {MAX_BREAKS_REPORTED}"))
        .fetch_all()
        .await
        .context("Solana continuity check")?;

    let breaks_total: u64 = if rows.len() < MAX_BREAKS_REPORTED {
        rows.len() as u64
    } else {
        db.db
            .query(&format!(
                "SELECT toUInt64(count()) FROM ({breaks_sql})"
            ))
            .fetch_one()
            .await
            .context("Solana continuity count")?
    };

    let breaks: Vec<ContinuityBreak> = rows
        .into_iter()
        .map(|row| ContinuityBreak {
            slot: row.slot,
            previous_slot: row.previous_slot,
            block_height: row.block_height,
            previous_height: row.previous_height,
            parent_slot: row.parent_slot,
            height_broken: row.height_broken == 1,
            parent_hash_broken: row.parent_hash_broken == 1,
        })
        .collect();

    // ---- check 4: orphan children.
    //
    // Open ended above the range without an explicit end: rows ABOVE the
    // highest stored slot are exactly what a flush that died before its
    // commit marker leaves behind.
    let orphans_to = if end_slot > 0 { range.to } else { u64::MAX };
    let mut orphans: Vec<OrphanReport> = Vec::new();

    for table in child_tables() {
        let upper = if orphans_to == u64::MAX {
            String::new()
        } else {
            format!(" AND block_number < {orphans_to}")
        };
        let marker_upper = if orphans_to == u64::MAX {
            String::new()
        } else {
            format!(" AND block_number < {orphans_to}")
        };

        let sql = format!(
            "SELECT toUInt64(count()), toUInt64(min(n)), toUInt64(max(n)) \
             FROM (SELECT DISTINCT block_number AS n FROM `{table}` FINAL \
             WHERE chain = {chain} AND block_number >= {from}{upper}) \
             WHERE n NOT IN (SELECT block_number FROM `{COMMIT_MARKER}` \
             FINAL WHERE chain = {chain} AND block_number >= \
             {from}{marker_upper})",
            from = range.from,
        );

        let (slots, first, last): (u64, u64, u64) = db
            .db
            .query(&sql)
            .fetch_one()
            .await
            .with_context(|| format!("orphan check of '{table}'"))?;

        if slots > 0 {
            orphans.push(OrphanReport {
                table,
                slots,
                first_slot: first,
                last_slot: last,
            });
        }
    }

    // What will the next start do? The orphan list above reads LIVE rows;
    // the heal detector also counts tombstoned ones no completed purge
    // settled, so a heal can be pending while the list is empty.
    let heal_pending = store
        .has_orphan_children(chain, range.from, None)
        .await
        .context("gap heal check")?;

    // ---- check 5: the candles agree with the swaps, per complete day.
    let (mut candles, mut candles_skipped) = (Vec::new(), None);

    if !unasked.is_empty() {
        candles_skipped = Some(
            "the cursor tiling has holes, so no day in it is complete",
        );
    } else if !breaks.is_empty() {
        candles_skipped = Some("the stored slots do not form a chain");
    } else if let Some((first_day, last_day)) = complete_days(
        db,
        range,
        // The day of the FLOOR is compared, from the floor onwards, for
        // the reason spelled out in the EVM twin (`pipeline::verify`):
        // the comparison is exact as long as nothing is stored below
        // where the range starts, and on Solana the floor is the head of
        // the chain's first start - so skipping that day would leave a
        // fresh chain with nothing checked at all for two days.
        crate::coverage::store::lowest_stored(db)
            .await
            .unwrap_or(None)
            .is_none_or(|lowest| range.from <= lowest),
    )
    .await?
    {
        if let Some(report) =
            check_candles(db, range, first_day, last_day).await?
        {
            candles.push(report);
        }
    } else {
        candles_skipped =
            Some("the range holds less than one complete UTC day");
    }

    Ok(SolanaVerifyReport {
        chain,
        range,
        stored_slots,
        skipped_slots,
        unasked,
        unasked_truncated,
        breaks,
        breaks_total,
        orphans,
        heal_pending,
        candles,
        candles_skipped,
        epoch: db.current_epoch().await?,
        below_floor,
    })
}

/// `[first, last)`: the UTC days of `range` whose stored slots can be
/// compared with the candles exactly. See the EVM twin
/// (`pipeline::verify::complete_days`): the two decide this identically.
async fn complete_days(
    db: &Database,
    range: BlockRange,
    first_day_complete: bool,
) -> Result<Option<(u32, u32)>> {
    const DAY: u32 = 86_400;

    let (slots, low, high): (u64, u32, u32) = db
        .db
        .query(&format!(
            "SELECT toUInt64(count()), toUInt32(min(timestamp)), \
             toUInt32(max(timestamp)) FROM `{COMMIT_MARKER}` FINAL \
             WHERE chain = {} AND block_number >= {} AND block_number < {}",
            db.chain_id, range.from, range.to
        ))
        .fetch_one()
        .await
        .context("timestamp span of the verified slots")?;

    if slots == 0 {
        return Ok(None);
    }

    let first = if first_day_complete {
        low - low % DAY
    } else {
        low - low % DAY + DAY
    };
    let last = high - high % DAY;

    Ok((first < last).then_some((first, last)))
}

/// Does `sol_dex_candles_1d_v` count the same swaps `sol_dex_swaps` holds?
///
/// The 1d view is the one whose grouping PARTITIONS the base rows: every
/// swap contributes to exactly one `(pool_id, venue_program, day)` bucket,
/// and the bucket is a day, so the comparison needs no bucket arithmetic.
///
/// **Both sides count the same rows.** The base side applies
/// [`SOL_CANDLE_FILTER`], the very text the materialized views and the
/// rebuild apply, because the view does not count a swap whose pool the
/// decoder could not name. Counting those on the base side only made
/// `verify` print PROBLEMS FOUND for every UTC day that held one -
/// perfectly healthy days (review F, NEW-2). The EVM twin does this with
/// `AggregateCheck::filter` (`pipeline::verify`).
async fn check_candles(
    db: &Database,
    range: BlockRange,
    first_day: u32,
    last_day: u32,
) -> Result<Option<CandleReport>> {
    let chain = db.chain_id;

    let sql = format!(
        "SELECT toUInt64(count()), toUInt64(countIf(delta != 0)), \
         toUInt32(ifNull(min(if(delta != 0, day, NULL)), 0)), \
         toInt64(sumIf(in_view, delta != 0)), \
         toInt64(sumIf(in_base, delta != 0)) FROM ( \
           SELECT day, sum(agg) AS in_view, sum(base) AS in_base, \
                  sum(agg) - sum(base) AS delta FROM ( \
             SELECT toUInt32(bucket) AS day, toInt64(swaps) AS agg, \
                    toInt64(0) AS base \
             FROM sol_dex_candles_1d_v \
             WHERE chain = {chain} AND bucket >= toDateTime({first_day}) \
               AND bucket < toDateTime({last_day}) \
             UNION ALL \
             SELECT intDiv(toUInt32(timestamp), 86400) * 86400 AS day, \
                    toInt64(0) AS agg, toInt64(count()) AS base \
             FROM sol_dex_swaps FINAL \
             WHERE chain = {chain} AND {SOL_CANDLE_FILTER} \
               AND timestamp >= toDateTime({first_day}) \
               AND timestamp < toDateTime({last_day}) \
               AND block_number >= {from} AND block_number < {to} \
             GROUP BY day \
           ) GROUP BY day \
         )",
        from = range.from,
        to = range.to,
    );

    let (
        days_checked,
        days_wrong,
        first_wrong_day,
        view_swaps,
        base_swaps,
    ): (u64, u64, u32, i64, i64) = db
        .db
        .query(&sql)
        .fetch_one()
        .await
        .context("Solana candle cross-check")?;

    Ok((days_wrong > 0).then_some(CandleReport {
        view: "sol_dex_candles_1d_v",
        days_checked,
        days_wrong,
        first_wrong_day,
        view_swaps,
        base_swaps,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> SolanaVerifyReport {
        SolanaVerifyReport {
            chain: 1_399_811_149,
            range: BlockRange::new(1_000, 1_100),
            stored_slots: 100,
            skipped_slots: 0,
            unasked: Vec::new(),
            unasked_truncated: false,
            breaks: Vec::new(),
            breaks_total: 0,
            orphans: Vec::new(),
            heal_pending: false,
            candles: Vec::new(),
            candles_skipped: None,
            epoch: 0,
            below_floor: None,
        }
    }

    /// A start below the floor is honoured, and the holes it exposes are
    /// explained rather than left to look like damage.
    #[test]
    fn a_start_below_the_floor_names_the_floor() {
        let below = SolanaVerifyReport {
            range: BlockRange::new(0, 1_100),
            unasked: vec![BlockRange::new(0, 1_000)],
            below_floor: Some(1_000),
            ..report()
        };

        assert!(!below.is_consistent());
        let text = below.to_string();
        assert!(
            text.contains("BELOW this chain's coverage floor"),
            "{text}"
        );
        assert!(text.contains("slot 1000"), "{text}");

        // And nothing of the sort when the start IS the floor.
        assert!(!report().to_string().contains("BELOW this chain's"));
    }

    #[test]
    fn a_clean_report_is_consistent() {
        let report = report();
        assert!(report.is_consistent());
        assert!(report.to_string().contains("Result: CONSISTENT"));
    }

    /// Skipped slots are NORMAL and must never make a report inconsistent.
    /// This is the whole difference from the EVM check.
    #[test]
    fn skipped_slots_alone_are_consistent() {
        let report = SolanaVerifyReport {
            stored_slots: 60,
            skipped_slots: 40,
            ..report()
        };

        assert!(report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("40 skipped"), "{text}");
        assert!(text.contains("that is normal on Solana"), "{text}");
        assert!(text.contains("Result: CONSISTENT"));
    }

    #[test]
    fn a_hole_in_the_tiling_is_inconsistent() {
        let report = SolanaVerifyReport {
            unasked: vec![BlockRange::new(1_020, 1_030)],
            ..report()
        };

        assert!(!report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("never asked for"), "{text}");
        assert!(text.contains("[1020, 1030)"), "{text}");
        assert!(text.contains("Result: PROBLEMS FOUND"));
    }

    #[test]
    fn a_height_break_says_a_block_is_missing_not_that_slots_were_skipped()
    {
        let report = SolanaVerifyReport {
            breaks: vec![ContinuityBreak {
                slot: 1_050,
                previous_slot: 1_049,
                block_height: 900,
                previous_height: 897,
                parent_slot: 1_049,
                height_broken: true,
                parent_hash_broken: false,
            }],
            breaks_total: 1,
            ..report()
        };

        assert!(!report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("a produced block is MISSING"), "{text}");
        assert!(text.contains("NOT a skipped slot"), "{text}");
    }

    #[test]
    fn a_parent_hash_break_says_a_different_block() {
        let report = SolanaVerifyReport {
            breaks: vec![ContinuityBreak {
                slot: 1_050,
                previous_slot: 1_049,
                block_height: 898,
                previous_height: 897,
                parent_slot: 1_049,
                height_broken: false,
                parent_hash_broken: true,
            }],
            breaks_total: 1,
            ..report()
        };

        let text = report.to_string();
        assert!(text.contains("a DIFFERENT block"), "{text}");
        assert!(text.contains("heights chain"), "{text}");
    }

    #[test]
    fn orphans_and_doubled_candles_are_inconsistent() {
        let orphaned = SolanaVerifyReport {
            orphans: vec![OrphanReport {
                table: "sol_dex_swaps",
                slots: 3,
                first_slot: 1_010,
                last_slot: 1_012,
            }],
            ..report()
        };
        assert!(!orphaned.is_consistent());
        assert!(orphaned.to_string().contains("sol_dex_swaps: 3 slot(s)"));

        let report = SolanaVerifyReport {
            candles: vec![CandleReport {
                view: "sol_dex_candles_1d_v",
                days_checked: 2,
                days_wrong: 1,
                first_wrong_day: 1_767_225_600,
                view_swaps: 20,
                base_swaps: 10,
            }],
            ..report()
        };
        assert!(!report.is_consistent());
        assert!(report.to_string().contains("20 swaps instead of 10"));
    }

    /// A pending repair IS a problem right now, exactly as it is on the
    /// EVM side (review round 4, MINOR 14): rows are tombstoned
    /// that no completed purge settled, and the aggregates of those days
    /// still COUNT them. The operator need do nothing - the next
    /// `indexer run` repairs it - but the numbers are wrong until it does,
    /// so `verify` must not call the index consistent.
    ///
    /// This replaces `a_pending_heal_is_reported_without_failing_the_run`,
    /// which asserted the opposite (lead decision, round 4 follow-ups).
    #[test]
    fn a_pending_heal_is_a_problem_until_the_next_run_repairs_it() {
        let report = SolanaVerifyReport { heal_pending: true, ..report() };
        assert!(!report.is_consistent());
        let text = report.to_string();
        assert!(text.contains("a gap heal is pending"), "{text}");
        assert!(text.contains("Result: PROBLEMS FOUND"), "{text}");
    }

    /// A check that could not run must not read as "checked and fine".
    #[test]
    fn a_skipped_candle_check_says_not_fully_checked() {
        let skipped = SolanaVerifyReport {
            candles_skipped: Some(
                "the range holds less than one \
                                   complete UTC day",
            ),
            ..report()
        };

        assert!(skipped.is_consistent());
        assert!(!skipped.fully_checked());
        let text = skipped.to_string();
        assert!(
            text.ends_with("Result: CONSISTENT, NOT FULLY CHECKED"),
            "{text}"
        );

        // ... and a report that DID check its candles says so plainly.
        assert!(report().fully_checked());
        assert!(report().to_string().ends_with("Result: CONSISTENT"));
    }
}
