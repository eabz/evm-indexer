//! Rows of the `prediction_*` tables
//! (`migrations/0020_prediction_tables.sql`).
//!
//! Field order is irrelevant (the clickhouse crate inserts by name), field
//! NAMES must match the columns. Hashes / ids / amounts go through the
//! `crate::utils::format` serializers, which write the binary column types
//! of docs/design.md §1 (`FixedString(32)`, `UInt256`, `Int256`).
//! `is_deleted` is never written by the decoder (it defaults to 0;
//! tombstones are server side `INSERT ... SELECT`s).
//!
//! **Chain neutral** (docs/design.md §13): these tables are shared with
//! whatever non-EVM prediction venue is fed into them later, so
//!
//! * every identity field is an `Address` written through [`SerId32`] as
//!   `FixedString(32)` (12 zero bytes + the 20 address bytes). The Rust
//!   type stays `Address` because THIS decoder only ever sees EVM logs; a
//!   non-EVM front end writes the same columns from its own row type.
//! * the transaction id is `tx_id`: `Bytes` through [`SerTxId`] into a
//!   `String` column of RAW bytes, because a Solana signature is 64 bytes
//!   and a `B256` cannot hold one. Never a sorting key column.
//!   [`tx_hash_of`] gives the EVM hash back.
//! * the position is `(chain, block_number, tx_index, ordinal)`:
//!   `tx_index` = the transaction index, `ordinal` = the log index.
//!   `block_number` keeps its name on every chain.
//!
//! [`SerId32`]: crate::utils::format::SerId32
//! [`SerTxId`]: crate::utils::format::SerTxId
//! [`tx_hash_of`]: crate::utils::format::tx_hash_of

use std::{fmt, str::FromStr};

use alloy::primitives::{Address, Bytes, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::utils::format::{SerB256, SerBytes, SerId32, SerTxId, SerU256};

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            /// Value of the column.
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = String;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                $name::ALL
                    .iter()
                    .copied()
                    .find(|item| item.as_str() == value)
                    .ok_or_else(|| {
                        format!(
                            concat!("unknown ", stringify!($name), " {:?}"),
                            value
                        )
                    })
            }
        }
    };
}

string_enum! {
    /// Event FAMILY a row was decoded from - never a brand: Polymarket,
    /// Predict.fun, Opinion and Limitless all emit `ctf_exchange` events.
    Protocol {
        /// Gnosis Conditional Tokens Framework (the position registry).
        Ctf => "ctf",
        /// Polymarket CTF Exchange V1 events (asset ids, 5 data words).
        CtfExchange => "ctf_exchange",
        /// Polymarket CTF Exchange V2 events (side + token id).
        CtfExchangeV2 => "ctf_exchange_v2",
        /// Gnosis FixedProductMarketMaker (Omen, Limitless AMM markets).
        Fpmm => "fpmm",
        /// A contract that splits / merges / redeems on behalf of users
        /// and names them in its own 3 argument events (NegRiskAdapter,
        /// Polymarket collateral adapters).
        CtfAdapter => "ctf_adapter",
        /// Polymarket NegRiskAdapter (multi outcome events, conversions).
        NegRisk => "neg_risk",
        /// Polymarket UmaCtfAdapter (questions, on chain titles).
        Uma => "uma",
    }
}

string_enum! {
    /// Buy / sell of OUTCOME SHARES against collateral.
    Side {
        Buy => "buy",
        Sell => "sell",
    }
}

