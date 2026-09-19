//! Rows of the `launchpad_*` tables
//! (`migrations/0030_launchpad_tables.sql`).
//!
//! Field order is irrelevant (the clickhouse crate inserts by name), field
//! NAMES must match the columns. Hashes / ids / amounts go through the
//! `crate::utils::format` serializers, which write the binary column types
//! of docs/design.md §1 and §13. `is_deleted` is never written by the
//! decoder (it defaults to 0; tombstones are server side
//! `INSERT ... SELECT`s).
//!
//! # Chain-neutral identity (docs/design.md §13)
//!
//! The tables are shared by every chain family, so
//!
//! * every identity field (token, curve, emitter, creator, trader,
//!   caller, recipient, quote_token, tx_from / tx_to) is an [`Address`]
//!   written through [`SerId32`] into a `FixedString(32)` column: 12 zero
//!   bytes + the 20 address bytes. The Rust type stays [`Address`]
//!   because THIS decoder only ever sees EVM logs; nothing here hand
//!   rolls the padding, and reading a row whose padding is not zero fails
//!   loudly instead of truncating a pubkey into an address.
//! * a `pool_id` is NOT an address even on EVM (a Uniswap V4 / Balancer
//!   pool id is a native 32 byte value), so it stays [`B256`] with
//!   `SerB256` - exactly like `dex_pools.pool_id`. `pool_kind` says
//!   whether the 32 bytes are such an id or a left-padded pool contract.
//! * the transaction id is `tx_id`: [`Bytes`] through [`SerTxId`] into a
//!   `String` column of RAW bytes, because a Solana signature is 64 bytes
//!   and a [`B256`] cannot hold one. Never a sorting key column; build it
//!   with [`tx_id`] and read the EVM hash back with [`tx_hash_of`].
//!
//! The position of a row is `(chain, block_number, tx_index, ordinal)`
//! instead of `(chain, block_number, log_index)` for the same reason:
//! `ordinal` is the log index on EVM and the packed instruction path on a
//! chain that has no block-global log index.
//!
//! [`SerId32`]: crate::utils::format::SerId32
//! [`SerTxId`]: crate::utils::format::SerTxId
//! [`tx_id`]: crate::utils::format::tx_id
//! [`tx_hash_of`]: crate::utils::format::tx_hash_of

use std::{fmt, str::FromStr};

use alloy::primitives::{Address, Bytes, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::utils::format::{SerB256, SerId32, SerTxId, SerU256};

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
    /// Event FAMILY a row was decoded from - which decoder, never a brand.
    /// Anyone can deploy a contract emitting these events: see
    /// `launchpad_trusted_emitters` and README §3.
    Family {
        /// Full bonding curve: launch, buy, sell, fees, graduation.
        PonsV2 => "pons_v2",
        /// Full bonding curve (Flap Portal ABI, several chains).
        FlapPortal => "flap_portal",
        /// Launch attribution only: the token goes straight into a
        /// Uniswap V3 style pool the DEX decoders already index.
        PonsV1 => "pons_v1",
        /// Launch attribution only (Uniswap V4 pool).
        LetsCash => "letscash",
        /// Launch attribution only (its per-token curve is a known gap).
        Bags => "bags",
        /// Launch attribution only (Uniswap V4 pool).
        ClankerV4 => "clanker_v4",
    }
}

impl Family {
    /// Does the family have a curve decoder (trades, graduations, fees)?
    pub const fn has_curve(&self) -> bool {
        matches!(self, Family::PonsV2 | Family::FlapPortal)
    }
}

string_enum! {
    /// Side of a curve trade, from the TRADER's point of view.
    Side {
        Buy => "buy",
        Sell => "sell",
    }
}

string_enum! {
    /// Which market a token was trading on when a fee was taken.
    FeePhase {
        /// On the bonding curve, before graduation.
        Curve => "curve",
        /// In the DEX pool the token graduated into.
        Dex => "dex",
    }
}

string_enum! {
    /// Who a fee row belongs to.
    FeeKind {
        /// The launch's creator share: the serial-rugger signal.
        Creator => "creator",
        /// The venue's own share.
        Protocol => "protocol",
        /// Set aside for a buyback by the venue.
        Buyback => "buyback",
        /// A tax the TOKEN charges on curve trades (Flap).
        Tax => "tax",
        /// Tokens locked in the graduated position (not a payout).
        Locked => "locked",
    }
}

string_enum! {
    /// Shape of the destination pool id in `launchpad_graduations`.
    PoolKind {
        /// A Uniswap V4 style `bytes32` pool id (join `dex_pools.pool_id`).
        PoolId => "pool_id",
        /// A pool CONTRACT; the id is its address left-padded (which is
        /// exactly how `dex_pools.pool_id` stores such pools).
        PoolAddress => "pool_address",
    }
}

