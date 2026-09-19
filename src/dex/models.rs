//! Rows of the `dex_*` tables (`migrations/0010_dex_tables.sql`).
//!
//! Field order is irrelevant (the clickhouse crate inserts by name), field
//! NAMES must match the columns. Hashes / addresses / amounts go through
//! the `crate::utils::format` serializers, which write the binary
//! column types of docs/design.md §1: (`FixedString(32)`, `FixedString(20)`, `UInt256`, `Int256`).

use std::{fmt, str::FromStr};

use alloy::primitives::{Address, B256, I256, U256};
use clickhouse::Row;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_with::{serde_as, DeserializeAs, DisplayFromStr, SerializeAs};

use crate::utils::format::{SerAddress, SerB256, SerI256, SerU256};

/// Event FAMILY a row was decoded from - never a concrete deployment:
/// every Uniswap V2 fork on every chain is `uniswap_v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Protocol {
    UniswapV2,
    Solidly,
    UniswapV3,
    UniswapV4,
    BalancerV2,
    Curve,
}

impl Protocol {
    pub const ALL: [Protocol; 6] = [
        Protocol::UniswapV2,
        Protocol::Solidly,
        Protocol::UniswapV3,
        Protocol::UniswapV4,
        Protocol::BalancerV2,
        Protocol::Curve,
    ];

    /// Value of the `protocol` columns.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Protocol::UniswapV2 => "uniswap_v2",
            Protocol::Solidly => "solidly",
            Protocol::UniswapV3 => "uniswap_v3",
            Protocol::UniswapV4 => "uniswap_v4",
            Protocol::BalancerV2 => "balancer_v2",
            Protocol::Curve => "curve",
        }
    }

    /// Whether the pool's tokens can only be learnt through `eth_call`
    /// when its creation event was not indexed. V4 and Balancer pools are
    /// described by events of their singleton and are never called.
    pub const fn resolvable_by_rpc(&self) -> bool {
        !matches!(self, Protocol::UniswapV4 | Protocol::BalancerV2)
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Protocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Protocol::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == value)
            .ok_or_else(|| format!("unknown dex protocol {value:?}"))
    }
}

/// Where a `dex_pools` row comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolSource {
    /// Creation event of the factory / singleton.
    Event,
    /// `eth_call`s of the background resolver.
    Rpc,
    /// The resolver asked and the contract is not a pool of the hinted
    /// family (calls revert / return garbage). Kept so it is never asked
    /// again and analytics can tell "checked" from "not checked yet".
    Unresolved,
}

impl PoolSource {
    pub const fn as_str(&self) -> &'static str {
        match self {
            PoolSource::Event => "event",
            PoolSource::Rpc => "rpc",
            PoolSource::Unresolved => "unresolved",
        }
    }
}

impl fmt::Display for PoolSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PoolSource {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [PoolSource::Event, PoolSource::Rpc, PoolSource::Unresolved]
            .into_iter()
            .find(|source| source.as_str() == value)
            .ok_or_else(|| format!("unknown pool source {value:?}"))
    }
}

/// `dex_liquidity.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiquidityKind {
    Mint,
    Burn,
    /// Reserves snapshot (V2 / Solidly `Sync`).
    Sync,
    /// V4 `ModifyLiquidity` (signed liquidity delta, no token amounts).
    Modify,
}

impl LiquidityKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            LiquidityKind::Mint => "mint",
            LiquidityKind::Burn => "burn",
            LiquidityKind::Sync => "sync",
            LiquidityKind::Modify => "modify",
        }
    }
}

impl fmt::Display for LiquidityKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LiquidityKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        [
            LiquidityKind::Mint,
            LiquidityKind::Burn,
            LiquidityKind::Sync,
            LiquidityKind::Modify,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or_else(|| format!("unknown liquidity kind {value:?}"))
    }
}

/// `Array(FixedString(20))`: every address as 20 raw bytes.
pub struct SerVecAddress(());

impl SerializeAs<Vec<Address>> for SerVecAddress {
    fn serialize_as<S>(
        addresses: &Vec<Address>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let raw: Vec<[u8; 20]> =
            addresses.iter().map(|address| address.0 .0).collect();
        raw.serialize(serializer)
    }
}

impl<'de> DeserializeAs<'de, Vec<Address>> for SerVecAddress {
    fn deserialize_as<D>(deserializer: D) -> Result<Vec<Address>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: Vec<[u8; 20]> = Deserialize::deserialize(deserializer)?;
        Ok(raw.into_iter().map(Address::from).collect())
    }
}

/// The pool id of a pool that IS a contract: its address left padded to
/// 32 bytes. (V4 and Balancer pools use their native `bytes32` id.)
pub fn pool_id_of(address: Address) -> B256 {
    address.into_word()
}

