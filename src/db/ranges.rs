//! Missing block range computation.
//!
//! The `blocks` table is the commit marker: a block is indexed if and only
//! if its row exists. Instead of loading every indexed block number into
//! memory, the holes are computed inside ClickHouse and only the (few)
//! missing RANGES travel to the indexer.

use clickhouse::Row;
use serde::Deserialize;

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

fn indexed_numbers_sql(chain: u64, range: BlockRange) -> String {
    format!(
        "SELECT DISTINCT number FROM blocks \
         WHERE chain = {chain} AND is_uncle = false \
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
    fn sql_is_scoped_to_chain_canonical_blocks_and_range() {
        let range = BlockRange::new(0, 500);
        let sql = gaps_sql(56, range, 1000);

        assert!(sql.contains("chain = 56"));
        assert!(sql.contains("is_uncle = false"));
        assert!(sql.contains("number >= 0 AND number < 500"));
        // First row compares against `from - 1` (signed, from may be 0).
        assert!(sql.contains("toInt64(0) - 1"));
        assert!(sql.contains("lagInFrame"));
        assert!(sql.contains("LIMIT 1000"));

        let sql = stats_sql(56, range);
        assert!(sql.contains("count()"));
        assert!(sql.contains("max(number)"));
        assert!(sql.contains("chain = 56"));
    }
}
