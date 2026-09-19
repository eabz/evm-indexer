//! The seam between the pipeline and the decoder modules (DEX today;
//! prediction markets and launchpads have the same shape).
//!
//! A module is: a pure `decode` over the logs of a batch, a `*Rows` struct,
//! its block scoped tables in insert order, its `DerivedTable`s, and a
//! background worker. Everything the pipeline does with a module goes
//! through this file, so **adding a module is mechanical**: every place
//! that needs a line is marked `MODULE:` below (a field in
//! [`EnabledModules`] and [`ModuleRows`], one line in each `ModuleRows`
//! method, one [`ModuleSpec`] constant, one arm in `store`).
//!
//! What the pipeline does with it:
//!
//! * transform: [`decode`] over ALL logs of the response, transactions
//!   attached (`tx_from` / `tx_to`);
//! * writer: `_version` + `epoch` stamped with the rest of the batch;
//! * store: the module's tables in ITS insert order, concurrently with the
//!   core children and BEFORE `blocks` (the commit marker);
//! * purge / verify / backfill: the tables, block columns, tombstone
//!   statements and derived tables of [`ModuleSpec`].

use crate::{
    db::{self, derived::DerivedTable, Database, FlushKey, RowBatch},
    dex::{self, DexRows},
    launchpads::{self, LaunchpadRows},
    predictions::{self, PredictionRows, RegistrySet},
    tokens::TokenStandard,
};
use alloy::primitives::{Address, B256, U256};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// Rows as strings with the per-flush stamps zeroed.
macro_rules! fingerprint {
    ($($rows:expr),+ $(,)?) => {{
        let mut prints: Vec<String> = Vec::new();
        $(
            prints.extend($rows.iter().map(|row| {
                let mut row = row.clone();
                row._version = 0;
                row.epoch = 0;
                format!("{row:?}")
            }));
        )+
        prints
    }};
}

/// Which modules decode. Everything is ON by default (owner decision);
/// `--no-<module>` opts out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnabledModules {
    pub dex: bool,
    pub predictions: bool,
    pub launchpads: bool,
    // MODULE: pub <module>: bool,
}

impl Default for EnabledModules {
    fn default() -> Self {
        Self { dex: true, predictions: true, launchpads: true }
    }
}

impl EnabledModules {
    pub fn none() -> Self {
        Self { dex: false, predictions: false, launchpads: false }
    }

    pub fn any(&self) -> bool {
        self.dex || self.predictions || self.launchpads
        // MODULE: || self.<module>
    }

    /// Static description of every enabled module.
    pub fn specs(&self) -> Vec<&'static ModuleSpec> {
        let mut specs = Vec::new();
        if self.dex {
            specs.push(&DEX);
        }
        if self.predictions {
            specs.push(&PREDICTIONS);
        }
        if self.launchpads {
            specs.push(&LAUNCHPADS);
        }
        // MODULE: if self.<module> { specs.push(&<MODULE>); }
        specs
    }
}

/// Rows of every module for one batch.
#[derive(Debug, Default)]
pub struct ModuleRows {
    pub dex: DexRows,
    pub predictions: PredictionRows,
    pub launchpads: LaunchpadRows,
    // MODULE: pub <module>: <Module>Rows,
}

impl ModuleRows {
    pub fn rows(&self) -> usize {
        self.dex.rows() + self.predictions.rows() + self.launchpads.rows()
        // MODULE: + self.<module>.rows()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    pub fn append(&mut self, other: &mut ModuleRows) {
        self.dex.append(&mut other.dex);
        self.predictions.append(&mut other.predictions);
        self.launchpads.append(&mut other.launchpads);
        // MODULE: self.<module>.append(&mut other.<module>);
    }

    pub fn set_version(&mut self, version: u64) {
        self.dex.set_version(version);
        self.predictions.set_version(version);
        self.launchpads.set_version(version);
        // MODULE: self.<module>.set_version(version);
    }

