//! Prediction markets, display first (docs/design.md §10).
//!
//! **Decode by event family from `logs`, never by address registry.** What
//! identifies a trade is its `topic0` AND the exact shape of the event, so
//! Polymarket, its forks on BNB Chain and Base (Predict.fun, Opinion,
//! Limitless...) and whatever launches tomorrow on a chain nobody has heard
//! of are indexed on day one. `protocol` is therefore the event FAMILY
//! (`ctf`, `ctf_exchange`, `ctf_exchange_v2`, `fpmm`, `ctf_adapter`,
//! `neg_risk`, `uma`), never a brand. See `README.md` in this directory for
//! the research behind every decision, the table / view catalogue and the
//! query cookbook (one query per screen of a trading UI).
//!
//! # The canonical trade
//!
//! An order book match emits one `OrderFilled` per MAKER order, then one
//! `OrderFilled` for the TAKER order (its `taker` is the exchange itself)
//! and one `OrdersMatched` - three descriptions of the same shares. Summing
//! all `OrderFilled` events doubles the volume.
//!
//! **One `prediction_trades` row = one filled maker order, told from the
//! taker's point of view** (`side`, `outcome_token_id`, `collateral_amount`
//! are the taker's). The taker's own `OrderFilled` and `OrdersMatched` are
//! never rows: they only tell the decoder whose point of view to take, the
//! match type and the taker's fee. The fills of a taker order add up to its
//! own `OrderFilled` exactly (asserted on a real 6 maker match).
//!
//! When both orders BUY complementary outcomes (`mint`) or both SELL
//! (`merge`) the fill is two real prints: the taker's token at
//! `collateral_amount / share_amount` and the maker's token at
//! `maker_collateral_amount / share_amount`; the two add up to 1. Candles
//! and volumes count both legs of such a fill and the single leg of a
//! `complementary` one: volume is the collateral that changed hands.
//!
//! # Conventions
//!
//! * `market_id` = the CTF `conditionId`, `registry` = the ERC-1155
//!   contract holding the positions. Both identify a market: a forged
//!   registry can never collide with the real one. Exchange events do not
//!   name the registry - it is the contract that moved the traded token id
//!   in the same transaction.
//! * outcome index <-> ERC-1155 token id is COMPUTED (`ids.rs`) from every
//!   `PositionSplit` / `PositionsMerge` / `PayoutRedemption`. No position
//!   can exist before its first split, so the map is always there before
//!   the first transfer - and it is resolved at QUERY time, so indexing
//!   from the middle of the chain heals itself at the next split.
//! * prices are stored as the two raw `UInt256` amounts; the views expose
//!   `price` = probability as `Float64`.
//! * decoding never needs RPC. The only thing events can not tell is what
//!   token an exchange is paid in; [`VenueWorker`] asks the exchange in the
//!   background.
//!
//! # Wiring (pipeline)
//!
//! ```text
//! let mut rows = predictions::decode(chain, &batch.logs);
//! rows.attach_transactions(|hash| ...);      // tx_from / tx_to
//! rows.retain_transfers(|registry| ...);     // optional, see below
//! rows.set_epoch(epoch); rows.set_version(version);
//! insert in INSERT_ORDER; venue_worker.discover(&rows.venue_candidates());
//! ```
//!
//! [`decode`] keeps EVERY ERC-1155 transfer of the batch (it can not know
//! which contracts are position registries without state). That is always
//! correct; on chains with a lot of unrelated ERC-1155 traffic the
//! pipeline should keep a [`RegistrySet`] and drop the rest.

pub mod decode;
pub mod derived;
pub mod events;
pub mod ids;
pub mod models;
pub mod resolve;
pub mod text;
pub mod worker;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod fixtures_data;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
pub(crate) mod sql;

use std::collections::HashSet;

use alloy::primitives::{Address, B256};

pub use self::{
    decode::decode,
    derived::PREDICTIONS_DERIVED,
    models::{
        FeeUnit, MatchType, PositionEventKind, PredictionMarket,
        PredictionOutcomeToken, PredictionPositionEvent, PredictionQuestion,
        PredictionResolution, PredictionTrade, PredictionTransfer,
        PredictionVenue, Protocol, QuestionKind, RowSource, Side,
        TransferReason,
    },
    worker::{
        MissingVenueSource, VenueSink, VenueWorker, VenueWorkerOptions,
        VenueWorkerStats,
    },
};