/// `launchpad_tokens`: one row per launch event.
///
/// NOT every column is filled by every family - a launch event carries
/// what it carries, and nothing here is ever inferred. Unknown ids are 32
/// zero bytes, unknown amounts 0, unknown text empty.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadToken {
    pub chain: u64,
    /// The launched token.
    #[serde_as(as = "SerId32")]
    pub token: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub family: Family,
    /// Who emitted the launch event (the factory / portal). This is the
    /// address `launchpad_trusted_emitters` is about.
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    /// The contract that emits the token's curve trades: the per-token
    /// curve (`pons_v2`, `bags`) or the portal itself (`flap_portal`).
    /// Zero for attribution-only families.
    #[serde_as(as = "SerId32")]
    pub curve: Address,
    #[serde_as(as = "SerId32")]
    pub creator: Address,
    pub name: String,
    pub symbol: String,
    pub metadata_uri: String,
    /// What the curve is priced in; zero = the chain's native coin.
    #[serde_as(as = "SerId32")]
    pub quote_token: Address,
    /// Minted in the launch transaction by the token itself (the
    /// `Transfer` from the zero address). 0 when the token did not mint in
    /// this transaction.
    #[serde_as(as = "SerU256")]
    pub initial_supply: U256,
    /// Quote units the curve must raise to graduate (`pons_v2`).
    #[serde_as(as = "SerU256")]
    pub graduation_threshold: U256,
    /// Destination pool named BY THE LAUNCH EVENT (attribution-only
    /// families launch straight into a pool). Zero for curve families:
    /// their pool only exists at graduation. A native 32 byte id or a
    /// left-padded pool contract, per `pool_kind`.
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    #[serde_as(as = "DisplayFromStr")]
    pub pool_kind: PoolKind,
    /// The venue's per-launch configuration id, when the event has one.
    #[serde_as(as = "SerU256")]
    pub launch_config_id: U256,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    /// Log index on EVM (docs/design.md §13).
    pub ordinal: u64,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_trades`: one row per bonding-curve buy / sell.
///
/// Amounts are the event's own, GROSS where the venue reports gross (see
/// `events.rs`). `token` / `quote_token` are only trustworthy when their
/// `*_verified` flag is 1 - see `corroborate.rs` and README §3.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadTrade {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub family: Family,
    /// The contract that emitted the trade: the curve (`pons_v2`) or the
    /// portal (`flap_portal`).
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    /// The traded token: named by the event (`flap_portal`) or proven by
    /// the corroborating `Transfer` (`pons_v2`). Zero when neither.
    #[serde_as(as = "SerId32")]
    pub token: Address,
    /// 1 when the token contract itself reported a movement of exactly
    /// `token_amount` to / from `emitter` in this transaction.
    pub token_verified: u8,
    /// Proven quote asset, zero when the quote leg is unverified (it is
    /// always unverified for a native-coin quote: there is no log).
    #[serde_as(as = "SerId32")]
    pub quote_token: Address,
    pub quote_verified: u8,
    #[serde_as(as = "DisplayFromStr")]
    pub side: Side,
    /// Who receives the tokens (buy) / gives them up (sell) per the event.
    #[serde_as(as = "SerId32")]
    pub trader: Address,
    /// The event's `buyer` / `seller` field: a router, an aggregator or
    /// the launch forwarder when it differs from `trader`.
    #[serde_as(as = "SerId32")]
    pub caller: Address,
    #[serde_as(as = "SerU256")]
    pub token_amount: U256,
    #[serde_as(as = "SerU256")]
    pub quote_amount: U256,
    #[serde_as(as = "SerU256")]
    pub fee_amount: U256,
    /// Snipe tax (`pons_v2`) / token tax (`flap_portal`) on this trade.
    #[serde_as(as = "SerU256")]
    pub tax_amount: U256,
    /// Curve progress after the trade as a wad (`1e18` = graduated), when
    /// the venue reports it in the same transaction; 0 otherwise.
    #[serde_as(as = "SerU256")]
    pub progress_wad: U256,
    /// 1 when the curve completed in this transaction.
    pub graduating: u8,
    /// 1 when this is the ONLY trade of the transaction whose quote leg is
    /// unverified. Only then can `tx_value` bound the native quote paid.
    pub sole_unverified_quote: u8,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    #[serde_as(as = "SerId32")]
    pub tx_to: Address,
    /// Native coin sent WITH the transaction ([`super::LaunchpadRows::attach_transactions`]).
    #[serde_as(as = "SerU256")]
    pub tx_value: U256,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_graduations`: the curve finished and the liquidity moved