    pub fn set_epoch(&mut self, epoch: u32) {
        self.dex.set_epoch(epoch);
        self.predictions.set_epoch(epoch);
        self.launchpads.set_epoch(epoch);
        // MODULE: self.<module>.set_epoch(epoch);
    }

    /// Token contracts the modules learned about (tokens of pools created
    /// in the batch), for the token metadata worker.
    pub fn token_hints(&self) -> Vec<(Address, TokenStandard)> {
        self.dex
            .token_addresses()
            .into_iter()
            // Collateral tokens: the views need their decimals.
            .chain(self.predictions.token_addresses())
            // Launch / quote tokens: the curve views need their decimals.
            .chain(self.launchpads.token_addresses())
            // MODULE: .chain(self.<module>.token_addresses())
            .map(|address| (address, TokenStandard::Erc20))
            .collect()
    }

    /// The block scoped rows of module `name` as comparable strings
    /// (without `_version` / `epoch`), sorted: what `indexer backfill`
    /// compares with [`stored_fingerprints`].
    pub fn fingerprints(&self, name: &str) -> Vec<String> {
        let mut prints: Vec<String> = match name {
            "dex" => fingerprint!(
                self.dex.swaps,
                self.dex.liquidity,
                self.dex.pools
            ),
            "predictions" => fingerprint!(
                self.predictions.trades,
                self.predictions.transfers,
                self.predictions.position_events,
                self.predictions.resolutions,
                self.predictions.questions,
                self.predictions.markets
            ),
            "launchpads" => fingerprint!(
                self.launchpads.creator_fees,
                self.launchpads.graduations,
                self.launchpads.trades,
                self.launchpads.tokens
            ),
            // MODULE: one arm per module
            _ => Vec::new(),
        };
        prints.sort_unstable();
        prints
    }

    /// `(table, rows)` for the flush log line and the tests.
    pub fn counts(&self) -> Vec<(&'static str, usize)> {
        vec![
            ("dex_swaps", self.dex.swaps.len()),
            ("dex_liquidity", self.dex.liquidity.len()),
            ("dex_pools", self.dex.pools.len()),
            (
                "prediction_outcome_tokens",
                self.predictions.outcome_tokens.len(),
            ),
            ("prediction_markets", self.predictions.markets.len()),
            ("prediction_questions", self.predictions.questions.len()),
            ("prediction_resolutions", self.predictions.resolutions.len()),
            (
                "prediction_position_events",
                self.predictions.position_events.len(),
            ),
            ("prediction_transfers", self.predictions.transfers.len()),
            ("prediction_trades", self.predictions.trades.len()),
            ("launchpad_tokens", self.launchpads.tokens.len()),
            ("launchpad_trades", self.launchpads.trades.len()),
            ("launchpad_graduations", self.launchpads.graduations.len()),
            ("launchpad_creator_fees", self.launchpads.creator_fees.len()),
            // MODULE: one entry per table
        ]
    }

