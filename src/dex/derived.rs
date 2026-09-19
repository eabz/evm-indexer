//! The DEX aggregates as [`DerivedTable`]s, so `purge_range` can repair
//! their buckets after a reorg (docs/design.md §2).
//!
//! Nothing is deleted. A purge bumps the chain's epoch, records `(chain,
//! epoch, from_ts)` in `reorgs` and runs every `rebuild_sql`: the surviving
//! swaps (`FINAL` hides the tombstoned ones) of every bucket `>= from_ts`
//! are aggregated again under the NEW epoch. The `*_v` views ignore the
//! older epochs of those buckets (validity rule).
//!
//! Every `rebuild_sql` is the `SELECT` of the table's materialized view in
//! `migrations/0011_dex_aggregates.sql` with `FINAL`, the [`REBUILD_RANGE`]
//! and the new epoch instead of the rows' own. A unit test compares the
//! texts, and the ClickHouse integration tests check that a repaired index
//! equals a clean one.
//!
//! Placeholders: `{chain}`, `{from_ts}` (unix seconds, a multiple of
//! `bucket_seconds`; the start of the UTC day recorded in `reorgs` always
//! is), `{to_ts}` (exclusive) and `{epoch}` (the new epoch).
//!
//! **Run a rebuild through [`rebuild_statements`]**, never as one statement:
//! the aggregates are partitioned by month, and one INSERT that spans more
//! than `max_partitions_per_insert_block` (100) months - a gap heal deep in
//! history - is refused by ClickHouse. It yields one INSERT per month.

use crate::db::derived::DerivedTable;

/// Range predicate the rebuild adds to the view's `SELECT`.
pub const REBUILD_RANGE: &str = "chain = {chain} \
     AND timestamp >= toDateTime({from_ts}) \
     AND timestamp < toDateTime({to_ts})";

/// What the rebuild selects instead of the rows' own epoch.
pub const REBUILD_EPOCH: &str = "toUInt32({epoch}) AS epoch";

macro_rules! candles {
    ($name:literal, $seconds:literal) => {
        DerivedTable {
            name: $name,
            bucket_seconds: $seconds,
            bucket_column: "bucket",
            rebuild_sql: concat!(
                "INSERT INTO ",
                $name,
                " WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok,",
                " abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price,",
                " sqrt_price_x96 != 0 OR (reserve0 != 0 AND reserve1 != 0) AS pool_ok,",
                " if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), toFloat64(reserve1) / toFloat64(reserve0)) AS pool_price",
                " SELECT chain, pool_id, emitter,",
                " toDateTime(intDiv(toUInt32(timestamp), ",
                $seconds,
                ") * ",
                $seconds,
                ", 'UTC') AS bucket,",
                " toUInt32({epoch}) AS epoch,",
                " argMinStateIf(trade_price, (block_number, log_index), trade_ok) AS open,",
                " argMaxStateIf(trade_price, (block_number, log_index), trade_ok) AS close,",
                " max(if(trade_ok, trade_price, NULL)) AS high,",
                " min(if(trade_ok, trade_price, NULL)) AS low,",
                " countIf(trade_ok) AS trades,",
                " argMinStateIf(pool_price, (block_number, log_index), pool_ok) AS pool_open,",
                " argMaxStateIf(pool_price, (block_number, log_index), pool_ok) AS pool_close,",
                " max(if(pool_ok, pool_price, NULL)) AS pool_high,",
                " min(if(pool_ok, pool_price, NULL)) AS pool_low,",
                " countIf(pool_ok) AS pool_prices,",
                " sum(abs(toFloat64(amount0))) AS volume0,",
                " sum(abs(toFloat64(amount1))) AS volume1,",
                " count() AS swaps,",
                " uniqState(trader) AS traders",
                " FROM dex_swaps FINAL",
                " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND timestamp < toDateTime({to_ts})",
                " AND is_deleted = 0 AND protocol NOT IN ('balancer_v2', 'curve')",
                " GROUP BY chain, pool_id, emitter, bucket, epoch"
            ),
        }
    };
}

