use std::{
    collections::HashMap,
    ops::{AddAssign, DivAssign, Mul, MulAssign, SubAssign},
    str::FromStr,
};

use ethers::types::TransactionReceipt;
use primitive_types::U256;
use serde::{Deserialize, Serialize};

use crate::db::models::block::DatabaseBlock;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BalanceAllocation {
    balance: U256,
}

#[derive(Debug, Clone)]
pub struct Chain {
    pub genesis_hash: &'static str,
    pub genesis_timestamp: u32,
    pub id: u64,
    pub name: &'static str,
    pub supports_blocks_receipts: bool,
    pub supports_trace_block: bool,
    pub has_miner_rewards: bool,
}

pub const ETHEREUM: Chain = Chain {
    genesis_hash: "0xd4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3",
    genesis_timestamp: 1438249573,
    id: 1,
    name: "ethereum",
    supports_blocks_receipts: true,
    supports_trace_block: true,
    has_miner_rewards: true,
};

fn calculate_ethereum_block_reward(
    block: &DatabaseBlock,
    receipts: Option<&HashMap<String, TransactionReceipt>>,
    uncles: &[DatabaseBlock],
    is_uncle: bool,
    uncle_parent_number: Option<u32>,
) -> (U256, U256, U256) {
    // The ETH base reward is 5 ETH
    let mut base_block_reward =
        U256::from_str("0x4563918244f40000").unwrap();

    // If the block is above the Bizantium fork the reward is 3 ETH
    if block.number > 4_370_000 {
        base_block_reward = U256::from_str("0x29a2241af62c0000").unwrap();
    }

    // If the block is above the Constantinople fork the reward is 3 ETH
    if block.number > 7_280_000 {
        base_block_reward = U256::from_str("0x1bc16d674ec80000").unwrap();
    }

    // If the block is above The Merge the base block reward is 0 ETH
    if block.number > 15_537_393 {
        base_block_reward = U256::zero();
    }

    let mut uncle_rewards = U256::zero();

    if !uncles.is_empty() {
        let base_uncle_reward =
            U256::from_str("0xde0b6b3a764000").unwrap();

        uncle_rewards = base_uncle_reward.mul(uncles.len());
    }

    if is_uncle {
        let mut uncle_block_reward = U256::zero();

        uncle_block_reward.add_assign(U256::from(8));
        uncle_block_reward.add_assign(U256::from(block.number));
        uncle_block_reward
            .sub_assign(U256::from(uncle_parent_number.unwrap()));

        uncle_block_reward.mul_assign(base_block_reward);

        uncle_block_reward.div_assign(U256::from(8));

        return (uncle_block_reward, U256::zero(), U256::zero());
    }

    (base_block_reward, get_total_fees(receipts), uncle_rewards)
}

pub const POLYGON: Chain = Chain {
    genesis_hash: "0xa9c28ce2141b56c474f1dc504bee9b01eb1bd7d1a507580d5519d4437a97de1b",
    genesis_timestamp: 1590814036,
    id: 137,
    name: "polygon",
    supports_blocks_receipts: true,
    supports_trace_block: true,
    has_miner_rewards: true,
};

fn calculate_polygon_block_reward(
    receipts: Option<&HashMap<String, TransactionReceipt>>,
) -> (U256, U256, U256) {
    (
        U256::from_dec_str("0").unwrap(),
        get_total_fees(receipts),
        U256::from_dec_str("0").unwrap(),
    )
}

pub const BSC: Chain = Chain {
    genesis_hash: "0x0d21840abff46b96c84b2ac9e10e4f5cdaeb5693cb665db62a2f3b02d2d57b5b",
    genesis_timestamp: 1598687048,
    id: 56,
    name: "bsc",
    supports_blocks_receipts: true,
    supports_trace_block: true,
    has_miner_rewards: true,
};

fn calculate_bsc_block_reward(
    receipts: Option<&HashMap<String, TransactionReceipt>>,
) -> (U256, U256, U256) {
    (
        U256::from_dec_str("0").unwrap(),
        get_total_fees(receipts),
        U256::from_dec_str("0").unwrap(),
    )
}

pub static CHAINS: [Chain; 3] = [ETHEREUM, POLYGON, BSC];

pub fn get_chains() -> HashMap<u64, Chain> {
    let mut chains: HashMap<u64, Chain> = HashMap::new();

    for chain in CHAINS.iter() {
        chains.insert(chain.id, chain.to_owned());
    }

    chains
}

pub fn get_chain(chain: u64) -> Chain {
    let chains = get_chains();

    let selected_chain = chains.get(&chain).expect("chain not found.");

    selected_chain.to_owned()
}

fn get_total_fees(
    receipts: Option<&HashMap<String, TransactionReceipt>>,
) -> U256 {
    let mut fees_reward = U256::zero();

    if let Some(receipts) = receipts {
        for receipt in receipts.values() {
            let reward = receipt
                .gas_used
                .unwrap()
                .mul(receipt.effective_gas_price.unwrap());

            fees_reward.add_assign(reward);
        }
    }

    fees_reward
}

pub fn get_block_reward(
    chain: u64,
    block: &DatabaseBlock,
    receipts: Option<&HashMap<String, TransactionReceipt>>,
    uncles: &[DatabaseBlock],
    is_uncle: bool,
    uncle_parent_number: Option<u32>,
) -> (U256, U256, U256) {
    match chain {
        1 => calculate_ethereum_block_reward(
            block,
            receipts,
            uncles,
            is_uncle,
            uncle_parent_number,
        ),
        56 => calculate_bsc_block_reward(receipts),
        137 => calculate_polygon_block_reward(receipts),
        _ => panic!("invalid chain"),
    }
}