    /// Stores the rows of every module, each module's tables in its own
    /// insert order. Called by `Database::store` next to the core
    /// children, i.e. BEFORE `blocks`. The rows go out through the structs'
    /// own column lists, so a module adding a column changes nothing here.
    pub async fn store(
        &self,
        db: &Database,
        key: &FlushKey,
    ) -> Result<()> {
        for table in dex::BASE_TABLES.iter().copied() {
            match table {
                "dex_swaps" => {
                    db.insert_flush(table, &self.dex.swaps, key).await?
                }
                "dex_liquidity" => {
                    db.insert_flush(table, &self.dex.liquidity, key)
                        .await?
                }
                "dex_pools" => {
                    db.insert_flush(table, &self.dex.pools, key).await?
                }
                // A new table in the module: fail loudly instead of
                // silently never writing it (unit tested below).
                other => bail!("no insert path for DEX table '{other}'"),
            }
        }

        let p = &self.predictions;
        for table in predictions::INSERT_ORDER.iter().copied() {
            match table {
                "prediction_outcome_tokens" => {
                    db.insert_flush(table, &p.outcome_tokens, key).await?
                }
                "prediction_markets" => {
                    db.insert_flush(table, &p.markets, key).await?
                }
                "prediction_questions" => {
                    db.insert_flush(table, &p.questions, key).await?
                }
                "prediction_resolutions" => {
                    db.insert_flush(table, &p.resolutions, key).await?
                }
                "prediction_position_events" => {
                    db.insert_flush(table, &p.position_events, key).await?
                }
                "prediction_transfers" => {
                    db.insert_flush(table, &p.transfers, key).await?
                }
                "prediction_trades" => {
                    db.insert_flush(table, &p.trades, key).await?
                }
                other => {
                    bail!("no insert path for predictions table '{other}'")
                }
            }
        }

        let l = &self.launchpads;
        for table in launchpads::INSERT_ORDER.iter().copied() {
            match table {
                "launchpad_tokens" => {
                    db.insert_flush(table, &l.tokens, key).await?
                }
                "launchpad_trades" => {
                    db.insert_flush(table, &l.trades, key).await?
                }
                "launchpad_graduations" => {
                    db.insert_flush(table, &l.graduations, key).await?
                }
                "launchpad_creator_fees" => {
                    db.insert_flush(table, &l.creator_fees, key).await?
                }
                other => {
                    bail!("no insert path for launchpads table '{other}'")
                }
            }
        }

        // MODULE: the same loop over <module>::INSERT_ORDER

        Ok(())
    }
}

/// What decoding remembers between batches. Owned by the sync loop and
/// handed to [`decode`] in chain order.
#[derive(Debug, Default)]
pub struct DecodeState {
    /// Position registries (contracts that emitted a CTF event). Seeded
    /// from the database at startup. Without it the predictions module
    /// would copy EVERY ERC-1155 transfer of the chain into
    /// `prediction_transfers`.
    pub registries: RegistrySet,
}

/// Live rows of a module table in `range`, read back through its model.
async fn read_rows<T>(
    db: &Database,
    spec: &ModuleSpec,
    table: &str,
    range: crate::db::ranges::BlockRange,
) -> Result<Vec<T>>
where
    T: clickhouse::Row + for<'b> serde::Deserialize<'b> + 'static,
    for<'a> T: clickhouse::Row<Value<'a> = T>,
{
    // Validation off: the crate can not validate (U)Int256 columns.
    db.db
        .clone()
        .with_validation(false)
        .query(&format!(
            "SELECT ?fields FROM `{table}` FINAL WHERE {}",
            range_predicate(
                spec,
                table,
                db.chain_id,
                range.from,
                Some(range.to)
            )
        ))
        .fetch_all::<T>()
        .await
        .map_err(|e| {
            anyhow::anyhow!(e).context(format!("read back '{table}'"))
        })
}

/// [`ModuleRows::fingerprints`] of what is STORED for `spec` in `range`.
pub async fn stored_fingerprints(
    db: &Database,
    spec: &ModuleSpec,
    range: crate::db::ranges::BlockRange,
) -> Result<Vec<String>> {
    let mut rows = ModuleRows::default();

    match spec.name {
        "dex" => {
            rows.dex.swaps =
                read_rows(db, spec, "dex_swaps", range).await?;
            rows.dex.liquidity =
                read_rows(db, spec, "dex_liquidity", range).await?;
            rows.dex.pools =
                read_rows(db, spec, "dex_pools", range).await?;
        }
        "predictions" => {
            let p = &mut rows.predictions;
            p.trades =
                read_rows(db, spec, "prediction_trades", range).await?;
            p.transfers =
                read_rows(db, spec, "prediction_transfers", range).await?;
            p.position_events =
                read_rows(db, spec, "prediction_position_events", range)
                    .await?;
            p.resolutions =
                read_rows(db, spec, "prediction_resolutions", range)
                    .await?;
            p.questions =
                read_rows(db, spec, "prediction_questions", range).await?;
            p.markets =
                read_rows(db, spec, "prediction_markets", range).await?;
        }
        "launchpads" => {
            let l = &mut rows.launchpads;
            l.creator_fees =
                read_rows(db, spec, "launchpad_creator_fees", range)
                    .await?;
            l.graduations =
                read_rows(db, spec, "launchpad_graduations", range)
                    .await?;
            l.trades =
                read_rows(db, spec, "launchpad_trades", range).await?;
            l.tokens =
                read_rows(db, spec, "launchpad_tokens", range).await?;
        }
        // MODULE: one arm per module
        other => bail!("module '{other}' has no read path"),
    }

    Ok(rows.fingerprints(spec.name))
}

