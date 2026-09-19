//! The core EVM dataset: blocks, transactions, logs, withdrawals and the
//! ERC-20 / ERC-721 / ERC-1155 transfers decoded out of the logs.
//!
//! A DATA MODULE like `dex`, `predictions` and `launchpads`
//! (docs/design.md section 12): it owns its row structs, its event
//! signatures, its decoding, its table constants and its aggregates. See
//! `README.md` in this directory for the table catalogue.

pub mod convert;
pub mod decode;
pub mod derived;
pub mod events;
pub mod models;

pub use self::{decode::decode, derived::CORE_DERIVED};

/// Core tables whose rows belong to a block and are written by the indexer,
/// in the order a purge must tombstone them: children first, `blocks`
/// LAST. While the old `blocks` row is alive a crashed purge is detected
/// and re-run (which is harmless), so the commit marker goes last,
/// mirroring the insert order.
///
/// Not listed on purpose: `tokens` (not block scoped), the aggregates of
/// `0003` (repaired per epoch, see [`CORE_DERIVED`]), `checkpoints`
/// (ranges, handled by the pipeline) and the `contracts` view. The
/// analytics datasets list their own in `dex::BASE_TABLES`,
/// `predictions::BASE_TABLES` and `launchpads::BASE_TABLES`.
pub const BASE_TABLES: &[&str] = &[
    "erc20_transfers",
    "erc721_transfers",
    "erc1155_transfers",
    "logs",
    "withdrawals",
    "transactions",
    "blocks",
];

