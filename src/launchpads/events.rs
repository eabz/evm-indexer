//! Event signatures of the supported launchpad families.
//!
//! Every `topic0` literal is asserted against `keccak256(signature)` in the
//! unit test below: never add a literal without its canonical signature.
//!
//! Canonical signature = event name + parameter TYPES only, no names, no
//! `indexed`, no spaces, `uint` spelled `uint256`, enums as `uint8`.
//!
//! Every event here was observed in a REAL transaction kept in
//! `fixtures_data.rs`, except [`FLAP_TAX_V1`] (read in the verified Flap
//! ABI, never seen live in the sampled window - see README §1.2).

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

const fn dynamic(
    signature: &'static str,
    topic0: B256,
    topics: u8,
) -> EventDef {
    EventDef { signature, topic0, topics, data_len: None }
}

// ------------------------------------------------- pons_v2 (full curve)

/// `TokenLaunched(address indexed token, address indexed curve, address
/// indexed deployer, address pairToken, uint256 launchConfigId, uint256
/// graduationThreshold)`, emitted by the launch FACTORY.
pub const PONS_V2_TOKEN_LAUNCHED: EventDef = event(
    "TokenLaunched(address,address,address,address,uint256,uint256)",
    b256!(
        "8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607"
    ),
    4,
    3,
);

/// `CurveBuy(address indexed buyer, address indexed recipient, uint256
/// quoteIn, uint256 tokensOut, uint256 fee, uint256 tax)`, emitted by the
/// per-token CURVE contract.
///
/// `quoteIn` is GROSS (fee + snipe tax included), `tokensOut` is what the
/// curve transferred to `recipient`. `buyer` is whoever called: a router,
/// the launch forwarder or the user.
pub const PONS_V2_CURVE_BUY: EventDef = event(
    "CurveBuy(address,address,uint256,uint256,uint256,uint256)",
    b256!(
        "ec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455"
    ),
    3,
    4,
);

/// `CurveSell(address indexed seller, address indexed recipient, uint256
/// tokensIn, uint256 quoteOut, uint256 fee, uint256 tax)`.
///
/// `quoteOut` is NET of `fee` and `tax`.
pub const PONS_V2_CURVE_SELL: EventDef = event(
    "CurveSell(address,address,uint256,uint256,uint256,uint256)",
    b256!(
        "8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df"
    ),
    3,
    4,
);

/// `CurveCompleted(address to, uint256 quoteRaised, uint256 tokensSold)`:
/// the curve reached its graduation threshold in this transaction.
pub const PONS_V2_CURVE_COMPLETED: EventDef = event(
    "CurveCompleted(address,uint256,uint256)",
    b256!(
        "f8d37a90738ae063b8b8058b66f5880cf3cf7ab0c5d4fa78219696591dfbfb67"
    ),
    1,
    3,
);

/// `FeesSwept(uint256 protocolAmount, uint256 buybackAmount, uint256
/// creatorAmount)`, emitted by the CURVE: curve-phase fees.
pub const PONS_V2_FEES_SWEPT: EventDef = event(
    "FeesSwept(uint256,uint256,uint256)",
    b256!(
        "9f4cd7c4ed99d08a797804560c9c5d71d2cf7e101f2e3b5e7d1ca8a24c370e4f"
    ),
    1,
    3,
);

/// `PoolGraduated(address indexed token, uint256 positionId, uint256
/// tokenAmount, uint256 pairTokenAmount)`, emitted by the FACTORY.
pub const PONS_V2_POOL_GRADUATED: EventDef = event(
    "PoolGraduated(address,uint256,uint256,uint256)",
    b256!(
        "0a44ef75df69c534f43cd6c1aa3ef8983065fe5fe79ef9e79f6494e6f258c259"
    ),
    2,
    3,
);