/// The position registries already stored, to seed [`DecodeState`].
pub async fn known_registries(
    db: &Database,
    enabled: EnabledModules,
) -> Result<RegistrySet> {
    if !enabled.predictions {
        return Ok(RegistrySet::default());
    }

    #[serde_with::serde_as]
    #[derive(clickhouse::Row, serde::Deserialize)]
    struct RegistryRow {
        #[serde_as(as = "crate::utils::format::SerAddress")]
        registry: Address,
    }

    let sql = predictions::KNOWN_REGISTRIES_SQL
        .replace("{chain}", &db.chain_id.to_string());

    let rows =
        db.db.query(&sql).fetch_all::<RegistryRow>().await.map_err(
            |e| anyhow::anyhow!(e).context("query known registries"),
        )?;

    Ok(RegistrySet::new(rows.into_iter().map(|row| row.registry)))
}

/// Decodes every enabled module from the rows of one response. Pure, no
/// I/O, never awaits: it runs inside transform.
///
/// The modules see EVERY log of the response (`rows.logs`), not a filtered
/// subset: the DEX decoder corroborates swaps with the ERC-20 transfers of
/// the same transaction.
pub fn decode(
    enabled: EnabledModules,
    chain: u64,
    rows: &RowBatch,
    state: &mut DecodeState,
) -> ModuleRows {
    if !enabled.any() {
        return ModuleRows::default();
    }

    // `from` / `to` of the batch's transactions, for attribution
    // (`dex_liquidity.tx_from` = who seeded the liquidity; the event
    // `sender` is usually a router).
    let origins: TxOrigins = rows
        .transactions
        .iter()
        .map(|tx| {
            (
                tx.hash,
                TxOrigin { from: tx.from, to: tx.to, value: tx.value },
            )
        })
        .collect();

    decode_with_origins(enabled, chain, rows, &origins, state)
}

/// `from`, `to` and the native `value` of a transaction: what the modules
/// may attach to their rows. Never a substitute for what the event itself
/// says (the sender of a swap is usually a router).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxOrigin {
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
}

/// [`TxOrigin`] by transaction hash.
pub type TxOrigins = HashMap<B256, TxOrigin>;

/// [`decode`] with the transaction origins given (the backfill reads them
/// from the `transactions` table instead of the batch).
pub fn decode_with_origins(
    enabled: EnabledModules,
    chain: u64,
    rows: &RowBatch,
    origins: &TxOrigins,
    state: &mut DecodeState,
) -> ModuleRows {
    let mut modules = ModuleRows::default();

    if enabled.dex {
        modules.dex = dex::decode(chain, &rows.logs);
        modules.dex.attach_transactions(|hash| {
            origins.get(hash).map(|origin| dex::TxOrigin {
                from: origin.from,
                to: origin.to,
            })
        });
    }

    if enabled.predictions {
        let mut decoded = predictions::decode(chain, &rows.logs);
        decoded.attach_transactions(|hash| {
            origins.get(hash).map(|origin| predictions::TxOrigin {
                from: origin.from,
                to: origin.to,
            })
        });
        state.registries.observe(&decoded);
        decoded.retain_transfers(|registry| {
            state.registries.contains(registry)
        });
        modules.predictions = decoded;
    }

    if enabled.launchpads {
        let mut decoded = launchpads::decode(chain, &rows.logs);
        decoded.attach_transactions(|hash| {
            origins.get(hash).map(|origin| launchpads::TxOrigin {
                from: origin.from,
                to: origin.to,
                value: origin.value,
            })
        });
        modules.launchpads = decoded;
    }

    // MODULE: if enabled.<module> { ... }

    modules
}

