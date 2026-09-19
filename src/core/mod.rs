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

use crate::{
    core::models::{
        block::DatabaseBlock, erc1155_transfer::DatabaseERC1155Transfer,
        erc20_transfer::DatabaseERC20Transfer,
        erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
        transaction::DatabaseTransaction, withdrawal::DatabaseWithdrawal,
    },
    db::{
        flush_windows,
        ranges::{contiguous_ranges, DatabaseCheckpoint},
        select, Database, FlushKey, FlushWindow, Timestamped,
        MAX_MONTHS_PER_FLUSH,
    },
    pipeline::modules::ModuleRows,
};
use anyhow::{bail, Result};
use log::info;

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

// ------------------------------------------------- the flush's row batch
//
// `RowBatch` and [`store`] are here and not in `db` because they name
// every core table by hand: `db` must not know a dataset's rows, only
// how to insert any `Row` (`db::insert_flush{,_refs}`).
//
// The batch also carries `modules: ModuleRows`, which is NOT core data.
// It hangs off the core batch because `blocks` is the chain's commit
// marker: every other module's rows have to be durable BEFORE the block
// row that claims them is written, so one flush writes them together and
// [`store`] is where that order lives. The module rows themselves stay
// opaque - core only forwards `set_version` / `set_epoch` / `store` to
// them through the `pipeline::modules` seam.

/// Sets `$field` on every block scoped row of a [`RowBatch`].
macro_rules! stamp {
    ($batch:expr, $field:ident = $value:expr) => {{
        stamp!(@rows $batch, $field = $value;
            blocks, logs, transactions, withdrawals,
            erc20_transfers, erc721_transfers, erc1155_transfers);
    }};
    (@rows $batch:expr, $field:ident = $value:expr; $($rows:ident),*) => {$(
        for row in &mut $batch.$rows {
            row.$field = $value;
        }
    )*};
}

/// Rows produced from one or more HyperSync responses. Always holds WHOLE
/// blocks: every row that belongs to a block in `blocks` is in here too.
#[derive(Debug, Default)]
pub struct RowBatch {
    pub blocks: Vec<DatabaseBlock>,
    pub logs: Vec<DatabaseLog>,
    pub transactions: Vec<DatabaseTransaction>,
    pub withdrawals: Vec<DatabaseWithdrawal>,
    pub erc20_transfers: Vec<DatabaseERC20Transfer>,
    pub erc721_transfers: Vec<DatabaseERC721Transfer>,
    pub erc1155_transfers: Vec<DatabaseERC1155Transfer>,
    /// Rows of the decoder modules (DEX, ...), decoded from `logs` in
    /// transform. Stored BEFORE `blocks`, like every other child.
    pub modules: ModuleRows,
}

