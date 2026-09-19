//! Token launchpads, display first (docs/design.md §11).
//!
//! **Decode by event family from `logs`, never by address registry.** What
//! identifies a launch or a curve trade is its `topic0` AND the exact shape
//! of the event, so a venue that deploys the same ABI on a chain nobody has
//! heard of is indexed on day one. `family` is therefore the event FAMILY
//! (`pons_v2`, `flap_portal`, `pons_v1`, `letscash`, `bags`,
//! `clanker_v4`), never a brand. `README.md` in this directory holds the
//! Phase-1 evidence (every address, every transaction), the trust rule and
//! the query cookbook - one query per screen of a trading UI.
//!
//! # Two kinds of family
//!
//! * **full curve** (`pons_v2`, `flap_portal`): launch, buys and sells on
//!   the bonding curve, fee sweeps and the graduation into a DEX pool.
//! * **launch attribution only** (everything else): these venues launch
//!   straight into a Uniswap V3 / V4 pool, so the trading is already in
//!   `dex_swaps` and the launch event is all this module adds. The row
//!   carries the destination `pool_id`, which IS the join key into
//!   `dex_pools` / `dex_pool_current_v` / the DEX candles.
//!
//! # Nothing here is trusted by default
//!
//! Anyone can emit a `TokenLaunched` or a `CurveBuy`. Two independent
//! defences, both applied at READ time:
//!
//! 1. **Corroboration** (`decode.rs`): a trade leg counts only when the
//!    asset contract itself reported the movement in the same transaction.
//!    Native-coin legs can never be corroborated from logs.
//! 2. **Trusted emitters**: the real venues are singletons with verified
//!    source. `launchpad_trusted_emitters` is operator data (the README
//!    ships the addresses as ready-to-run INSERTs, migrations seed
//!    nothing); every headline view counts only trusted emitters, and the
//!    `*_all_v` variants exist for exploration.
//!
//! # Wiring (pipeline)
//!
//! ```text
//! let mut rows = launchpads::decode(chain, &batch.logs);
//! rows.attach_transactions(|hash| ...);   // tx_from / tx_to / value
//! rows.set_epoch(epoch); rows.set_version(version);
//! insert in INSERT_ORDER;
//! ```
//!
//! [`decode`] needs no RPC, no state and no registry, so a range can be
//! re-decoded from the stored `logs` at any time.

pub mod cookbook;
pub mod decode;
pub mod derived;
pub mod events;
pub mod models;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod fixtures_data;
#[cfg(test)]
mod integration_tests;
#[cfg(test)]
pub(crate) mod sql;

use std::collections::HashSet;

use alloy::primitives::{B256, U256};

pub use self::{
    decode::decode,
    derived::LAUNCHPADS_DERIVED,
    models::{
        address_of, id_of, Family, FeeKind, FeePhase, Id,
        LaunchpadCreatorFee, LaunchpadFrontend, LaunchpadGraduation,
        LaunchpadToken, LaunchpadTrade, LaunchpadTrustedEmitter, PoolKind,
        Side,
    },
};

/// Block scoped tables the pipeline writes (and `purge_range` tombstones),
/// children first. Every one has `chain`, `block_number`, `timestamp`,
/// `epoch`, `_version`, `is_deleted`.
pub const BASE_TABLES: &[&str] = &[
    "launchpad_creator_fees",
    "launchpad_graduations",
    "launchpad_trades",
    "launchpad_tokens",
];

/// Read path tables fed by materialized views that pass `_version`,
/// `is_deleted` and `epoch` through: never written or tombstoned directly.
pub const SIDE_TABLES: &[&str] = &[
    "launchpad_trades_by_token",
    "launchpad_trades_by_trader",
    "launchpad_launches_by_time",
    "launchpad_launches_by_creator",
];

/// Everything with a `block_number` column, side tables first (the list
/// `db::BLOCK_SCOPED_TABLES` is extended with).
pub const BLOCK_SCOPED_TABLES: &[&str] = &[
    "launchpad_trades_by_token",
    "launchpad_trades_by_trader",
    "launchpad_launches_by_time",
    "launchpad_launches_by_creator",
    "launchpad_creator_fees",
    "launchpad_graduations",
    "launchpad_trades",
    "launchpad_tokens",
];

/// Tables that are NOT block scoped and never purged: operator data.
pub const UNSCOPED_TABLES: &[&str] =
    &["launchpad_trusted_emitters", "launchpad_frontends"];

/// The order the pipeline inserts in: a reader must never see a trade of a
/// token whose launch row is not there yet.
pub const INSERT_ORDER: &[&str] = &[
    "launchpad_tokens",
    "launchpad_trades",
    "launchpad_graduations",
    "launchpad_creator_fees",
];

/// The block column of every table of [`BLOCK_SCOPED_TABLES`].
pub fn block_column(_table: &str) -> &'static str {
    "block_number"
}