/// `PoolRegistered(bytes32 indexed poolId, address memecoin, address
/// quoteToken, address creator)`, emitted by the Uniswap V4 HOOK: the only
/// event of the graduation transaction that names the destination pool id.
pub const PONS_V2_POOL_REGISTERED: EventDef = event(
    "PoolRegistered(bytes32,address,address,address)",
    b256!(
        "01bf263a1db1652580721573296e1a1fa70b3d4c87f61d02a69c4e1109d2d573"
    ),
    2,
    3,
);

/// `PoolFeesSwept(bytes32 indexed poolId, uint256 protocolAmount, uint256
/// buybackAmount, uint256 creatorAmount, uint256 tokensLocked)`, emitted by
/// the HOOK: fees of the graduated (DEX phase) pool.
pub const PONS_V2_POOL_FEES_SWEPT: EventDef = event(
    "PoolFeesSwept(bytes32,uint256,uint256,uint256,uint256)",
    b256!(
        "2f3c43579b9064b6f28edcf41608f3815792d274a56afe024359703cb4ea9b30"
    ),
    2,
    4,
);

/// `Credited(address indexed recipient, address indexed source, uint256
/// amount)`, emitted by the fee ESCROW next to every sweep: the only event
/// that names WHO the creator share went to.
pub const PONS_V2_CREDITED: EventDef = event(
    "Credited(address,address,uint256)",
    b256!(
        "4e45da441832cf53bdaa69235704fc0575e68210f459ee1562911024b12967d5"
    ),
    3,
    1,
);

// --------------------------------------------- flap_portal (full curve)

/// `TokenCreated(uint256 timestamp, address creator, uint256 nonce,
/// address token, string name, string symbol, string metadata)`. No
/// indexed parameter anywhere in this family.
pub const FLAP_TOKEN_CREATED: EventDef = dynamic(
    "TokenCreated(uint256,address,uint256,address,string,string,string)",
    b256!(
        "504e7f360b2e5fe33cbaaae4c593bc55305328341bf79009e43e0e3b7f699603"
    ),
    1,
);

/// `TokenBought(uint256 timestamp, address token, address buyer, uint256
/// amount, uint256 eth, uint256 fee, uint256 postPrice)`.
///
/// `eth` is the GROSS quote paid (in the token's quote asset, which is not
/// always the native coin), `amount` the tokens the portal sent.
pub const FLAP_TOKEN_BOUGHT: EventDef = event(
    "TokenBought(uint256,address,address,uint256,uint256,uint256,uint256)",
    b256!(
        "a800a2038683844fac66747f771bfdfae862eb28b16bcfa387afa9fbacce8ff7"
    ),
    1,
    7,
);

/// `TokenSold(uint256 timestamp, address token, address seller, uint256
/// amount, uint256 eth, uint256 fee, uint256 postPrice)`.
pub const FLAP_TOKEN_SOLD: EventDef = event(
    "TokenSold(uint256,address,address,uint256,uint256,uint256,uint256)",
    b256!(
        "03a4693e592f5e75dc7c136acb39b146d2b4966c0e509c34f362dee02b3b861a"
    ),
    1,
    7,
);

/// `LaunchedToDEX(address token, address pool, uint256 amount, uint256
/// eth)`: graduation into a Uniswap-V2 style pair.
pub const FLAP_LAUNCHED_TO_DEX: EventDef = event(
    "LaunchedToDEX(address,address,uint256,uint256)",
    b256!(
        "6e4f47630b8745b8cacbd44f42a8a33e7eea7cc08ef22fc7630f4f385784ff7d"
    ),
    1,
    4,
);

/// `TokenQuoteSet(address token, address quoteToken)`: zero = native coin.
pub const FLAP_TOKEN_QUOTE_SET: EventDef = event(
    "TokenQuoteSet(address,address)",
    b256!(
        "3ceb902d3c555c21c3415b6aa839104b18e4825b2f8324011ff979089a507a8c"
    ),
    1,
    2,
);