impl Side {
    pub const fn opposite(&self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

string_enum! {
    /// How the two orders of a fill were settled.
    MatchType {
        /// Buy against sell of the SAME outcome: shares change hands.
        Complementary => "complementary",
        /// Two buys of complementary outcomes: a full set is minted.
        Mint => "mint",
        /// Two sells of complementary outcomes: a full set is merged.
        Merge => "merge",
        /// An order filled directly by the operator (no taker order).
        Direct => "direct",
        /// Against an automated market maker (the pool is the maker).
        Amm => "amm",
    }
}

string_enum! {
    /// Unit a fee was charged in.
    FeeUnit {
        Collateral => "collateral",
        Shares => "shares",
    }
}

string_enum! {
    /// `prediction_position_events.kind`.
    PositionEventKind {
        Split => "split",
        Merge => "merge",
        Redeem => "redeem",
        /// NegRisk conversion (NO of some questions -> YES of the others).
        Convert => "convert",
    }
}

string_enum! {
    /// Why an account's balance of an outcome token changed.
    TransferReason {
        /// Minted / received because the account split collateral.
        Split => "split",
        /// Burnt / sent because the account merged a full set.
        Merge => "merge",
        /// Burnt / sent because the account redeemed after resolution.
        Redeem => "redeem",
        /// Moved to / from an exchange (or AMM) that traded in the same
        /// transaction: the price is in `prediction_trades`.
        Trade => "trade",
        /// Anything else: wallet to wallet, escrow, conversions. Changes
        /// the balance, never the cost basis.
        Transfer => "transfer",
    }
}

string_enum! {
    /// `prediction_questions.kind`.
    QuestionKind {
        /// UmaCtfAdapter `QuestionInitialized`.
        UmaQuestion => "uma_question",
        /// UmaCtfAdapter `QuestionReset`: the proposal was disputed.
        UmaReset => "uma_reset",
        /// UmaCtfAdapter `QuestionFlagged`: manual resolution pending.
        UmaFlagged => "uma_flagged",
        /// NegRiskAdapter `MarketPrepared`: a multi outcome event.
        NegRiskEvent => "neg_risk_event",
        /// NegRiskAdapter `QuestionPrepared`: one market of an event.
        NegRiskQuestion => "neg_risk_question",
    }
}

string_enum! {
    /// Where a row comes from.
    RowSource {
        Event => "event",
        Rpc => "rpc",
        /// The resolver asked and the contract did not answer like the
        /// family does. Kept so it is never asked again.
        Unresolved => "unresolved",
    }
}

/// `_version` of rows written by the RPC resolver: loses against every
/// event row.
pub const VERSION_RPC: u64 = 1;
/// `_version` of [`RowSource::Unresolved`] rows: loses against everything.
pub const VERSION_UNRESOLVED: u64 = 0;

/// `_version` that DEcreases with the position of an event, so the FIRST
/// sighting wins whatever the insertion order and a re-inserted block
/// yields the same version (idempotent). Used for the outcome token map.
///
/// The position is `(block_number, tx_index, ordinal)` (docs/design.md
/// §13) packed into 39 + 12 + 12 bits, each saturating. The total stays
/// under 63 bits so every event row beats [`VERSION_RPC`]. Saturation only
/// costs tie breaking between two sightings of the SAME token in the same
/// block, which differ in `first_seen_block` / `first_seen_timestamp`
/// alone.
pub fn first_seen_version(
    block_number: u64,
    tx_index: u32,
    ordinal: u64,
) -> u64 {
    const ORDINAL_BITS: u32 = 12;
    const TX_BITS: u32 = 12;
    const BLOCK_BITS: u32 = 39;

    let ordinal = ordinal.min((1 << ORDINAL_BITS) - 1);
    let tx_index = u64::from(tx_index).min((1 << TX_BITS) - 1);
    let block_number = block_number.min((1 << BLOCK_BITS) - 1);

    u64::MAX
        - ((block_number << (TX_BITS + ORDINAL_BITS))
            | (tx_index << ORDINAL_BITS)
            | ordinal)
}

/// `prediction_markets`: one row per `ConditionPreparation` (plus at most
/// one row written by the RPC resolver for a market prepared before the
/// first indexed block).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionMarket {
    pub chain: u64,
    /// The CTF `conditionId`. Verified against
    /// `keccak256(oracle, questionId, outcomeSlotCount)` for event rows.
    #[serde_as(as = "SerB256")]
    pub market_id: B256,
    /// The contract holding the positions (the CTF). Part of the market's
    /// identity: a forged registry can not collide with the real one.
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// Who may report the payouts (UmaCtfAdapter, NegRiskAdapter,
    /// Reality.eth proxy...). Zero for RPC rows.
    #[serde_as(as = "SerId32")]
    pub oracle: Address,
    #[serde_as(as = "SerB256")]
    pub question_id: B256,
    pub outcome_count: u16,
    /// 0 for RPC rows (never purged: chain state, not fork dependent).
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub source: RowSource,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_resolutions`: one row per `ConditionResolution`.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionResolution {
    pub chain: u64,
    #[serde_as(as = "SerB256")]
    pub market_id: B256,
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    #[serde_as(as = "SerId32")]
    pub oracle: Address,
    #[serde_as(as = "SerB256")]
    pub question_id: B256,
    pub outcome_count: u16,
    /// One numerator per outcome slot. `[1, 0]` = outcome 0 won,
    /// `[1, 1]` = split 50 / 50.
    #[serde_as(as = "Vec<SerU256>")]
    pub payout_numerators: Vec<U256>,
    /// Sum of the numerators (saturating).
    #[serde_as(as = "SerU256")]
    pub payout_denominator: U256,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_questions`: what the chain says ABOUT a market (titles,
/// event grouping, disputes).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionQuestion {
    pub chain: u64,
    /// Equals `prediction_markets.question_id` when `emitter` is the
    /// market's oracle. For [`QuestionKind::NegRiskEvent`] the event id.
    #[serde_as(as = "SerB256")]
    pub question_id: B256,
    /// The adapter that emitted the event.
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub kind: QuestionKind,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// NegRisk `marketId` grouping the markets of one multi outcome event,
    /// zero when the question stands alone.
    #[serde_as(as = "SerB256")]
    pub event_id: B256,
    /// Position of the question inside its event.
    pub question_index: u32,
    /// Parsed from the on chain text, empty when it has none.
    pub title: String,
    pub description: String,
    /// Labels by outcome index when the text names them, else empty.
    pub outcomes: Vec<String>,
    /// The raw on chain payload (UMA ancillary data / NegRisk data).
    #[serde_as(as = "SerBytes")]
    pub data: Bytes,
    /// UMA: who initialized the question.
    #[serde_as(as = "SerId32")]
    pub creator: Address,
    /// NegRisk `MarketPrepared`: the oracle (operator) of the event.
    #[serde_as(as = "SerId32")]
    pub oracle: Address,
    /// UMA proposer reward / bond (raw units of `reward_token`).
    #[serde_as(as = "SerId32")]
    pub reward_token: Address,
    #[serde_as(as = "SerU256")]
    pub reward: U256,
    #[serde_as(as = "SerU256")]
    pub proposal_bond: U256,
    /// NegRisk `MarketPrepared.feeBips`.
    pub fee_bips: u32,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_outcome_tokens`: outcome index <-> ERC-1155 token id.
///
/// NOT block scoped: the id is `keccak(collateral, collection(condition,
/// 1 << index))`, a mathematical fact no reorg can change. Rows are
/// COMPUTED from `PositionSplit` / `PositionsMerge` / `PayoutRedemption`
/// (see `ids.rs`), never taken from a contract's claim.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionOutcomeToken {
    pub chain: u64,
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    #[serde_as(as = "SerU256")]
    pub outcome_token_id: U256,
    #[serde_as(as = "SerB256")]
    pub market_id: B256,
    pub outcome_index: u16,
    #[serde_as(as = "SerId32")]
    pub collateral_token: Address,
    /// Block of the first event that revealed the token (informational).
    pub first_seen_block: u64,
    pub first_seen_timestamp: u32,
    /// [`first_seen_version`]: the earliest sighting wins.
    pub _version: u64,
}

/// `prediction_trades`: THE canonical trade, one row per filled MAKER
/// order, told from the TAKER's point of view (see the module docs).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionTrade {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    /// Index of the transaction inside the block.
    pub tx_index: u32,
    /// Log index of the maker order's `OrderFilled` (or of the AMM event).
    pub ordinal: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// Emitter: the exchange, or the AMM pool.
    #[serde_as(as = "SerId32")]
    pub exchange: Address,
    /// The ERC-1155 contract that moved the outcome token in the same
    /// transaction; zero when no such transfer exists (the trade then
    /// belongs to no market).
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    /// Hash of the maker order, zero for AMM trades.
    #[serde_as(as = "SerB256")]
    pub order_hash: B256,
    /// Owner of the resting order (the AMM pool for `fpmm`).
    #[serde_as(as = "SerId32")]
    pub maker: Address,
    /// Owner of the order that crossed the book (the buyer / seller for
    /// `fpmm`, the operator for a `direct` fill).
    #[serde_as(as = "SerId32")]
    pub taker: Address,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    #[serde_as(as = "SerId32")]
    pub tx_to: Address,
    /// The token the TAKER bought or sold.
    #[serde_as(as = "SerU256")]
    pub outcome_token_id: U256,
    /// The taker's side.
    #[serde_as(as = "DisplayFromStr")]
    pub side: Side,
    /// Outcome shares of this fill (the same number for both parties in
    /// every match type).
    #[serde_as(as = "SerU256")]
    pub share_amount: U256,
    /// Collateral the taker paid (buy) or received (sell) for
    /// `share_amount`, fees excluded. price = collateral / shares.
    #[serde_as(as = "SerU256")]
    pub collateral_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub match_type: MatchType,
    /// 1 when the SHARES of this fill are proven by the chain: the same
    /// transaction carries ERC-1155 transfers of this exact
    /// `outcome_token_id` emitted by this `registry`, and the fills of
    /// that (registry, token) do not claim more shares than actually
    /// moved. Candles, the ledger's priced legs and the leaderboard count
    /// `verified = 1` only - an `OrderFilled` is free to emit, so without
    /// the bound one forged log owns a real market's volume and price.
    /// See `decode::verify_shares`.
    pub verified: u8,
    /// The token of the MAKER order: the same as `outcome_token_id` for
    /// `complementary` / `direct` / `amm`, the other outcome for `mint` /
    /// `merge`.
    #[serde_as(as = "SerU256")]
    pub maker_outcome_token_id: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub maker_side: Side,
    /// Collateral of the maker for `share_amount`, straight from the
    /// event. `mint` / `merge`: `share_amount - collateral_amount`.
    #[serde_as(as = "SerU256")]
    pub maker_collateral_amount: U256,
    /// Fee charged to the maker order (from its event).
    #[serde_as(as = "SerU256")]
    pub maker_fee_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub maker_fee_unit: FeeUnit,
    /// Share of the taker order's fee attributed to this fill (pro rata
    /// by shares, the last fill takes the remainder: the fills of a taker
    /// order add up to its fee exactly).
    #[serde_as(as = "SerU256")]
    pub taker_fee_amount: U256,
    #[serde_as(as = "DisplayFromStr")]
    pub taker_fee_unit: FeeUnit,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_position_events`: collateral entering / leaving a market.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionPositionEvent {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    /// `ctf`: the registry itself (the source of open interest).
    /// `neg_risk`: the adapter naming the real user (attribution only).
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub kind: PositionEventKind,
    #[serde_as(as = "SerId32")]
    pub stakeholder: Address,
    /// conditionId; for `convert` the NegRisk event (market) id.
    #[serde_as(as = "SerB256")]
    pub market_id: B256,
    /// Zero when the event does not carry it and no sibling event of the
    /// same transaction does.
    #[serde_as(as = "SerId32")]
    pub collateral_token: Address,
    #[serde_as(as = "SerB256")]
    pub parent_collection_id: B256,
    /// Partition (split / merge), redeemed index sets (redeem), the
    /// converted question set (convert).
    #[serde_as(as = "Vec<SerU256>")]
    pub index_sets: Vec<U256>,
    /// Collateral: split / merged amount, payout of a redemption, amount
    /// of a conversion.
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_transfers`: one row per ERC-1155 id moved.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionTransfer {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    /// Position inside a `TransferBatch`, 0 for `TransferSingle`.
    pub batch_index: u32,
    /// Emitter of the ERC-1155 event.
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    #[serde_as(as = "SerId32")]
    pub operator: Address,
    #[serde_as(as = "SerId32")]
    pub from: Address,
    #[serde_as(as = "SerId32")]
    pub to: Address,
    #[serde_as(as = "SerU256")]
    pub outcome_token_id: U256,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    /// Why `from` lost the shares.
    #[serde_as(as = "DisplayFromStr")]
    pub from_reason: TransferReason,
    /// Why `to` got them.
    #[serde_as(as = "DisplayFromStr")]
    pub to_reason: TransferReason,
    /// Cost basis convention of a split / merge leg: `amount / number of
    /// outcomes of the partition` (a full set costs / returns exactly
    /// `amount`). Zero for every other reason.
    #[serde_as(as = "SerU256")]
    pub priced_collateral: U256,
    pub epoch: u32,
    pub _version: u64,
}

/// `prediction_venues`: what only the contract itself can tell about an
/// exchange / AMM pool (written by the background resolver).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct PredictionVenue {
    pub chain: u64,
    #[serde_as(as = "SerId32")]
    pub exchange: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// What trades are paid in (`getCollateral()` / `collateralToken()`).
    #[serde_as(as = "SerId32")]
    pub collateral_token: Address,
    /// `getCtf()` / `conditionalTokens()`.
    #[serde_as(as = "SerId32")]
    pub registry: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub source: RowSource,
    pub _version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_their_column_value() {
        for protocol in Protocol::ALL {
            assert_eq!(
                protocol.as_str().parse::<Protocol>(),
                Ok(*protocol)
            );
        }
        for reason in TransferReason::ALL {
            assert_eq!(
                reason.as_str().parse::<TransferReason>(),
                Ok(*reason)
            );
        }
        assert!("v9".parse::<Protocol>().is_err());
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.opposite(), Side::Buy);
    }

    #[test]
    fn the_first_sighting_has_the_highest_version() {
        // Ordered by (block_number, tx_index, ordinal), descending.
        assert!(
            first_seen_version(10, 0, 5) > first_seen_version(10, 0, 6)
        );
        assert!(
            first_seen_version(10, 0, 6) > first_seen_version(10, 1, 0)
        );
        assert!(
            first_seen_version(10, 1, 0) > first_seen_version(11, 0, 0)
        );
        assert_eq!(
            first_seen_version(10, 0, 5),
            first_seen_version(10, 0, 5)
        );
        assert!(
            first_seen_version(u64::MAX, u32::MAX, u64::MAX) > VERSION_RPC
        );
        assert!(
            first_seen_version(u64::MAX, u32::MAX, u64::MAX)
                > VERSION_UNRESOLVED
        );
    }
}
