//! Logs -> DEX rows. Pure: no I/O, no registry, no panics.
//!
//! A log is decoded when its `topic0` belongs to a family AND its shape
//! (topic count, data length, value ranges of narrow integer types, zero
//! padding of addresses) is the one the family emits. Anything else is
//! ignored silently: plenty of unrelated contracts reuse event names.

use std::{collections::HashMap, sync::OnceLock};

use alloy::primitives::{Address, B256, I256, U256};

use crate::db::models::log::DatabaseLog;

use super::{
    events::{self, EventDef},
    models::{
        pool_id_of, DexLiquidity, DexPool, DexSwap, LiquidityKind,
        PoolSource, Protocol,
    },
    DexRows,
};

/// Most tokens a `TokensRegistered` may carry (Balancer allows 50).
const MAX_POOL_TOKENS: usize = 64;

/// The topics of a log plus how many of them are known to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topics {
    values: [B256; 4],
    count: usize,
    /// `false` when absent topics are stored as zeros: a trailing zero
    /// topic can then not be told from a missing one.
    exact: bool,
}

impl Topics {
    /// Topics stored as `Option` (absent = `None`).
    pub fn from_optional(topics: [Option<B256>; 4]) -> Self {
        let count =
            topics.iter().take_while(|topic| topic.is_some()).count();

        // A hole (`None` followed by `Some`) can not come from a node.
        let count = if topics[count..].iter().any(Option::is_some) {
            0
        } else {
            count
        };

        Self {
            values: topics.map(Option::unwrap_or_default),
            count,
            exact: true,
        }
    }

    /// Topics stored as four non-null columns defaulting to 32 zero bytes
    /// (docs/design.md §1). The count is a lower bound: an indexed zero value
    /// (the V4 native currency...) is indistinguishable from "absent".
    pub fn from_zero_default(topics: [B256; 4]) -> Self {
        let count = topics
            .iter()
            .rposition(|topic| !topic.is_zero())
            .map_or(0, |last| last + 1);

        Self { values: topics, count, exact: false }
    }

    /// Whether the log can have exactly `expected` topics.
    fn matches(&self, expected: u8) -> bool {
        let expected = usize::from(expected);

        if self.count == 0 {
            false
        } else if self.exact {
            self.count == expected
        } else {
            self.count <= expected
        }
    }

    fn get(&self, index: usize) -> B256 {
        self.values.get(index).copied().unwrap_or_default()
    }
}

/// THE place that reads the topic columns of [`DatabaseLog`]. When they
/// become non-`Option` (zero default) this body turns into
/// `Topics::from_zero_default([log.topic0, log.topic1, log.topic2,
/// log.topic3])` and nothing else changes.
fn topics_of(log: &DatabaseLog) -> Topics {
    Topics::from_optional([log.topic0, log.topic1, log.topic2, log.topic3])
}

/// Integer widening that compiles whatever width the log model uses.
fn to_u64<T: Into<u64>>(value: T) -> u64 {
    value.into()
}

fn to_u32<T: Into<u32>>(value: T) -> u32 {
    value.into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    V2PairCreated,
    SolidlyPairCreated,
    SolidlyPoolCreated,
    V3PoolCreated,
    SlipstreamPoolCreated,
    AlgebraPool,
    AlgebraCustomPool,
    V4Initialize,
    BalancerPoolRegistered,
    BalancerTokensRegistered,
    /// V2 shaped swap: sender / to in the topics, four amounts.
    PairSwap(Protocol),
    V3Swap,
    V4Swap,
    BalancerSwap,
    CurveExchange {
        underlying: bool,
    },
    PairSync {
        protocol: Protocol,
        max_bits: usize,
    },
    PairMint,
    V2Burn,
    SolidlyBurn,
    V3Mint,
    V3Burn,
    V4ModifyLiquidity,
}

