//! Incremental aggregates and how to repair them.
//!
//! Aggregates are `AggregatingMergeTree` tables fed by materialized views
//! (`migrations/0003_core_aggregates.sql`, DEX: `0010+`). A view only ever
//! ADDS what an insert brings, it can not take back the rows a rollback
//! tombstones, and nothing is ever deleted (docs/design.md, section 2). So
//! every contribution is filed under the `epoch` of the rows it came from,
//! and a purge repairs the aggregates by epoch:
//!
//! ```text
//! epoch   = the chain's highest epoch + 1
//! from_ts = start of day (UTC) of the earliest purged row
//! to_ts   = start of the day AFTER the latest purged row
//! 1. INSERT INTO reorgs (chain, epoch, from_ts, to_ts, ...)
//! 2. table.rebuild_statements(chain, from_ts, to_ts, epoch, ...)
//!                                               -- for every DerivedTable
//! ```
//!
//! The `*_v` views (`0004`, through `epoch_floor_v`) then apply the
//! validity rule: a contribution with epoch `e` in bucket `b` counts iff
//! `e >= max(r.epoch)` over the `reorgs` rows of the chain with
//! `r.from_ts <= b AND b < r.to_ts` (0 when none covers `b`). Blocks
//! streamed afterwards carry the new epoch and flow through the views as
//! usual.
//!
//! The window is what makes the repair BOUNDED: a purge only invalidates
//! the buckets its own rows contributed to, so it hides and rebuilds
//! `[from_ts, to_ts)` and nothing else. A gap heal deep in history used to
//! re-aggregate the chain from that day to the head, on every pass.
//!
//! `from_ts` must be a start of day for EVERY table, whatever its bucket
//! width: the validity rule hides a whole bucket, so a rebuild has to cover
//! every bucket it hides, from its first second on.
//!
//! Amounts are aggregated as `Float64`, never as raw `UInt256` sums (the
//! "256-bit arithmetic rule" of docs/design.md: `sum(UInt256)` wraps
//! silently and hostile tokens emit `2^256-1`). A unit test rejects any
//! `sum(` over a 256 bit column in a view or in `rebuild_sql`.
//!
//! Buckets are computed from the unix timestamp with integer arithmetic
//! (`intDiv(ts, N) * N`) both here and in SQL, so they never depend on the
//! server time zone.

/// An aggregate table that needs bucket repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedTable {
    /// Target table of the materialized view.
    pub name: &'static str,
    /// Bucket width: 60, 3600, 86400.
    pub bucket_seconds: u32,
    /// `DateTime` column holding the bucket start.
    pub bucket_column: &'static str,
    /// `INSERT INTO <name> SELECT ..., toUInt32({epoch}) AS epoch, ... FROM
    /// <base> FINAL WHERE is_deleted = 0 AND chain = {chain} AND timestamp
    /// >= {from_ts} GROUP BY ...`. Must produce exactly what the view
    /// produces. `{chain}` and `{epoch}` are rendered as integers,
    /// `{from_ts}` as unix seconds.
    pub rebuild_sql: &'static str,
}

/// The widest bucket in play: `reorgs.from_ts` is a start of day (UTC).
pub const REPAIR_ALIGNMENT_SECONDS: u32 = 86_400;

/// `reorgs.from_ts` for a purge whose earliest row is at `timestamp`.
pub fn repair_start(timestamp: u32) -> u32 {
    timestamp - timestamp % REPAIR_ALIGNMENT_SECONDS
}

impl DerivedTable {
    /// Start of the bucket `timestamp` (unix seconds) falls into.
    pub fn bucket_start(&self, timestamp: u32) -> u32 {
        timestamp - timestamp % self.bucket_seconds.max(1)
    }

