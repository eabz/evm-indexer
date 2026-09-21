//! Missing block range computation.
//!
//! The `blocks` table is the commit marker: a block is indexed if and only
//! if its row exists. Instead of loading every indexed block number into
//! memory, the holes are computed inside ClickHouse and only the (few)
//! missing RANGES travel to the indexer.

use clickhouse::Row;
use serde::{Deserialize, Serialize};

/// Half open block range `[from, to)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRange {
    pub from: u64,
    pub to: u64,
}

impl BlockRange {
    pub fn new(from: u64, to: u64) -> Self {
        Self { from, to }
    }

    pub fn len(&self) -> u64 {
        self.to.saturating_sub(self.from)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Display for BlockRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}, {})", self.from, self.to)
    }
}

/// Row of `checkpoints`: `[from_block, to_block)` was committed by one
/// flush (written AFTER its `blocks` rows; docs/design.md, section 3).
#[derive(
    Debug, Clone, Copy, Row, Serialize, Deserialize, PartialEq, Eq,
)]
pub struct DatabaseCheckpoint {
    pub chain: u64,
    pub from_block: u64,
    /// Exclusive.
    pub to_block: u64,
    pub epoch: u32,
    pub _version: u64,
}

/// The contiguous ranges covered by `numbers` (any order, duplicates
/// allowed), ascending. A flush normally covers one range; a pass healing
/// several gaps can put more than one into the same flush.
pub fn contiguous_ranges(
    numbers: impl IntoIterator<Item = u64>,
) -> Vec<BlockRange> {
    let mut numbers: Vec<u64> = numbers.into_iter().collect();
    numbers.sort_unstable();
    numbers.dedup();

    let mut ranges: Vec<BlockRange> = Vec::new();

    for number in numbers {
        match ranges.last_mut() {
            Some(last) if last.to == number => last.to = number + 1,
            _ => ranges
                .push(BlockRange::new(number, number.saturating_add(1))),
        }
    }

    ranges
}

/// Live checkpoints of a chain ending above `start`, ordered for
/// [`contiguous_until`]. `FINAL`: a purge tombstones the checkpoints it
/// overlaps.
pub fn checkpoints_sql(chain: u64, start: u64) -> String {
    format!(
        "SELECT from_block, to_block FROM checkpoints FINAL \
         WHERE chain = {chain} AND to_block > {start} \
         ORDER BY from_block ASC, to_block ASC"
    )
}

/// Resume point: the block up to which the checkpoints cover `start`
/// onwards without a hole (`start` itself when nothing covers it).
/// `checkpoints` must be ordered by `from_block`; they may overlap.
pub fn contiguous_until(
    start: u64,
    checkpoints: impl IntoIterator<Item = (u64, u64)>,
) -> u64 {
    let mut until = start;

    for (from, to) in checkpoints {
        if from > until {
            break;
        }
        until = until.max(to);
    }

    until
}

/// `ranges` without the blocks of `known` (both ordered or not; the result
/// is ordered when `ranges` is). Used to keep blocks this process has
/// just committed out of a gap listing that may not see them yet
/// (ClickHouse gives no read-your-writes guarantee; docs/design.md,
/// section 2).
pub fn subtract_ranges(
    ranges: &[BlockRange],
    known: &[BlockRange],
) -> Vec<BlockRange> {
    let mut result: Vec<BlockRange> = ranges.to_vec();

    for cut in known.iter().filter(|cut| !cut.is_empty()) {
        result = result
            .into_iter()
            .flat_map(|range| {
                let left =
                    BlockRange::new(range.from, range.to.min(cut.from));
                let right =
                    BlockRange::new(range.from.max(cut.to), range.to);
                [left, right]
            })
            .filter(|range| !range.is_empty())
            .collect();
    }

    result
}

/// A row written to `checkpoints`: a live range, or the tombstone of one.
#[derive(Debug, Clone, Copy, Row, Serialize, PartialEq, Eq)]
pub struct CheckpointWrite {
    pub chain: u64,
    pub from_block: u64,
    pub to_block: u64,
    pub epoch: u32,
    pub _version: u64,
    pub is_deleted: u8,
}