const TABLE: &[(EventDef, Kind)] = &[
    (events::V2_PAIR_CREATED, Kind::V2PairCreated),
    (events::SOLIDLY_PAIR_CREATED, Kind::SolidlyPairCreated),
    (events::SOLIDLY_POOL_CREATED, Kind::SolidlyPoolCreated),
    (events::V3_POOL_CREATED, Kind::V3PoolCreated),
    (events::SLIPSTREAM_POOL_CREATED, Kind::SlipstreamPoolCreated),
    (events::ALGEBRA_POOL, Kind::AlgebraPool),
    (events::ALGEBRA_CUSTOM_POOL, Kind::AlgebraCustomPool),
    (events::V4_INITIALIZE, Kind::V4Initialize),
    (events::BALANCER_POOL_REGISTERED, Kind::BalancerPoolRegistered),
    (events::BALANCER_TOKENS_REGISTERED, Kind::BalancerTokensRegistered),
    (events::V2_SWAP, Kind::PairSwap(Protocol::UniswapV2)),
    (events::SOLIDLY_SWAP, Kind::PairSwap(Protocol::Solidly)),
    (events::V3_SWAP, Kind::V3Swap),
    (events::PANCAKE_V3_SWAP, Kind::V3Swap),
    (events::ALGEBRA_INTEGRAL_SWAP, Kind::V3Swap),
    (events::V4_SWAP, Kind::V4Swap),
    (events::BALANCER_SWAP, Kind::BalancerSwap),
    (
        events::CURVE_TOKEN_EXCHANGE,
        Kind::CurveExchange { underlying: false },
    ),
    (
        events::CURVE_TOKEN_EXCHANGE_UNDERLYING,
        Kind::CurveExchange { underlying: true },
    ),
    (
        events::CURVE_CRYPTO_TOKEN_EXCHANGE,
        Kind::CurveExchange { underlying: false },
    ),
    (
        events::CURVE_NG_TOKEN_EXCHANGE,
        Kind::CurveExchange { underlying: false },
    ),
    (
        events::V2_SYNC,
        Kind::PairSync { protocol: Protocol::UniswapV2, max_bits: 112 },
    ),
    (
        events::SOLIDLY_SYNC,
        Kind::PairSync { protocol: Protocol::Solidly, max_bits: 256 },
    ),
    (events::V2_MINT, Kind::PairMint),
    (events::V2_BURN, Kind::V2Burn),
    (events::SOLIDLY_BURN, Kind::SolidlyBurn),
    (events::V3_MINT, Kind::V3Mint),
    (events::V3_BURN, Kind::V3Burn),
    (events::V4_MODIFY_LIQUIDITY, Kind::V4ModifyLiquidity),
];

fn lookup(topic0: &B256) -> Option<&'static (EventDef, Kind)> {
    static INDEX: OnceLock<HashMap<B256, &'static (EventDef, Kind)>> =
        OnceLock::new();

    INDEX
        .get_or_init(|| {
            TABLE.iter().map(|entry| (entry.0.topic0, entry)).collect()
        })
        .get(topic0)
        .copied()
}

// ------------------------------------------------------------ word readers

fn word(data: &[u8], index: usize) -> Option<&[u8; 32]> {
    let start = index.checked_mul(32)?;
    let end = start.checked_add(32)?;
    data.get(start..end)?.try_into().ok()
}

/// Unsigned integer of at most `bits` bits.
fn uint(bytes: &[u8; 32], bits: usize) -> Option<U256> {
    let value = U256::from_be_bytes(*bytes);
    (bits >= 256 || (value >> bits).is_zero()).then_some(value)
}

/// Sign extended integer of at most `bits` bits.
fn int(bytes: &[u8; 32], bits: usize) -> Option<I256> {
    let value = I256::from_be_bytes(*bytes);

    if bits >= 256 {
        return Some(value);
    }

    let limit = I256::from_raw(U256::from(1u8) << (bits - 1));
    (value < limit && value >= -limit).then_some(value)
}

