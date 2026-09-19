//! Rows of the `dex_*` tables (`migrations/0010_dex_tables.sql`).
//!
//! Field order is irrelevant (the clickhouse crate inserts by name), field
//! NAMES must match the columns. Hashes / ids / amounts go through
//! the `crate::utils::format` serializers, which write the binary
//! column types of docs/design.md §1 and §13.
//!
//! The tables are CHAIN NEUTRAL (docs/design.md §13): every identity column
//! is a `FixedString(32)` holding an EVM address left padded with 12 zero
//! bytes, or a 32 byte Solana pubkey. The EVM decoders keep their fields
//! typed [`Address`] and let [`SerId32`] / [`SerVecId32`] do the padding -
//! nothing here hand rolls it, and reading a row whose padding is NOT zero
//! fails loudly instead of truncating a pubkey into an address. Fields that
//! are natively 32 bytes (`pool_id`: a V4 / Balancer id is not an address)
//! stay [`B256`] with `SerB256`.
//!
//! Position is `(chain, block_number, tx_index, ordinal)` in every table:
//! `tx_index` is the EVM transaction index, `ordinal` the log index.
//! `tx_id` is the raw transaction hash in a `String` column, because a
//! Solana signature is 64 bytes.

use std::{fmt, str::FromStr};

use alloy::primitives::{Address, Bytes, B256, I256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

use crate::utils::format::{
    address_of_id32, id32, SerB256, SerI256, SerId32, SerTxId, SerU256,
    SerVecId32,
};

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

    /// Families whose pools are their own contract: the pool answers
    /// `token0()` / `coins(i)` itself, which is the only thing that can
    /// not be forged by a third party's event. V4 and Balancer pools live
    /// inside a singleton and are described by its events only.
    pub const fn resolvable_by_rpc(&self) -> bool {
        !matches!(self, Protocol::UniswapV4 | Protocol::BalancerV2)
    }

    /// The emitter is a singleton shared by every pool of the family.
    pub const fn is_singleton(&self) -> bool {
        !self.resolvable_by_rpc()
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
    /// The resolver got no usable answer (no code at the address, or the
    /// RPC kept failing for this one contract). Persisted so dead emitters
    /// can not fill the backfill pages for ever; asked again with an
    /// exponential backoff on `attempts`.
    NoAnswer,
}

impl PoolSource {
    pub const fn as_str(&self) -> &'static str {
        match self {
            PoolSource::Event => "event",
            PoolSource::Rpc => "rpc",
            PoolSource::Unresolved => "unresolved",
            PoolSource::NoAnswer => "no_answer",
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
        [
            PoolSource::Event,
            PoolSource::Rpc,
            PoolSource::Unresolved,
            PoolSource::NoAnswer,
        ]
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

/// The pool id of a pool that IS a contract: its address left padded to
/// 32 bytes, the same encoding every identity column uses. (V4 and Balancer
/// pools use their native `bytes32` id.)
pub fn pool_id_of(address: Address) -> B256 {
    id32(address)
}

/// The contract behind an address-derived pool id, `None` when the upper
/// 12 bytes are not zero (V4 / Balancer ids, Solana pubkeys).
pub fn pool_address_of(pool_id: B256) -> Option<Address> {
    address_of_id32(pool_id)
}

/// `dex_pools`: one row per CREATION EVENT of a pool, plus at most one row
/// written by the RPC resolver (`created_block = 0`, `tx_index = 0`,
/// `ordinal = 0`).
///
/// The table is positional like every block scoped table - key `(chain,
/// pool_id, emitter, created_block, tx_index, ordinal)` - so versions,
/// tombstones and re-inserts work exactly as everywhere else
/// (docs/design.md §2).
/// Several live rows of one pool can exist (a forged `PairCreated` costs
/// one transaction); readers pick THE row through the `dex_pool_current_v`
/// view: event rows before resolver rows, then the earliest position. The
/// first creation event wins.
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
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    /// Emitter of the creation event (zero when resolved through RPC and
    /// the pool has no `factory()`).
    #[serde_as(as = "SerId32")]
    pub factory: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    /// Zero for multi asset pools (see `tokens`). V4: zero = native coin.
    #[serde_as(as = "SerId32")]
    pub token0: Address,
    #[serde_as(as = "SerId32")]
    pub token1: Address,
    /// Every token of the pool in pool order (Curve coin index order).
    /// `[token0, token1]` for two token pools.
    #[serde_as(as = "SerVecId32")]
    pub tokens: Vec<Address>,
    /// Curve only: coins addressed by `TokenExchangeUnderlying`.
    #[serde_as(as = "SerVecId32")]
    pub underlying_tokens: Vec<Address>,
    /// Hundredths of a bip (V3 / V4 `fee`), 0 when the family has none.
    pub fee: u32,
    pub tick_spacing: i32,
    /// V4 hooks contract.
    #[serde_as(as = "SerId32")]
    pub hooks: Address,
    /// Solidly: stable (`x3y+y3x`) instead of volatile curve.
    pub stable: bool,
    /// Block of the creation event, 0 when resolved through RPC.
    pub created_block: u64,
    /// Timestamp of the creation event, 0 when resolved through RPC.
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    /// Index of the creation event's transaction inside the block.
    pub tx_index: u32,
    /// Position inside the transaction: the log index on EVM.
    pub ordinal: u64,
    #[serde_as(as = "DisplayFromStr")]
    pub source: PoolSource,
    /// Resolver rows: how often the pool was asked without an answer.
    pub attempts: u32,
    /// Purge generation of the chain, stamped by the writer.
    pub epoch: u32,
    pub _version: u64,
}

/// `dex_swaps`: one row per swap event.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct DexSwap {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    /// Index of the swap's transaction inside the block.
    pub tx_index: u32,
    /// Position inside the transaction: the log index on EVM.
    pub ordinal: u64,
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    /// Contract that emitted the event (pool, PoolManager or Vault).
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    #[serde_as(as = "SerId32")]
    pub sender: Address,
    /// Zero when the event has none (V4, Balancer, Curve).
    #[serde_as(as = "SerId32")]
    pub recipient: Address,
    /// `from` of the transaction; zero until
    /// [`super::DexRows::attach_transactions`] ran.
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    /// `to` of the transaction (router / aggregator attribution).
    #[serde_as(as = "SerId32")]
    pub tx_to: Address,
    /// Best guess of who traded: `tx_from` when known, else `recipient`,
    /// else `sender`. Feeds the unique trader counts.
    #[serde_as(as = "SerId32")]
    pub trader: Address,
    /// Pool relative, signed: positive = INTO the pool. Zero for the
    /// multi asset families (Balancer, Curve).
    #[serde_as(as = "SerI256")]
    pub amount0: I256,
    #[serde_as(as = "SerI256")]
    pub amount1: I256,
    /// Only when the EVENT carries them (Balancer), else zero. A CLAIM of
    /// the emitter, nothing more: see `verified_in` / `verified_out`.
    #[serde_as(as = "SerId32")]
    pub token_in: Address,
    #[serde_as(as = "SerId32")]
    pub token_out: Address,
    /// What went into / came out of the pool, every family. Two token
    /// families: the positive / negative side of `amount0`, `amount1`;
    /// both zero when the signs do not describe a swap (same sign, or a
    /// zero side).
    #[serde_as(as = "SerU256")]
    pub amount_in: U256,
    #[serde_as(as = "SerU256")]
    pub amount_out: U256,
    /// The token PROVEN to have moved: an ERC-20 `Transfer` of exactly
    /// `amount_in` to the emitter (`amount_out` from the emitter) earlier
    /// in the same transaction, emitted by this token contract. Zero when
    /// nothing proves the leg (see `dex::corroborate`). USD valuation only
    /// ever uses these.
    #[serde_as(as = "SerId32")]
    pub verified_in: Address,
    #[serde_as(as = "SerId32")]
    pub verified_out: Address,
    /// V2 / Solidly: reserves AFTER the swap, from the `Sync` the pool
    /// emits right before its `Swap`. Zero when there is none.
    #[serde_as(as = "SerU256")]
    pub reserve0: U256,
    #[serde_as(as = "SerU256")]
    pub reserve1: U256,
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
    /// Purge generation of the chain, stamped by the writer. The
    /// aggregates are keyed by it (docs/design.md §2).
    pub epoch: u32,
    pub _version: u64,
}

/// `dex_liquidity`: mint / burn / sync / modify events.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Row, Serialize, Deserialize)]
pub struct DexLiquidity {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    /// Raw transaction id: the 32 bytes of the EVM transaction hash.
    #[serde_as(as = "SerTxId")]
    pub tx_id: Bytes,
    /// Index of the event's transaction inside the block.
    pub tx_index: u32,
    /// Position inside the transaction: the log index on EVM.
    pub ordinal: u64,
    #[serde_as(as = "SerB256")]
    pub pool_id: B256,
    #[serde_as(as = "SerId32")]
    pub emitter: Address,
    #[serde_as(as = "DisplayFromStr")]
    pub protocol: Protocol,
    #[serde_as(as = "DisplayFromStr")]
    pub kind: LiquidityKind,
    #[serde_as(as = "SerId32")]
    pub sender: Address,
    /// Position owner (V3), recipient of the tokens (V2 / Solidly burn),
    /// zero otherwise.
    #[serde_as(as = "SerId32")]
    pub owner: Address,
    /// `from` of the transaction: WHO provided / removed the liquidity.
    /// `sender` is usually a router and must not be used for attribution.
    /// Zero until [`super::DexRows::attach_transactions`] ran.
    #[serde_as(as = "SerId32")]
    pub tx_from: Address,
    #[serde_as(as = "SerId32")]
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
    pub epoch: u32,
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
    fn enum_columns_serialize_as_strings() {
        #[serde_as]
        #[derive(Serialize)]
        struct Probe {
            #[serde_as(as = "DisplayFromStr")]
            protocol: Protocol,
            #[serde_as(as = "SerVecId32")]
            tokens: Vec<Address>,
        }

        let json = serde_json::to_value(Probe {
            protocol: Protocol::BalancerV2,
            tokens: vec![Address::repeat_byte(1)],
        })
        .unwrap();

        assert_eq!(json["protocol"], "balancer_v2");
        // Chain neutral identity: 12 zero bytes + the 20 address bytes.
        let token = json["tokens"][0].as_array().unwrap();
        assert_eq!(token.len(), 32);
        assert!(token[..12].iter().all(|byte| byte.as_u64() == Some(0)));
        assert!(token[12..].iter().all(|byte| byte.as_u64() == Some(1)));
    }

    /// Every identity column of every row is a 32 byte id, and a
    /// transaction id is the raw hash bytes - not a `FixedString(20)`
    /// anywhere (docs/design.md §13).
    #[test]
    fn identity_columns_are_32_bytes_and_tx_ids_are_raw() {
        let swap = DexSwap {
            chain: 1,
            block_number: 7,
            timestamp: 1,
            tx_id: crate::utils::format::tx_id(B256::repeat_byte(0xcd)),
            tx_index: 4,
            ordinal: 9,
            pool_id: pool_id_of(Address::repeat_byte(0xab)),
            emitter: Address::repeat_byte(0xab),
            protocol: Protocol::UniswapV2,
            sender: Address::repeat_byte(1),
            recipient: Address::repeat_byte(2),
            tx_from: Address::repeat_byte(3),
            tx_to: Address::repeat_byte(4),
            trader: Address::repeat_byte(3),
            amount0: I256::ONE,
            amount1: I256::MINUS_ONE,
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            verified_in: Address::repeat_byte(5),
            verified_out: Address::repeat_byte(6),
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            coin_in: 0,
            coin_out: 0,
            underlying: false,
            sqrt_price_x96: U256::ZERO,
            liquidity: U256::ZERO,
            tick: 0,
            fee: 0,
            epoch: 0,
            _version: 0,
        };

        let json = serde_json::to_value(&swap).unwrap();
        for column in [
            "pool_id",
            "emitter",
            "sender",
            "recipient",
            "tx_from",
            "tx_to",
            "trader",
            "token_in",
            "token_out",
            "verified_in",
            "verified_out",
        ] {
            assert_eq!(
                json[column].as_array().map(Vec::len),
                Some(32),
                "{column}"
            );
        }
        assert_eq!(json["tx_id"].as_array().map(Vec::len), Some(32));
        assert_eq!(json["tx_index"], 4);
        assert_eq!(json["ordinal"], 9);

        let back: DexSwap = serde_json::from_value(json).unwrap();
        assert_eq!(back, swap);
    }
}