/// Live checkpoints read per compaction pass: the work one pass does is
/// bounded, whatever the table looks like. What is left over is compacted
/// by the next pass.
pub const MAX_CHECKPOINTS_PER_COMPACTION: usize = 2_000;

/// A chain with fewer live checkpoints than this is left alone: one row
/// per flush is only a problem once there are many, and rewriting the tip
/// every second would be pure churn.
pub const COMPACT_CHECKPOINTS_ABOVE: usize = 256;

/// Replaces every run of contiguous or overlapping live checkpoints by ONE
/// covering row: the cover plus a tombstone per row it replaces.
///
/// `checkpoints` gains a row per flush and nothing ever removes them, so a
/// long running chain accumulates millions of rows that all say the same
/// thing ("this range is committed"). Compaction keeps the ANSWER
/// identical - the union of the live ranges never changes - while the row
/// count collapses to the number of holes plus one.
///
/// Insert only, like everything else (docs/design.md, section 2): the
/// cover and the tombstones go out in ONE insert, so a reader never sees
/// the tombstones without the cover, and a crash before it leaves the
/// table exactly as it was. Repeating it is free.
///
/// `live` must be the live rows of ONE chain ordered by `from_block` then
/// `to_block`. A row that is already the cover of its run is left alone
/// (it must not be tombstoned and re-inserted in the same block: same
/// key, same `_version`, and `ReplacingMergeTree` would pick either).
pub fn compaction_writes(
    live: &[DatabaseCheckpoint],
    version: u64,
) -> Vec<CheckpointWrite> {
    let mut writes = Vec::new();
    let mut run: Vec<&DatabaseCheckpoint> = Vec::new();
    let mut end = 0u64;

    fn flush(
        run: &mut Vec<&DatabaseCheckpoint>,
        end: u64,
        version: u64,
        writes: &mut Vec<CheckpointWrite>,
    ) {
        if run.len() >= 2 {
            let first = run[0];
            let epoch =
                run.iter().map(|c| c.epoch).max().unwrap_or_default();

            writes.push(CheckpointWrite {
                chain: first.chain,
                from_block: first.from_block,
                to_block: end,
                epoch,
                _version: version,
                is_deleted: 0,
            });

            writes.extend(
                run.iter()
                    .filter(|c| {
                        c.from_block != first.from_block
                            || c.to_block != end
                    })
                    .map(|c| CheckpointWrite {
                        chain: c.chain,
                        from_block: c.from_block,
                        to_block: c.to_block,
                        epoch: c.epoch,
                        _version: version,
                        is_deleted: 1,
                    }),
            );
        }

        run.clear();
    }

    for checkpoint in live {
        if !run.is_empty() && checkpoint.from_block > end {
            flush(&mut run, end, version, &mut writes);
        }

        if run.is_empty() {
            end = checkpoint.to_block;
        } else {
            end = end.max(checkpoint.to_block);
        }

        run.push(checkpoint);
    }

    flush(&mut run, end, version, &mut writes);

    writes
}

/// Upper bound on the gap rows fetched per pass, so memory stays bounded
/// even for a table that looks like swiss cheese. The remaining holes are
/// picked up by the next pass.
pub const MAX_GAPS_PER_PASS: usize = 10_000;

#[derive(Debug, Clone, Copy, Row, Deserialize, PartialEq, Eq)]
pub struct RangeStats {
    /// Distinct indexed block numbers inside the range.
    pub indexed: u64,
    /// Highest indexed block number inside the range (0 when none).
    pub max_number: u64,
}

#[derive(Debug, Clone, Copy, Row, Deserialize, PartialEq, Eq)]
pub struct GapRow {
    /// First missing block.
    pub gap_start: i64,
    /// First indexed block after the gap (exclusive end).
    pub gap_end: i64,
}

