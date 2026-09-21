//! Event signatures of the supported prediction market families.
//!
//! Every `topic0` literal is asserted against `keccak256(signature)` in the
//! unit test below: never add a literal without its canonical signature.
//!
//! Canonical signature = event name + parameter TYPES only, no names, no
//! `indexed`, no spaces, `uint` spelled `uint256`, enums as `uint8`.
//!
//! Every event listed here was observed on a public chain (see
//! `fixtures.rs` and the README for the transactions).

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

// ----------------------------------------------- ctf (Conditional Tokens)

/// `ConditionPreparation(bytes32 indexed conditionId, address indexed
/// oracle, bytes32 indexed questionId, uint outcomeSlotCount)`
pub const CTF_CONDITION_PREPARATION: EventDef = event(
    "ConditionPreparation(bytes32,address,bytes32,uint256)",
    b256!(
        "ab3760c3bd2bb38b5bcf54dc79802ed67338b4cf29f3054ded67ed24661e4177"
    ),
    4,
    1,
);

/// `ConditionResolution(bytes32 indexed conditionId, address indexed
/// oracle, bytes32 indexed questionId, uint outcomeSlotCount,
/// uint[] payoutNumerators)`
pub const CTF_CONDITION_RESOLUTION: EventDef = dynamic(
    "ConditionResolution(bytes32,address,bytes32,uint256,uint256[])",
    b256!(
        "b44d84d3289691f71497564b85d4233648d9dbae8cbdbb4329f301c3a0185894"
    ),
    4,
);

/// `PositionSplit(address indexed stakeholder, IERC20 collateralToken,
/// bytes32 indexed parentCollectionId, bytes32 indexed conditionId,
/// uint[] partition, uint amount)`
pub const CTF_POSITION_SPLIT: EventDef = dynamic(
    "PositionSplit(address,address,bytes32,bytes32,uint256[],uint256)",
    b256!(
        "2e6bb91f8cbcda0c93623c54d0403a43514fabc40084ec96b6d5379a74786298"
    ),
    4,
);

/// `PositionsMerge(address indexed stakeholder, IERC20 collateralToken,
/// bytes32 indexed parentCollectionId, bytes32 indexed conditionId,
/// uint[] partition, uint amount)`
pub const CTF_POSITIONS_MERGE: EventDef = dynamic(
    "PositionsMerge(address,address,bytes32,bytes32,uint256[],uint256)",
    b256!(
        "6f13ca62553fcc2bcd2372180a43949c1e4cebba603901ede2f4e14f36b282ca"
    ),
    4,
);

/// `PayoutRedemption(address indexed redeemer, IERC20 indexed
/// collateralToken, bytes32 indexed parentCollectionId, bytes32
/// conditionId, uint[] indexSets, uint payout)`
pub const CTF_PAYOUT_REDEMPTION: EventDef = dynamic(
    "PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)",
    b256!(
        "2682012a4a4f1973119f1c9b90745d1bd91fa2bab387344f044cb3586864d18d"
    ),
    4,
);

/// ERC-1155 `TransferSingle(address indexed operator, address indexed
/// from, address indexed to, uint256 id, uint256 value)`
pub const ERC1155_TRANSFER_SINGLE: EventDef = event(
    "TransferSingle(address,address,address,uint256,uint256)",
    b256!(
        "c3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62"
    ),
    4,
    2,
);

/// ERC-1155 `TransferBatch(address indexed operator, address indexed from,
/// address indexed to, uint256[] ids, uint256[] values)`
pub const ERC1155_TRANSFER_BATCH: EventDef = dynamic(
    "TransferBatch(address,address,address,uint256[],uint256[])",
    b256!(
        "4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb"
    ),
    4,
);

// ------------------------------------- ctf_exchange (Polymarket V1 + forks)

/// `OrderFilled(bytes32 indexed orderHash, address indexed maker,
/// address indexed taker, uint256 makerAssetId, uint256 takerAssetId,
/// uint256 makerAmountFilled, uint256 takerAmountFilled, uint256 fee)`
///
/// Asset id 0 is the collateral. Emitted once per maker order AND once
/// for the taker order (there `taker` is the exchange itself).
pub const EXCHANGE_ORDER_FILLED: EventDef = event(
    "OrderFilled(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)",
    b256!(
        "d0a08e8c493f9c94f29311604c9de1b4e8c8d4c06bd0c789af57f2d65bfec0f6"
    ),
    4,
    5,
);