/// `FlapTokenProgressChanged(address token, uint256 newProgress)`: curve
/// progress as a wad, `1e18` = graduated.
pub const FLAP_PROGRESS_CHANGED: EventDef = event(
    "FlapTokenProgressChanged(address,uint256)",
    b256!(
        "4c35e20d1e9bce377c7d9ec1572d934e46d62961f4da8af5beb8002d5906742d"
    ),
    1,
    2,
);

/// `TaxV2OnBondingCurvePaid(address indexed token, uint256 amount)`: the
/// token's own tax on a curve trade, paid in the quote asset.
pub const FLAP_TAX_V2: EventDef = event(
    "TaxV2OnBondingCurvePaid(address,uint256)",
    b256!(
        "b4aa5d6b2390b2b8892ff84cb0ef9a64e969e1af22d049fa61cc2cf66c6ab7bd"
    ),
    2,
    1,
);

/// `TaxOnBondingCurvePaid(address indexed token, uint256 amount)`: the V1
/// variant, in the verified ABI but not observed in the sampled window.
pub const FLAP_TAX_V1: EventDef = event(
    "TaxOnBondingCurvePaid(address,uint256)",
    b256!(
        "62c3d276a0a60596ef9bc2d78900e00d8df2eb8101c1b1923e41425251e1d54f"
    ),
    2,
    1,
);

// --------------------------------------- launch attribution only (§1.3)

/// Pons V1 / NOXA `TokenLaunched(address indexed token, address indexed
/// deployer, address indexed dexFactory, address pairToken, address pool,
/// uint256 dexId, uint256 launchConfigId, uint256 positionId, uint256
/// restrictionsEndBlock, uint256 initialBuyAmount)`: the token goes
/// straight into `pool`, so the trades are `dex_swaps` rows.
pub const PONS_V1_TOKEN_LAUNCHED: EventDef = event(
    "TokenLaunched(address,address,address,address,address,uint256,uint256,uint256,uint256,uint256)",
    b256!("db51ea9ad51ab453a65a4cb7e60c3cb378c9501bb002609f8f97778fb6c4235a"),
    4,
    7,
);

/// LetsCash `TokenLaunched(address indexed token, address indexed creator,
/// bytes32 indexed poolId, uint256 configId, uint256 firstBuyIn, uint256
/// firstBuyOut, address hook, address feeRecipient)` (Uniswap V4 pool id).
pub const LETSCASH_TOKEN_LAUNCHED: EventDef = event(
    "TokenLaunched(address,address,bytes32,uint256,uint256,uint256,address,address)",
    b256!("17091df68f499cf4e20dcfc5d42f064dd22359e785b77691c4c4ed0322608897"),
    4,
    5,
);

/// Bags `TokenCreated(address indexed token, address indexed curve,
/// address indexed creator, address feeShare, address partner, bytes32
/// poolId, string name, string symbol, string metadataURI)`.
pub const BAGS_TOKEN_CREATED: EventDef = dynamic(
    "TokenCreated(address,address,address,address,address,bytes32,string,string,string)",
    b256!("643b3b606052cbadac2f906ad0b462da99eda2a1d4f824d315d7f6edd3e4cced"),
    4,
);

/// Clanker v4 `TokenCreated(address msgSender, address indexed
/// tokenAddress, address indexed tokenAdmin, string tokenImage, string
/// tokenName, string tokenSymbol, string tokenMetadata, string
/// tokenContext, int24 startingTick, address poolHook, bytes32 poolId,
/// address pairedToken, address locker, address mevModule, uint256
/// extensionsSupply, address[] extensions)`.
pub const CLANKER_V4_TOKEN_CREATED: EventDef = dynamic(
    "TokenCreated(address,address,address,string,string,string,string,string,int24,address,bytes32,address,address,address,uint256,address[])",
    b256!("9299d1d1a88d8e1abdc591ae7a167a6bc63a8f17d695804e9091ee33aa89fb67"),
    3,
);

// ------------------------------------------------------------ evidence

