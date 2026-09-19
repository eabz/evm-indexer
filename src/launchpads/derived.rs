//! The launchpad aggregates as [`DerivedTable`]s, so `purge_range` can
//! repair their buckets after a reorg (docs/design.md §2).
//!
//! Nothing is deleted. A purge bumps the chain's epoch, records `(chain,
//! epoch, from_ts)` in `reorgs` and runs every `rebuild_sql`: the surviving
//! rows (`FINAL` hides the tombstoned ones) of every bucket `>= from_ts`
//! are aggregated again under the NEW epoch. The `*_v` views then ignore
//! the older epochs of those buckets (validity rule, `epoch_floor_v`).
//!
//! Every `rebuild_sql` below is GENERATED from the `SELECT` of its
//! materialized view in `migrations/0031_launchpad_aggregates.sql` by two
//! textual substitutions ([`REBUILD_RANGE`] for the view's `WHERE
//! is_deleted = 0`, [`REBUILD_EPOCH`] for the rows' own `epoch`); the unit
//! test at the bottom redoes them and compares, so the two can not drift.
//!
//! Every aggregate here reads a CHILD table (trades, tokens, graduations,
//! fees), never `blocks`, so it needs no `{purge_from}`/`{purge_to}`
//! exclusion: those tables are tombstoned before the repair runs
//! (docs/design.md §2, "child-sourced aggregates do not need it"). The DEX
//! module makes the same call for the same reason.
//!
//! **Run a rebuild through [`rebuild_statements`]**, never as one
//! statement: the aggregates are partitioned by month and ClickHouse
//! refuses an insert block spanning more than
//! `max_partitions_per_insert_block` (100) partitions, which a gap heal
//! deep in history would.
//!
//! Placeholders: `{chain}`, `{from_ts}` (unix seconds, a multiple of
//! `bucket_seconds`; the start of the UTC day recorded in `reorgs` always
//! is), `{to_ts}` (exclusive) and `{epoch}` (the new epoch).

use crate::db::derived::DerivedTable;

/// What a rebuild puts in place of the view's `WHERE is_deleted = 0`.
pub const REBUILD_RANGE: &str =
    "FINAL WHERE chain = {chain} AND timestamp \
                                 >= toDateTime({from_ts}) AND timestamp < \
                                 toDateTime({to_ts}) AND is_deleted = 0";

/// What a rebuild selects in place of the rows' own `epoch`.
pub const REBUILD_EPOCH: &str = "toUInt32({epoch}) AS epoch";

pub const LAUNCHPAD_CANDLES_1M: DerivedTable = DerivedTable {
    name: "launchpad_candles_1m",
    bucket_seconds: 60,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_candles_1m WITH token_amount != 0 AND ",
        "quote_amount != 0 AS priced, toFloat64(quote_amount) / ",
        "toFloat64(token_amount) AS price, (block_number, tx_index, ",
        "ordinal) AS position SELECT chain, token, emitter, ",
        "toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS ",
        "bucket, toUInt32({epoch}) AS epoch, argMinStateIf(price, ",
        "position, priced) AS open, argMaxStateIf(price, position, ",
        "priced) AS close, max(if(priced, price, NULL)) AS high, ",
        "min(if(priced, price, NULL)) AS low, countIf(priced) AS ",
        "priced_trades, count() AS trades, countIf(side = 'buy') AS ",
        "buys, sum(toFloat64(quote_amount)) AS volume_quote, ",
        "sum(toFloat64(token_amount)) AS volume_token, ",
        "sumIf(toFloat64(quote_amount), quote_verified = 1) AS ",
        "volume_quote_verified, uniqState(trader) AS traders, ",
        "argMaxState(toFloat64(progress_wad), position) AS ",
        "progress_wad FROM launchpad_trades FINAL WHERE chain = ",
        "{chain} AND timestamp >= toDateTime({from_ts}) AND timestamp ",
        "< toDateTime({to_ts}) AND is_deleted = 0 AND token != ",
        "toFixedString('', 32) GROUP BY chain, token, emitter, ",
        "bucket, epoch"
    ),
};

pub const LAUNCHPAD_CANDLES_1H: DerivedTable = DerivedTable {
    name: "launchpad_candles_1h",
    bucket_seconds: 3600,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_candles_1h WITH token_amount != 0 AND ",
        "quote_amount != 0 AS priced, toFloat64(quote_amount) / ",
        "toFloat64(token_amount) AS price, (block_number, tx_index, ",
        "ordinal) AS position SELECT chain, token, emitter, ",
        "toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') ",
        "AS bucket, toUInt32({epoch}) AS epoch, argMinStateIf(price, ",
        "position, priced) AS open, argMaxStateIf(price, position, ",
        "priced) AS close, max(if(priced, price, NULL)) AS high, ",
        "min(if(priced, price, NULL)) AS low, countIf(priced) AS ",
        "priced_trades, count() AS trades, countIf(side = 'buy') AS ",
        "buys, sum(toFloat64(quote_amount)) AS volume_quote, ",
        "sum(toFloat64(token_amount)) AS volume_token, ",
        "sumIf(toFloat64(quote_amount), quote_verified = 1) AS ",
        "volume_quote_verified, uniqState(trader) AS traders, ",
        "argMaxState(toFloat64(progress_wad), position) AS ",
        "progress_wad FROM launchpad_trades FINAL WHERE chain = ",
        "{chain} AND timestamp >= toDateTime({from_ts}) AND timestamp ",
        "< toDateTime({to_ts}) AND is_deleted = 0 AND token != ",
        "toFixedString('', 32) GROUP BY chain, token, emitter, ",
        "bucket, epoch"
    ),
};

