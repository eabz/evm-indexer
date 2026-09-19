//! The prediction market aggregates as [`DerivedTable`]s, so `purge_range`
//! can repair their buckets after a reorg (docs/design.md §2).
//!
//! Nothing is deleted. A purge bumps the chain's epoch, records `(chain,
//! epoch, from_ts)` in `reorgs` and runs every `rebuild_sql`: the surviving
//! rows (`FINAL` hides the tombstoned ones) of every bucket `>= from_ts`
//! are aggregated again under the NEW epoch. The `*_v` views ignore the
//! older epochs of those buckets (validity rule, `epoch_floor_v`).
//!
//! Every `rebuild_sql` is the `SELECT` of the table's materialized view in
//! `migrations/0021_prediction_aggregates.sql` with `FINAL`, the range
//! [`REBUILD_RANGE`] and the new epoch instead of the rows' own. The
//! constants below are GENERATED from the migration (scratch script
//! `gen_derived.py`: whitespace collapsed, two textual substitutions); a
//! unit test redoes the substitutions and compares, and the ClickHouse
//! integration tests check that a repaired index equals a clean one.
//!
//! Placeholders: `{chain}` = chain id, `{from_ts}` = unix seconds of the
//! first bucket to rebuild (a multiple of `bucket_seconds`; the start of
//! the UTC day recorded in `reorgs` always is), `{epoch}` = the new epoch,
//! `{purge_from}` / `{purge_to}` = the purged block range `[from, to)`,
//! excluded explicitly so a rebuild never depends on seeing the tombstones
//! (the pattern every module shares).

use crate::db::derived::DerivedTable;

/// What a rebuild puts in place of the view's `WHERE is_deleted = 0`.
pub const REBUILD_RANGE: &str = "FINAL WHERE chain = {chain} AND timestamp \
                                 >= toDateTime({from_ts}) AND NOT \
                                 (block_number >= {purge_from} AND \
                                 block_number < {purge_to}) AND is_deleted = 0";

/// What a rebuild selects in place of the rows' own `epoch`.
pub const REBUILD_EPOCH: &str = "toUInt32({epoch}) AS epoch";

pub const PREDICTION_CANDLES_1M: DerivedTable = DerivedTable {
    name: "prediction_candles_1m",
    bucket_seconds: 60,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_candles_1m WITH toFloat64(tupleElement(print, 2)) /",
        " toFloat64(share_amount) AS price SELECT chain, registry, tupleElement(print, 1) AS",
        " outcome_token_id, toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS bucket,",
        " toUInt32({epoch}) AS epoch, argMinState(price, (block_number, log_index)) AS open,",
        " argMaxState(price, (block_number, log_index)) AS close, max(price) AS high, min(price)",
        " AS low, sum(toFloat64(tupleElement(print, 2))) AS volume, sum(toFloat64(share_amount))",
        " AS shares, count() AS trades, sum(toUInt64(tupleElement(print, 3))) AS fills,",
        " uniqArrayState([maker, taker]) AS traders, max(timestamp) AS last_trade_at FROM ( SELECT",
        " chain, registry, block_number, log_index, timestamp, epoch, maker, taker, share_amount,",
        " arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id,",
        " collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)),",
        " (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print FROM",
        " prediction_trades FINAL WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND",
        " NOT (block_number >= {purge_from} AND block_number < {purge_to}) AND is_deleted = 0 AND",
        " share_amount != 0 ) WHERE tupleElement(print, 2) <= share_amount GROUP BY chain,",
        " registry, outcome_token_id, bucket, epoch",
    ),
};

pub const PREDICTION_CANDLES_1H: DerivedTable = DerivedTable {
    name: "prediction_candles_1h",
    bucket_seconds: 3600,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_candles_1h WITH toFloat64(tupleElement(print, 2)) /",
        " toFloat64(share_amount) AS price SELECT chain, registry, tupleElement(print, 1) AS",
        " outcome_token_id, toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,",
        " toUInt32({epoch}) AS epoch, argMinState(price, (block_number, log_index)) AS open,",
        " argMaxState(price, (block_number, log_index)) AS close, max(price) AS high, min(price)",
        " AS low, sum(toFloat64(tupleElement(print, 2))) AS volume, sum(toFloat64(share_amount))",
        " AS shares, count() AS trades, sum(toUInt64(tupleElement(print, 3))) AS fills,",
        " uniqArrayState([maker, taker]) AS traders, max(timestamp) AS last_trade_at FROM ( SELECT",
        " chain, registry, block_number, log_index, timestamp, epoch, maker, taker, share_amount,",
        " arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id,",
        " collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)),",
        " (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print FROM",
        " prediction_trades FINAL WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND",
        " NOT (block_number >= {purge_from} AND block_number < {purge_to}) AND is_deleted = 0 AND",
        " share_amount != 0 ) WHERE tupleElement(print, 2) <= share_amount GROUP BY chain,",
        " registry, outcome_token_id, bucket, epoch",
    ),
};

