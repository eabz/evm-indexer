//! The aggregates of the core dataset
//! (`migrations/0003_core_aggregates.sql`) and nothing else: the
//! [`DerivedTable`] type, the placeholder rendering and the monthly
//! slicing are infrastructure and stay in `db::derived`, the dataset owns
//! only its instances.
//!
//! The SELECT of each entry is the SELECT of its materialized view plus
//! `FINAL WHERE ...` (unit tested), so the rebuild a purge issues
//! reproduces exactly what the view wrote.

use crate::db::derived::DerivedTable;

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