/// Block scoped tables the pipeline writes (and `purge_range` tombstones),
/// children first. Every one has `chain`, `block_number`, `timestamp`,
/// `epoch`, `_version`, `is_deleted`.
pub const BASE_TABLES: &[&str] = &[
    "prediction_trades",
    "prediction_transfers",
    "prediction_position_events",
    "prediction_resolutions",
    "prediction_questions",
    "prediction_markets",
];

/// Read path tables fed by materialized views that pass `_version`,
/// `is_deleted` and `epoch` through: never written or tombstoned directly.
pub const SIDE_TABLES: &[&str] = &[
    "prediction_trades_by_token",
    "prediction_ledger_by_holder",
    "prediction_ledger_by_token",
];

/// Everything with a `block_number` column, side tables first (the list
/// `db::BLOCK_SCOPED_TABLES` is extended with).
pub const BLOCK_SCOPED_TABLES: &[&str] = &[
    "prediction_trades_by_token",
    "prediction_ledger_by_holder",
    "prediction_ledger_by_token",
    "prediction_trades",
    "prediction_transfers",
    "prediction_position_events",
    "prediction_resolutions",
    "prediction_questions",
    "prediction_markets",
];

/// Tables that are NOT block scoped and never purged: the outcome token
/// map is arithmetic, venues are chain state, the rest is user data.
pub const UNSCOPED_TABLES: &[&str] = &[
    "prediction_outcome_tokens",
    "prediction_outcome_tokens_by_market",
    "prediction_venues",
    "prediction_venue_labels",
    "prediction_market_metadata",
];

/// The order the pipeline inserts in. The outcome token map goes first so
/// a reader never sees a trade whose market can not be resolved yet.
pub const INSERT_ORDER: &[&str] = &[
    "prediction_outcome_tokens",
    "prediction_markets",
    "prediction_questions",
    "prediction_resolutions",
    "prediction_position_events",
    "prediction_transfers",
    "prediction_trades",
];

/// The block column of every table of [`BLOCK_SCOPED_TABLES`].
pub fn block_column(_table: &str) -> &'static str {
    "block_number"
}

/// Exchanges / pools that traded in the last week and have no
/// `prediction_venues` row, for the [`MissingVenueSource`] of the pipeline.
/// Placeholders: `{chain}`, `{limit}`. Columns: `exchange
/// FixedString(20)`, `protocol String`.
pub const MISSING_VENUES_SQL: &str = "\
SELECT exchange, any(toString(protocol)) AS protocol \
FROM prediction_trades \
WHERE chain = {chain} AND timestamp >= now() - INTERVAL 7 DAY \
AND exchange NOT IN (\
SELECT exchange FROM prediction_venues WHERE chain = {chain}) \
GROUP BY exchange \
LIMIT {limit}";

/// Registries known to the database, to seed a [`RegistrySet`].
/// Placeholder: `{chain}`. Column: `registry FixedString(20)`.
pub const KNOWN_REGISTRIES_SQL: &str = "\
SELECT DISTINCT registry FROM prediction_markets FINAL \
WHERE chain = {chain}";

/// An exchange / pool whose collateral is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VenueCandidate {
    pub exchange: Address,
    /// Family of the event that revealed it: which getters to try.
    pub protocol: Protocol,
}

/// `from` and `to` of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxOrigin {
    pub from: Address,
    /// `None` for contract creations.
    pub to: Option<Address>,
}

/// Contracts that are position registries: they emitted a CTF event at
/// some point. A CTF must prepare a condition before it can mint a single
/// position, so when indexing starts before the registry's deployment the
/// set is exact by the time the first transfer arrives.
///
/// Seed it with [`KNOWN_REGISTRIES_SQL`] at startup, call
/// [`RegistrySet::observe`] on every batch IN CHAIN ORDER, then
/// [`PredictionRows::retain_transfers`].
#[derive(Debug, Clone, Default)]
pub struct RegistrySet {
    known: HashSet<Address>,
}

impl RegistrySet {
    pub fn new<I: IntoIterator<Item = Address>>(known: I) -> Self {
        Self { known: known.into_iter().collect() }
    }