pub const PREDICTION_CANDLES_1D: DerivedTable = DerivedTable {
    name: "prediction_candles_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_candles_1d WITH toFloat64(tupleElement(print, 2)) /",
        " toFloat64(share_amount) AS price SELECT chain, registry, tupleElement(print, 1) AS",
        " outcome_token_id, toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS",
        " bucket, toUInt32({epoch}) AS epoch, argMinState(price, (block_number, log_index)) AS",
        " open, argMaxState(price, (block_number, log_index)) AS close, max(price) AS high,",
        " min(price) AS low, sum(toFloat64(tupleElement(print, 2))) AS volume,",
        " sum(toFloat64(share_amount)) AS shares, count() AS trades,",
        " sum(toUInt64(tupleElement(print, 3))) AS fills, uniqArrayState([maker, taker]) AS",
        " traders, max(timestamp) AS last_trade_at FROM ( SELECT chain, registry, block_number,",
        " log_index, timestamp, epoch, maker, taker, share_amount,",
        " arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id,",
        " collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)),",
        " (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print FROM",
        " prediction_trades FINAL WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND",
        " NOT (block_number >= {purge_from} AND block_number < {purge_to}) AND is_deleted = 0 AND",
        " share_amount != 0 ) WHERE tupleElement(print, 2) <= share_amount GROUP BY chain,",
        " registry, outcome_token_id, bucket, epoch",
    ),
};

pub const PREDICTION_MARKET_FLOWS_1D: DerivedTable = DerivedTable {
    name: "prediction_market_flows_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_market_flows_1d SELECT chain, emitter AS registry, market_id,",
        " collateral_token, toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS",
        " bucket, toUInt32({epoch}) AS epoch, sum(if(kind = 'split', toFloat64(amount), 0.)) AS",
        " split, sum(if(kind = 'merge', toFloat64(amount), 0.)) AS merged, sum(if(kind = 'redeem',",
        " toFloat64(amount), 0.)) AS redeemed, count() AS events, uniqState(stakeholder) AS",
        " stakeholders FROM prediction_position_events FINAL WHERE chain = {chain} AND timestamp",
        " >= toDateTime({from_ts}) AND NOT (block_number >= {purge_from} AND block_number <",
        " {purge_to}) AND is_deleted = 0 AND protocol = 'ctf' GROUP BY chain, registry, market_id,",
        " collateral_token, bucket, epoch",
    ),
};

pub const PREDICTION_TRADER_TRADES_1D: DerivedTable = DerivedTable {
    name: "prediction_trader_trades_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_trader_trades_1d SELECT chain,",
        " toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,",
        " tupleElement(party, 1) AS trader, exchange, toUInt32({epoch}) AS epoch,",
        " sum(if(tupleElement(party, 2) = 'buy', toFloat64(tupleElement(party, 3)), 0.)) AS",
        " bought, sum(if(tupleElement(party, 2) = 'sell', toFloat64(tupleElement(party, 3)), 0.))",
        " AS sold, sum(if(tupleElement(party, 5) = 'collateral', toFloat64(tupleElement(party,",
        " 4)), 0.)) AS fees, count() AS trades, uniqState(tupleElement(party, 6)) AS tokens FROM (",
        " SELECT chain, timestamp, exchange, epoch, share_amount, arrayJoin([(maker,",
        " toString(maker_side), maker_collateral_amount, maker_fee_amount,",
        " toString(maker_fee_unit), maker_outcome_token_id), (taker, toString(side),",
        " collateral_amount, taker_fee_amount, toString(taker_fee_unit), outcome_token_id)]) AS",
        " party FROM prediction_trades FINAL WHERE chain = {chain} AND timestamp >=",
        " toDateTime({from_ts}) AND NOT (block_number >= {purge_from} AND block_number <",
        " {purge_to}) AND is_deleted = 0 ) WHERE tupleElement(party, 3) <= share_amount GROUP BY",
        " chain, bucket, trader, exchange, epoch",
    ),
};

pub const PREDICTION_TRADER_FLOWS_1D: DerivedTable = DerivedTable {
    name: "prediction_trader_flows_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO prediction_trader_flows_1d SELECT chain,",
        " toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket, stakeholder AS",
        " trader, collateral_token, toUInt32({epoch}) AS epoch, sum(if(kind = 'split',",
        " toFloat64(amount), 0.)) AS split, sum(if(kind = 'merge', toFloat64(amount), 0.)) AS",
        " merged, sum(if(kind = 'redeem', toFloat64(amount), 0.)) AS redeemed, count() AS events",
        " FROM prediction_position_events FINAL WHERE chain = {chain} AND timestamp >=",
        " toDateTime({from_ts}) AND NOT (block_number >= {purge_from} AND block_number <",
        " {purge_to}) AND is_deleted = 0 AND kind != 'convert' GROUP BY chain, bucket, trader,",
        " collateral_token, epoch",
    ),
};

/// Every aggregate of `migrations/0021_prediction_aggregates.sql`.
pub const PREDICTIONS_DERIVED: &[DerivedTable] = &[
    PREDICTION_CANDLES_1M,
    PREDICTION_CANDLES_1H,
    PREDICTION_CANDLES_1D,
    PREDICTION_MARKET_FLOWS_1D,
    PREDICTION_TRADER_TRADES_1D,
    PREDICTION_TRADER_FLOWS_1D,
];