fn int24(bytes: &[u8; 32]) -> Option<i32> {
    int(bytes, 24)?;
    Some(i32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]))
}

fn uint24(bytes: &[u8; 32]) -> Option<u32> {
    uint(bytes, 24)?;
    Some(u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]))
}

fn uint8(bytes: &[u8; 32]) -> Option<u8> {
    uint(bytes, 8)?;
    Some(bytes[31])
}

fn boolean(bytes: &[u8; 32]) -> Option<bool> {
    match uint8(bytes)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/// An ABI encoded address: the upper 12 bytes must be zero.
fn address(bytes: &[u8; 32]) -> Option<Address> {
    bytes[..12]
        .iter()
        .all(|byte| *byte == 0)
        .then(|| Address::from_slice(&bytes[12..]))
}

/// An unsigned amount as a signed one; `None` above `Int256` max (no real
/// token amount is, a forged one is not worth a row).
fn signed(value: U256) -> Option<I256> {
    I256::try_from(value).ok()
}

/// `address[]` at the head slot `slot` of an ABI encoded tuple.
fn address_array(data: &[u8], slot: usize) -> Option<Vec<Address>> {
    let offset = usize::try_from(uint(word(data, slot)?, 32)?).ok()?;
    let body = data.get(offset..)?;
    let length = usize::try_from(uint(word(body, 0)?, 32)?).ok()?;

    if length > MAX_POOL_TOKENS {
        return None;
    }

    (1..=length).map(|index| address(word(body, index)?)).collect()
}

// ----------------------------------------------------------------- decoding

struct Event<'a> {
    chain: u64,
    log: &'a DatabaseLog,
    topics: Topics,
    data: &'a [u8],
}

impl Event<'_> {
    fn topic(&self, index: usize) -> [u8; 32] {
        self.topics.get(index).0
    }

    fn topic_address(&self, index: usize) -> Option<Address> {
        address(&self.topic(index))
    }

    fn data(&self, index: usize) -> Option<&[u8; 32]> {
        word(self.data, index)
    }

    fn pool(
        &self,
        protocol: Protocol,
        pool_id: B256,
        emitter: Address,
        tokens: Vec<Address>,
    ) -> DexPool {
        let two = tokens.len() == 2 && protocol != Protocol::BalancerV2;
        let block_number = to_u64(self.log.block_number);
        let log_index = to_u32(self.log.log_index);

        DexPool {
            chain: self.chain,
            pool_id,
            emitter,
            factory: self.log.address,
            protocol,
            token0: if two { tokens[0] } else { Address::ZERO },
            token1: if two { tokens[1] } else { Address::ZERO },
            tokens,
            underlying_tokens: Vec::new(),
            fee: 0,
            tick_spacing: 0,
            hooks: Address::ZERO,
            stable: false,
            created_block: block_number,
            timestamp: self.log.timestamp,
            transaction_hash: self.log.transaction_hash,
            log_index,
            source: PoolSource::Event,
            epoch: 0,
            _version: 0,
        }
    }

    /// A pool that is its own contract, announced by a factory.
    fn contract_pool(
        &self,
        protocol: Protocol,
        pool: Address,
        token0: Address,
        token1: Address,
    ) -> Option<DexPool> {
        // A pool can not be one of its tokens nor the zero address.
        if pool.is_zero() || token0 == token1 {
            return None;
        }

        Some(self.pool(
            protocol,
            pool_id_of(pool),
            pool,
            vec![token0, token1],
        ))
    }

    fn swap(&self, protocol: Protocol, pool_id: B256) -> DexSwap {
        DexSwap {
            chain: self.chain,
            block_number: to_u64(self.log.block_number),
            timestamp: self.log.timestamp,
            transaction_hash: self.log.transaction_hash,
            log_index: to_u32(self.log.log_index),
            pool_id,
            emitter: self.log.address,
            protocol,
            sender: Address::ZERO,
            recipient: Address::ZERO,
            tx_from: Address::ZERO,
            tx_to: Address::ZERO,
            trader: Address::ZERO,
            amount0: I256::ZERO,
            amount1: I256::ZERO,
            token_in: Address::ZERO,
            token_out: Address::ZERO,
            amount_in: U256::ZERO,
            amount_out: U256::ZERO,
            coin_in: 0,
            coin_out: 0,
            underlying: false,
            sqrt_price_x96: U256::ZERO,
            liquidity: U256::ZERO,
            tick: 0,
            fee: 0,
            epoch: 0,
            _version: 0,
        }
    }

    fn liquidity(
        &self,
        protocol: Protocol,
        kind: LiquidityKind,
        pool_id: B256,
    ) -> DexLiquidity {
        DexLiquidity {
            chain: self.chain,
            block_number: to_u64(self.log.block_number),
            timestamp: self.log.timestamp,
            transaction_hash: self.log.transaction_hash,
            log_index: to_u32(self.log.log_index),
            pool_id,
            emitter: self.log.address,
            protocol,
            kind,
            sender: Address::ZERO,
            owner: Address::ZERO,
            tx_from: Address::ZERO,
            tx_to: Address::ZERO,
            amount0: I256::ZERO,
            amount1: I256::ZERO,
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            liquidity_delta: I256::ZERO,
            tick_lower: 0,
            tick_upper: 0,
            epoch: 0,
            _version: 0,
        }
    }

    fn own_pool_id(&self) -> B256 {
        pool_id_of(self.log.address)
    }
}