    /// `rebuild_sql` for `chain` over the buckets of `[from_ts, to_ts)`,
    /// as ONE statement. `from_ts` is aligned down with [`repair_start`],
    /// the same value `reorgs.from_ts` holds; `to_ts` is exclusive and is
    /// what `reorgs.to_ts` holds.
    ///
    /// **The upper bound is not an optimization.** The `reorgs` row raises
    /// the epoch floor of exactly `[from_ts, to_ts)`. A rebuild that
    /// reaches PAST `to_ts` files new-epoch contributions into buckets
    /// whose floor was not raised, where they are counted ON TOP of the
    /// old ones - a silent doubling. A rebuild that stops SHORT of `to_ts`
    /// leaves hidden buckets nobody refilled - a silent zero.
    ///
    /// `purged_from..purged_to` (open ended when `None`) is the block range
    /// of the purge this repair belongs to, which the rebuild must NOT
    /// count: the canonical blocks streamed afterwards add themselves
    /// through the view. Relying on the tombstones for that is not enough:
    ///
    /// - `blocks` is tombstoned LAST, after the repair (that is what makes
    ///   a crashed purge detectable), so its orphaned rows are still alive
    ///   when the rebuild runs;
    /// - ClickHouse gives no read-your-writes guarantee right after an
    ///   INSERT returns (seen on 25.12: a part can stay invisible to the
    ///   next query for a few ms), so a rebuild issued right after the
    ///   tombstones may not see them yet.
    ///
    /// So every `rebuild_sql` excludes the range itself, with the
    /// `{purge_from}` / `{purge_to}` placeholders. (A `rebuild_sql` without
    /// them is rendered unchanged.)
    pub fn rebuild_slice(
        &self,
        chain: u64,
        from_ts: u32,
        to_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> String {
        self.render_slice(
            chain,
            repair_start(from_ts),
            to_ts,
            epoch,
            purged_from,
            purged_to,
        )
    }

    /// The rebuild as ONE INSERT PER UTC MONTH of `[from_ts, to_ts)`,
    /// oldest first. The aggregates are `PARTITION BY toYYYYMM(bucket)` and
    /// ClickHouse refuses an insert block that touches more than 100
    /// partitions (code 252): a single open ended INSERT would make a purge
    /// deep in history (a gap heal while backfilling old blocks) fail at
    /// this step on every restart, for ever.
    ///
    /// `to_ts` is exclusive and is `reorgs.to_ts`: the start of the day
    /// after the newest row the purge removed (NEVER `u32::MAX` - see
    /// [`Self::rebuild_slice`] on why both bounds are load bearing).
    /// A `rebuild_sql` without a `{to_ts}` placeholder can not be bounded
    /// or sliced at all; `every_aggregate_can_be_bounded_and_sliced`
    /// asserts no declared aggregate is like that.
    pub fn rebuild_statements(
        &self,
        chain: u64,
        from_ts: u32,
        to_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> Vec<String> {
        if !self.rebuild_sql.contains("{to_ts}") {
            return vec![self.rebuild_slice(
                chain,
                from_ts,
                to_ts,
                epoch,
                purged_from,
                purged_to,
            )];
        }

        let mut statements = Vec::new();
        let mut start = repair_start(from_ts);

        while start < to_ts {
            let end = next_month_start(start).min(u64::from(to_ts)) as u32;
            statements.push(self.render_slice(
                chain,
                start,
                end,
                epoch,
                purged_from,
                purged_to,
            ));
            start = end;
        }

        statements
    }

    fn render_slice(
        &self,
        chain: u64,
        from_ts: u32,
        to_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> String {
        render(self.rebuild_sql, chain, from_ts, epoch)
            .replace("{to_ts}", &to_ts.to_string())
            .replace("{purge_from}", &purged_from.to_string())
            .replace(
                "{purge_to}",
                &purged_to.unwrap_or(u64::MAX).to_string(),
            )
    }
}

/// Unix seconds of the first instant of the UTC month after the one
/// `timestamp` falls into (civil-from-days, proleptic Gregorian).
pub fn next_month_start(timestamp: u32) -> u64 {
    let days = i64::from(timestamp / 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1_460
        + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    let (next_year, next_month) =
        if month == 12 { (year + 1, 1) } else { (year, month + 1) };

    // days-from-civil of the first of the next month
    let y = if next_month <= 2 { next_year - 1 } else { next_year };
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let shifted = (next_month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4
        - year_of_era / 100
        + day_of_year;

    ((era * 146_097 + day_of_era - 719_468) * 86_400) as u64
}

/// Fills the `{chain}`, `{from_ts}` and `{epoch}` placeholders.
fn render(sql: &str, chain: u64, from_ts: u32, epoch: u32) -> String {
    sql.replace("{chain}", &chain.to_string())
        .replace("{from_ts}", &from_ts.to_string())
        .replace("{epoch}", &epoch.to_string())
}

const DAY: u32 = 86_400;

/// Aggregates of `migrations/0003_core_aggregates.sql`. The SELECT of each
/// entry is the SELECT of its view plus `FINAL WHERE ...` (unit tested).
pub const CORE_DERIVED: &[DerivedTable] = &[
    DerivedTable {
        name: "daily_block_stats",
        bucket_seconds: DAY,
        bucket_column: "day",
        rebuild_sql: "INSERT INTO daily_block_stats \
            SELECT \
            chain, \
            toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day, \
            toUInt32({epoch}) AS epoch, \
            count() AS blocks, \
            sum(toUInt64(b.transactions)) AS transactions, \
            sum(b.gas_used) AS gas_used, \
            sum(b.gas_limit) AS gas_limit, \
            sum(b.size) AS size, \
            min(number) AS first_block, \
            max(number) AS last_block, \
            uniqState(miner) AS miners, \
            avgState(toFloat64(b.base_fee_per_gas)) AS base_fee_per_gas \
            FROM blocks AS b \
            FINAL WHERE is_deleted = 0 AND chain = {chain} AND timestamp >= {from_ts} AND timestamp < {to_ts} \
            AND NOT (number >= {purge_from} AND number < {purge_to}) \
            GROUP BY chain, day, epoch",
    },
    DerivedTable {
        name: "daily_transaction_stats",
        bucket_seconds: DAY,
        bucket_column: "day",
        rebuild_sql: "INSERT INTO daily_transaction_stats \
            SELECT \
            chain, \
            toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day, \
            toUInt32({epoch}) AS epoch, \
            count() AS transactions, \
            countIf(t.status = 'success') AS successful, \
            countIf(t.status = 'failure') AS failed, \
            countIf(t.`to` IS NULL) AS contract_creations, \
            sum(t.gas_used) AS gas_used, \
            sum(toFloat64(t.value)) AS value, \
            sum(toFloat64(t.gas_used) * toFloat64(t.effective_gas_price)) AS fees, \
            uniqState(t.`from`) AS senders, \
            uniqState(t.`to`) AS recipients, \
            avgState(toFloat64(t.effective_gas_price)) AS effective_gas_price \
            FROM transactions AS t \
            FINAL WHERE is_deleted = 0 AND chain = {chain} AND timestamp >= {from_ts} AND timestamp < {to_ts} \
            AND NOT (block_number >= {purge_from} AND block_number < {purge_to}) \
            GROUP BY chain, day, epoch",
    },
    DerivedTable {
        name: "daily_erc20_transfer_stats",
        bucket_seconds: DAY,
        bucket_column: "day",
        rebuild_sql: "INSERT INTO daily_erc20_transfer_stats \
            SELECT \
            chain, \
            token_address, \
            toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day, \
            toUInt32({epoch}) AS epoch, \
            count() AS transfers, \
            sum(toFloat64(e.amount)) AS volume_raw, \
            uniqState(e.`from`) AS senders, \
            uniqState(e.`to`) AS recipients \
            FROM erc20_transfers AS e \
            FINAL WHERE is_deleted = 0 AND chain = {chain} AND timestamp >= {from_ts} AND timestamp < {to_ts} \
            AND NOT (block_number >= {purge_from} AND block_number < {purge_to}) \
            GROUP BY chain, token_address, day, epoch",
    },
];

/// How a rebuild differs from the SELECT of the materialized view: it
/// reads deduplicated rows of one chain from `from_ts` on ...
#[cfg(test)]
const REBUILD_FILTER: &str =
    " FINAL WHERE is_deleted = 0 AND chain = {chain} AND timestamp >= {from_ts} AND timestamp < {to_ts}";
#[cfg(test)]
const VIEW_FILTER: &str = " WHERE is_deleted = 0";
/// ... without the range being purged (`number` when it reads `blocks`) ...
#[cfg(test)]
const REBUILD_EXCLUSION: &str =
    " AND NOT (block_number >= {purge_from} AND \
                                 block_number < {purge_to})";
/// ... and files everything under the new epoch instead of the rows' own.
#[cfg(test)]
const REBUILD_EPOCH: &str = "toUInt32({epoch}) AS epoch,";
#[cfg(test)]
const VIEW_EPOCH: &str = "epoch,";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::models::transaction::{STATUS_FAILURE, STATUS_SUCCESS},
        db::schema::{
            tables_with_columns,
            test_support::{view_selects, CORE_MIGRATIONS},
        },
    };

    fn normalize(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// SELECT of the (one) materialized view writing `TO <table>`.
    fn view_select(table: &str) -> String {
        let mut selects = view_selects(table);
        assert_eq!(selects.len(), 1, "{table}");
        selects.remove(0)
    }

    #[test]
    fn rebuilds_are_sliced_by_utc_month() {
        let table = DerivedTable {
            name: "t",
            bucket_seconds: 86_400,
            bucket_column: "day",
            rebuild_sql: "ts >= {from_ts} AND ts < {to_ts} e{epoch} \
                          NOT [{purge_from}, {purge_to})",
        };

        // 2023-11-14 22:13:20 .. 2024-02-10: Nov (from the start of the
        // day), Dec, Jan, Feb (up to `to_ts`).
        let statements = table.rebuild_statements(
            1,
            1_700_000_000,
            1_707_523_200,
            3,
            10,
            Some(20),
        );
        assert_eq!(
            statements,
            vec![
                "ts >= 1699920000 AND ts < 1701388800 e3 NOT [10, 20)",
                "ts >= 1701388800 AND ts < 1704067200 e3 NOT [10, 20)",
                "ts >= 1704067200 AND ts < 1706745600 e3 NOT [10, 20)",
                "ts >= 1706745600 AND ts < 1707523200 e3 NOT [10, 20)",
            ]
        );

        // 15 years: far more than the 100 partitions one INSERT may touch,
        // and every slice stays inside one month.
        let statements = table.rebuild_statements(
            1,
            1_438_269_973,
            1_911_655_573,
            1,
            0,
            None,
        );
        assert_eq!(statements.len(), 181);

        // Nothing to rebuild.
        assert!(table
            .rebuild_statements(
                1,
                1_700_000_000,
                1_600_000_000,
                1,
                0,
                None
            )
            .is_empty());

        // Every core aggregate can be sliced.
        for table in CORE_DERIVED {
            assert!(
                table.rebuild_sql.contains("{to_ts}"),
                "{}",
                table.name
            );
        }

        assert_eq!(next_month_start(0), 2_678_400);
        assert_eq!(next_month_start(1_709_164_800), 1_709_251_200); // leap Feb 29
        assert_eq!(next_month_start(1_703_980_800), 1_704_067_200); // Dec -> Jan
    }

    #[test]
    fn renders_placeholders_and_aligns_to_the_day() {
        let table = DerivedTable {
            name: "t",
            bucket_seconds: 3_600,
            bucket_column: "hour",
            rebuild_sql:
                "INSERT INTO t SELECT {epoch} FROM b FINAL WHERE \
                          chain = {chain} AND timestamp >= {from_ts}",
        };

        assert_eq!(table.bucket_start(7_200), 7_200);
        assert_eq!(table.bucket_start(7_201), 7_200);
        assert_eq!(table.bucket_start(10_799), 7_200);
        assert_eq!(table.bucket_start(0), 0);
        assert_eq!(
            table.bucket_start(u32::MAX),
            u32::MAX - u32::MAX % 3_600
        );

        // Whatever the bucket width, a repair starts at a start of day:
        // it is what `reorgs.from_ts` holds and what the views hide from.
        assert_eq!(repair_start(86_400 + 7_201), 86_400);
        assert_eq!(repair_start(86_399), 0);
        assert_eq!(
            table.rebuild_slice(
                137,
                86_400 + 7_201,
                u32::MAX,
                4,
                10,
                Some(20)
            ),
            "INSERT INTO t SELECT 4 FROM b FINAL WHERE chain = 137 AND \
             timestamp >= 86400"
        );

        let blocks = DerivedTable {
            rebuild_sql: "number >= {purge_from} AND number < {purge_to}",
            ..table
        };
        assert_eq!(
            blocks.rebuild_slice(1, 0, u32::MAX, 1, 10, Some(20)),
            "number >= 10 AND number < 20"
        );
        assert_eq!(
            blocks.rebuild_slice(1, 0, u32::MAX, 1, 10, None),
            format!("number >= 10 AND number < {}", u64::MAX)
        );
    }

    #[test]
    fn daily_buckets_are_utc_days() {
        let table = CORE_DERIVED[0];
        // 2023-11-14T22:13:20Z -> 2023-11-14T00:00:00Z
        assert_eq!(table.bucket_start(1_700_000_000), 1_699_920_000);
    }

    #[test]
    fn every_core_aggregate_is_declared_once() {
        // Every table of 0003 is an aggregate fed by a materialized view.
        let aggregates: Vec<String> =
            tables_with_columns(CORE_MIGRATIONS[2].1)
                .into_iter()
                .map(|(table, _)| table)
                .filter(|table| !view_selects(table).is_empty())
                .collect();

        let declared: Vec<&str> =
            CORE_DERIVED.iter().map(|table| table.name).collect();

        assert_eq!(aggregates, declared);
    }

    #[test]
    fn rebuild_sql_is_the_view_select_over_final_rows() {
        for table in CORE_DERIVED {
            let sql = normalize(table.rebuild_sql);

            // No placeholder left behind, none unknown.
            let rendered = table.rebuild_slice(1, 0, u32::MAX, 3, 5, None);
            assert!(!rendered.contains('{'), "{rendered}");
            assert_eq!(
                sql.matches("{chain}").count(),
                1,
                "{}",
                table.name
            );
            assert_eq!(
                sql.matches("{epoch}").count(),
                1,
                "{}",
                table.name
            );
            assert_eq!(
                sql.matches("{from_ts}").count(),
                1,
                "{}",
                table.name
            );

            let prefix = format!("INSERT INTO {} ", table.name);
            let select = sql
                .strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{}: {sql}", table.name));

            assert_eq!(
                select.matches(REBUILD_FILTER).count(),
                1,
                "{}",
                table.name
            );

            assert_eq!(select.matches(REBUILD_EPOCH).count(), 1);

            // The purged range is left out by the statement itself, by
            // the block number column of the table it reads.
            let exclusion = if select.contains(" FROM blocks AS ") {
                REBUILD_EXCLUSION.replace("block_number", "number")
            } else {
                REBUILD_EXCLUSION.to_string()
            };
            assert_eq!(
                select.matches(&exclusion).count(),
                1,
                "{}",
                table.name
            );

            assert_eq!(
                select
                    .replace(&exclusion, "")
                    .replace(REBUILD_FILTER, VIEW_FILTER)
                    .replace(REBUILD_EPOCH, VIEW_EPOCH),
                view_select(table.name),
                "{}: rebuild_sql and the materialized view differ",
                table.name
            );

            // Filed under its epoch, which is part of the sorting key.
            assert!(select.ends_with(", epoch"), "{}", table.name);

            // The bucket expression matches bucket_seconds / bucket_column.
            assert!(
                select.contains(&format!(
                    "intDiv(toUnixTimestamp(timestamp), {0}) * {0}, 'UTC') \
                     AS {1},",
                    table.bucket_seconds, table.bucket_column
                )),
                "{}",
                table.name
            );
        }
    }

    /// Arguments of every `sum(...)` call in `sql`.
    fn sum_arguments(sql: &str) -> Vec<&str> {
        let mut arguments = Vec::new();
        let mut rest = sql;

        while let Some(at) = rest.find("sum(") {
            let inner = &rest[at + 4..];
            let mut depth = 1usize;
            let mut end = inner.len();
            for (index, c) in inner.char_indices() {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    end = index;
                    break;
                }
            }
            arguments.push(&inner[..end]);
            rest = &inner[end..];
        }

        arguments
    }

    #[test]
    fn amounts_are_never_summed_as_raw_256_bit_integers() {
        // UInt256 columns of the base tables an aggregate may touch.
        const WIDE: [&str; 8] = [
            "value",
            "amount",
            "effective_gas_price",
            "gas_price",
            "base_fee_per_gas",
            "max_fee_per_gas",
            "max_priority_fee_per_gas",
            "difficulty",
        ];

        assert_eq!(
            sum_arguments("sum(a) + sum(f(b) * c)"),
            ["a", "f(b) * c"]
        );

        for table in CORE_DERIVED {
            assert!(
                !table.rebuild_sql.contains("toUInt256"),
                "{}",
                table.name
            );
            assert!(
                !table.rebuild_sql.contains("toInt256"),
                "{}",
                table.name
            );

            for argument in sum_arguments(table.rebuild_sql) {
                for column in WIDE {
                    assert!(
                        !argument.contains(column)
                            || argument.starts_with("toFloat64("),
                        "{}: sum({argument}) would wrap, sum toFloat64() \
                         instead",
                        table.name
                    );
                }
            }
        }
    }

    #[test]
    fn status_comparisons_use_the_stored_values() {
        let sql = CORE_DERIVED[1].rebuild_sql;
        assert!(sql.contains(&format!("status = '{STATUS_SUCCESS}'")));
        assert!(sql.contains(&format!("status = '{STATUS_FAILURE}'")));
        assert!(!sql.contains("0x1"));
    }
}