impl RowBatch {
    /// Total rows over all tables.
    pub fn rows(&self) -> usize {
        self.blocks.len()
            + self.logs.len()
            + self.transactions.len()
            + self.withdrawals.len()
            + self.erc20_transfers.len()
            + self.erc721_transfers.len()
            + self.erc1155_transfers.len()
            + self.modules.rows()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Moves every row of `other` into `self`.
    pub fn append(&mut self, other: &mut RowBatch) {
        self.blocks.append(&mut other.blocks);
        self.logs.append(&mut other.logs);
        self.transactions.append(&mut other.transactions);
        self.withdrawals.append(&mut other.withdrawals);
        self.erc20_transfers.append(&mut other.erc20_transfers);
        self.erc721_transfers.append(&mut other.erc721_transfers);
        self.erc1155_transfers.append(&mut other.erc1155_transfers);
        self.modules.append(&mut other.modules);
    }

    /// Stamps `_version` on every block scoped row of the batch (module
    /// rows included). Called once per flush with [`next_version`].
    pub fn set_version(&mut self, version: u64) {
        stamp!(self, _version = version);
        self.modules.set_version(version);
    }

    /// Stamps the chain's current purge generation on every block scoped
    /// row of the batch (docs/design.md, section 2). Called once per flush,
    /// like [`Self::set_version`]: the aggregates file every contribution
    /// under the epoch of the rows it came from.
    pub fn set_epoch(&mut self, epoch: u32) {
        stamp!(self, epoch = epoch);
        self.modules.set_epoch(epoch);
    }

    /// `_version` of the batch (0 before [`Self::set_version`]).
    pub fn version(&self) -> u64 {
        self.blocks.first().map(|block| block._version).unwrap_or(0)
    }

    /// `epoch` of the batch (0 before [`Self::set_epoch`]).
    pub fn epoch(&self) -> u32 {
        self.blocks.first().map(|block| block.epoch).unwrap_or(0)
    }

    /// Lowest and highest block number in the batch.
    pub fn block_span(&self) -> Option<(u64, u64)> {
        let min = self.blocks.iter().map(|b| b.number).min()?;
        let max = self.blocks.iter().map(|b| b.number).max()?;
        Some((min, max))
    }
}

macro_rules! timestamped {
    ($($row:ty),+ $(,)?) => {$(
        impl Timestamped for $row {
            fn timestamp(&self) -> u32 {
                self.timestamp
            }
        }
    )+};
}

timestamped!(
    DatabaseBlock,
    DatabaseTransaction,
    DatabaseLog,
    DatabaseWithdrawal,
    DatabaseERC20Transfer,
    DatabaseERC721Transfer,
    DatabaseERC1155Transfer,
);

/// Stores a batch. Every non-block table (module tables included) is
/// written concurrently, then `blocks` LAST: a block row only exists
/// once all of its data is durable, which is what resume / gap
/// detection relies on. The checkpoint rows follow `blocks`.
///
/// Returns an error only after every retry is exhausted, in which case
/// NO block row of this batch was written (or, for a failed checkpoint
/// insert, everything was: checkpoints are an index, `blocks` decides).
pub async fn store(db: &Database, batch: &RowBatch) -> Result<()> {
    if batch.block_span().is_none() {
        if batch.is_empty() {
            return Ok(());
        }
        // Rows can not be committed without their block.
        bail!("refusing to store a batch of rows without block rows");
    }

    let windows =
        flush_windows(batch.blocks.iter().map(|block| block.timestamp));

    if windows.len() > 1 {
        info!(
            "Chain {}: this flush spans {} UTC months, more than one \
             insert may touch; storing it in {} parts, oldest first.",
            db.chain_id,
            windows.len() * MAX_MONTHS_PER_FLUSH,
            windows.len()
        );
    }

    // Oldest first, each part complete in itself (children, then
    // `blocks`, then its checkpoints): a crash between two parts leaves
    // the later months as ordinary gaps.
    for window in windows {
        store_window(db, batch, window).await?;
    }

    Ok(())
}

/// One part of a flush: every row whose `timestamp` falls into
/// `window`. With the single [`FlushWindow::ALL`] this is the whole
/// flush and behaves exactly as an unsplit one - the deduplication
/// token included, because the key's block span is computed from the
/// window's own blocks.
async fn store_window(
    db: &Database,
    batch: &RowBatch,
    window: FlushWindow,
) -> Result<()> {
    let blocks: Vec<&DatabaseBlock> = batch
        .blocks
        .iter()
        .filter(|block| window.holds(block.timestamp))
        .collect();

    let Some(span) = blocks
        .iter()
        .map(|block| block.number)
        .min()
        .zip(blocks.iter().map(|block| block.number).max())
    else {
        return Ok(());
    };

    let key =
        FlushKey { chain: db.chain_id, span, version: batch.version() };

    let logs = select(&batch.logs, window);
    let transactions = select(&batch.transactions, window);
    let withdrawals = select(&batch.withdrawals, window);
    let erc20 = select(&batch.erc20_transfers, window);
    let erc721 = select(&batch.erc721_transfers, window);
    let erc1155 = select(&batch.erc1155_transfers, window);

    let results = tokio::join!(
        db.insert_flush_refs("logs", &logs, &key),
        db.insert_flush_refs("transactions", &transactions, &key),
        db.insert_flush_refs("withdrawals", &withdrawals, &key),
        db.insert_flush_refs("erc20_transfers", &erc20, &key),
        db.insert_flush_refs("erc721_transfers", &erc721, &key),
        db.insert_flush_refs("erc1155_transfers", &erc1155, &key),
        batch.modules.store(db, &key, window),
    );

    let (r0, r1, r2, r3, r4, r5, r6) = results;
    let failures: Vec<String> = [r0, r1, r2, r3, r4, r5, r6]
        .into_iter()
        .filter_map(|r| r.err())
        .map(|e| format!("{e:#}"))
        .collect();

    if !failures.is_empty() {
        bail!("failed to store batch: {}", failures.join("; "));
    }

    db.insert_flush_refs("blocks", &blocks, &key).await?;

    let checkpoints: Vec<DatabaseCheckpoint> =
        contiguous_ranges(blocks.iter().map(|b| b.number))
            .into_iter()
            .map(|range| DatabaseCheckpoint {
                chain: db.chain_id,
                from_block: range.from,
                to_block: range.to,
                epoch: batch.epoch(),
                _version: key.version,
            })
            .collect();

    let checkpoints: Vec<&DatabaseCheckpoint> =
        checkpoints.iter().collect();

    db.insert_flush_refs("checkpoints", &checkpoints, &key).await
}

#[cfg(test)]
mod row_batch_tests {
    use super::*;

    #[test]
    fn row_batch_append_moves_rows() {
        use crate::core::models::log::test_support::log_with;

        let mut a = RowBatch::default();
        let mut b = RowBatch::default();
        b.logs.push(log_with(&[], vec![]));
        b.modules = crate::pipeline::modules::test_support::dex_rows(1, 5);
        assert_eq!(b.modules.rows(), 1);

        assert!(a.is_empty());
        a.append(&mut b);
        assert_eq!(a.rows(), 2);
        assert!(b.is_empty());
        assert_eq!(a.block_span(), None);
    }

    #[test]
    fn set_version_stamps_every_block_scoped_row() {
        use crate::core::models::{
            block::test_support::block_row, log::test_support::log_with,
        };

        let mut batch = RowBatch::default();
        batch.blocks.push(block_row(5, 5, 4));
        batch.blocks.push(block_row(6, 6, 5));
        batch.logs.push(log_with(&[], vec![]));

        batch.modules =
            crate::pipeline::modules::test_support::dex_rows(1, 5);

        batch.set_version(1_234);
        batch.set_epoch(7);

        assert_eq!((batch.version(), batch.epoch()), (1_234, 7));
        assert!(!batch.modules.dex.liquidity.is_empty());
        assert!(batch
            .modules
            .dex
            .liquidity
            .iter()
            .all(|row| row._version == 1_234 && row.epoch == 7));

        assert!(batch.blocks.iter().all(|row| row._version == 1_234));
        assert!(batch.logs.iter().all(|row| row._version == 1_234));
        assert!(batch.blocks.iter().all(|row| row.epoch == 7));
        assert!(batch.logs.iter().all(|row| row.epoch == 7));
        assert_eq!(batch.block_span(), Some((5, 6)));
    }
}

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