enum Decoded {
    Pool(DexPool),
    /// Tokens of a Balancer pool, merged into its `PoolRegistered` row.
    PoolTokens(DexPool),
    Swap(DexSwap),
    Liquidity(DexLiquidity),
}

fn finish_swap(mut swap: DexSwap) -> Decoded {
    swap.trader = if swap.recipient.is_zero() {
        swap.sender
    } else {
        swap.recipient
    };
    Decoded::Swap(swap)
}

fn decode_event(event: &Event<'_>, kind: Kind) -> Option<Decoded> {
    match kind {
        Kind::V2PairCreated => event
            .contract_pool(
                Protocol::UniswapV2,
                address(event.data(0)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )
            .map(Decoded::Pool),

        Kind::SolidlyPairCreated => {
            let mut pool = event.contract_pool(
                Protocol::Solidly,
                address(event.data(1)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )?;
            pool.stable = boolean(event.data(0)?)?;
            Some(Decoded::Pool(pool))
        }

        Kind::SolidlyPoolCreated => {
            let mut pool = event.contract_pool(
                Protocol::Solidly,
                address(event.data(0)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )?;
            pool.stable = boolean(&event.topic(3))?;
            Some(Decoded::Pool(pool))
        }

        Kind::V3PoolCreated => {
            let mut pool = event.contract_pool(
                Protocol::UniswapV3,
                address(event.data(1)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )?;
            pool.fee = uint24(&event.topic(3))?;
            pool.tick_spacing = int24(event.data(0)?)?;
            Some(Decoded::Pool(pool))
        }

        Kind::SlipstreamPoolCreated => {
            let mut pool = event.contract_pool(
                Protocol::UniswapV3,
                address(event.data(0)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )?;
            pool.tick_spacing = int24(&event.topic(3))?;
            Some(Decoded::Pool(pool))
        }

        Kind::AlgebraPool => event
            .contract_pool(
                Protocol::UniswapV3,
                address(event.data(0)?)?,
                event.topic_address(1)?,
                event.topic_address(2)?,
            )
            .map(Decoded::Pool),

        Kind::AlgebraCustomPool => {
            event.topic_address(1)?;
            event
                .contract_pool(
                    Protocol::UniswapV3,
                    address(event.data(0)?)?,
                    event.topic_address(2)?,
                    event.topic_address(3)?,
                )
                .map(Decoded::Pool)
        }

        Kind::V4Initialize => {
            let currency0 = event.topic_address(2)?;
            let currency1 = event.topic_address(3)?;

            // The PoolManager enforces currency0 < currency1.
            if currency0 >= currency1 {
                return None;
            }

            let mut pool = event.pool(
                Protocol::UniswapV4,
                B256::from(event.topic(1)),
                event.log.address,
                vec![currency0, currency1],
            );
            pool.fee = uint24(event.data(0)?)?;
            pool.tick_spacing = int24(event.data(1)?)?;
            pool.hooks = address(event.data(2)?)?;
            uint(event.data(3)?, 160)?;
            int24(event.data(4)?)?;
            Some(Decoded::Pool(pool))
        }

        Kind::BalancerPoolRegistered => {
            let pool_id = B256::from(event.topic(1));
            let pool_address = event.topic_address(2)?;

            // A Balancer pool id starts with the pool's address.
            if pool_address.is_zero()
                || pool_id.0[..20] != pool_address.0 .0
                || uint8(event.data(0)?)? > 2
            {
                return None;
            }

            Some(Decoded::Pool(event.pool(
                Protocol::BalancerV2,
                pool_id,
                event.log.address,
                Vec::new(),
            )))
        }

        Kind::BalancerTokensRegistered => {
            let tokens = address_array(event.data, 0)?;
            let managers = address_array(event.data, 1)?;

            if tokens.is_empty() || tokens.len() != managers.len() {
                return None;
            }

            Some(Decoded::PoolTokens(event.pool(
                Protocol::BalancerV2,
                B256::from(event.topic(1)),
                event.log.address,
                tokens,
            )))
        }

        Kind::PairSwap(protocol) => {
            let amount0_in = signed(uint(event.data(0)?, 256)?)?;
            let amount1_in = signed(uint(event.data(1)?, 256)?)?;
            let amount0_out = signed(uint(event.data(2)?, 256)?)?;
            let amount1_out = signed(uint(event.data(3)?, 256)?)?;

            let mut swap = event.swap(protocol, event.own_pool_id());
            swap.sender = event.topic_address(1)?;
            swap.recipient = event.topic_address(2)?;
            swap.amount0 = amount0_in.checked_sub(amount0_out)?;
            swap.amount1 = amount1_in.checked_sub(amount1_out)?;
            Some(finish_swap(swap))
        }

        Kind::V3Swap => {
            let mut swap =
                event.swap(Protocol::UniswapV3, event.own_pool_id());
            swap.sender = event.topic_address(1)?;
            swap.recipient = event.topic_address(2)?;
            swap.amount0 = int(event.data(0)?, 256)?;
            swap.amount1 = int(event.data(1)?, 256)?;
            swap.sqrt_price_x96 = uint(event.data(2)?, 160)?;
            swap.liquidity = uint(event.data(3)?, 128)?;
            swap.tick = int24(event.data(4)?)?;
            Some(finish_swap(swap))
        }

        Kind::V4Swap => {
            let mut swap = event
                .swap(Protocol::UniswapV4, B256::from(event.topic(1)));
            swap.sender = event.topic_address(2)?;
            // V4 reports the CALLER's deltas (negative = paid in).
            swap.amount0 = int(event.data(0)?, 128)?.checked_neg()?;
            swap.amount1 = int(event.data(1)?, 128)?.checked_neg()?;
            swap.sqrt_price_x96 = uint(event.data(2)?, 160)?;
            swap.liquidity = uint(event.data(3)?, 128)?;
            swap.tick = int24(event.data(4)?)?;
            swap.fee = uint24(event.data(5)?)?;
            Some(finish_swap(swap))
        }

        Kind::BalancerSwap => {
            let mut swap = event
                .swap(Protocol::BalancerV2, B256::from(event.topic(1)));
            swap.token_in = event.topic_address(2)?;
            swap.token_out = event.topic_address(3)?;
            swap.amount_in = uint(event.data(0)?, 256)?;
            swap.amount_out = uint(event.data(1)?, 256)?;

            if swap.token_in == swap.token_out {
                return None;
            }

            Some(finish_swap(swap))
        }

        Kind::CurveExchange { underlying } => {
            let mut swap =
                event.swap(Protocol::Curve, event.own_pool_id());
            swap.sender = event.topic_address(1)?;
            // `int128` and `uint256` indices share this encoding for
            // every valid (small, non negative) value.
            swap.coin_in = uint8(event.data(0)?)?;
            swap.amount_in = uint(event.data(1)?, 256)?;
            swap.coin_out = uint8(event.data(2)?)?;
            swap.amount_out = uint(event.data(3)?, 256)?;
            swap.underlying = underlying;

            if swap.coin_in == swap.coin_out {
                return None;
            }

            Some(finish_swap(swap))
        }

        Kind::PairSync { protocol, max_bits } => {
            let mut row = event.liquidity(
                protocol,
                LiquidityKind::Sync,
                event.own_pool_id(),
            );
            row.reserve0 = uint(event.data(0)?, max_bits)?;
            row.reserve1 = uint(event.data(1)?, max_bits)?;
            Some(Decoded::Liquidity(row))
        }

        Kind::PairMint => {
            let mut row = event.liquidity(
                Protocol::UniswapV2,
                LiquidityKind::Mint,
                event.own_pool_id(),
            );
            row.sender = event.topic_address(1)?;
            row.amount0 = signed(uint(event.data(0)?, 256)?)?;
            row.amount1 = signed(uint(event.data(1)?, 256)?)?;
            Some(Decoded::Liquidity(row))
        }

        Kind::V2Burn | Kind::SolidlyBurn => {
            let protocol = if kind == Kind::V2Burn {
                Protocol::UniswapV2
            } else {
                Protocol::Solidly
            };

            let mut row = event.liquidity(
                protocol,
                LiquidityKind::Burn,
                event.own_pool_id(),
            );
            row.sender = event.topic_address(1)?;
            row.owner = event.topic_address(2)?;
            row.amount0 =
                signed(uint(event.data(0)?, 256)?)?.checked_neg()?;
            row.amount1 =
                signed(uint(event.data(1)?, 256)?)?.checked_neg()?;
            Some(Decoded::Liquidity(row))
        }

        Kind::V3Mint => {
            let mut row = event.liquidity(
                Protocol::UniswapV3,
                LiquidityKind::Mint,
                event.own_pool_id(),
            );
            row.owner = event.topic_address(1)?;
            row.tick_lower = int24(&event.topic(2))?;
            row.tick_upper = int24(&event.topic(3))?;
            row.sender = address(event.data(0)?)?;
            row.liquidity_delta = signed(uint(event.data(1)?, 128)?)?;
            row.amount0 = signed(uint(event.data(2)?, 256)?)?;
            row.amount1 = signed(uint(event.data(3)?, 256)?)?;
            Some(Decoded::Liquidity(row))
        }

        Kind::V3Burn => {
            let mut row = event.liquidity(
                Protocol::UniswapV3,
                LiquidityKind::Burn,
                event.own_pool_id(),
            );
            row.owner = event.topic_address(1)?;
            row.sender = row.owner;
            row.tick_lower = int24(&event.topic(2))?;
            row.tick_upper = int24(&event.topic(3))?;
            row.liquidity_delta =
                signed(uint(event.data(0)?, 128)?)?.checked_neg()?;
            row.amount0 =
                signed(uint(event.data(1)?, 256)?)?.checked_neg()?;
            row.amount1 =
                signed(uint(event.data(2)?, 256)?)?.checked_neg()?;
            Some(Decoded::Liquidity(row))
        }

        Kind::V4ModifyLiquidity => {
            let mut row = event.liquidity(
                Protocol::UniswapV4,
                LiquidityKind::Modify,
                B256::from(event.topic(1)),
            );
            row.sender = event.topic_address(2)?;
            row.owner = row.sender;
            row.tick_lower = int24(event.data(0)?)?;
            row.tick_upper = int24(event.data(1)?)?;
            row.liquidity_delta = int(event.data(2)?, 256)?;
            Some(Decoded::Liquidity(row))
        }
    }
}

/// Decodes every DEX event of `logs` (any order, any mix of contracts).
///
/// Rows come out in input order with `_version = 0` and `epoch = 0`: stamp
/// them with [`DexRows::set_version`] and [`DexRows::set_epoch`].
pub fn decode(chain: u64, logs: &[DatabaseLog]) -> DexRows {
    let mut rows = DexRows::default();
    // Balancer pools registered in this batch, by (pool id, vault).
    let mut registered: HashMap<(B256, Address), usize> = HashMap::new();

    for log in logs {
        let topics = topics_of(log);

        if topics.count == 0 {
            continue;
        }

        let Some((definition, kind)) = lookup(&topics.get(0)) else {
            continue;
        };

        if !topics.matches(definition.topics) {
            continue;
        }

        let data: &[u8] = log.data.as_ref();

        if definition.data_len.is_some_and(|length| data.len() != length) {
            continue;
        }

        let event = Event { chain, log, topics, data };

        match decode_event(&event, *kind) {
            Some(Decoded::Pool(pool)) => {
                if pool.protocol == Protocol::BalancerV2 {
                    registered.insert(
                        (pool.pool_id, pool.emitter),
                        rows.pools.len(),
                    );
                }
                rows.pools.push(pool);
            }
            Some(Decoded::PoolTokens(pool)) => {
                match registered.get(&(pool.pool_id, pool.emitter)) {
                    Some(&index) => rows.pools[index].tokens = pool.tokens,
                    // Registered in an earlier block: the row keeps the
                    // position of this event and loses against the
                    // `PoolRegistered` one (known gap, see README).
                    None => rows.pools.push(pool),
                }
            }
            Some(Decoded::Swap(swap)) => rows.swaps.push(swap),
            Some(Decoded::Liquidity(row)) => rows.liquidity.push(row),
            None => {}
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::{
        address_topic, log_with, word as word_of,
    };

    #[test]
    fn every_event_is_dispatched_once() {
        assert_eq!(TABLE.len(), events::ALL.len());

        for definition in events::ALL {
            let (found, _) = lookup(&definition.topic0).unwrap();
            assert_eq!(found, definition);
        }
    }

    #[test]
    fn optional_topics_are_counted_exactly() {
        let one = B256::repeat_byte(1);

        let topics =
            Topics::from_optional([Some(one), Some(one), None, None]);
        assert!(topics.matches(2));
        assert!(!topics.matches(3));
        assert!(!topics.matches(1));

        // A hole is malformed.
        let topics =
            Topics::from_optional([Some(one), None, Some(one), None]);
        assert!(!topics.matches(3));
        assert!(!topics.matches(1));

        assert!(!Topics::from_optional([None; 4]).matches(0));
    }

    #[test]
    fn zero_default_topics_are_a_lower_bound() {
        let one = B256::repeat_byte(1);

        // V4 Initialize with the native currency: topic2 is all zero.
        let topics =
            Topics::from_zero_default([one, one, B256::ZERO, B256::ZERO]);
        assert!(topics.matches(4));
        assert!(topics.matches(2));
        assert!(!topics.matches(1));

        assert!(!Topics::from_zero_default([B256::ZERO; 4]).matches(1));
    }

    #[test]
    fn narrow_integers_are_range_checked() {
        let minus_one = [0xffu8; 32];
        assert_eq!(int24(&minus_one), Some(-1));
        assert_eq!(uint24(&minus_one), None);
        assert_eq!(uint8(&minus_one), None);

        let mut too_wide = [0u8; 32];
        too_wide[28] = 1; // 2^24
        assert_eq!(int24(&too_wide), None);
        assert_eq!(uint24(&too_wide), None);

        let mut max = [0u8; 32];
        max[29..].copy_from_slice(&[0x7f, 0xff, 0xff]);
        assert_eq!(int24(&max), Some(8_388_607));

        let mut min = [0xffu8; 32];
        min[29..].copy_from_slice(&[0x80, 0x00, 0x00]);
        assert_eq!(int24(&min), Some(-8_388_608));
        min[29] = 0x7f;
        assert_eq!(int24(&min), None);

        let mut dirty = [0u8; 32];
        dirty[0] = 1;
        assert_eq!(address(&dirty), None);
        assert_eq!(boolean(&word_of(2).try_into().unwrap()), None);
    }

    #[test]
    fn wrong_shapes_are_ignored() {
        let sender = address_topic(1);
        let to = address_topic(2);
        let good: Vec<u8> =
            [word_of(5), word_of(0), word_of(0), word_of(9)].concat();

        let decoded = decode(
            1,
            &[log_with(
                &[events::V2_SWAP.topic0, sender, to],
                good.clone(),
            )],
        );
        assert_eq!(decoded.swaps.len(), 1);

        let bad = [
            // Missing topic.
            log_with(&[events::V2_SWAP.topic0, sender], good.clone()),
            // Extra topic.
            log_with(
                &[events::V2_SWAP.topic0, sender, to, to],
                good.clone(),
            ),
            // Short / long / empty data.
            log_with(
                &[events::V2_SWAP.topic0, sender, to],
                good[..96].into(),
            ),
            log_with(
                &[events::V2_SWAP.topic0, sender, to],
                [good.clone(), word_of(1)].concat(),
            ),
            log_with(&[events::V2_SWAP.topic0, sender, to], Vec::new()),
            // Dirty address topic.
            log_with(
                &[events::V2_SWAP.topic0, B256::repeat_byte(0xff), to],
                good.clone(),
            ),
            // Amount above Int256 max.
            log_with(
                &[events::V2_SWAP.topic0, sender, to],
                [vec![0xff; 32], word_of(0), word_of(0), word_of(9)]
                    .concat(),
            ),
            // Unknown topic0 / no topics at all.
            log_with(&[B256::repeat_byte(3), sender, to], good.clone()),
            log_with(&[], good),
        ];

        assert!(decode(1, &bad).is_empty());
    }

    #[test]
    fn garbage_never_panics() {
        // Every known topic0 with every topic count and odd data sizes.
        let mut logs = Vec::new();

        for definition in events::ALL {
            for topic_count in 0..4 {
                for length in
                    [0usize, 1, 31, 32, 33, 64, 95, 160, 224, 512]
                {
                    for fill in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
                        let mut topics = vec![definition.topic0];
                        topics.extend(std::iter::repeat_n(
                            B256::repeat_byte(fill),
                            topic_count,
                        ));
                        logs.push(log_with(&topics, vec![fill; length]));
                    }
                }
            }
        }

        let _ = decode(1, &logs);
    }

    #[test]
    fn dynamic_arrays_are_bounds_checked() {
        // Offset far outside the data.
        let data = [vec![0xffu8; 32], word_of(64)].concat();
        assert_eq!(address_array(&data, 0), None);

        // Length larger than the data.
        let data =
            [word_of(64), word_of(64), word_of(3), word_of(1)].concat();
        assert_eq!(address_array(&data, 0), None);

        // Absurd length.
        let data = [word_of(32), word_of(1_000_000)].concat();
        assert_eq!(address_array(&data, 0), None);

        let data = [word_of(32), word_of(1), word_of(7)].concat();
        assert_eq!(
            address_array(&data, 0),
            Some(vec![Address::from_word(B256::from(U256::from(7u8)))])
        );
    }
}
