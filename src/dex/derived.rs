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
//! `migrations/0011_dex_aggregates.sql` with `FINAL`, the `chain = {chain}
//! AND timestamp >= toDateTime({from_ts})` range and the new epoch instead
//! of the rows' own. A unit test compares the texts, and the ClickHouse
//! integration tests check that a repaired index equals a clean one.
//!
//! Placeholders: `{chain}` = chain id, `{from_ts}` = unix seconds of the
//! first bucket to rebuild (a multiple of `bucket_seconds`; the start of
//! the UTC day recorded in `reorgs` always is), `{epoch}` = the new epoch.

use crate::db::derived::DerivedTable;

/// Range predicate the rebuild adds to the view's `SELECT`.
pub const REBUILD_RANGE: &str =
    "chain = {chain} AND timestamp >= toDateTime({from_ts})";

macro_rules! candles {
    ($name:literal, $seconds:literal) => {
        DerivedTable {
            name: $name,
            bucket_seconds: $seconds,
            bucket_column: "bucket",
            rebuild_sql: concat!(
                "INSERT INTO ",
                $name,
                " WITH if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), abs(toFloat64(amount1)) / abs(toFloat64(amount0))) AS price",
                " SELECT chain, pool_id, emitter,",
                " toDateTime(intDiv(toUInt32(timestamp), ",
                $seconds,
                ") * ",
                $seconds,
                ", 'UTC') AS bucket,",
                " toUInt32({epoch}) AS epoch,",
                " argMinState(price, (block_number, log_index)) AS open,",
                " argMaxState(price, (block_number, log_index)) AS close,",
                " max(price) AS high,",
                " min(price) AS low,",
                " sum(abs(toFloat64(amount0))) AS volume0,",
                " sum(abs(toFloat64(amount1))) AS volume1,",
                " count() AS swaps,",
                " uniqState(trader) AS traders",
                " FROM dex_swaps FINAL",
                " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts})",
                " AND is_deleted = 0",
                " AND (sqrt_price_x96 != 0 OR (amount0 != 0 AND amount1 != 0))",
                " GROUP BY chain, pool_id, emitter, bucket, epoch"
            ),
        }
    };
}

pub const DEX_CANDLES_1M: DerivedTable = candles!("dex_candles_1m", 60);
pub const DEX_CANDLES_1H: DerivedTable = candles!("dex_candles_1h", 3600);
pub const DEX_CANDLES_1D: DerivedTable = candles!("dex_candles_1d", 86400);

pub const DEX_POOL_VOLUME_1D: DerivedTable = DerivedTable {
    name: "dex_pool_volume_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO dex_pool_volume_1d",
        " WITH toFixedString('', 20) AS zero, toFloat64(0) AS none,",
        " arrayJoin(multiIf(protocol = 'curve',",
        " [(if(underlying, 'ucoin', 'coin'), coin_in, zero, toFloat64(amount_in), none),",
        " (if(underlying, 'ucoin', 'coin'), coin_out, zero, none, toFloat64(amount_out))],",
        " token_in != zero OR token_out != zero,",
        " [('token', toUInt8(0), token_in, toFloat64(amount_in), none),",
        " ('token', toUInt8(0), token_out, none, toFloat64(amount_out))],",
        " [('side', toUInt8(0), zero, if(amount0 > 0, abs(toFloat64(amount0)), none), if(amount0 < 0, abs(toFloat64(amount0)), none)),",
        " ('side', toUInt8(1), zero, if(amount1 > 0, abs(toFloat64(amount1)), none), if(amount1 < 0, abs(toFloat64(amount1)), none))])) AS leg",
        " SELECT chain, pool_id, emitter, protocol,",
        " toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,",
        " leg.1 AS leg_kind,",
        " leg.2 AS leg_index,",
        " leg.3 AS leg_token,",
        " toUInt32({epoch}) AS epoch,",
        " sum(leg.4) AS volume_in,",
        " sum(leg.5) AS volume_out,",
        " count() AS swaps,",
        " uniqState(trader) AS traders",
        " FROM dex_swaps FINAL",
        " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts})",
        " AND is_deleted = 0",
        " GROUP BY chain, pool_id, emitter, protocol, bucket, leg_kind, leg_index, leg_token, epoch"
    ),
};

pub const DEX_PROTOCOL_STATS_1D: DerivedTable = DerivedTable {
    name: "dex_protocol_stats_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO dex_protocol_stats_1d",
        " SELECT chain, protocol,",
        " toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,",
        " toUInt32({epoch}) AS epoch,",
        " count() AS swaps,",
        " uniqState(trader) AS traders,",
        " uniqState(pool_id, emitter) AS pools",
        " FROM dex_swaps FINAL",
        " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts})",
        " AND is_deleted = 0",
        " GROUP BY chain, protocol, bucket, epoch"
    ),
};

/// Every DEX aggregate, all fed from `dex_swaps`.
pub const DEX_DERIVED: &[DerivedTable] = &[
    DEX_CANDLES_1M,
    DEX_CANDLES_1H,
    DEX_CANDLES_1D,
    DEX_POOL_VOLUME_1D,
    DEX_PROTOCOL_STATS_1D,
];

/// What the rebuild selects instead of the rows' own epoch.
pub const REBUILD_EPOCH: &str = "toUInt32({epoch}) AS epoch";

/// `rebuild_sql` with its placeholders filled in.
pub fn render_rebuild(
    table: &DerivedTable,
    chain: u64,
    from_ts: u32,
    epoch: u32,
) -> String {
    table
        .rebuild_sql
        .replace("{chain}", &chain.to_string())
        .replace("{from_ts}", &from_ts.to_string())
        .replace("{epoch}", &epoch.to_string())
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

        let ranged = format!(" FINAL WHERE {REBUILD_RANGE} AND ");
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
        let sql = render_rebuild(&DEX_CANDLES_1H, 8453, 1_700_006_400, 7);

        assert!(sql.contains("chain = 8453 AND"));
        assert!(sql.contains("toDateTime(1700006400)"));
        assert!(sql.contains("toUInt32(7) AS epoch"));
        assert!(!sql.contains('{'));
    }
}