/// The contract behind an address-derived pool id, `None` when the upper
/// 12 bytes are not zero (V4 / Balancer ids).
pub fn pool_address_of(pool_id: B256) -> Option<Address> {
    pool_id.0[..12]
        .iter()
        .all(|byte| *byte == 0)
        .then(|| Address::from_word(pool_id))
}

/// `dex_pools._version` of rows written by the RPC resolver: below every
/// event version, so a creation event always wins whatever the insertion
/// order.
pub const POOL_VERSION_RPC: u64 = 1;
/// `dex_pools._version` of [`PoolSource::Unresolved`] rows: loses against
/// everything.
pub const POOL_VERSION_UNRESOLVED: u64 = 0;

/// `dex_pools._version` of a row decoded from a creation event.
///
/// NOT the flush timestamp: it DEcreases with the position of the event
/// so the FIRST creation event of a `(chain, pool_id, emitter)` wins. A
/// contract emitting a forged `PairCreated` for an existing pool can
/// therefore not overwrite its tokens. Re-inserting the same block yields
/// the same version (idempotent).
pub fn pool_event_version(block_number: u64, log_index: u32) -> u64 {
    const LOG_BITS: u32 = 24;
    let log_index = u64::from(log_index).min((1 << LOG_BITS) - 1);
    let block_number = block_number.min((1 << (63 - LOG_BITS)) - 1);

    u64::MAX - ((block_number << LOG_BITS) | log_index)
}

/// `dex_pools`: one row per pool, keyed by `(chain, pool_id, emitter)`.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct DexPool {
    pub chain: u64,
    /// Pool address left padded, or the V4 / Balancer `bytes32` id.
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    /// The contract that emits the pool's swap events: the pool itself,
    /// the V4 PoolManager or the Balancer Vault. Matches
    /// `dex_swaps.emitter`.
    #[serde_as(as = "SerAddress")]
    pub emitter: Address,
    /// Emitter of the creation event (zero when resolved through RPC and
    /// the pool has no `factory()`).
    #[serde_as(as = "SerAddress")]
    pub factory: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// Zero for multi asset pools (see `tokens`). V4: zero = native coin.
    #[serde_as(as = "SerAddress")]
    pub token0: Address,
    #[serde_as(as = "SerAddress")]
    pub token1: Address,
    /// Every token of the pool in pool order (Curve coin index order).
    /// `[token0, token1]` for two token pools.
    #[serde_as(as = "SerVecAddress")]
    pub tokens: Vec<Address>,
    /// Curve only: coins addressed by `TokenExchangeUnderlying`.
    #[serde_as(as = "SerVecAddress")]
    pub underlying_tokens: Vec<Address>,
    /// Hundredths of a bip (V3 / V4 `fee`), 0 when the family has none.
    pub fee: u32,
    pub tick_spacing: i32,
    /// V4 hooks contract.
    #[serde_as(as = "SerAddress")]
    pub hooks: Address,
    /// Solidly: stable (`x3y+y3x`) instead of volatile curve.
    pub stable: bool,
    /// Block of the creation event, 0 when resolved through RPC.
    pub created_block: u64,
    /// Timestamp of the creation event, 0 when resolved through RPC.
    pub timestamp: u32,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub log_index: u32,
    #[serde_as(as = "DisplayFromStr")]
    pub source: PoolSource,
    /// See [`pool_event_version`]: set by the producer, the writer must
    /// NOT stamp it with the flush time.
    pub _version: u64,
}

/// `dex_swaps`: one row per swap event.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct DexSwap {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub log_index: u32,
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    /// Contract that emitted the event (pool, PoolManager or Vault).
    #[serde_as(as = "SerAddress")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    #[serde_as(as = "SerAddress")]
    pub sender: Address,
    /// Zero when the event has none (V4, Balancer, Curve).
    #[serde_as(as = "SerAddress")]
    pub recipient: Address,
    /// `from` of the transaction; zero until
    /// [`super::DexRows::attach_transactions`] ran.
    #[serde_as(as = "SerAddress")]
    pub tx_from: Address,
    /// `to` of the transaction (router / aggregator attribution).
    #[serde_as(as = "SerAddress")]
    pub tx_to: Address,
    /// Best guess of who traded: `tx_from` when known, else `recipient`,
    /// else `sender`. Feeds the unique trader counts.
    #[serde_as(as = "SerAddress")]
    pub trader: Address,
    /// Pool relative, signed: positive = INTO the pool. Zero for the
    /// multi asset families (Balancer, Curve).
    #[serde_as(as = "SerI256")]
    pub amount0: I256,
    #[serde_as(as = "SerI256")]
    pub amount1: I256,
    /// Only when the EVENT carries them (Balancer), else zero: resolved
    /// at query time from `dex_pools`.
    #[serde_as(as = "SerAddress")]
    pub token_in: Address,
    #[serde_as(as = "SerAddress")]
    pub token_out: Address,
    /// Only for the multi asset families (Balancer, Curve), else zero.
    #[serde_as(as = "SerU256")]
    pub amount_in: U256,
    #[serde_as(as = "SerU256")]
    pub amount_out: U256,
    /// Curve coin indices into `dex_pools.tokens` (or
    /// `underlying_tokens` when `underlying`).
    pub coin_in: u8,
    pub coin_out: u8,
    pub underlying: bool,
    /// Price AFTER the swap, 0 when the family has none.
    #[serde_as(as = "SerU256")]
    pub sqrt_price_x96: U256,
    #[serde_as(as = "SerU256")]
    pub liquidity: U256,
    pub tick: i32,
    /// Fee charged by this swap when the event reports it (V4), else 0.
    pub fee: u32,
    pub _version: u64,
}