/// `OrdersMatched(bytes32 indexed takerOrderHash, address indexed
/// takerOrderMaker, uint256 makerAssetId, uint256 takerAssetId,
/// uint256 makerAmountFilled, uint256 takerAmountFilled)`
///
/// A summary of the taker order: never a trade of its own.
pub const EXCHANGE_ORDERS_MATCHED: EventDef = event(
    "OrdersMatched(bytes32,address,uint256,uint256,uint256,uint256)",
    b256!(
        "63bf4d16b7fa898ef4c4b2b6d90fd201e9c56313b65638af6088d149d2ce956c"
    ),
    3,
    4,
);

/// `TokenRegistered(uint256 indexed token0, uint256 indexed token1,
/// bytes32 indexed conditionId)`. Only used by tests (as an independent
/// witness of the position id derivation): the mapping the indexer stores
/// is COMPUTED, a registration is merely claimed by its emitter.
pub const EXCHANGE_TOKEN_REGISTERED: EventDef = event(
    "TokenRegistered(uint256,uint256,bytes32)",
    b256!(
        "bc9a2432e8aeb48327246cddd6e872ef452812b4243c04e6bfb786a2cd8faf0d"
    ),
    4,
    0,
);

// ------------------------------------------ ctf_exchange_v2 (Polymarket V2)

/// `OrderFilled(bytes32 indexed orderHash, address indexed maker,
/// address indexed taker, Side side, uint256 tokenId,
/// uint256 makerAmountFilled, uint256 takerAmountFilled, uint256 fee,
/// bytes32 builder, bytes32 metadata)` - `Side`: 0 = BUY, 1 = SELL.
pub const EXCHANGE_V2_ORDER_FILLED: EventDef = event(
    "OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)",
    b256!(
        "d543adfd945773f1a62f74f0ee55a5e3b9b1a28262980ba90b1a89f2ea84d8ee"
    ),
    4,
    7,
);

/// `OrdersMatched(bytes32 indexed takerOrderHash, address indexed
/// takerOrderMaker, Side side, uint256 tokenId, uint256 makerAmountFilled,
/// uint256 takerAmountFilled)`
pub const EXCHANGE_V2_ORDERS_MATCHED: EventDef = event(
    "OrdersMatched(bytes32,address,uint8,uint256,uint256,uint256)",
    b256!(
        "174b3811690657c217184f89418266767c87e4805d09680c39fc9c031c0cab7c"
    ),
    3,
    4,
);

// ------------------------------------------------ fpmm (Gnosis / Omen AMM)

/// `FPMMBuy(address indexed buyer, uint investmentAmount, uint feeAmount,
/// uint indexed outcomeIndex, uint outcomeTokensBought)`
pub const FPMM_BUY: EventDef = event(
    "FPMMBuy(address,uint256,uint256,uint256,uint256)",
    b256!(
        "4f62630f51608fc8a7603a9391a5101e58bd7c276139366fc107dc3b67c3dcf8"
    ),
    3,
    3,
);

/// `FPMMSell(address indexed seller, uint returnAmount, uint feeAmount,
/// uint indexed outcomeIndex, uint outcomeTokensSold)`
pub const FPMM_SELL: EventDef = event(
    "FPMMSell(address,uint256,uint256,uint256,uint256)",
    b256!(
        "adcf2a240ed9300d681d9a3f5382b6c1beed1b7e46643e0c7b42cbe6e2d766b4"
    ),
    3,
    3,
);

// ------------------------------------ neg_risk (Polymarket NegRiskAdapter)

/// `MarketPrepared(bytes32 indexed marketId, address indexed oracle,
/// uint256 feeBips, bytes data)` - a multi outcome EVENT.
pub const NEG_RISK_MARKET_PREPARED: EventDef = dynamic(
    "MarketPrepared(bytes32,address,uint256,bytes)",
    b256!(
        "f059ab16d1ca60e123eab60e3c02b68faf060347c701a5d14885a8e1def7b3a8"
    ),
    3,
);

/// `QuestionPrepared(bytes32 indexed marketId, bytes32 indexed questionId,
/// uint256 index, bytes data)` - one binary market of an event.
pub const NEG_RISK_QUESTION_PREPARED: EventDef = dynamic(
    "QuestionPrepared(bytes32,bytes32,uint256,bytes)",
    b256!(
        "aac410f87d423a922a7b226ac68f0c2eaf5bf6d15e644ac0758c7f96e2c253f7"
    ),
    3,
);