/// What purge, verify and backfill need to know about a module's tables.
#[derive(Debug)]
pub struct ModuleSpec {
    /// `--module <name>` of `indexer backfill`.
    pub name: &'static str,
    /// Block scoped tables the indexer writes, in insert order (which is
    /// also the order a purge tombstones them in, before `blocks`).
    pub base_tables: &'static [&'static str],
    /// Read-path tables fed by materialized views of `base_tables`. Never
    /// written directly; a purge only tombstones them directly to REPAIR a
    /// view push that was lost (`pipeline::store`).
    pub side_tables: &'static [&'static str],
    pub derived: &'static [DerivedTable],
    /// Column holding the block number of a base table.
    pub block_column: fn(&str) -> &'static str,
    /// Extra predicate selecting the rows of a base table that belong to
    /// a block (e.g. not the RPC resolver's rows of `dex_pools`).
    pub purge_filter: fn(&str) -> Option<&'static str>,
    /// The tombstone INSERT for `[from, to)` of a base table.
    pub tombstone_sql:
        fn(&str, u64, u64, Option<u64>, u64) -> Result<String>,
    /// The module's `rebuild_statements`: the INSERTs that rebuild one of
    /// `derived` for a [`Rebuild`], ONE PER UTC MONTH (ClickHouse refuses
    /// an insert touching more than 100 partitions).
    pub rebuild_statements: fn(&DerivedTable, &Rebuild) -> Vec<String>,
}

/// One bucket repair (docs/design.md, section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rebuild {
    pub chain: u64,
    /// Start of the first bucket to rebuild (start of day, unix seconds).
    pub from_ts: u32,
    /// Exclusive: timestamp of the newest stored block + 1.
    pub to_ts: u32,
    pub epoch: u32,
    /// The block range being purged, which the rebuild must leave out.
    pub purged_from: u64,
    pub purged_to: Option<u64>,
}

/// The rebuild of a table whose `rebuild_sql` follows the core convention
/// (`{chain}`, `{from_ts}`, `{to_ts}`, `{epoch}`, `{purge_from}`,
/// `{purge_to}`): one INSERT per UTC month, see
/// `DerivedTable::rebuild_statements`.
pub fn plain_rebuild(table: &DerivedTable, r: &Rebuild) -> Vec<String> {
    table.rebuild_statements(
        r.chain,
        r.from_ts,
        r.to_ts,
        r.epoch,
        r.purged_from,
        r.purged_to,
    )
}

/// DEX: one statement per month, bounded by `to_ts`. Its SQL has no
/// purge-range exclusion: it relies on the tombstones of `dex_swaps`
/// (which the purge verifies with `live_children` before it rebuilds).
fn dex_rebuild(table: &DerivedTable, r: &Rebuild) -> Vec<String> {
    dex::derived::rebuild_statements(
        table, r.chain, r.from_ts, r.to_ts, r.epoch,
    )
}

/// Predictions: one statement per month, bounded by `to_ts`. Its own
/// renderer (the module aligns `from_ts` to each table's bucket) and, like
/// the DEX one, no purge-range exclusion: every aggregate reads a child
/// table, which the purge tombstones before it rebuilds.
fn predictions_rebuild(table: &DerivedTable, r: &Rebuild) -> Vec<String> {
    predictions::derived::rebuild_statements(
        table,
        r.chain,
        r.from_ts,
        r.to_ts,
        r.epoch,
        (r.purged_from, r.purged_to),
    )
}

fn no_filter(_table: &str) -> Option<&'static str> {
    None
}

fn dex_tombstone_sql(
    table: &str,
    chain: u64,
    from: u64,
    to: Option<u64>,
    version: u64,
) -> Result<String> {
    Ok(dex::tombstone_sql(table, chain, from, to, version))
}

