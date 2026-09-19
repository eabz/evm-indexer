//! Pool metadata over `eth_call`, for pools whose creation event was not
//! indexed (partial sync) or does not exist (Curve).
//!
//! Only DEFINITIVE answers become rows:
//!
//! * the getters answer -> [`Resolution::Resolved`] (`source = 'rpc'`),
//! * the EVM reverts / returns garbage -> [`Resolution::NotAPool`]
//!   (`source = 'unresolved'`, the persistent negative cache),
//! * the node could not be reached -> [`Resolution::Retry`], never cached,
//! * empty return data -> [`Resolution::NoAnswer`]: an address without code
//!   answers like this, and the node may simply lag behind the indexed
//!   head, so nothing is concluded.

use alloy::primitives::{Address, Bytes, B256, U256};

use crate::tokens::multicall::{CallError, EthCaller};

use super::{
    models::{DexPool, PoolSource, Protocol},
    PoolCandidate,
};

/// A getter: canonical signature and its 4 byte selector (asserted against
/// keccak in the tests).
#[derive(Debug, Clone, Copy)]
pub struct Getter {
    pub signature: &'static str,
    pub selector: [u8; 4],
}

pub const TOKEN0: Getter =
    Getter { signature: "token0()", selector: [0x0d, 0xfe, 0x16, 0x81] };
pub const TOKEN1: Getter =
    Getter { signature: "token1()", selector: [0xd2, 0x12, 0x20, 0xa7] };
pub const FEE: Getter =
    Getter { signature: "fee()", selector: [0xdd, 0xca, 0x3f, 0x43] };
pub const TICK_SPACING: Getter = Getter {
    signature: "tickSpacing()",
    selector: [0xd0, 0xc9, 0x3a, 0x7c],
};
pub const FACTORY: Getter =
    Getter { signature: "factory()", selector: [0xc4, 0x5a, 0x01, 0x55] };
pub const STABLE: Getter =
    Getter { signature: "stable()", selector: [0x22, 0xbe, 0x3d, 0xe1] };
pub const COINS_UINT: Getter = Getter {
    signature: "coins(uint256)",
    selector: [0xc6, 0x61, 0x06, 0x57],
};
pub const COINS_INT: Getter = Getter {
    signature: "coins(int128)",
    selector: [0x23, 0x74, 0x6e, 0xb8],
};
pub const UNDERLYING_COINS_UINT: Getter = Getter {
    signature: "underlying_coins(uint256)",
    selector: [0xb9, 0x94, 0x7e, 0xb0],
};
pub const UNDERLYING_COINS_INT: Getter = Getter {
    signature: "underlying_coins(int128)",
    selector: [0xb7, 0x39, 0x95, 0x3e],
};
pub const BASE_POOL: Getter = Getter {
    signature: "base_pool()",
    selector: [0x5d, 0x63, 0x62, 0xbb],
};
pub const BASE_POOL_CONSTANT: Getter = Getter {
    signature: "BASE_POOL()",
    selector: [0x71, 0x51, 0x1a, 0x5e],
};

pub const GETTERS: &[Getter] = &[
    TOKEN0,
    TOKEN1,
    FEE,
    TICK_SPACING,
    FACTORY,
    STABLE,
    COINS_UINT,
    COINS_INT,
    UNDERLYING_COINS_UINT,
    UNDERLYING_COINS_INT,
    BASE_POOL,
    BASE_POOL_CONSTANT,
];

/// Most coins asked of a Curve pool (the largest pools have 8).
pub const MAX_CURVE_COINS: u64 = 8;

/// Outcome of resolving one pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(Box<DexPool>),
    /// Definitive: the contract is not a pool of the hinted family.
    NotAPool,
    /// No code at the address (yet?). Not cached persistently.
    NoAnswer,
    /// Transient RPC failure. Never cached.
    Retry(String),
}

/// One getter call.
enum Answer {
    Word([u8; 32]),
    /// Reverted, or returned something that is not one ABI word.
    Refused,
    Empty,
    Transient(String),
}