/// `PositionSplit(address indexed stakeholder, bytes32 indexed
/// conditionId, uint256 amount)` - who really split through the adapter.
pub const NEG_RISK_POSITION_SPLIT: EventDef = event(
    "PositionSplit(address,bytes32,uint256)",
    b256!(
        "bbed930dbfb7907ae2d60ddf78345610214f26419a0128df39b6cc3d9e5df9b0"
    ),
    3,
    1,
);

/// `PositionsMerge(address indexed stakeholder, bytes32 indexed
/// conditionId, uint256 amount)`
pub const NEG_RISK_POSITIONS_MERGE: EventDef = event(
    "PositionsMerge(address,bytes32,uint256)",
    b256!(
        "ba33ac50d8894676597e6e35dc09cff59854708b642cd069d21eb9c7ca072a04"
    ),
    3,
    1,
);

/// `PayoutRedemption(address indexed redeemer, bytes32 indexed
/// conditionId, uint256[] amounts, uint256 payout)`
pub const NEG_RISK_PAYOUT_REDEMPTION: EventDef = dynamic(
    "PayoutRedemption(address,bytes32,uint256[],uint256)",
    b256!(
        "9140a6a270ef945260c03894b3c6b3b2695e9d5101feef0ff24fec960cfd3224"
    ),
    3,
);

/// `PositionsConverted(address indexed stakeholder, bytes32 indexed
/// marketId, uint256 indexed indexSet, uint256 amount)` - NO positions of
/// the questions in `indexSet` become YES positions of the others.
pub const NEG_RISK_POSITIONS_CONVERTED: EventDef = event(
    "PositionsConverted(address,bytes32,uint256,uint256)",
    b256!(
        "b03d19dddbc72a87e735ff0ea3b57bef133ebe44e1894284916a84044deb367e"
    ),
    4,
    1,
);

// ------------------------------------------- uma (Polymarket UmaCtfAdapter)

/// `QuestionInitialized(bytes32 indexed questionID, uint256 indexed
/// requestTimestamp, address indexed creator, bytes ancillaryData,
/// address rewardToken, uint256 reward, uint256 proposalBond)`
pub const UMA_QUESTION_INITIALIZED: EventDef = dynamic(
    "QuestionInitialized(bytes32,uint256,address,bytes,address,uint256,uint256)",
    b256!(
        "eee0897acd6893adcaf2ba5158191b3601098ab6bece35c5d57874340b64c5b7"
    ),
    4,
);

/// `QuestionReset(bytes32 indexed questionID)`: the first proposal was
/// disputed and a new price request went out.
pub const UMA_QUESTION_RESET: EventDef = event(
    "QuestionReset(bytes32)",
    b256!(
        "7981b5832932948db4e32a4a16a0f44b2ce7ff088574afb9364b313f70f82e8f"
    ),
    2,
    0,
);

/// `QuestionFlagged(bytes32 indexed questionID)`: flagged for manual
/// (emergency) resolution.
pub const UMA_QUESTION_FLAGGED: EventDef = event(
    "QuestionFlagged(bytes32)",
    b256!(
        "2435a0347185933b12027c6f394a5fd9c03646dba233e956f50658719dfc0b35"
    ),
    2,
    0,
);

/// Every event the decoder knows.
pub const ALL: &[EventDef] = &[
    CTF_CONDITION_PREPARATION,
    CTF_CONDITION_RESOLUTION,
    CTF_POSITION_SPLIT,
    CTF_POSITIONS_MERGE,
    CTF_PAYOUT_REDEMPTION,
    ERC1155_TRANSFER_SINGLE,
    ERC1155_TRANSFER_BATCH,
    EXCHANGE_ORDER_FILLED,
    EXCHANGE_ORDERS_MATCHED,
    EXCHANGE_TOKEN_REGISTERED,
    EXCHANGE_V2_ORDER_FILLED,
    EXCHANGE_V2_ORDERS_MATCHED,
    FPMM_BUY,
    FPMM_SELL,
    NEG_RISK_MARKET_PREPARED,
    NEG_RISK_QUESTION_PREPARED,
    NEG_RISK_POSITION_SPLIT,
    NEG_RISK_POSITIONS_MERGE,
    NEG_RISK_PAYOUT_REDEMPTION,
    NEG_RISK_POSITIONS_CONVERTED,
    UMA_QUESTION_INITIALIZED,
    UMA_QUESTION_RESET,
    UMA_QUESTION_FLAGGED,
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

            // Indexed parameters can not outnumber the parameters.
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
}