pub const DEX_CANDLES_1M: DerivedTable = candles!("dex_candles_1m", 60);
pub const DEX_CANDLES_1H: DerivedTable = candles!("dex_candles_1h", 3600);
pub const DEX_CANDLES_1D: DerivedTable = candles!("dex_candles_1d", 86400);

pub const DEX_POOL_VOLUME_1H: DerivedTable = DerivedTable {
    name: "dex_pool_volume_1h",
    bucket_seconds: 3600,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO dex_pool_volume_1h",
        " SELECT chain, pool_id, emitter, protocol,",
        " toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,",
        " verified_in AS token_in,",
        " verified_out AS token_out,",
        " toUInt32({epoch}) AS epoch,",
        " sum(toFloat64(dex_swaps.amount_in)) AS volume_in,",
        " sum(toFloat64(dex_swaps.amount_out)) AS volume_out,",
        " count() AS swaps,",
        " uniqState(trader) AS traders",
        " FROM dex_swaps FINAL",
        " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND timestamp < toDateTime({to_ts})",
        " AND is_deleted = 0",
        " GROUP BY chain, pool_id, emitter, protocol, bucket, token_in, token_out, epoch"
    ),
};

/// Every DEX aggregate, all fed from `dex_swaps`.
pub const DEX_DERIVED: &[DerivedTable] =
    &[DEX_CANDLES_1M, DEX_CANDLES_1H, DEX_CANDLES_1D, DEX_POOL_VOLUME_1H];