/// Early exit of a resolution.
enum Stop {
    NotAPool,
    NoAnswer,
    Retry(String),
}

async fn ask(
    caller: &dyn EthCaller,
    to: Address,
    getter: Getter,
    argument: Option<u64>,
) -> Answer {
    let mut calldata = getter.selector.to_vec();
    if let Some(argument) = argument {
        calldata
            .extend_from_slice(&U256::from(argument).to_be_bytes::<32>());
    }

    match caller.call(to, Bytes::from(calldata)).await {
        Ok(data) if data.is_empty() => Answer::Empty,
        Ok(data) => match <[u8; 32]>::try_from(data.as_ref()) {
            Ok(word) => Answer::Word(word),
            Err(_) => Answer::Refused,
        },
        Err(CallError::Execution(_)) => Answer::Refused,
        Err(CallError::Transient(error)) => Answer::Transient(error),
    }
}

fn word_address(word: &[u8; 32]) -> Option<Address> {
    word[..12]
        .iter()
        .all(|byte| *byte == 0)
        .then(|| Address::from_slice(&word[12..]))
}

/// A getter the family MUST have.
async fn required_address(
    caller: &dyn EthCaller,
    to: Address,
    getter: Getter,
) -> Result<Address, Stop> {
    match ask(caller, to, getter, None).await {
        Answer::Word(word) => word_address(&word).ok_or(Stop::NotAPool),
        Answer::Refused => Err(Stop::NotAPool),
        Answer::Empty => Err(Stop::NoAnswer),
        Answer::Transient(error) => Err(Stop::Retry(error)),
    }
}

/// A getter only some members of the family have: `None` when refused.
async fn optional_word(
    caller: &dyn EthCaller,
    to: Address,
    getter: Getter,
) -> Result<Option<[u8; 32]>, Stop> {
    match ask(caller, to, getter, None).await {
        Answer::Word(word) => Ok(Some(word)),
        Answer::Refused | Answer::Empty => Ok(None),
        Answer::Transient(error) => Err(Stop::Retry(error)),
    }
}

fn small_uint(word: &[u8; 32], bytes: usize) -> Option<u32> {
    word[..32 - bytes].iter().all(|byte| *byte == 0).then(|| {
        u32::from_be_bytes([word[28], word[29], word[30], word[31]])
    })
}

fn int24(word: &[u8; 32]) -> Option<i32> {
    let value =
        i32::from_be_bytes([word[28], word[29], word[30], word[31]]);
    let extension = if value < 0 { 0xff } else { 0x00 };

    (word[..28].iter().all(|byte| *byte == extension)
        && (-(1 << 23)..(1 << 23)).contains(&value))
    .then_some(value)
}

/// The coins of a Curve style array getter, trying the `uint256` index
/// first and the `int128` one (old pools) second. Empty when the contract
/// has neither getter.
async fn coin_list(
    caller: &dyn EthCaller,
    pool: Address,
    getters: [Getter; 2],
) -> Result<Vec<Address>, Stop> {
    for getter in getters {
        let mut coins = Vec::new();

        for index in 0..MAX_CURVE_COINS {
            match ask(caller, pool, getter, Some(index)).await {
                Answer::Word(word) => match word_address(&word) {
                    Some(coin) if !coin.is_zero() => coins.push(coin),
                    // Unused slots of fixed size arrays are zero.
                    _ => break,
                },
                Answer::Refused => break,
                Answer::Empty if index == 0 => return Err(Stop::NoAnswer),
                Answer::Empty => break,
                Answer::Transient(error) => {
                    return Err(Stop::Retry(error))
                }
            }
        }

        if !coins.is_empty() {
            return Ok(coins);
        }
    }

    Ok(Vec::new())
}