/// `FINAL`: a tombstoned block (rolled back, docs/design.md section 2) is
/// NOT indexed, it has to show up as a gap and be streamed again. Without
/// it the dead row would still be seen and the range would look complete.
fn indexed_numbers_sql(chain: u64, range: BlockRange) -> String {
    format!(
        "SELECT DISTINCT number FROM blocks FINAL \
         WHERE chain = {chain} \
         AND number >= {from} AND number < {to}",
        from = range.from,
        to = range.to,
    )
}

/// How many blocks of the range are indexed, and the highest one.
pub fn stats_sql(chain: u64, range: BlockRange) -> String {
    format!(
        "SELECT toUInt64(count()) AS indexed, \
         toUInt64(max(number)) AS max_number \
         FROM ({})",
        indexed_numbers_sql(chain, range)
    )
}

/// Holes BEFORE each indexed block of the range: every indexed number is
/// compared with its predecessor (window function, `from - 1` for the first
/// row so a hole at the very start of the range is reported too). The tail
/// after the highest indexed block is derived from [`RangeStats`].
pub fn gaps_sql(chain: u64, range: BlockRange, limit: usize) -> String {
    format!(
        "SELECT previous + 1 AS gap_start, current AS gap_end \
         FROM ( \
           SELECT toInt64(number) AS current, \
                  lagInFrame(toInt64(number), 1, toInt64({from}) - 1) \
                    OVER (ORDER BY number ASC \
                          ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) \
                    AS previous \
           FROM ({numbers}) \
         ) \
         WHERE current > previous + 1 \
         ORDER BY gap_start ASC \
         LIMIT {limit}",
        from = range.from,
        numbers = indexed_numbers_sql(chain, range),
    )
}

/// Result of a missing range computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingRanges {
    /// Ordered, non overlapping missing ranges.
    pub ranges: Vec<BlockRange>,
    /// Everything in the inspected range below this block is either indexed
    /// or covered by `ranges`. Equals the end of the inspected range unless
    /// the gap listing was truncated by the limit.
    pub covered_until: u64,
}

/// True when the stats alone prove there are no holes before `max_number`.
pub fn is_dense(range: BlockRange, stats: RangeStats) -> bool {
    stats.indexed > 0
        && stats.max_number >= range.from
        && (stats.max_number - range.from).checked_add(1)
            == Some(stats.indexed)
}