/// `rebuild_sql` with its placeholders filled in, for `[from_ts, to_ts)`.
/// Keep the range within one month (ClickHouse refuses an insert block of
/// more than 100 partitions): use [`rebuild_statements`].
pub fn render_rebuild(
    table: &DerivedTable,
    chain: u64,
    from_ts: u32,
    to_ts: u32,
    epoch: u32,
) -> String {
    table
        .rebuild_sql
        .replace("{chain}", &chain.to_string())
        .replace("{from_ts}", &from_ts.to_string())
        .replace("{to_ts}", &to_ts.to_string())
        .replace("{epoch}", &epoch.to_string())
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

/// The rebuild of `table` for `[from_ts, to_ts)` as one INSERT per UTC
/// month, oldest first. `to_ts` is exclusive: pass the timestamp of the
/// newest stored block + 1 (or "now"), not `u32::MAX`.
pub fn rebuild_statements(
    table: &DerivedTable,
    chain: u64,
    from_ts: u32,
    to_ts: u32,
    epoch: u32,
) -> Vec<String> {
    let mut statements = Vec::new();
    let mut start = from_ts - from_ts % table.bucket_seconds.max(1);

    while start < to_ts {
        let end = next_month_start(start).min(u64::from(to_ts)) as u32;
        statements.push(render_rebuild(table, chain, start, end, epoch));
        start = end;
    }

    statements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::sql::{normalize, statements, AGGREGATES_SQL};

    /// The `SELECT` of the materialized view feeding `table`.
    fn view_select(table: &str) -> String {
        let header = format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS {table}_mv TO {table} AS "
        );

        statements(AGGREGATES_SQL)
            .into_iter()
            .map(|statement| normalize(&statement))
            .find_map(|statement| {
                statement.strip_prefix(&header).map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no materialized view for {table}"))
    }

    /// The rebuild statement turned back into the view it must repeat:
    /// no `FINAL`, no range, the rows' own epoch.
    fn rebuild_select(table: &DerivedTable) -> String {
        let sql = normalize(table.rebuild_sql);
        let sql = sql
            .strip_prefix(&format!("INSERT INTO {} ", table.name))
            .expect("rebuild must insert into its own table")
            .to_owned();

        let ranged =
            format!(" FINAL WHERE {} AND ", normalize(REBUILD_RANGE));
        assert_eq!(sql.matches(&ranged).count(), 1, "{}", table.name);
        assert_eq!(
            sql.matches(REBUILD_EPOCH).count(),
            1,
            "{}",
            table.name
        );

        sql.replace(&ranged, " WHERE ").replace(REBUILD_EPOCH, "epoch")
    }

    #[test]
    fn rebuilds_repeat_their_materialized_view() {
        for table in DEX_DERIVED {
            assert_eq!(
                rebuild_select(table),
                view_select(table.name),
                "{}",
                table.name
            );
        }
    }

    #[test]
    fn buckets_match_the_declared_size() {
        for table in DEX_DERIVED {
            assert!([60, 3600, 86400].contains(&table.bucket_seconds));
            assert_eq!(table.bucket_column, "bucket");

            let bucket = format!(
                "intDiv(toUInt32(timestamp), {0}) * {0}, 'UTC') AS bucket",
                table.bucket_seconds
            );
            assert!(table.rebuild_sql.contains(&bucket), "{}", table.name);
        }
    }

    #[test]
    fn every_aggregate_table_is_declared() {
        let declared: Vec<&str> =
            DEX_DERIVED.iter().map(|table| table.name).collect();

        let aggregating: Vec<String> = statements(AGGREGATES_SQL)
            .into_iter()
            .map(|statement| normalize(&statement))
            .filter(|statement| statement.contains("AggregatingMergeTree"))
            .filter_map(|statement| {
                statement
                    .strip_prefix("CREATE TABLE IF NOT EXISTS ")
                    .and_then(|rest| rest.split_whitespace().next())
                    .map(str::to_owned)
            })
            .collect();

        assert_eq!(aggregating, declared);
    }

    #[test]
    fn placeholders_are_rendered() {
        let sql = render_rebuild(
            &DEX_CANDLES_1H,
            8453,
            1_700_006_400,
            1_700_010_000,
            7,
        );

        assert!(sql.contains("chain = 8453 AND"));
        assert!(sql.contains("timestamp >= toDateTime(1700006400)"));
        assert!(sql.contains("timestamp < toDateTime(1700010000)"));
        assert!(sql.contains("toUInt32(7) AS epoch"));
        assert!(!sql.contains('{'));
    }

    #[test]
    fn month_starts() {
        // 2023-11-15 -> 2023-12-01, 2023-12-31 23:59:59 -> 2024-01-01,
        // 2024-02-29 (leap) -> 2024-03-01, 1970-01-01 -> 1970-02-01.
        assert_eq!(next_month_start(1_700_006_400), 1_701_388_800);
        assert_eq!(next_month_start(1_704_067_199), 1_704_067_200);
        assert_eq!(next_month_start(1_709_164_800), 1_709_251_200);
        assert_eq!(next_month_start(0), 2_678_400);
        assert!(next_month_start(u32::MAX) > u64::from(u32::MAX));
    }

    #[test]
    fn a_deep_rebuild_is_one_insert_per_month() {
        // 2013-01-01 .. 2023-11-15: 131 months, more than ClickHouse
        // accepts in one insert block.
        let statements = rebuild_statements(
            &DEX_CANDLES_1D,
            1,
            1_356_998_400,
            1_700_006_400,
            3,
        );

        assert_eq!(statements.len(), 131);
        assert!(statements[0].contains(
            "timestamp >= toDateTime(1356998400) \
             AND timestamp < toDateTime(1359676800)"
        ));
        assert!(statements[130].contains(
            "timestamp >= toDateTime(1698796800) \
             AND timestamp < toDateTime(1700006400)"
        ));

        // Contiguous, nothing twice.
        for pair in statements.windows(2) {
            let end =
                pair[0].split("timestamp < toDateTime(").nth(1).unwrap();
            let end = end.split(')').next().unwrap();
            assert!(pair[1]
                .contains(&format!("timestamp >= toDateTime({end})")));
        }

        // An unaligned start is aligned down to its bucket.
        let one = rebuild_statements(
            &DEX_CANDLES_1D,
            1,
            1_700_006_401,
            1_700_010_000,
            1,
        );
        assert_eq!(one.len(), 1);
        assert!(one[0].contains("toDateTime(1700006400)"));
        assert!(rebuild_statements(&DEX_CANDLES_1D, 1, 86_400, 86_400, 1)
            .is_empty());
    }
}