pub const LAUNCHPAD_VENUE_TRADES_1D: DerivedTable = DerivedTable {
    name: "launchpad_venue_trades_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_venue_trades_1d SELECT chain, family, ",
        "emitter, toDateTime(intDiv(toUInt32(timestamp), 86400) * ",
        "86400, 'UTC') AS bucket, toUInt32({epoch}) AS epoch, count() ",
        "AS trades, countIf(side = 'buy') AS buys, ",
        "sum(toFloat64(quote_amount)) AS volume_quote, ",
        "sumIf(toFloat64(quote_amount), quote_verified = 1) AS ",
        "volume_quote_verified, sum(toFloat64(fee_amount)) AS fees, ",
        "uniqState(trader) AS traders, uniqState(token) AS tokens ",
        "FROM launchpad_trades FINAL WHERE chain = {chain} AND ",
        "timestamp >= toDateTime({from_ts}) AND timestamp < ",
        "toDateTime({to_ts}) AND is_deleted = 0 GROUP BY chain, ",
        "family, emitter, bucket, epoch"
    ),
};

pub const LAUNCHPAD_LAUNCHES_1D: DerivedTable = DerivedTable {
    name: "launchpad_launches_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_launches_1d SELECT chain, family, ",
        "emitter, creator, toDateTime(intDiv(toUInt32(timestamp), ",
        "86400) * 86400, 'UTC') AS bucket, toUInt32({epoch}) AS ",
        "epoch, count() AS launches FROM launchpad_tokens FINAL WHERE ",
        "chain = {chain} AND timestamp >= toDateTime({from_ts}) AND ",
        "timestamp < toDateTime({to_ts}) AND is_deleted = 0 GROUP BY ",
        "chain, family, emitter, creator, bucket, epoch"
    ),
};

pub const LAUNCHPAD_GRADUATIONS_1D: DerivedTable = DerivedTable {
    name: "launchpad_graduations_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_graduations_1d SELECT chain, family, ",
        "emitter, toDateTime(intDiv(toUInt32(timestamp), 86400) * ",
        "86400, 'UTC') AS bucket, toUInt32({epoch}) AS epoch, count() ",
        "AS graduations, sum(toFloat64(quote_amount)) AS quote_in, ",
        "uniqState(token) AS tokens FROM launchpad_graduations FINAL ",
        "WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) ",
        "AND timestamp < toDateTime({to_ts}) AND is_deleted = 0 GROUP ",
        "BY chain, family, emitter, bucket, epoch"
    ),
};

pub const LAUNCHPAD_CREATOR_FEES_1D: DerivedTable = DerivedTable {
    name: "launchpad_creator_fees_1d",
    bucket_seconds: 86400,
    bucket_column: "bucket",
    rebuild_sql: concat!(
        "INSERT INTO launchpad_creator_fees_1d SELECT chain, family, ",
        "emitter, recipient, kind, phase, ",
        "toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, ",
        "'UTC') AS bucket, toUInt32({epoch}) AS epoch, count() AS ",
        "events, sum(toFloat64(amount)) AS amount FROM ",
        "launchpad_creator_fees FINAL WHERE chain = {chain} AND ",
        "timestamp >= toDateTime({from_ts}) AND timestamp < ",
        "toDateTime({to_ts}) AND is_deleted = 0 GROUP BY chain, ",
        "family, emitter, recipient, kind, phase, bucket, epoch"
    ),
};

/// Every launchpad aggregate, in migration order.
pub const LAUNCHPADS_DERIVED: &[DerivedTable] = &[
    LAUNCHPAD_CANDLES_1M,
    LAUNCHPAD_CANDLES_1H,
    LAUNCHPAD_VENUE_TRADES_1D,
    LAUNCHPAD_LAUNCHES_1D,
    LAUNCHPAD_GRADUATIONS_1D,
    LAUNCHPAD_CREATOR_FEES_1D,
];

/// `rebuild_sql` with its placeholders filled in, for `[from_ts, to_ts)`.
/// Keep the range inside one month: use [`rebuild_statements`].
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