/// `dex_liquidity`: mint / burn / sync / modify events.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct DexLiquidity {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub log_index: u32,
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    #[serde_as(as = "SerAddress")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    #[serde_as(as = "DisplayFromStr")]
    pub kind: LiquidityKind,
    #[serde_as(as = "SerAddress")]
    pub sender: Address,
    /// Position owner (V3), recipient of the tokens (V2 / Solidly burn),
    /// zero otherwise.
    #[serde_as(as = "SerAddress")]
    pub owner: Address,
    #[serde_as(as = "SerAddress")]
    pub tx_from: Address,
    #[serde_as(as = "SerAddress")]
    pub tx_to: Address,
    /// Pool relative, signed: mint > 0, burn < 0. Zero for sync / modify.
    #[serde_as(as = "SerI256")]
    pub amount0: I256,
    #[serde_as(as = "SerI256")]
    pub amount1: I256,
    /// Reserves after a `sync`, else zero.
    #[serde_as(as = "SerU256")]
    pub reserve0: U256,
    #[serde_as(as = "SerU256")]
    pub reserve1: U256,
    /// Signed change of concentrated liquidity (V3 mint / burn, V4).
    #[serde_as(as = "SerI256")]
    pub liquidity_delta: I256,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub _version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_their_column_value() {
        for protocol in Protocol::ALL {
            assert_eq!(protocol.as_str().parse(), Ok(protocol));
        }
        assert!("uniswap".parse::<Protocol>().is_err());
        assert_eq!("rpc".parse(), Ok(PoolSource::Rpc));
        assert_eq!("modify".parse(), Ok(LiquidityKind::Modify));
    }

    #[test]
    fn only_contract_pools_are_resolvable() {
        assert!(Protocol::UniswapV2.resolvable_by_rpc());
        assert!(Protocol::Curve.resolvable_by_rpc());
        assert!(!Protocol::UniswapV4.resolvable_by_rpc());
        assert!(!Protocol::BalancerV2.resolvable_by_rpc());
    }

    #[test]
    fn pool_ids_round_trip() {
        let address = Address::repeat_byte(0xab);
        assert_eq!(pool_address_of(pool_id_of(address)), Some(address));
        assert_eq!(pool_address_of(B256::repeat_byte(1)), None);
    }

    #[test]
    fn first_creation_event_wins() {
        let first = pool_event_version(100, 5);
        let forged_later = pool_event_version(100, 6);
        let much_later = pool_event_version(20_000_000, 0);

        assert!(first > forged_later);
        assert!(forged_later > much_later);
        assert!(much_later > POOL_VERSION_RPC);
        const { assert!(POOL_VERSION_RPC > POOL_VERSION_UNRESOLVED) };
        // Out of range positions saturate instead of wrapping around.
        assert!(pool_event_version(u64::MAX, u32::MAX) > POOL_VERSION_RPC);
        assert_eq!(pool_event_version(7, 1), pool_event_version(7, 1));
    }

    #[test]
    fn enum_columns_serialize_as_strings() {
        #[serde_as]
        #[derive(Serialize)]
        struct Probe {
            #[serde_as(as = "DisplayFromStr")]
            protocol: Protocol,
            #[serde_as(as = "SerVecAddress")]
            tokens: Vec<Address>,
        }

        let json = serde_json::to_value(Probe {
            protocol: Protocol::BalancerV2,
            tokens: vec![Address::repeat_byte(1)],
        })
        .unwrap();

        assert_eq!(json["protocol"], "balancer_v2");
        assert_eq!(json["tokens"][0].as_array().unwrap().len(), 20);
    }
}