    pub fn observe(&mut self, rows: &PredictionRows) {
        self.known.extend(rows.registries());
    }

    pub fn contains(&self, registry: &Address) -> bool {
        self.known.contains(registry)
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }
}

/// Rows decoded from one batch of logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PredictionRows {
    pub markets: Vec<PredictionMarket>,
    pub resolutions: Vec<PredictionResolution>,
    pub questions: Vec<PredictionQuestion>,
    pub outcome_tokens: Vec<PredictionOutcomeToken>,
    pub trades: Vec<PredictionTrade>,
    pub position_events: Vec<PredictionPositionEvent>,
    pub transfers: Vec<PredictionTransfer>,
}

impl PredictionRows {
    pub fn rows(&self) -> usize {
        self.markets.len()
            + self.resolutions.len()
            + self.questions.len()
            + self.outcome_tokens.len()
            + self.trades.len()
            + self.position_events.len()
            + self.transfers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Moves every row of `other` into `self`.
    pub fn append(&mut self, other: &mut PredictionRows) {
        self.markets.append(&mut other.markets);
        self.resolutions.append(&mut other.resolutions);
        self.questions.append(&mut other.questions);
        self.outcome_tokens.append(&mut other.outcome_tokens);
        self.trades.append(&mut other.trades);
        self.position_events.append(&mut other.position_events);
        self.transfers.append(&mut other.transfers);
    }

    /// Stamps the flush version on every block scoped row. The outcome
    /// token map keeps its own version on purpose, see
    /// [`models::first_seen_version`].
    pub fn set_version(&mut self, version: u64) {
        self.markets.iter_mut().for_each(|row| row._version = version);
        self.resolutions.iter_mut().for_each(|row| row._version = version);
        self.questions.iter_mut().for_each(|row| row._version = version);
        self.trades.iter_mut().for_each(|row| row._version = version);
        self.position_events
            .iter_mut()
            .for_each(|row| row._version = version);
        self.transfers.iter_mut().for_each(|row| row._version = version);
    }

    /// Stamps the chain's purge generation (docs/design.md §2) on every
    /// block scoped row.
    pub fn set_epoch(&mut self, epoch: u32) {
        self.markets.iter_mut().for_each(|row| row.epoch = epoch);
        self.resolutions.iter_mut().for_each(|row| row.epoch = epoch);
        self.questions.iter_mut().for_each(|row| row.epoch = epoch);
        self.trades.iter_mut().for_each(|row| row.epoch = epoch);
        self.position_events.iter_mut().for_each(|row| row.epoch = epoch);
        self.transfers.iter_mut().for_each(|row| row.epoch = epoch);
    }

    /// Fills `tx_from` / `tx_to` from the transactions of the same batch.
    /// Optional. Note that `maker` / `taker` are the traders: `tx_from` is
    /// usually the operator or a relayer.
    pub fn attach_transactions<F>(&mut self, lookup: F)
    where
        F: Fn(&B256) -> Option<TxOrigin>,
    {
        for trade in &mut self.trades {
            if let Some(origin) = lookup(&trade.transaction_hash) {
                trade.tx_from = origin.from;
                trade.tx_to = origin.to.unwrap_or_default();
            }
        }
        for market in &mut self.markets {
            if let Some(origin) = lookup(&market.transaction_hash) {
                market.tx_from = origin.from;
            }
        }
        for event in &mut self.position_events {
            if let Some(origin) = lookup(&event.transaction_hash) {
                event.tx_from = origin.from;
            }
        }
    }

    /// Contracts that showed to be a position registry in this batch.
    pub fn registries(&self) -> HashSet<Address> {
        self.markets
            .iter()
            .map(|market| market.registry)
            .chain(self.resolutions.iter().map(|row| row.registry))
            .chain(self.outcome_tokens.iter().map(|row| row.registry))
            .collect()
    }

    /// Drops the ERC-1155 transfers of contracts `keep` rejects (unrelated
    /// NFT traffic). See [`RegistrySet`].
    pub fn retain_transfers<F>(&mut self, keep: F)
    where
        F: Fn(&Address) -> bool,
    {
        self.transfers.retain(|transfer| keep(&transfer.registry));
    }

    /// Exchanges / pools that traded in this batch: what to hand to
    /// [`VenueWorker::discover`]. Deduplicated, in first-seen order.
    pub fn venue_candidates(&self) -> Vec<VenueCandidate> {
        let mut seen = HashSet::new();

        self.trades
            .iter()
            .filter(|trade| seen.insert(trade.exchange))
            .map(|trade| VenueCandidate {
                exchange: trade.exchange,
                protocol: trade.protocol,
            })
            .collect()
    }

    /// Collateral tokens named by this batch (for the token metadata
    /// worker: the views need their decimals).
    pub fn token_addresses(&self) -> Vec<Address> {
        let mut seen = HashSet::new();

        self.outcome_tokens
            .iter()
            .map(|token| token.collateral_token)
            .chain(
                self.position_events.iter().map(|row| row.collateral_token),
            )
            .filter(|token| !token.is_zero() && seen.insert(*token))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictions::sql::{normalize, statements, MIGRATIONS};

    fn all_rows() -> PredictionRows {
        let mut rows = PredictionRows::default();
        for tx in fixtures::ALL {
            rows.append(&mut decode(tx.chain, &tx.logs()));
        }
        rows
    }

    #[test]
    fn versions_and_epochs_are_stamped_on_block_rows_only() {
        let mut rows = all_rows();
        let map_versions: Vec<u64> =
            rows.outcome_tokens.iter().map(|row| row._version).collect();

        rows.set_version(42);
        rows.set_epoch(7);

        assert!(rows.trades.iter().all(|row| row._version == 42));
        assert!(rows.transfers.iter().all(|row| row.epoch == 7));
        assert!(rows.markets.iter().all(|row| row._version == 42));
        assert!(rows.questions.iter().all(|row| row.epoch == 7));
        assert!(rows.resolutions.iter().all(|row| row._version == 42));
        assert!(rows.position_events.iter().all(|row| row.epoch == 7));
        assert_eq!(
            rows.outcome_tokens
                .iter()
                .map(|row| row._version)
                .collect::<Vec<_>>(),
            map_versions
        );
    }

    #[test]
    fn transactions_are_attached_by_hash() {
        let tx = &fixtures::V2_MINT_MATCH;
        let mut rows = decode(tx.chain, &tx.logs());

        rows.attach_transactions(|hash| {
            (*hash == tx.hash()).then(|| tx.origin())
        });

        assert_eq!(rows.trades[0].tx_from, tx.origin().from);
        assert_eq!(rows.trades[0].tx_to, tx.origin().to.unwrap());
        // The operator sent the transaction, the traders are in the event.
        assert_ne!(rows.trades[0].tx_from, rows.trades[0].taker);
        assert!(rows
            .position_events
            .iter()
            .all(|event| event.tx_from == tx.origin().from));
    }

    #[test]
    fn unrelated_erc1155_traffic_can_be_dropped() {
        let tx = &fixtures::V2_MINT_MATCH;
        let mut logs = tx.logs();

        // The same transfer from a contract that never was a registry.
        let mut game = logs
            .iter()
            .find(|log| {
                log.topic0 == Some(events::ERC1155_TRANSFER_SINGLE.topic0)
            })
            .unwrap()
            .clone();
        game.address = Address::repeat_byte(0x99);
        game.log_index = 10_000;
        logs.push(game);

        let mut rows = decode(tx.chain, &logs);
        let before = rows.transfers.len();

        let mut registries = RegistrySet::default();
        assert!(registries.is_empty());
        registries.observe(&rows);
        assert_eq!(registries.len(), 1);

        rows.retain_transfers(|registry| registries.contains(registry));
        assert_eq!(rows.transfers.len(), before - 1);
    }

    #[test]
    fn candidates_and_collaterals_are_deduplicated() {
        let rows = all_rows();

        let candidates = rows.venue_candidates();
        let unique: HashSet<Address> =
            candidates.iter().map(|candidate| candidate.exchange).collect();
        assert_eq!(unique.len(), candidates.len());
        assert!(candidates.len() >= 5);

        let tokens = rows.token_addresses();
        let unique: HashSet<&Address> = tokens.iter().collect();
        assert_eq!(unique.len(), tokens.len());
        assert!(tokens.iter().all(|token| !token.is_zero()));
    }

    /// `CREATE TABLE` statements of the migrations as (name, body).
    fn tables() -> Vec<(String, String)> {
        MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| statements(sql))
            .map(|statement| normalize(&statement))
            .filter_map(|statement| {
                let rest =
                    statement.strip_prefix("CREATE TABLE IF NOT EXISTS ")?;
                let (name, body) = rest.split_once(' ')?;
                Some((name.to_owned(), body.to_owned()))
            })
            .collect()
    }

    #[test]
    fn every_table_with_a_block_column_is_block_scoped() {
        let mut expected: Vec<String> = tables()
            .into_iter()
            .filter(|(_, body)| body.contains(" block_number UInt64"))
            .map(|(name, _)| name)
            .collect();
        expected.sort();

        let mut listed: Vec<String> =
            BLOCK_SCOPED_TABLES.iter().map(|name| name.to_string()).collect();
        listed.sort();
        assert_eq!(listed, expected);

        // Side tables + base tables = the block scoped tables, and the
        // pipeline inserts into base (and unscoped) tables only.
        let mut parts: Vec<&str> =
            SIDE_TABLES.iter().chain(BASE_TABLES).copied().collect();
        parts.sort_unstable();
        let mut all = BLOCK_SCOPED_TABLES.to_vec();
        all.sort_unstable();
        assert_eq!(parts, all);

        for table in INSERT_ORDER {
            assert!(
                BASE_TABLES.contains(table)
                    || UNSCOPED_TABLES.contains(table),
                "{table}"
            );
        }
        for table in BASE_TABLES {
            assert!(INSERT_ORDER.contains(table), "{table}");
        }
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (name, sql) in MIGRATIONS {
            assert!(name.starts_with("002"), "{name}");

            // Naive splitters must survive: no `;` in comments.
            for line in sql.lines() {
                if let Some((_, comment)) = line.split_once("--") {
                    assert!(!comment.contains(';'), "{name}: {line}");
                }
            }

            for statement in statements(sql) {
                let statement = normalize(&statement);
                assert!(
                    statement.starts_with("CREATE TABLE IF NOT EXISTS ")
                        || statement.starts_with("CREATE VIEW IF NOT EXISTS ")
                        || statement.starts_with(
                            "CREATE MATERIALIZED VIEW IF NOT EXISTS "
                        ),
                    "{name}: {statement}"
                );
                assert!(!statement.contains("indexer."), "{name}");
                assert!(!statement.contains("PROJECTION"), "{name}");
                // Insert only: nothing here may ever delete.
                for forbidden in
                    ["DELETE FROM", "DROP ", "TRUNCATE ", "ALTER TABLE"]
                {
                    assert!(
                        !statement.to_uppercase().contains(forbidden),
                        "{name}: {forbidden}"
                    );
                }
            }
        }

        for (name, body) in tables() {
            let scoped = BLOCK_SCOPED_TABLES.contains(&name.as_str());
            let aggregate = body.contains("AggregatingMergeTree");

            if scoped {
                assert!(
                    body.contains(
                        "ReplacingMergeTree(_version, is_deleted)"
                    ),
                    "{name}"
                );
                assert!(body.contains(" epoch UInt32"), "{name}");
                assert!(
                    body.contains(
                        "do_not_merge_across_partitions_select_final = 1"
                    ),
                    "{name}"
                );

                // Month-only partitions for tables written by the
                // pipeline in block order, by chain for lookups.
                let by_month =
                    body.contains("PARTITION BY toYYYYMM(timestamp)");
                let by_chain = body.contains("PARTITION BY chain ");
                assert!(by_month != by_chain, "{name}");
                if SIDE_TABLES.contains(&name.as_str()) {
                    assert!(by_chain, "{name}");
                }
            }

            if aggregate {
                assert!(
                    body.contains("PARTITION BY toYYYYMM(bucket)"),
                    "{name}"
                );
                // epoch is the LAST sorting key column.
                assert!(body.trim_end().ends_with(", epoch)"), "{name}");
            }
        }
    }

    #[test]
    fn sql_constants_have_their_placeholders() {
        assert_eq!(MISSING_VENUES_SQL.matches("{chain}").count(), 2);
        assert_eq!(MISSING_VENUES_SQL.matches("{limit}").count(), 1);
        assert_eq!(KNOWN_REGISTRIES_SQL.matches("{chain}").count(), 1);
    }
}
