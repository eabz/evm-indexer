//! Event signatures of the supported DEX families.
//!
//! Every `topic0` literal is asserted against `keccak256(signature)` in the
//! unit test below: never add a literal without its canonical signature.
//!
//! Canonical signature = event name + parameter TYPES only, no names, no
//! `indexed`, no spaces, `uint` spelled `uint256`.

use alloy::primitives::{b256, B256};

/// One event of a family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventDef {
    pub signature: &'static str,
    pub topic0: B256,
    /// Topics including `topic0`.
    pub topics: u8,
    /// Exact length of the data section, `None` for dynamic payloads.
    pub data_len: Option<usize>,
}

const fn event(
    signature: &'static str,
    topic0: B256,
    topics: u8,
    words: usize,
) -> EventDef {
    EventDef { signature, topic0, topics, data_len: Some(words * 32) }
}

// ---------------------------------------------------------------- uniswap_v2

/// `PairCreated(address indexed token0, address indexed token1,
/// address pair, uint256)`
pub const V2_PAIR_CREATED: EventDef = event(
    "PairCreated(address,address,address,uint256)",
    b256!(
        "0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9"
    ),
    3,
    2,
);

/// `Swap(address indexed sender, uint256 amount0In, uint256 amount1In,
/// uint256 amount0Out, uint256 amount1Out, address indexed to)`
///
/// Also emitted by Solidly V1 forks (Velodrome V1, Thena, Ramses...).
pub const V2_SWAP: EventDef = event(
    "Swap(address,uint256,uint256,uint256,uint256,address)",
    b256!(
        "d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
    ),
    3,
    4,
);

/// `Sync(uint112 reserve0, uint112 reserve1)`
pub const V2_SYNC: EventDef = event(
    "Sync(uint112,uint112)",
    b256!(
        "1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"
    ),
    1,
    2,
);

/// `Mint(address indexed sender, uint256 amount0, uint256 amount1)`
/// (shared with the Solidly family).
pub const V2_MINT: EventDef = event(
    "Mint(address,uint256,uint256)",
    b256!(
        "4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f"
    ),
    2,
    2,
);

/// `Burn(address indexed sender, uint256 amount0, uint256 amount1,
/// address indexed to)` (shared with Solidly V1).
pub const V2_BURN: EventDef = event(
    "Burn(address,uint256,uint256,address)",
    b256!(
        "dccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496"
    ),
    3,
    2,
);

// ------------------------------------------------------------------- solidly

/// Solidly V1 factory: `PairCreated(address indexed token0,
/// address indexed token1, bool stable, address pair, uint256)`
pub const SOLIDLY_PAIR_CREATED: EventDef = event(
    "PairCreated(address,address,bool,address,uint256)",
    b256!(
        "c4805696c66d7cf352fc1d6bb633ad5ee82f6cb577c453024b6e0eb8306c6fc9"
    ),
    3,
    3,
);

/// Velodrome V2 / Aerodrome factory: `PoolCreated(address indexed token0,
/// address indexed token1, bool indexed stable, address pool, uint256)`
pub const SOLIDLY_POOL_CREATED: EventDef = event(
    "PoolCreated(address,address,bool,address,uint256)",
    b256!(
        "2128d88d14c80cb081c1252a5acff7a264671bf199ce226b53788fb26065005e"
    ),
    4,
    2,
);

/// Velodrome V2 / Aerodrome: `Swap(address indexed sender,
/// address indexed to, uint256 amount0In, uint256 amount1In,
/// uint256 amount0Out, uint256 amount1Out)`
pub const SOLIDLY_SWAP: EventDef = event(
    "Swap(address,address,uint256,uint256,uint256,uint256)",
    b256!(
        "b3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b"
    ),
    3,
    4,
);

/// `Sync(uint256 reserve0, uint256 reserve1)`
pub const SOLIDLY_SYNC: EventDef = event(
    "Sync(uint256,uint256)",
    b256!(
        "cf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a"
    ),
    1,
    2,
);

/// Velodrome V2 / Aerodrome: `Burn(address indexed sender,
/// address indexed to, uint256 amount0, uint256 amount1)`
pub const SOLIDLY_BURN: EventDef = event(
    "Burn(address,address,uint256,uint256)",
    b256!(
        "5d624aa9c148153ab3446c1b154f660ee7701e549fe9b62dab7171b1c80e6fa2"
    ),
    3,
    2,
);

// ---------------------------------------------------------------- uniswap_v3

/// `PoolCreated(address indexed token0, address indexed token1,
/// uint24 indexed fee, int24 tickSpacing, address pool)`
pub const V3_POOL_CREATED: EventDef = event(
    "PoolCreated(address,address,uint24,int24,address)",
    b256!(
        "783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118"
    ),
    4,
    2,
);