async fn resolve_pair(
    caller: &dyn EthCaller,
    pool: &mut DexPool,
) -> Result<(), Stop> {
    let address = pool.emitter;
    let token0 = required_address(caller, address, TOKEN0).await?;
    let token1 = required_address(caller, address, TOKEN1).await?;

    if token0 == token1 {
        return Err(Stop::NotAPool);
    }

    pool.token0 = token0;
    pool.token1 = token1;
    pool.tokens = vec![token0, token1];

    if let Some(factory) = optional_word(caller, address, FACTORY).await? {
        pool.factory = word_address(&factory).unwrap_or_default();
    }

    match pool.protocol {
        Protocol::UniswapV3 => {
            // Algebra pools have no `fee()`: it stays 0.
            if let Some(fee) = optional_word(caller, address, FEE).await? {
                pool.fee = small_uint(&fee, 3).unwrap_or_default();
            }
            if let Some(spacing) =
                optional_word(caller, address, TICK_SPACING).await?
            {
                pool.tick_spacing = int24(&spacing).unwrap_or_default();
            }
        }
        _ => {
            // Solidly V1 pools emit the V2 `Swap`: `stable()` tells them
            // apart from real V2 pairs.
            if let Some(stable) =
                optional_word(caller, address, STABLE).await?
            {
                if let Some(flag) = small_uint(&stable, 1) {
                    if flag <= 1 {
                        pool.protocol = Protocol::Solidly;
                        pool.stable = flag == 1;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn resolve_curve(
    caller: &dyn EthCaller,
    pool: &mut DexPool,
) -> Result<(), Stop> {
    let address = pool.emitter;
    let coins =
        coin_list(caller, address, [COINS_UINT, COINS_INT]).await?;

    if coins.len() < 2 {
        return Err(Stop::NotAPool);
    }

    // Lending pools expose their underlying coins directly.
    let mut underlying = coin_list(
        caller,
        address,
        [UNDERLYING_COINS_UINT, UNDERLYING_COINS_INT],
    )
    .await
    .or_else(|stop| match stop {
        Stop::Retry(error) => Err(Stop::Retry(error)),
        _ => Ok(Vec::new()),
    })?;

    // Metapools: [meta coin, coins of the base pool...].
    if underlying.is_empty() {
        for getter in [BASE_POOL, BASE_POOL_CONSTANT] {
            let Some(word) =
                optional_word(caller, address, getter).await?
            else {
                continue;
            };

            let Some(base) = word_address(&word).filter(|a| !a.is_zero())
            else {
                continue;
            };

            let base_coins =
                coin_list(caller, base, [COINS_UINT, COINS_INT])
                    .await
                    .or_else(|stop| match stop {
                        Stop::Retry(error) => Err(Stop::Retry(error)),
                        _ => Ok(Vec::new()),
                    })?;

            if !base_coins.is_empty() {
                underlying = coins[..coins.len() - 1].to_vec();
                underlying.extend(base_coins);
                break;
            }
        }
    }

    pool.tokens = coins;
    pool.underlying_tokens = underlying;

    Ok(())
}

fn blank_pool(chain: u64, candidate: &PoolCandidate) -> DexPool {
    DexPool {
        chain,
        pool_id: candidate.pool_id,
        emitter: candidate.address,
        factory: Address::ZERO,
        protocol: candidate.protocol,
        token0: Address::ZERO,
        token1: Address::ZERO,
        tokens: Vec::new(),
        underlying_tokens: Vec::new(),
        fee: 0,
        tick_spacing: 0,
        hooks: Address::ZERO,
        stable: false,
        created_block: 0,
        timestamp: 0,
        transaction_hash: B256::ZERO,
        log_index: 0,
        source: PoolSource::Rpc,
        attempts: 0,
        epoch: 0,
        _version: 0,
    }
}

/// The negative cache row of a candidate that is definitely not a pool.
pub fn unresolved_pool(chain: u64, candidate: &PoolCandidate) -> DexPool {
    DexPool {
        source: PoolSource::Unresolved,
        ..blank_pool(chain, candidate)
    }
}

/// The row of a candidate that gave no usable answer: asked again after a
/// backoff that grows with `attempts`.
pub fn no_answer_pool(chain: u64, candidate: &PoolCandidate) -> DexPool {
    DexPool {
        source: PoolSource::NoAnswer,
        attempts: candidate.attempts.saturating_add(1),
        ..blank_pool(chain, candidate)
    }
}

/// Asks the pool contract for its tokens (and fee / tick spacing / coins,
/// depending on the family of `candidate.protocol`).
pub async fn resolve_pool(
    caller: &dyn EthCaller,
    chain: u64,
    candidate: &PoolCandidate,
) -> Resolution {
    if !candidate.protocol.resolvable_by_rpc() {
        return Resolution::NotAPool;
    }

    let mut pool = blank_pool(chain, candidate);

    let outcome = match candidate.protocol {
        Protocol::Curve => resolve_curve(caller, &mut pool).await,
        _ => resolve_pair(caller, &mut pool).await,
    };

    match outcome {
        Ok(()) => Resolution::Resolved(Box::new(pool)),
        Err(Stop::NotAPool) => Resolution::NotAPool,
        Err(Stop::NoAnswer) => Resolution::NoAnswer,
        Err(Stop::Retry(error)) => Resolution::Retry(error),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use futures::future::BoxFuture;
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Mutex,
        },
    };

    /// What a fake contract does with a call.
    #[derive(Debug, Clone)]
    pub enum Reply {
        Return(Vec<u8>),
        Revert,
    }

    /// An in-memory chain: (contract, calldata) -> reply. Unknown calldata
    /// on a known contract reverts, unknown contracts have no code (empty
    /// return data), like a real node.
    #[derive(Default)]
    pub struct FakeNode {
        pub replies: Mutex<HashMap<(Address, Vec<u8>), Reply>>,
        pub contracts: Mutex<Vec<Address>>,
        pub offline: AtomicBool,
        pub calls: AtomicUsize,
    }

    pub fn address_word(address: Address) -> Vec<u8> {
        address.into_word().to_vec()
    }

    pub fn number_word(value: u64) -> Vec<u8> {
        U256::from(value).to_be_bytes::<32>().to_vec()
    }

    impl FakeNode {
        pub fn set(
            &self,
            contract: Address,
            getter: Getter,
            argument: Option<u64>,
            reply: Reply,
        ) {
            let mut calldata = getter.selector.to_vec();
            if let Some(argument) = argument {
                calldata.extend(number_word(argument));
            }

            self.replies
                .lock()
                .unwrap()
                .insert((contract, calldata), reply);
            self.contracts.lock().unwrap().push(contract);
        }

        pub fn pair(
            &self,
            pool: Address,
            token0: Address,
            token1: Address,
        ) {
            self.set(
                pool,
                TOKEN0,
                None,
                Reply::Return(address_word(token0)),
            );
            self.set(
                pool,
                TOKEN1,
                None,
                Reply::Return(address_word(token1)),
            );
        }

        pub fn coins(
            &self,
            pool: Address,
            getter: Getter,
            coins: &[Address],
        ) {
            for (index, coin) in coins.iter().enumerate() {
                self.set(
                    pool,
                    getter,
                    Some(index as u64),
                    Reply::Return(address_word(*coin)),
                );
            }
        }

        pub fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl EthCaller for FakeNode {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);

                if self.offline.load(Ordering::SeqCst) {
                    return Err(CallError::Transient("offline".into()));
                }

                let reply = self
                    .replies
                    .lock()
                    .unwrap()
                    .get(&(to, data.to_vec()))
                    .cloned();

                match reply {
                    Some(Reply::Return(bytes)) => Ok(Bytes::from(bytes)),
                    Some(Reply::Revert) => Err(CallError::Execution(
                        "execution reverted".into(),
                    )),
                    None if self
                        .contracts
                        .lock()
                        .unwrap()
                        .contains(&to) =>
                    {
                        Err(CallError::Execution(
                            "execution reverted".into(),
                        ))
                    }
                    None => Ok(Bytes::new()),
                }
            })
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            Box::pin(async { Ok(1) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::*, *};
    use crate::dex::models::pool_id_of;
    use alloy::primitives::keccak256;
    use std::sync::atomic::Ordering;

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn candidate(pool: Address, protocol: Protocol) -> PoolCandidate {
        PoolCandidate {
            pool_id: pool_id_of(pool),
            address: pool,
            protocol,
            attempts: 0,
        }
    }

    #[test]
    fn selectors_are_the_keccak_of_their_signatures() {
        for getter in GETTERS {
            assert_eq!(
                keccak256(getter.signature.as_bytes())[..4],
                getter.selector,
                "{}",
                getter.signature
            );
        }
    }

    #[tokio::test]
    async fn resolves_a_v2_pair() {
        let node = FakeNode::default();
        node.pair(addr(1), addr(0xa), addr(0xb));
        node.set(
            addr(1),
            FACTORY,
            None,
            Reply::Return(address_word(addr(0xf))),
        );

        let resolution = resolve_pool(
            &node,
            5,
            &candidate(addr(1), Protocol::UniswapV2),
        )
        .await;

        let Resolution::Resolved(pool) = resolution else {
            panic!("{resolution:?}");
        };

        assert_eq!(pool.chain, 5);
        assert_eq!(pool.protocol, Protocol::UniswapV2);
        assert_eq!(pool.tokens, vec![addr(0xa), addr(0xb)]);
        assert_eq!((pool.token0, pool.token1), (addr(0xa), addr(0xb)));
        assert_eq!(pool.factory, addr(0xf));
        assert_eq!(pool.source, PoolSource::Rpc);
        assert_eq!(pool.created_block, 0);
    }

    #[tokio::test]
    async fn a_v2_shaped_pool_with_stable_is_solidly() {
        let node = FakeNode::default();
        node.pair(addr(1), addr(0xa), addr(0xb));
        node.set(addr(1), STABLE, None, Reply::Return(number_word(1)));

        let Resolution::Resolved(pool) = resolve_pool(
            &node,
            1,
            &candidate(addr(1), Protocol::UniswapV2),
        )
        .await
        else {
            panic!("not resolved");
        };

        assert_eq!(pool.protocol, Protocol::Solidly);
        assert!(pool.stable);
    }

    #[tokio::test]
    async fn resolves_v3_and_algebra_pools() {
        let node = FakeNode::default();
        node.pair(addr(1), addr(0xa), addr(0xb));
        node.set(addr(1), FEE, None, Reply::Return(number_word(500)));
        let mut spacing = vec![0xffu8; 32];
        spacing[31] = 0xc4; // -60
        node.set(addr(1), TICK_SPACING, None, Reply::Return(spacing));

        // Algebra: no fee().
        node.pair(addr(2), addr(0xa), addr(0xb));
        node.set(
            addr(2),
            TICK_SPACING,
            None,
            Reply::Return(number_word(60)),
        );

        let Resolution::Resolved(v3) = resolve_pool(
            &node,
            1,
            &candidate(addr(1), Protocol::UniswapV3),
        )
        .await
        else {
            panic!("not resolved");
        };
        assert_eq!((v3.fee, v3.tick_spacing), (500, -60));

        let Resolution::Resolved(algebra) = resolve_pool(
            &node,
            1,
            &candidate(addr(2), Protocol::UniswapV3),
        )
        .await
        else {
            panic!("not resolved");
        };
        assert_eq!((algebra.fee, algebra.tick_spacing), (0, 60));
    }

    #[tokio::test]
    async fn curve_falls_back_to_int128_indices() {
        let node = FakeNode::default();
        // Old pool: only coins(int128).
        node.coins(addr(1), COINS_INT, &[addr(0xa), addr(0xb), addr(0xc)]);

        let Resolution::Resolved(pool) =
            resolve_pool(&node, 1, &candidate(addr(1), Protocol::Curve))
                .await
        else {
            panic!("not resolved");
        };

        assert_eq!(pool.tokens, vec![addr(0xa), addr(0xb), addr(0xc)]);
        assert_eq!(
            (pool.token0, pool.token1),
            (Address::ZERO, Address::ZERO)
        );
        assert!(pool.underlying_tokens.is_empty());
    }

    #[tokio::test]
    async fn curve_metapool_underlying_comes_from_the_base_pool() {
        let node = FakeNode::default();
        let lp = addr(0x33);
        node.coins(addr(1), COINS_UINT, &[addr(0xf), lp]);
        node.set(
            addr(1),
            BASE_POOL,
            None,
            Reply::Return(address_word(addr(2))),
        );
        node.coins(
            addr(2),
            COINS_UINT,
            &[addr(0xa), addr(0xb), addr(0xc)],
        );

        let Resolution::Resolved(pool) =
            resolve_pool(&node, 1, &candidate(addr(1), Protocol::Curve))
                .await
        else {
            panic!("not resolved");
        };

        assert_eq!(pool.tokens, vec![addr(0xf), lp]);
        assert_eq!(
            pool.underlying_tokens,
            vec![addr(0xf), addr(0xa), addr(0xb), addr(0xc)]
        );
    }

    #[tokio::test]
    async fn curve_lending_pool_exposes_underlying_coins() {
        let node = FakeNode::default();
        node.coins(addr(1), COINS_INT, &[addr(0xa), addr(0xb)]);
        node.coins(addr(1), UNDERLYING_COINS_INT, &[addr(0xc), addr(0xd)]);

        let Resolution::Resolved(pool) =
            resolve_pool(&node, 1, &candidate(addr(1), Protocol::Curve))
                .await
        else {
            panic!("not resolved");
        };

        assert_eq!(pool.underlying_tokens, vec![addr(0xc), addr(0xd)]);
    }

    #[tokio::test]
    async fn reverts_and_garbage_are_definitive() {
        let node = FakeNode::default();
        // A contract without the getters.
        node.set(addr(1), FEE, None, Reply::Return(number_word(1)));
        // Garbage: not an address / wrong size.
        node.set(addr(2), TOKEN0, None, Reply::Return(vec![0xff; 32]));
        node.set(addr(3), TOKEN0, None, Reply::Return(vec![1, 2, 3]));
        // Same token twice.
        node.pair(addr(4), addr(0xa), addr(0xa));
        // A single coin is not a pool.
        node.coins(addr(5), COINS_UINT, &[addr(0xa)]);

        for (pool, protocol) in [
            (addr(1), Protocol::UniswapV2),
            (addr(2), Protocol::UniswapV3),
            (addr(3), Protocol::Solidly),
            (addr(4), Protocol::UniswapV2),
            (addr(5), Protocol::Curve),
            (addr(1), Protocol::Curve),
            // Never asked at all.
            (addr(1), Protocol::UniswapV4),
        ] {
            assert_eq!(
                resolve_pool(&node, 1, &candidate(pool, protocol)).await,
                Resolution::NotAPool,
                "{pool} {protocol}"
            );
        }
    }

    #[tokio::test]
    async fn transient_failures_and_missing_code_conclude_nothing() {
        let node = FakeNode::default();
        node.pair(addr(1), addr(0xa), addr(0xb));

        // No code at the address.
        for protocol in [Protocol::UniswapV2, Protocol::Curve] {
            assert_eq!(
                resolve_pool(&node, 1, &candidate(addr(9), protocol))
                    .await,
                Resolution::NoAnswer
            );
        }

        node.offline.store(true, Ordering::SeqCst);

        for protocol in [Protocol::UniswapV2, Protocol::Curve] {
            assert!(matches!(
                resolve_pool(&node, 1, &candidate(addr(1), protocol))
                    .await,
                Resolution::Retry(_)
            ));
        }
    }

    #[test]
    fn unresolved_rows_lose_against_everything() {
        let row = unresolved_pool(1, &candidate(addr(1), Protocol::Curve));

        assert_eq!(row.source, PoolSource::Unresolved);
        assert_eq!((row.created_block, row.log_index), (0, 0));
        assert!(row.tokens.is_empty());
    }
}