// STUB: until `DerivedTable::rebuild_sql(chain, from_ts, epoch, ..)` of
// the core (docs/design.md §1) is merged, this renders the placeholders.
// `from_ts` is aligned down to the table's bucket.
/// `purged` = the purged block range `[from, to)`, `to = None` for an
/// open ended purge (a reorg rollback).
pub fn render_rebuild(
    table: &DerivedTable,
    chain: u64,
    from_ts: u32,
    epoch: u32,
    purged: (u64, Option<u64>),
) -> String {
    table
        .rebuild_sql
        .replace("{chain}", &chain.to_string())
        .replace("{from_ts}", &table.bucket_start(from_ts).to_string())
        .replace("{epoch}", &epoch.to_string())
        .replace("{purge_from}", &purged.0.to_string())
        .replace("{purge_to}", &purged.1.unwrap_or(u64::MAX).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictions::sql::{normalize, statements, AGGREGATES_SQL};

    /// SELECT of the materialized view writing `TO <table>`.
    fn view_select(table: &str) -> String {
        let marker = format!("_mv TO {table} AS ");

        statements(AGGREGATES_SQL)
            .iter()
            .map(|statement| normalize(statement))
            .find_map(|statement| {
                statement
                    .starts_with("CREATE MATERIALIZED VIEW")
                    .then(|| statement.split_once(&marker))
                    .flatten()
                    .map(|(_, select)| select.to_owned())
            })
            .unwrap_or_else(|| panic!("no materialized view TO {table}"))
    }

    #[test]
    fn every_aggregate_is_declared_once() {
        let mut tables: Vec<String> = statements(AGGREGATES_SQL)
            .iter()
            .map(|statement| normalize(statement))
            .filter(|statement| statement.contains("AggregatingMergeTree"))
            .filter_map(|statement| {
                statement
                    .strip_prefix("CREATE TABLE IF NOT EXISTS ")
                    .and_then(|rest| rest.split_once(' '))
                    .map(|(name, _)| name.to_owned())
            })
            .collect();
        tables.sort();

        let mut declared: Vec<String> = PREDICTIONS_DERIVED
            .iter()
            .map(|table| table.name.to_owned())
            .collect();
        declared.sort();

        assert_eq!(tables, declared);
    }

    #[test]
    fn rebuild_sql_is_the_view_select_over_final_rows() {
        for table in PREDICTIONS_DERIVED {
            let view = view_select(table.name);

            // The two substitutions a rebuild makes, each exactly once.
            assert_eq!(view.matches(" WHERE is_deleted = 0").count(), 1);
            let expected = view.replace(
                " WHERE is_deleted = 0",
                &format!(" {REBUILD_RANGE}"),
            );

            let own_epoch: Vec<_> =
                [" epoch, argMinState(", " epoch, sum(if("]
                    .into_iter()
                    .filter(|needle| expected.contains(needle))
                    .collect();
            assert_eq!(own_epoch.len(), 1, "{}", table.name);
            assert_eq!(expected.matches(own_epoch[0]).count(), 1);
            let expected = expected.replace(
                own_epoch[0],
                &own_epoch[0].replace(" epoch,", &format!(" {REBUILD_EPOCH},")),
            );

            assert_eq!(
                normalize(table.rebuild_sql),
                format!("INSERT INTO {} {expected}", table.name),
                "{}: rebuild_sql and the materialized view differ",
                table.name
            );

            // The bucket expression matches bucket_seconds.
            assert!(
                view.contains(&format!(
                    "intDiv(toUInt32(timestamp), {0}) * {0}, 'UTC') AS {1}",
                    table.bucket_seconds, table.bucket_column
                )),
                "{}",
                table.name
            );
        }
    }

    #[test]
    fn placeholders_render_and_align_to_the_bucket() {
        for table in PREDICTIONS_DERIVED {
            let sql =
                render_rebuild(table, 137, 1_700_000_123, 9, (500, None));
            assert!(!sql.contains('{'), "{sql}");
            assert!(sql.contains("chain = 137"));
            assert!(sql.contains("toUInt32(9) AS epoch"));
            assert!(sql.contains(&format!(
                "NOT (block_number >= 500 AND block_number < {})",
                u64::MAX
            )));
            assert!(sql.contains(&format!(
                "toDateTime({})",
                table.bucket_start(1_700_000_123)
            )));
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
        for table in PREDICTIONS_DERIVED {
            assert!(!table.rebuild_sql.contains("toUInt256"));
            assert!(!table.rebuild_sql.contains("toInt256"));

            let sums = sum_arguments(table.rebuild_sql);
            assert!(!sums.is_empty(), "{}", table.name);
            for argument in sums {
                assert!(
                    argument.starts_with("toFloat64(")
                        || argument.starts_with("toUInt64(")
                        || argument.starts_with("if("),
                    "{}: sum({argument})",
                    table.name
                );
                if argument.starts_with("if(") {
                    assert!(argument.contains("toFloat64("), "{argument}");
                }
            }
        }
    }
}