/// Velodrome Slipstream / Aerodrome CL factory: `PoolCreated(address indexed
/// token0, address indexed token1, int24 indexed tickSpacing, address pool)`
pub const SLIPSTREAM_POOL_CREATED: EventDef = event(
    "PoolCreated(address,address,int24,address)",
    b256!(
        "ab0d57f0df537bb25e80245ef7748fa62353808c54d6e528a9dd20887aed9ac2"
    ),
    4,
    1,
);

/// Algebra factory: `Pool(address indexed token0, address indexed token1,
/// address pool)`
pub const ALGEBRA_POOL: EventDef = event(
    "Pool(address,address,address)",
    b256!(
        "91ccaa7a278130b65168c3a0c8d3bcae84cf5e43704342bd3ec0b59e59c036db"
    ),
    3,
    1,
);

/// Algebra Integral factory: `CustomPool(address indexed deployer,
/// address indexed token0, address indexed token1, address pool)`
pub const ALGEBRA_CUSTOM_POOL: EventDef = event(
    "CustomPool(address,address,address,address)",
    b256!(
        "8a5f030f5fc13b04a1e4ef7c47177e3d76b0e80e1d9be9843db37caa5b7b9b8f"
    ),
    4,
    1,
);

/// `Swap(address indexed sender, address indexed recipient, int256 amount0,
/// int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)`
///
/// Algebra V1 emits the same types (`price` instead of `sqrtPriceX96`).
pub const V3_SWAP: EventDef = event(
    "Swap(address,address,int256,int256,uint160,uint128,int24)",
    b256!(
        "c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
    ),
    3,
    5,
);

/// PancakeSwap V3: the V3 swap plus `uint128 protocolFeesToken0,
/// uint128 protocolFeesToken1`.
pub const PANCAKE_V3_SWAP: EventDef = event(
    "Swap(address,address,int256,int256,uint160,uint128,int24,uint128,uint128)",
    b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83"),
    3,
    7,
);

/// Algebra Integral (1.2+): the V3 swap plus `uint24 overrideFee,
/// uint24 pluginFee`.
pub const ALGEBRA_INTEGRAL_SWAP: EventDef = event(
    "Swap(address,address,int256,int256,uint160,uint128,int24,uint24,uint24)",
    b256!("121cb44ee54098b1a04743c487e7460d8dd429b27f88b1f4d4767396e1a59f79"),
    3,
    7,
);

/// `Mint(address sender, address indexed owner, int24 indexed tickLower,
/// int24 indexed tickUpper, uint128 amount, uint256 amount0,
/// uint256 amount1)`
pub const V3_MINT: EventDef = event(
    "Mint(address,address,int24,int24,uint128,uint256,uint256)",
    b256!(
        "7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde"
    ),
    4,
    4,
);

/// `Burn(address indexed owner, int24 indexed tickLower,
/// int24 indexed tickUpper, uint128 amount, uint256 amount0,
/// uint256 amount1)`
pub const V3_BURN: EventDef = event(
    "Burn(address,int24,int24,uint128,uint256,uint256)",
    b256!(
        "0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c"
    ),
    4,
    3,
);

// ---------------------------------------------------------------- uniswap_v4

/// `Initialize(bytes32 indexed id, address indexed currency0,
/// address indexed currency1, uint24 fee, int24 tickSpacing, address hooks,
/// uint160 sqrtPriceX96, int24 tick)`
pub const V4_INITIALIZE: EventDef = event(
    "Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)",
    b256!("dd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438"),
    4,
    5,
);

/// `Swap(bytes32 indexed id, address indexed sender, int128 amount0,
/// int128 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick,
/// uint24 fee)` - amounts are CALLER relative (negative = paid to the pool).
pub const V4_SWAP: EventDef = event(
    "Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)",
    b256!(
        "40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f"
    ),
    3,
    6,
);

/// `ModifyLiquidity(bytes32 indexed id, address indexed sender,
/// int24 tickLower, int24 tickUpper, int256 liquidityDelta, bytes32 salt)`
pub const V4_MODIFY_LIQUIDITY: EventDef = event(
    "ModifyLiquidity(bytes32,address,int24,int24,int256,bytes32)",
    b256!(
        "f208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec"
    ),
    3,
    4,
);

// --------------------------------------------------------------- balancer_v2

/// `PoolRegistered(bytes32 indexed poolId, address indexed poolAddress,
/// uint8 specialization)`
pub const BALANCER_POOL_REGISTERED: EventDef = event(
    "PoolRegistered(bytes32,address,uint8)",
    b256!(
        "3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e"
    ),
    3,
    1,
);

/// `TokensRegistered(bytes32 indexed poolId, address[] tokens,
/// address[] assetManagers)`
pub const BALANCER_TOKENS_REGISTERED: EventDef = EventDef {
    signature: "TokensRegistered(bytes32,address[],address[])",
    topic0: b256!(
        "f5847d3f2197b16cdcd2098ec95d0905cd1abdaf415f07bb7cef2bba8ac5dec4"
    ),
    topics: 2,
    data_len: None,
};

