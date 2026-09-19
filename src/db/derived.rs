//! Incremental aggregates and how to repair them.
//!
//! Aggregates are `AggregatingMergeTree` tables fed by materialized views
//! (`migrations/0003_core_aggregates.sql`, DEX: `0010+`). A view only ever
//! ADDS what an insert brings, it can not take back the rows a rollback
//! deletes. So after `purge_range` every affected time bucket is deleted
//! and rebuilt from the surviving base rows ("bucket repair"):
//!
//! ```text
//! from = table.bucket_start(min timestamp of the purged blocks)
//! 1. table.delete_sql(chain, from)      -- drop buckets >= from
//! 2. table.rebuild_sql(chain, from)     -- re-aggregate base rows >= from
//! ```
//!
//! Blocks streamed afterwards flow through the views as usual.
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
    /// `INSERT INTO <name> SELECT ... FROM <base> FINAL WHERE chain =
    /// {chain} AND timestamp >= {from_ts} GROUP BY ...`. Must produce
    /// exactly what the view produces. `{chain}` is rendered as an
    /// integer, `{from_ts}` as unix seconds (always a bucket start).
    pub rebuild_sql: &'static str,
}

impl DerivedTable {
    /// Start of the bucket `timestamp` (unix seconds) falls into.
    pub fn bucket_start(&self, timestamp: u32) -> u32 {
        timestamp - timestamp % self.bucket_seconds.max(1)
    }

    /// Lightweight delete of every bucket of `chain` starting at or after
    /// the bucket of `from_ts`.
    pub fn delete_sql(&self, chain: u64, from_ts: u32) -> String {
        format!(
            "DELETE FROM {} WHERE chain = {chain} AND {} >= {}",
            self.name,
            self.bucket_column,
            self.bucket_start(from_ts)
        )
    }

    /// `rebuild_sql` for `chain`, re-aggregating everything from the
    /// bucket of `from_ts` on. `from_ts` is aligned down to its bucket
    /// start: rebuilding half a bucket would lose the other half.
    pub fn rebuild_sql(&self, chain: u64, from_ts: u32) -> String {
        render(self.rebuild_sql, chain, self.bucket_start(from_ts))
    }
}

/// Fills the `{chain}` and `{from_ts}` placeholders.
fn render(sql: &str, chain: u64, from_ts: u32) -> String {
    sql.replace("{chain}", &chain.to_string())
        .replace("{from_ts}", &from_ts.to_string())
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
            FINAL WHERE chain = {chain} AND timestamp >= {from_ts} \
            GROUP BY chain, day",
    },
    DerivedTable {
        name: "daily_transaction_stats",
        bucket_seconds: DAY,
        bucket_column: "day",
        rebuild_sql: "INSERT INTO daily_transaction_stats \
            SELECT \
            chain, \
            toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day, \
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
            FINAL WHERE chain = {chain} AND timestamp >= {from_ts} \
            GROUP BY chain, day",
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
            count() AS transfers, \
            sum(toFloat64(e.amount)) AS volume_raw, \
            uniqState(e.`from`) AS senders, \
            uniqState(e.`to`) AS recipients \
            FROM erc20_transfers AS e \
            FINAL WHERE chain = {chain} AND timestamp >= {from_ts} \
            GROUP BY chain, token_address, day",
    },
    DerivedTable {
        name: "daily_contract_deployments",
        bucket_seconds: DAY,
        bucket_column: "day",
        rebuild_sql: "INSERT INTO daily_contract_deployments \
            SELECT \
            chain, \
            toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day, \
            count() AS contracts, \
            uniqState(c.creator) AS deployers \
            FROM contracts AS c \
            FINAL WHERE chain = {chain} AND timestamp >= {from_ts} \
            GROUP BY chain, day",
    },
];

/// What a rebuild adds to the SELECT of the materialized view.
#[cfg(test)]
const REBUILD_FILTER: &str =
    " FINAL WHERE chain = {chain} AND timestamp >= {from_ts}";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{
        models::transaction::{STATUS_FAILURE, STATUS_SUCCESS},
        schema::{
            split_sql_statements, tables_with_columns,
            test_support::CORE_MIGRATIONS,
        },
    };

    fn normalize(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// SELECT of the materialized view writing `TO <table>`.
    fn view_select(table: &str) -> String {
        let marker = format!(" TO {table} AS ");

        CORE_MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| split_sql_statements(sql))
            .map(|statement| normalize(&statement))
            .find_map(|statement| {
                statement
                    .starts_with("CREATE MATERIALIZED VIEW")
                    .then(|| statement.split_once(&marker))
                    .flatten()
                    .map(|(_, select)| select.to_string())
            })
            .unwrap_or_else(|| panic!("no materialized view TO {table}"))
    }

    #[test]
    fn renders_placeholders_and_aligns_to_the_bucket() {
        let table = DerivedTable {
            name: "t",
            bucket_seconds: 3_600,
            bucket_column: "hour",
            rebuild_sql: "INSERT INTO t SELECT 1 FROM b FINAL WHERE \
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

        assert_eq!(
            table.rebuild_sql(137, 7_201),
            "INSERT INTO t SELECT 1 FROM b FINAL WHERE chain = 137 AND \
             timestamp >= 7200"
        );
        assert_eq!(
            table.delete_sql(137, 7_201),
            "DELETE FROM t WHERE chain = 137 AND hour >= 7200"
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
        let aggregates: Vec<String> =
            tables_with_columns(CORE_MIGRATIONS[2].1)
                .into_iter()
                .map(|(table, _)| table)
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
            let rendered = table.rebuild_sql(1, 0);
            assert!(!rendered.contains('{'), "{rendered}");
            assert_eq!(
                sql.matches("{chain}").count(),
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

            assert_eq!(
                select.replace(REBUILD_FILTER, ""),
                view_select(table.name),
                "{}: rebuild_sql and the materialized view differ",
                table.name
            );

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