/// Pure range math: combines the SQL results into the list of ranges to
/// stream. `gaps` must be ordered by `gap_start`.
pub fn assemble_missing_ranges(
    range: BlockRange,
    stats: RangeStats,
    gaps: &[GapRow],
    limit: usize,
) -> MissingRanges {
    if range.is_empty() {
        return MissingRanges { ranges: vec![], covered_until: range.to };
    }

    if stats.indexed == 0 {
        return MissingRanges {
            ranges: vec![range],
            covered_until: range.to,
        };
    }

    let mut ranges: Vec<BlockRange> = Vec::with_capacity(gaps.len() + 1);

    for gap in gaps {
        // Defensive clamping: never trust the rows to be inside the range.
        let from =
            u64::try_from(gap.gap_start).unwrap_or(0).max(range.from);
        let to = u64::try_from(gap.gap_end).unwrap_or(0).min(range.to);

        if from >= to {
            continue;
        }

        match ranges.last_mut() {
            // Merge touching / overlapping rows.
            Some(last) if from <= last.to => last.to = last.to.max(to),
            _ => ranges.push(BlockRange::new(from, to)),
        }
    }

    // The listing was cut: only the blocks up to the last returned gap are
    // accounted for. The next pass continues from there.
    if gaps.len() >= limit {
        let covered_until =
            ranges.last().map(|last| last.to).unwrap_or(range.from);
        return MissingRanges { ranges, covered_until };
    }

    let tail_from = stats.max_number.saturating_add(1).max(range.from);

    if tail_from < range.to {
        match ranges.last_mut() {
            Some(last) if tail_from <= last.to => last.to = range.to,
            _ => ranges.push(BlockRange::new(tail_from, range.to)),
        }
    }

    MissingRanges { ranges, covered_until: range.to }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contiguous_ranges_of_block_numbers() {
        assert!(contiguous_ranges([]).is_empty());
        assert_eq!(
            contiguous_ranges([7, 5, 6, 6, 10, 12, 11]),
            vec![BlockRange::new(5, 8), BlockRange::new(10, 13)]
        );
        assert_eq!(
            contiguous_ranges([u64::MAX]),
            vec![BlockRange::new(u64::MAX, u64::MAX)]
        );
    }

    #[test]
    fn resume_point_is_the_end_of_the_contiguous_checkpoints() {
        assert_eq!(contiguous_until(10, []), 10);
        // Starts above the start block: nothing is covered.
        assert_eq!(contiguous_until(10, [(11, 20)]), 10);
        assert_eq!(contiguous_until(10, [(10, 20), (20, 30)]), 30);
        // A checkpoint straddling the start block counts.
        assert_eq!(contiguous_until(10, [(0, 15), (15, 18)]), 18);
        // Overlaps and contained ranges.
        assert_eq!(
            contiguous_until(0, [(0, 10), (2, 5), (8, 12), (12, 13)]),
            13
        );
        // A hole stops it, whatever comes later.
        assert_eq!(contiguous_until(0, [(0, 10), (11, 50)]), 10);
    }

    fn checkpoint(from_block: u64, to_block: u64) -> DatabaseCheckpoint {
        DatabaseCheckpoint {
            chain: 7,
            from_block,
            to_block,
            epoch: 0,
            _version: 1,
        }
    }

    /// What a compaction writes, `(from, to, is_deleted)` each.
    type Written = Vec<(u64, u64, u8)>;
    /// The live ranges a reader sees afterwards.
    type Claimed = Vec<(u64, u64)>;

    fn writes(live: &[DatabaseCheckpoint]) -> (Written, Claimed) {
        let written = compaction_writes(live, 99);
        assert!(
            written.iter().all(|w| w._version == 99 && w.chain == 7),
            "{written:?}"
        );

        // What a reader sees afterwards: the live rows that were not
        // tombstoned, plus the covers.
        let dead: Vec<(u64, u64)> = written
            .iter()
            .filter(|w| w.is_deleted == 1)
            .map(|w| (w.from_block, w.to_block))
            .collect();

        let mut after: Vec<(u64, u64)> = live
            .iter()
            .map(|c| (c.from_block, c.to_block))
            .filter(|range| !dead.contains(range))
            .chain(
                written
                    .iter()
                    .filter(|w| w.is_deleted == 0)
                    .map(|w| (w.from_block, w.to_block)),
            )
            .collect();
        after.sort_unstable();
        after.dedup();

        (
            written
                .iter()
                .map(|w| (w.from_block, w.to_block, w.is_deleted))
                .collect(),
            after,
        )
    }

    #[test]
    fn compaction_collapses_contiguous_checkpoints() {
        // The normal case: a run of flushes at the tip becomes one row.
        let live =
            [checkpoint(0, 10), checkpoint(10, 20), checkpoint(20, 30)];
        let (written, after) = writes(&live);
        assert_eq!(
            written,
            vec![(0, 30, 0), (0, 10, 1), (10, 20, 1), (20, 30, 1)]
        );
        assert_eq!(after, vec![(0, 30)]);
        // Same resume point before and after.
        assert_eq!(contiguous_until(0, after.iter().copied()), 30);

        // A hole is never bridged: the two runs stay two rows, and the
        // blocks in the hole stay missing.
        let live = [
            checkpoint(0, 10),
            checkpoint(10, 20),
            checkpoint(30, 40),
            checkpoint(40, 50),
        ];
        let (_, after) = writes(&live);
        assert_eq!(after, vec![(0, 20), (30, 50)]);
        assert_eq!(contiguous_until(0, after.iter().copied()), 20);

        // Overlapping and contained ranges (a re-streamed range after a
        // rollback) collapse too, and nothing is lost.
        let live = [
            checkpoint(0, 10),
            checkpoint(5, 30),
            checkpoint(7, 9),
            checkpoint(30, 31),
        ];
        let (_, after) = writes(&live);
        assert_eq!(after, vec![(0, 31)]);

        // A row that already IS the cover of its run is left alone: it
        // must never be tombstoned and re-inserted in the same block.
        let live = [checkpoint(0, 30), checkpoint(0, 10)];
        let (written, after) = writes(&live);
        assert_eq!(written, vec![(0, 30, 0), (0, 10, 1)]);
        assert_eq!(after, vec![(0, 30)]);

        // Nothing to do.
        assert!(compaction_writes(&[], 1).is_empty());
        assert!(compaction_writes(&[checkpoint(0, 10)], 1).is_empty());
        assert!(compaction_writes(
            &[checkpoint(0, 10), checkpoint(20, 30)],
            1
        )
        .is_empty());
    }

    /// The sweep cannot get stuck on a page of rows that all start at the
    /// same block (review F, MINOR 7).
    ///
    /// `Database::compact_checkpoints` takes the next cursor from the last
    /// row of the page it read, so a FULL page sharing one `from_block`
    /// would leave the cursor where it was. It cannot: rows sharing a
    /// `from_block` overlap by definition, so they are one run and the
    /// pass collapses them into a single row - the next pass reads a
    /// different page. (The cursor is additionally floored at `cursor + 1`
    /// in `compact_checkpoints`, as insurance rather than as the fix.)
    #[test]
    fn a_page_of_rows_that_all_start_together_is_collapsed() {
        let live: Vec<DatabaseCheckpoint> =
            (1..=50).map(|to| checkpoint(100, 100 + to)).collect();

        let (written, after) = writes(&live);

        assert_eq!(after, vec![(100, 150)], "{written:?}");
        assert_eq!(
            written.iter().filter(|w| w.2 == 1).count(),
            live.len() - 1,
            "every row but the cover is tombstoned"
        );
    }

    /// Running it again on the result writes nothing: a pass that died
    /// after the insert must not make the next one rewrite everything.
    #[test]
    fn compaction_is_idempotent() {
        let live =
            [checkpoint(0, 10), checkpoint(10, 20), checkpoint(25, 30)];
        let (_, after) = writes(&live);

        let compacted: Vec<DatabaseCheckpoint> = after
            .iter()
            .map(|(from, to)| checkpoint(*from, *to))
            .collect();

        assert!(compaction_writes(&compacted, 100).is_empty());
    }

    #[test]
    fn subtracting_known_ranges() {
        let r = BlockRange::new;

        assert_eq!(subtract_ranges(&[r(0, 10)], &[]), vec![r(0, 10)]);
        assert_eq!(subtract_ranges(&[r(0, 10)], &[r(0, 10)]), vec![]);
        assert_eq!(
            subtract_ranges(&[r(0, 10)], &[r(3, 5)]),
            vec![r(0, 3), r(5, 10)]
        );
        assert_eq!(
            subtract_ranges(
                &[r(0, 10), r(20, 30)],
                &[r(8, 25), r(28, 40)]
            ),
            vec![r(0, 8), r(25, 28)]
        );
        assert_eq!(
            subtract_ranges(&[r(0, 10)], &[r(50, 60), r(5, 5)]),
            vec![r(0, 10)]
        );
    }

    fn stats(indexed: u64, max_number: u64) -> RangeStats {
        RangeStats { indexed, max_number }
    }

    fn gap(gap_start: i64, gap_end: i64) -> GapRow {
        GapRow { gap_start, gap_end }
    }

    #[test]
    fn empty_table_means_the_whole_range() {
        let range = BlockRange::new(100, 200);
        let missing = assemble_missing_ranges(range, stats(0, 0), &[], 10);

        assert_eq!(missing.ranges, vec![range]);
        assert_eq!(missing.covered_until, 200);
    }

    #[test]
    fn empty_range_is_nothing() {
        let missing = assemble_missing_ranges(
            BlockRange::new(10, 10),
            stats(0, 0),
            &[],
            10,
        );
        assert!(missing.ranges.is_empty());
    }

    #[test]
    fn fully_indexed_range_has_no_work() {
        let range = BlockRange::new(0, 100);
        assert!(is_dense(range, stats(100, 99)));

        let missing =
            assemble_missing_ranges(range, stats(100, 99), &[], 10);

        assert!(missing.ranges.is_empty());
        assert_eq!(missing.covered_until, 100);
    }

    #[test]
    fn only_the_tail_is_missing() {
        let range = BlockRange::new(0, 100);
        assert!(is_dense(range, stats(50, 49)));

        let missing =
            assemble_missing_ranges(range, stats(50, 49), &[], 10);

        assert_eq!(missing.ranges, vec![BlockRange::new(50, 100)]);
    }

    #[test]
    fn head_inner_and_tail_gaps_in_order() {
        // indexed: 5..=9, 20..=29, 40 inside [0, 100)
        let range = BlockRange::new(0, 100);
        let s = stats(16, 40);
        assert!(!is_dense(range, s));

        let missing = assemble_missing_ranges(
            range,
            s,
            &[gap(0, 5), gap(10, 20), gap(30, 40)],
            10,
        );

        assert_eq!(
            missing.ranges,
            vec![
                BlockRange::new(0, 5),
                BlockRange::new(10, 20),
                BlockRange::new(30, 40),
                BlockRange::new(41, 100),
            ]
        );
        assert_eq!(missing.covered_until, 100);
    }

    #[test]
    fn start_block_offset_is_respected() {
        // indexed 1000..=1009 and 1015 inside [1000, 1020)
        let range = BlockRange::new(1000, 1020);
        let s = stats(11, 1015);
        assert!(!is_dense(range, s));

        let missing =
            assemble_missing_ranges(range, s, &[gap(1010, 1015)], 10);

        assert_eq!(
            missing.ranges,
            vec![BlockRange::new(1010, 1015), BlockRange::new(1016, 1020)]
        );
    }

    #[test]
    fn truncated_listing_does_not_claim_the_tail() {
        let range = BlockRange::new(0, 1000);
        let missing = assemble_missing_ranges(
            range,
            stats(500, 900),
            &[gap(0, 5), gap(10, 20)],
            2,
        );

        assert_eq!(
            missing.ranges,
            vec![BlockRange::new(0, 5), BlockRange::new(10, 20)]
        );
        // The pass only vouches for blocks below 20.
        assert_eq!(missing.covered_until, 20);
    }

    #[test]
    fn rows_outside_the_range_are_clamped_or_dropped() {
        let range = BlockRange::new(10, 50);
        let missing = assemble_missing_ranges(
            range,
            stats(3, 49),
            &[gap(-1, 12), gap(20, 80), gap(90, 95)],
            10,
        );

        assert_eq!(
            missing.ranges,
            vec![BlockRange::new(10, 12), BlockRange::new(20, 50)]
        );
    }

    #[test]
    fn range_math_does_not_overflow() {
        let range = BlockRange::new(u64::MAX - 1, u64::MAX);
        let missing =
            assemble_missing_ranges(range, stats(1, u64::MAX), &[], 10);
        assert!(missing.ranges.is_empty());
        assert_eq!(BlockRange::new(5, 3).len(), 0);
    }

    #[test]
    fn sql_is_scoped_to_chain_and_range() {
        let range = BlockRange::new(0, 500);
        let sql = gaps_sql(56, range, 1000);

        assert!(sql.contains("chain = 56"));
        // Tombstoned blocks must count as missing.
        assert!(sql.contains("FROM blocks FINAL "));
        assert!(sql.contains("number >= 0 AND number < 500"));
        // First row compares against `from - 1` (signed, from may be 0).
        assert!(sql.contains("toInt64(0) - 1"));
        assert!(sql.contains("lagInFrame"));
        assert!(sql.contains("LIMIT 1000"));

        let sql = stats_sql(56, range);
        assert!(sql.contains("count()"));
        assert!(sql.contains("max(number)"));
        assert!(sql.contains("FROM blocks FINAL "));
        assert!(sql.contains("chain = 56"));
    }
}