pub const DEX: ModuleSpec = ModuleSpec {
    name: "dex",
    base_tables: dex::BASE_TABLES,
    side_tables: dex::SIDE_TABLES,
    derived: dex::DEX_DERIVED,
    block_column: dex::block_column,
    purge_filter: dex::purge_filter,
    tombstone_sql: dex_tombstone_sql,
    rebuild_statements: dex_rebuild,
};

pub const PREDICTIONS: ModuleSpec = ModuleSpec {
    name: "predictions",
    base_tables: predictions::BASE_TABLES,
    side_tables: predictions::SIDE_TABLES,
    derived: predictions::PREDICTIONS_DERIVED,
    block_column: predictions::block_column,
    purge_filter: no_filter,
    // Plain `block_number` tables: the generic statement built from the
    // embedded migration DDL.
    tombstone_sql: db::tombstone_sql,
    rebuild_statements: predictions_rebuild,
};

pub const LAUNCHPADS: ModuleSpec = ModuleSpec {
    name: "launchpads",
    base_tables: launchpads::BASE_TABLES,
    side_tables: launchpads::SIDE_TABLES,
    derived: launchpads::LAUNCHPADS_DERIVED,
    block_column: launchpads::block_column,
    purge_filter: no_filter,
    // Plain `block_number` tables: the generic statement built from the
    // embedded migration DDL.
    tombstone_sql: db::tombstone_sql,
    // Every aggregate reads a CHILD table, so its SQL needs no
    // `{purge_from}`/`{purge_to}`; it carries `{to_ts}`, so it is sliced
    // by month.
    rebuild_statements: plain_rebuild,
};

// MODULE: pub const <MODULE>: ModuleSpec = ...

/// Every module the binary knows, enabled or not (`indexer verify` looks
/// at the data, not at the run flags).
pub const ALL_MODULES: &[&ModuleSpec] = &[&DEX, &PREDICTIONS, &LAUNCHPADS];

/// Every table a process writes versioned rows of a chain into: what
/// `Database::seed_version` looks at.
pub fn versioned_tables() -> Vec<&'static str> {
    let mut tables: Vec<&'static str> = db::BASE_TABLES.to_vec();
    tables.push("checkpoints");
    for spec in ALL_MODULES {
        tables.extend_from_slice(spec.base_tables);
    }
    tables
}