/// Curves that traded and whose token has no `launchpad_tokens` row (the
/// launch is older than the indexed range, or the emitter is a forgery).
/// Diagnostics only - nothing resolves them over RPC. Placeholders:
/// `{chain}`, `{limit}`.
pub const UNKNOWN_CURVES_SQL: &str = "\
SELECT emitter, any(toString(family)) AS family, count() AS trades \
FROM launchpad_trades \
WHERE chain = {chain} AND emitter NOT IN (\
SELECT curve FROM launchpad_tokens FINAL WHERE chain = {chain}) \
GROUP BY emitter \
ORDER BY trades DESC \
LIMIT {limit}";

/// `from`, `to` and `value` of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TxOrigin {
    pub from: alloy::primitives::Address,
    /// `None` for contract creations.
    pub to: Option<alloy::primitives::Address>,
    /// Native coin sent with the transaction.
    pub value: U256,
}

/// Rows decoded from one batch of logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaunchpadRows {
    pub tokens: Vec<LaunchpadToken>,
    pub trades: Vec<LaunchpadTrade>,
    pub graduations: Vec<LaunchpadGraduation>,
    pub creator_fees: Vec<LaunchpadCreatorFee>,
}

impl LaunchpadRows {
    pub fn rows(&self) -> usize {
        self.tokens.len()
            + self.trades.len()
            + self.graduations.len()
            + self.creator_fees.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// Moves every row of `other` into `self`.
    pub fn append(&mut self, other: &mut LaunchpadRows) {
        self.tokens.append(&mut other.tokens);
        self.trades.append(&mut other.trades);
        self.graduations.append(&mut other.graduations);
        self.creator_fees.append(&mut other.creator_fees);
    }

    /// Stamps the flush version on every row.
    pub fn set_version(&mut self, version: u64) {
        self.tokens.iter_mut().for_each(|row| row._version = version);
        self.trades.iter_mut().for_each(|row| row._version = version);
        self.graduations.iter_mut().for_each(|row| row._version = version);
        self.creator_fees
            .iter_mut()
            .for_each(|row| row._version = version);
    }

    /// Stamps the chain's purge generation (docs/design.md §2).
    pub fn set_epoch(&mut self, epoch: u32) {
        self.tokens.iter_mut().for_each(|row| row.epoch = epoch);
        self.trades.iter_mut().for_each(|row| row.epoch = epoch);
        self.graduations.iter_mut().for_each(|row| row.epoch = epoch);
        self.creator_fees.iter_mut().for_each(|row| row.epoch = epoch);
    }

    /// Fills `tx_from` / `tx_to` / `tx_value` from the transactions of the
    /// same batch. Optional, and never a substitute for the event: on a
    /// curve the trader is in the event, `tx_from` is whoever paid the gas
    /// (a router, a bundler, the launch forwarder).
    pub fn attach_transactions<F>(&mut self, lookup: F)
    where
        F: Fn(&B256) -> Option<TxOrigin>,
    {
        for row in &mut self.tokens {
            if let Some(origin) = lookup(&row.transaction_hash) {
                row.tx_from = id_of(origin.from);
            }
        }
        for row in &mut self.trades {
            if let Some(origin) = lookup(&row.transaction_hash) {
                row.tx_from = id_of(origin.from);
                row.tx_to = origin.to.map(id_of).unwrap_or_default();
                row.tx_value = origin.value;
            }
        }
        for row in &mut self.graduations {
            if let Some(origin) = lookup(&row.transaction_hash) {
                row.tx_from = id_of(origin.from);
            }
        }
        for row in &mut self.creator_fees {
            if let Some(origin) = lookup(&row.transaction_hash) {
                row.tx_from = id_of(origin.from);
            }
        }
    }

    /// Emitters this batch showed: what an operator has to decide about
    /// (`launchpad_trusted_emitters`). Deduplicated, in first-seen order.
    pub fn emitters(&self) -> Vec<(Id, Family)> {
        let mut seen = HashSet::new();

        self.tokens
            .iter()
            .map(|row| (row.emitter, row.family))
            .chain(
                self.graduations
                    .iter()
                    .map(|row| (row.emitter, row.family)),
            )
            .filter(|entry| seen.insert(*entry))
            .collect()
    }

    /// Tokens this batch named, for the token metadata worker (the views
    /// need their decimals). EVM addresses only.
    pub fn token_addresses(&self) -> Vec<alloy::primitives::Address> {
        let mut seen = HashSet::new();

        self.tokens
            .iter()
            .flat_map(|row| [row.token, row.quote_token])
            .chain(
                self.trades
                    .iter()
                    .flat_map(|row| [row.token, row.quote_token]),
            )
            .filter(|id| *id != B256::ZERO && seen.insert(*id))
            .filter_map(address_of)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launchpads::sql::{normalize, statements, MIGRATIONS};

    fn all_rows() -> LaunchpadRows {
        let mut rows = LaunchpadRows::default();
        for tx in fixtures::ALL {
            rows.append(&mut decode(tx.chain, &tx.logs()));
        }
        rows
    }

    #[test]
    fn versions_and_epochs_are_stamped_on_every_row() {
        let mut rows = all_rows();
        assert!(rows.rows() > 60);

        rows.set_version(42);
        rows.set_epoch(7);

        assert!(rows.tokens.iter().all(|row| row._version == 42));
        assert!(rows.trades.iter().all(|row| row.epoch == 7));
        assert!(rows.graduations.iter().all(|row| row._version == 42));
        assert!(rows.creator_fees.iter().all(|row| row.epoch == 7));
    }

    #[test]
    fn transactions_are_attached_by_hash() {
        let tx = &fixtures::PONS_LAUNCH;
        let mut rows = decode(tx.chain, &tx.logs());

        rows.attach_transactions(|hash| {
            (*hash == tx.hash()).then(|| tx.origin())
        });

        let trade = &rows.trades[0];
        assert_eq!(trade.tx_from, id_of(tx.origin().from));
        assert_eq!(trade.tx_to, id_of(tx.origin().to.unwrap()));
        assert_eq!(trade.tx_value, tx.origin().value);
        // The launch forwarder sent the transaction; the trader is the
        // creator, and the event's `caller` is the forwarder.
        assert_eq!(trade.caller, trade.tx_to);
        assert_ne!(trade.trader, trade.caller);
        assert_eq!(trade.trader, id_of(tx.origin().from));
    }

    #[test]
    fn emitters_and_tokens_are_deduplicated() {
        let rows = all_rows();

        let emitters = rows.emitters();
        let unique: HashSet<&(Id, Family)> = emitters.iter().collect();
        assert_eq!(unique.len(), emitters.len());
        assert!(emitters.len() >= 6);

        let tokens = rows.token_addresses();
        let unique: HashSet<&alloy::primitives::Address> =
            tokens.iter().collect();
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
                let rest = statement
                    .strip_prefix("CREATE TABLE IF NOT EXISTS ")?;
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
            BLOCK_SCOPED_TABLES.iter().map(|n| n.to_string()).collect();
        listed.sort();
        assert_eq!(listed, expected);

        let mut parts: Vec<&str> =
            SIDE_TABLES.iter().chain(BASE_TABLES).copied().collect();
        parts.sort_unstable();
        let mut all = BLOCK_SCOPED_TABLES.to_vec();
        all.sort_unstable();
        assert_eq!(parts, all);

        for table in INSERT_ORDER {
            assert!(BASE_TABLES.contains(table), "{table}");
        }
        for table in BASE_TABLES {
            assert!(INSERT_ORDER.contains(table), "{table}");
        }
        // Children first in BASE_TABLES (the purge order), parents first
        // in INSERT_ORDER: the two are exact mirrors.
        let reversed: Vec<&&str> = INSERT_ORDER.iter().rev().collect();
        let base: Vec<&&str> = BASE_TABLES.iter().collect();
        assert_eq!(reversed, base);
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (name, sql) in MIGRATIONS {
            assert!(name.starts_with("003"), "{name}");

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
                        || statement
                            .starts_with("CREATE VIEW IF NOT EXISTS ")
                        || statement.starts_with(
                            "CREATE MATERIALIZED VIEW IF NOT EXISTS "
                        ),
                    "{name}: {statement}"
                );
                assert!(!statement.contains("indexer."), "{name}");
                assert!(!statement.contains("PROJECTION"), "{name}");
                // Insert only: nothing here may ever delete. Dedup windows
                // are added by migration 0090, never here.
                for forbidden in [
                    "DELETE FROM",
                    "DROP ",
                    "TRUNCATE ",
                    "ALTER TABLE",
                    "NON_REPLICATED_DEDUPLICATION_WINDOW",
                ] {
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

    /// Every identity column is chain neutral (docs/solana-research.md §0).
    #[test]
    fn identity_columns_are_32_bytes_and_positions_are_chain_neutral() {
        for (name, body) in tables() {
            if !name.starts_with("launchpad_") {
                continue;
            }
            assert!(!body.contains("FixedString(20)"), "{name}");
            assert!(!body.contains(" log_index "), "{name}");
            for column in
                ["token", "emitter", "creator", "trader", "recipient"]
            {
                if body.contains(&format!(" {column} ")) {
                    assert!(
                        body.contains(&format!(
                            " {column} FixedString(32)"
                        )),
                        "{name}.{column}"
                    );
                }
            }
        }
    }

    #[test]
    fn sql_constants_have_their_placeholders() {
        assert_eq!(UNKNOWN_CURVES_SQL.matches("{chain}").count(), 2);
        assert_eq!(UNKNOWN_CURVES_SQL.matches("{limit}").count(), 1);
    }
}