/// into a DEX pool. `pool_id` is the join key into `dex_pools` /
/// `dex_pool_current_v` / the DEX candles.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadGraduation {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub family: Family,
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "SerId32")]
    pub token: Address,
    /// Zero when the graduation event does not name the pool and no
    /// sibling event of the same transaction does. A native 32 byte id or
    /// a left-padded pool contract, per `pool_kind`.
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    #[serde_as(as = "DisplayFromStr")]
    pub pool_kind: PoolKind,
    #[serde_as(as = "SerId32")]
    pub quote_token: Address,
    /// Tokens moved into the pool.
    #[serde_as(as = "SerU256")]
    pub token_amount: U256,
    /// Quote moved into the pool.
    #[serde_as(as = "SerU256")]
    pub quote_amount: U256,
    /// Liquidity position id, when the venue mints one.
    #[serde_as(as = "SerU256")]
    pub position_id: U256,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_creator_fees`: one row per fee component of a sweep.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadCreatorFee {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    pub tx_index: u32,
    pub ordinal: u64,
    /// Several components share one log: 0, 1, 2 ... inside it.
    pub component: u32,
    #[serde_as(as = "DisplayFromStr")]
    pub family: Family,
    /// The contract that swept (a curve, the graduation hook, a portal).
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    /// Zero when the sweep names a pool instead of a token.
    #[serde_as(as = "SerId32")]
    pub token: Address,
    /// A native 32 byte pool id, or a left-padded pool contract.
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    #[serde_as(as = "DisplayFromStr")]
    pub phase: FeePhase,
    #[serde_as(as = "DisplayFromStr")]
    pub kind: FeeKind,
    /// Who was credited, when an escrow `Credited` of the same amount and
    /// the same source is in the transaction. Zero otherwise.
    #[serde_as(as = "SerId32")]
    pub recipient: Address,
    pub recipient_known: u8,
    #[serde_as(as = "SerId32")]
    pub quote_token: Address,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    pub epoch: u32,
    pub _version: u64,
}

/// `launchpad_trusted_emitters`: operator data, never written by the
/// indexer. The README ships the verified addresses as ready-to-run
/// INSERTs; migrations seed nothing (docs/design.md §5, same rule as
/// `quote_tokens` and `dex_trusted_emitters`).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadTrustedEmitter {
    pub chain: u64,
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub family: Family,
    pub label: String,
    pub _version: u64,
}

/// `launchpad_frontends`: fomo, GMGN, Axiom ... have no contracts of their
/// own. Operator data; front-end volume is NEVER added to venue volume
/// (docs/design.md §11).
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct LaunchpadFrontend {
    pub chain: u64,
    #[serde_as(as = "SerId32")]
    pub address: Address,
    pub name: String,
    /// `fee_recipient` | `router`.
    pub kind: String,
    pub _version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_their_column_value() {
        for family in Family::ALL {
            assert_eq!(family.as_str().parse::<Family>(), Ok(*family));
        }
        for kind in FeeKind::ALL {
            assert_eq!(kind.as_str().parse::<FeeKind>(), Ok(*kind));
        }
        for side in Side::ALL {
            assert_eq!(side.as_str().parse::<Side>(), Ok(*side));
        }
        for phase in FeePhase::ALL {
            assert_eq!(phase.as_str().parse::<FeePhase>(), Ok(*phase));
        }
        for kind in PoolKind::ALL {
            assert_eq!(kind.as_str().parse::<PoolKind>(), Ok(*kind));
        }
        assert!("pump_fun".parse::<Family>().is_err());
    }

    #[test]
    fn only_two_families_have_a_curve() {
        let with_curve: Vec<&Family> =
            Family::ALL.iter().filter(|f| f.has_curve()).collect();
        assert_eq!(with_curve, [&Family::PonsV2, &Family::FlapPortal]);
    }

    #[test]
    fn evm_ids_round_trip_and_keep_the_dex_pool_id_convention() {
        use crate::utils::format::{address_of_id32, id32};

        let address = Address::repeat_byte(0x5a);
        let id = id32(address);

        assert_eq!(id.0[..12], [0u8; 12]);
        assert_eq!(address_of_id32(id), Some(address));
        // Exactly what src/dex/models.rs does for pool addresses.
        assert_eq!(id, crate::dex::models::pool_id_of(address));

        // A 32 byte id that is not an EVM address stays unconvertible.
        assert_eq!(address_of_id32(B256::repeat_byte(0x11)), None);
    }
}