/// ERC-20 `Transfer(address indexed from, address indexed to, uint256
/// value)`: the only claim that comes from the TOKEN itself, used to
/// corroborate every curve leg (`corroborate.rs`).
pub const ERC20_TRANSFER: EventDef = event(
    "Transfer(address,address,uint256)",
    b256!(
        "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
    ),
    3,
    1,
);

/// Every event the decoder knows.
pub const ALL: &[EventDef] = &[
    PONS_V2_TOKEN_LAUNCHED,
    PONS_V2_CURVE_BUY,
    PONS_V2_CURVE_SELL,
    PONS_V2_CURVE_COMPLETED,
    PONS_V2_FEES_SWEPT,
    PONS_V2_POOL_GRADUATED,
    PONS_V2_POOL_REGISTERED,
    PONS_V2_POOL_FEES_SWEPT,
    PONS_V2_CREDITED,
    FLAP_TOKEN_CREATED,
    FLAP_TOKEN_BOUGHT,
    FLAP_TOKEN_SOLD,
    FLAP_LAUNCHED_TO_DEX,
    FLAP_TOKEN_QUOTE_SET,
    FLAP_PROGRESS_CHANGED,
    FLAP_TAX_V2,
    FLAP_TAX_V1,
    PONS_V1_TOKEN_LAUNCHED,
    LETSCASH_TOKEN_LAUNCHED,
    BAGS_TOKEN_CREATED,
    CLANKER_V4_TOKEN_CREATED,
    ERC20_TRANSFER,
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use alloy::primitives::keccak256;

    use super::*;

    #[test]
    fn every_topic0_is_the_keccak_of_its_signature() {
        for def in ALL {
            assert_eq!(
                keccak256(def.signature.as_bytes()),
                def.topic0,
                "{}",
                def.signature
            );
        }
    }

    #[test]
    fn signatures_are_canonical_and_unique() {
        let mut seen = HashSet::new();

        for def in ALL {
            assert!(seen.insert(def.topic0), "{}", def.signature);
            assert!(!def.signature.contains(' '), "{}", def.signature);
            assert!(!def.signature.contains("indexed"));
            assert!(!def.signature.contains("uint,"));
            assert!(!def.signature.contains("uint)"));
            assert!((1..=4).contains(&def.topics), "{}", def.signature);

            let parameters = def.signature.matches(',').count() + 1;
            assert!(usize::from(def.topics) - 1 <= parameters);
        }
    }

    #[test]
    fn static_events_have_one_word_per_unindexed_parameter() {
        for def in ALL {
            let Some(len) = def.data_len else { continue };
            let parameters = if def.signature.ends_with("()") {
                0
            } else {
                def.signature.matches(',').count() + 1
            };

            assert_eq!(
                len / 32 + usize::from(def.topics) - 1,
                parameters,
                "{}",
                def.signature
            );
        }
    }

    /// The same event NAME in three families: only the shape tells them
    /// apart, which is exactly why decoding is by topic0 and never by
    /// address.
    #[test]
    fn same_name_different_family_is_a_different_topic() {
        assert_ne!(
            PONS_V2_TOKEN_LAUNCHED.topic0,
            PONS_V1_TOKEN_LAUNCHED.topic0
        );
        assert_ne!(
            PONS_V2_TOKEN_LAUNCHED.topic0,
            LETSCASH_TOKEN_LAUNCHED.topic0
        );
        assert_ne!(FLAP_TOKEN_CREATED.topic0, BAGS_TOKEN_CREATED.topic0);
        assert_ne!(
            FLAP_TOKEN_CREATED.topic0,
            CLANKER_V4_TOKEN_CREATED.topic0
        );
    }

    #[test]
    fn the_transfer_signature_is_the_shared_one() {
        assert_eq!(
            ERC20_TRANSFER.topic0,
            crate::core::events::TRANSFER_EVENT_SIGNATURE
        );
    }
}