/// `Swap(bytes32 indexed poolId, address indexed tokenIn,
/// address indexed tokenOut, uint256 amountIn, uint256 amountOut)`
pub const BALANCER_SWAP: EventDef = event(
    "Swap(bytes32,address,address,uint256,uint256)",
    b256!(
        "2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b"
    ),
    4,
    2,
);

// --------------------------------------------------------------------- curve

/// StableSwap (classic and NG): `TokenExchange(address indexed buyer,
/// int128 sold_id, uint256 tokens_sold, int128 bought_id,
/// uint256 tokens_bought)`
pub const CURVE_TOKEN_EXCHANGE: EventDef = event(
    "TokenExchange(address,int128,uint256,int128,uint256)",
    b256!(
        "8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140"
    ),
    2,
    4,
);

/// Same payload, coin indices refer to the UNDERLYING coins.
pub const CURVE_TOKEN_EXCHANGE_UNDERLYING: EventDef = event(
    "TokenExchangeUnderlying(address,int128,uint256,int128,uint256)",
    b256!(
        "d013ca23e77a65003c2c659c5442c00c805371b7fc1ebd4c206c41d1536bd90b"
    ),
    2,
    4,
);

/// CryptoSwap (tricrypto2, two-coin crypto pools): `uint256` indices.
pub const CURVE_CRYPTO_TOKEN_EXCHANGE: EventDef = event(
    "TokenExchange(address,uint256,uint256,uint256,uint256)",
    b256!(
        "b2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98"
    ),
    2,
    4,
);

/// Tricrypto-NG / Twocrypto-NG: crypto payload plus `uint256 fee,
/// uint256 packed_price_scale`.
pub const CURVE_NG_TOKEN_EXCHANGE: EventDef = event(
    "TokenExchange(address,uint256,uint256,uint256,uint256,uint256,uint256)",
    b256!("143f1f8e861fbdeddd5b46e844b7d3ac7b86a122f36e8c463859ee6811b1f29c"),
    2,
    6,
);

/// Every event the decoder understands.
pub const ALL: &[EventDef] = &[
    V2_PAIR_CREATED,
    V2_SWAP,
    V2_SYNC,
    V2_MINT,
    V2_BURN,
    SOLIDLY_PAIR_CREATED,
    SOLIDLY_POOL_CREATED,
    SOLIDLY_SWAP,
    SOLIDLY_SYNC,
    SOLIDLY_BURN,
    V3_POOL_CREATED,
    SLIPSTREAM_POOL_CREATED,
    ALGEBRA_POOL,
    ALGEBRA_CUSTOM_POOL,
    V3_SWAP,
    PANCAKE_V3_SWAP,
    ALGEBRA_INTEGRAL_SWAP,
    V3_MINT,
    V3_BURN,
    V4_INITIALIZE,
    V4_SWAP,
    V4_MODIFY_LIQUIDITY,
    BALANCER_POOL_REGISTERED,
    BALANCER_TOKENS_REGISTERED,
    BALANCER_SWAP,
    CURVE_TOKEN_EXCHANGE,
    CURVE_TOKEN_EXCHANGE_UNDERLYING,
    CURVE_CRYPTO_TOKEN_EXCHANGE,
    CURVE_NG_TOKEN_EXCHANGE,
];

/// `topic0` values to request from the source when only DEX logs are
/// wanted (the indexer normally streams every log anyway).
pub fn all_topic0() -> Vec<B256> {
    ALL.iter().map(|event| event.topic0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::keccak256;
    use std::collections::HashSet;

    #[test]
    fn every_topic0_is_the_keccak_of_its_signature() {
        for event in ALL {
            assert_eq!(
                keccak256(event.signature.as_bytes()),
                event.topic0,
                "{}",
                event.signature
            );
        }
    }

    #[test]
    fn signatures_are_canonical() {
        for event in ALL {
            let signature = event.signature;
            assert!(!signature.contains(' '), "{signature}");
            assert!(!signature.contains("indexed"), "{signature}");
            assert!(!signature.contains("uint,"), "{signature}");
            assert!(!signature.contains("uint)"), "{signature}");
            assert!(signature.ends_with(')'), "{signature}");
        }
    }

    #[test]
    fn topic0s_are_unique() {
        let unique: HashSet<B256> =
            ALL.iter().map(|event| event.topic0).collect();
        assert_eq!(unique.len(), ALL.len());
        assert_eq!(all_topic0().len(), ALL.len());
    }

    #[test]
    fn indexed_arguments_fit_the_topics() {
        for event in ALL {
            assert!(
                (1..=4).contains(&event.topics),
                "{}",
                event.signature
            );
        }
    }
}