/// The rebuild of `table` for `[from_ts, to_ts)` as one INSERT per UTC
/// month, oldest first. `to_ts` is exclusive: pass the timestamp of the
/// newest stored block + 1 (or "now"), never `u32::MAX`.
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
        // The month arithmetic is the DEX module's, unit tested there.
        let end = crate::dex::derived::next_month_start(start)
            .min(u64::from(to_ts)) as u32;
        statements.push(render_rebuild(table, chain, start, end, epoch));
        start = end;
    }

    statements
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchpads::sql::{normalize, statements, AGGREGATES_SQL};

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

    /// The rebuild statement turned back into the view it must repeat: no
    /// `FINAL`, no range, the rows' own epoch.
    fn rebuild_select(table: &DerivedTable) -> String {
        let sql = normalize(table.rebuild_sql);
        let sql = sql
            .strip_prefix(&format!("INSERT INTO {} ", table.name))
            .expect("a rebuild must insert into its own table")
            .to_owned();

        let range = normalize(REBUILD_RANGE);
        assert_eq!(sql.matches(&range).count(), 1, "{}", table.name);
        assert_eq!(
            sql.matches(REBUILD_EPOCH).count(),
            1,
            "{}",
            table.name
        );

        sql.replace(&range, "WHERE is_deleted = 0")
            .replace(REBUILD_EPOCH, "epoch")
    }

    #[test]
    fn rebuilds_repeat_their_materialized_view() {
        for table in LAUNCHPADS_DERIVED {
            assert_eq!(
                rebuild_select(table),
                view_select(table.name),
                "{}: rebuild_sql and the materialized view differ",
                table.name
            );

            let rendered = render_rebuild(table, 1, 0, 60, 3);
            assert!(!rendered.contains('{'), "{rendered}");

            // The bucket expression matches bucket_seconds.
            assert!(
                normalize(table.rebuild_sql).contains(&format!(
                    "intDiv(toUInt32(timestamp), {0}) * {0}, 'UTC') AS \
                     bucket,",
                    table.bucket_seconds
                )),
                "{}",
                table.name
            );
            assert_eq!(table.bucket_column, "bucket");
        }
    }

    #[test]
    fn every_aggregate_of_the_migration_is_declared_once() {
        let mut targets: Vec<String> = statements(AGGREGATES_SQL)
            .into_iter()
            .map(|statement| normalize(&statement))
            .filter_map(|statement| {
                let rest = statement
                    .strip_prefix("CREATE TABLE IF NOT EXISTS ")?;
                let (name, body) = rest.split_once(' ')?;
                body.contains("AggregatingMergeTree")
                    .then(|| name.to_owned())
            })
            .collect();
        targets.sort();

        let mut declared: Vec<String> = LAUNCHPADS_DERIVED
            .iter()
            .map(|table| table.name.to_owned())
            .collect();
        declared.sort();

        assert_eq!(declared, targets);
    }

    /// docs/design.md, 256-bit arithmetic rule: a raw `UInt256` sum wraps
    /// silently and a forged trade can carry `2^256-1`.
    #[test]
    fn amounts_are_never_summed_as_raw_256_bit_integers() {
        const WIDE: [&str; 6] = [
            "token_amount",
            "quote_amount",
            "fee_amount",
            "tax_amount",
            "amount",
            "progress_wad",
        ];

        for table in LAUNCHPADS_DERIVED {
            let sql = table.rebuild_sql;
            assert!(!sql.contains("toUInt256"), "{}", table.name);
            assert!(!sql.contains("toInt256"), "{}", table.name);

            for argument in sum_arguments(sql) {
                for column in WIDE {
                    assert!(
                        !argument.contains(column)
                            || argument.starts_with("toFloat64("),
                        "{}: sum({argument}) would wrap",
                        table.name
                    );
                }
            }
        }
    }

    /// Arguments of every `sum(` / `sumIf(` call in `sql`.
    fn sum_arguments(sql: &str) -> Vec<&str> {
        let mut arguments = Vec::new();
        let mut rest = sql;

        while let Some(at) = rest.find("sum") {
            let after = &rest[at..];
            let Some(open) = after.find('(') else { break };
            if !matches!(&after[..open], "sum" | "sumIf") {
                rest = &after[3..];
                continue;
            }

            let inner = &after[open + 1..];
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
    fn a_rebuild_is_chunked_by_month() {
        // 2026-01-15 .. 2026-03-02 -> January, February, March.
        let statements = rebuild_statements(
            &LAUNCHPAD_CANDLES_1M,
            56,
            1_768_435_200,
            1_772_409_600,
            4,
        );

        assert_eq!(statements.len(), 3);
        assert!(statements[0].contains("toDateTime(1768435200)"));
        assert!(statements[0].contains("toDateTime(1769904000)"));
        assert!(statements[1].contains("toDateTime(1769904000)"));
        assert!(statements[2].contains("toDateTime(1772409600)"));
        assert!(statements.iter().all(|sql| sql.contains("chain = 56")));
    }
}