/// `WHERE` clause selecting the block scoped rows of `[from, to)` in a
/// module table.
pub fn range_predicate(
    spec: &ModuleSpec,
    table: &str,
    chain: u64,
    from: u64,
    to: Option<u64>,
) -> String {
    let column = (spec.block_column)(table);
    let mut predicate =
        format!("chain = {chain} AND `{column}` >= {from}");

    if let Some(to) = to {
        predicate.push_str(&format!(" AND `{column}` < {to}"));
    }
    if let Some(filter) = (spec.purge_filter)(table) {
        predicate.push_str(&format!(" AND {filter}"));
    }

    predicate
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::db::models::log::{test_support::log_with, DatabaseLog};
    use alloy::primitives::U256;

    /// topic0 of the Uniswap V2 `Sync(uint112,uint112)` event.
    pub const V2_SYNC_TOPIC: &str =
        "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";

    /// A Uniswap V2 `Sync` log: the smallest event the DEX module decodes
    /// (one `dex_liquidity` row).
    pub fn sync_log(chain: u64, block_number: u64) -> DatabaseLog {
        let mut data = Vec::with_capacity(64);
        data.extend_from_slice(&U256::from(1_000u64).to_be_bytes::<32>());
        data.extend_from_slice(&U256::from(2_000u64).to_be_bytes::<32>());

        let mut log = log_with(&[V2_SYNC_TOPIC.parse().unwrap()], data);
        log.chain = chain;
        log.address = Address::repeat_byte(0xaa);
        log.block_number = block_number;
        log.log_index = 0;
        log
    }

    /// Module rows holding exactly one (DEX) row.
    pub fn dex_rows(chain: u64, block_number: u64) -> ModuleRows {
        let batch = RowBatch {
            logs: vec![sync_log(chain, block_number)],
            ..Default::default()
        };
        decode(
            EnabledModules::default(),
            chain,
            &batch,
            &mut DecodeState::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};

    #[test]
    fn dex_is_decoded_by_default_and_not_when_disabled() {
        let batch =
            RowBatch { logs: vec![sync_log(1, 5)], ..Default::default() };

        let mut state = DecodeState::default();

        let on = decode(EnabledModules::default(), 1, &batch, &mut state);
        assert_eq!(on.dex.liquidity.len(), 1);
        assert_eq!(on.rows(), 1);

        let no_dex = EnabledModules { dex: false, ..Default::default() };
        assert!(decode(no_dex, 1, &batch, &mut state).is_empty());

        let off = decode(EnabledModules::none(), 1, &batch, &mut state);
        assert!(off.is_empty());
        assert!(EnabledModules::none().specs().is_empty());
        assert_eq!(
            EnabledModules::default().specs().len(),
            ALL_MODULES.len()
        );
    }

    #[test]
    fn transactions_are_attached_to_liquidity_rows() {
        use crate::db::models::transaction::DatabaseTransaction;
        use hypersync_client::{
            format::{Address as HsAddress, Hash},
            simple_types::Transaction,
        };

        let log = sync_log(1, 5);
        let tx = DatabaseTransaction::from_hypersync(
            &Transaction {
                hash: Some(Hash::from(log.transaction_hash.0)),
                block_number: Some(5u64.into()),
                from: Some(HsAddress::from([0x11; 20])),
                to: Some(HsAddress::from([0x22; 20])),
                ..Default::default()
            },
            1,
            0,
            None,
        )
        .unwrap();

        let batch = RowBatch {
            logs: vec![log],
            transactions: vec![tx.clone()],
            ..Default::default()
        };

        let rows = decode(
            EnabledModules::default(),
            1,
            &batch,
            &mut DecodeState::default(),
        );
        assert_eq!(rows.dex.liquidity[0].tx_from, tx.from);
        assert_eq!(rows.dex.liquidity[0].tx_to, tx.to.unwrap());
    }

    #[test]
    fn counts_and_store_cover_every_module_table() {
        let listed: Vec<&str> = ModuleRows::default()
            .counts()
            .into_iter()
            .map(|(table, _)| table)
            .collect();

        for spec in ALL_MODULES {
            for table in spec.base_tables {
                assert!(
                    listed.contains(table),
                    "{table} has no row count"
                );
            }
        }

        for table in predictions::INSERT_ORDER {
            assert!(listed.contains(table), "{table} has no row count");
        }

        for table in launchpads::INSERT_ORDER {
            assert!(listed.contains(table), "{table} has no row count");
        }

        // `store` has an arm per table: keep these lists in sync with it.
        assert_eq!(
            dex::BASE_TABLES,
            ["dex_swaps", "dex_liquidity", "dex_pools"],
            "a DEX table was added or reordered: add its arm to \
             ModuleRows::store and ModuleRows::counts"
        );
        assert_eq!(
            predictions::INSERT_ORDER.len(),
            7,
            "a predictions table was added: add its arm to \
             ModuleRows::store and ModuleRows::counts"
        );
        assert_eq!(
            launchpads::INSERT_ORDER.len(),
            4,
            "a launchpads table was added: add its arm to \
             ModuleRows::store and ModuleRows::counts"
        );
    }

    #[test]
    fn range_predicates_use_the_module_block_column_and_filter() {
        assert_eq!(
            range_predicate(&DEX, "dex_swaps", 1, 10, Some(20)),
            "chain = 1 AND `block_number` >= 10 AND `block_number` < 20"
        );
        assert_eq!(
            range_predicate(&DEX, "dex_pools", 1, 10, None),
            "chain = 1 AND `created_block` >= 10 AND source = 'event'"
        );
    }
}