/// Read-path side tables, fed by materialized views. NEVER tombstoned (or
/// written) directly: their views pass `_version` and `is_deleted` through,
/// so a tombstone inserted into the base table tombstones exactly the side
/// rows of that base row.
pub const SIDE_TABLES: &[&str] = &[
    "tx_lookup",
    "block_lookup",
    "transactions_by_address",
    "logs_by_address",
    "erc20_transfers_by_account",
    "nft_transfers_by_account",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::{
        block_number_column, split_sql_statements, strip_sql_comments,
        tables_with_block_number, tables_with_columns,
        test_support::{view_selects, CORE_MIGRATIONS},
        tombstone_sql,
    };
    use std::collections::{HashMap, HashSet};

    fn all_sql() -> String {
        CORE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_table_with_a_block_number_is_classified() {
        let listed: HashSet<&str> =
            BASE_TABLES.iter().chain(SIDE_TABLES).copied().collect();
        assert_eq!(
            listed.len(),
            BASE_TABLES.len() + SIDE_TABLES.len(),
            "a table is listed twice"
        );

        let mut found = HashSet::new();
        for (file, sql) in CORE_MIGRATIONS {
            for table in tables_with_block_number(sql) {
                assert!(
                    listed.contains(table.as_str()),
                    "{table} ({file}) has a block number column but is \
                     neither in BASE_TABLES nor in SIDE_TABLES: a rollback \
                     would leave its rows behind"
                );
                found.insert(table);
            }
        }

        // And nothing is listed that does not exist.
        for table in listed {
            assert!(found.contains(table), "{table} is not in migrations");
        }
    }

    #[test]
    fn base_tables_are_written_directly_and_side_tables_only_by_views() {
        assert_eq!(BASE_TABLES.last(), Some(&"blocks"));
        assert_eq!(block_number_column("blocks"), "number");
        assert_eq!(block_number_column("logs"), "block_number");

        for table in BASE_TABLES {
            assert!(
                view_selects(table).is_empty(),
                "{table} is fed by a materialized view: it is a side table"
            );
        }

        for table in SIDE_TABLES {
            let selects = view_selects(table);
            assert!(
                !selects.is_empty(),
                "{table} has no materialized view"
            );

            for select in selects {
                // The pass-through that makes tombstones propagate.
                for column in ["epoch", "_version", "is_deleted"] {
                    assert!(
                        select.contains(&format!(" {column}")),
                        "the view of {table} does not pass {column} through"
                    );
                }
                // A filter would stop tombstones from reaching the table.
                assert!(
                    !select.contains("is_deleted ="),
                    "the view of {table} filters on is_deleted"
                );
                // Fed from a base table, never from another side table.
                assert!(
                    BASE_TABLES
                        .iter()
                        .any(|base| select
                            .contains(&format!("FROM {base}"))),
                    "{table}: {select}"
                );
            }
        }
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (file, sql) in CORE_MIGRATIONS {
            let lowered = strip_sql_comments(sql).to_lowercase();

            // The database comes from the connection.
            assert!(!lowered.contains("indexer."), "{file}: db prefix");
            assert!(!lowered.contains("create database"), "{file}");
            // No projections, no bloom filter zoo.
            assert!(!lowered.contains("projection"), "{file}");
            assert!(!lowered.contains("bloom_filter"), "{file}");
            // Distinct counts are states, never uniqExact in a sum.
            assert!(!lowered.contains("uniqexact"), "{file}");
            // Traces are out of scope (docs/design.md, section 9).
            assert!(!lowered.contains("trace"), "{file}");

            for statement in split_sql_statements(sql) {
                let lowered = statement.to_lowercase();
                assert!(
                    lowered.starts_with("create table if not exists ")
                        || lowered.starts_with(
                            "create materialized view if not exists "
                        )
                        || lowered
                            .starts_with("create view if not exists "),
                    "{file}: not idempotent: {statement}"
                );
            }
        }

        let all = all_sql();
        let statements = split_sql_statements(&all);

        for (table, columns) in tables_with_columns(&all) {
            let is_base = BASE_TABLES.contains(&table.as_str());
            let is_side = SIDE_TABLES.contains(&table.as_str());
            if !is_base && !is_side {
                continue;
            }

            for required in ["chain", "epoch", "_version", "is_deleted"] {
                assert!(
                    columns.iter().any(|c| c == required),
                    "{table} lacks {required}"
                );
            }

            let ddl = statements
                .iter()
                .find(|s| {
                    s.starts_with(&format!(
                        "CREATE TABLE IF NOT EXISTS {table} "
                    ))
                })
                .unwrap();
            assert!(
                ddl.contains(
                    "ENGINE = ReplacingMergeTree(_version, is_deleted)"
                ),
                "{table}"
            );
            assert!(
                ddl.contains(
                    "do_not_merge_across_partitions_select_final = 1"
                ),
                "{table}"
            );
            assert!(ddl.contains("is_deleted UInt8 DEFAULT 0"), "{table}");
            assert!(ddl.contains("epoch UInt32 DEFAULT 0"), "{table}");

            // 50+ chains in one database: base tables by month ONLY, side
            // tables by chain (lookups do not know the month).
            let partition = if is_base {
                "PARTITION BY toYYYYMM(timestamp) "
            } else {
                "PARTITION BY chain "
            };
            let normalized =
                ddl.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                normalized.contains(partition),
                "{table}: {partition}"
            );
        }
    }

    #[test]
    fn contracts_is_a_view_over_successful_deployments() {
        let all = all_sql();

        assert!(tables_with_columns(&all)
            .iter()
            .all(|(table, _)| table != "contracts"));

        let view = split_sql_statements(&all)
            .into_iter()
            .find(|s| {
                s.starts_with("CREATE VIEW IF NOT EXISTS contracts ")
            })
            .expect("contracts view");

        assert!(view.contains("FROM transactions FINAL"));
        // NULL (pre-Byzantium receipts have no status) counts as success.
        assert!(view.contains("ifNull(status, 'success') = 'success'"));
        assert!(view.contains("contract_created != toFixedString('', 20)"));
    }

    #[test]
    fn tombstones_copy_every_column_of_the_migration_ddl() {
        let all = all_sql();
        let ddl: HashMap<String, Vec<String>> =
            tables_with_columns(&all).into_iter().collect();

        for table in BASE_TABLES {
            let sql = tombstone_sql(table, 56, 100, None, 1_700).unwrap();
            let columns = &ddl[*table];

            // INSERT list == DDL columns, in order, all quoted.
            let list = sql
                .split_once(" (")
                .and_then(|(_, rest)| rest.split_once(") SELECT "))
                .map(|(list, _)| list)
                .unwrap();
            let expected: Vec<String> =
                columns.iter().map(|c| format!("`{c}`")).collect();
            assert_eq!(list, expected.join(", "), "{table}");

            // SELECT list: the same columns, except the two that make it
            // a tombstone.
            let select = sql
                .split_once(") SELECT ")
                .and_then(|(_, rest)| rest.split_once(" FROM "))
                .map(|(select, _)| select)
                .unwrap();
            let expected: Vec<String> = columns
                .iter()
                .map(|c| match c.as_str() {
                    "_version" => "toUInt64(1700)".to_string(),
                    "is_deleted" => "toUInt8(1)".to_string(),
                    _ => format!("`{c}`"),
                })
                .collect();
            assert_eq!(select, expected.join(", "), "{table}");

            let block_column = block_number_column(table);
            assert!(
                sql.ends_with(&format!(
                    " FROM `{table}` FINAL WHERE chain = 56 AND \
                     `{block_column}` >= 100"
                )),
                "{sql}"
            );
            assert!(!sql.to_uppercase().contains("DELETE "), "{sql}");
        }
    }
}
