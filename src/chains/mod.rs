use std::{
    collections::HashMap,
    ops::{Add, Mul},
    str::FromStr,
};

use ethabi::ethereum_types::U256;
use serde::{Deserialize, Serialize};

use crate::db::models::{block::DatabaseBlock, receipt::DatabaseReceipt};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BalanceAllocation {
    balance: U256,
}

#[derive(Debug, Clone)]
pub struct Chain {
    pub genesis_hash: &'static str,
    pub genesis_timestamp: u64,
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
    receipts: &[DatabaseReceipt],
) -> (U256, U256) {
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
        base_block_reward = U256::from_str("0x0").unwrap();
    }

    (base_block_reward, get_total_fees(receipts))
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
    _block: &DatabaseBlock,
    receipts: &[DatabaseReceipt],
) -> (U256, U256) {
    (U256::from_dec_str("0").unwrap(), get_total_fees(receipts))
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
    _block: &DatabaseBlock,
    receipts: &[DatabaseReceipt],
) -> (U256, U256) {
    (U256::from_dec_str("0").unwrap(), get_total_fees(receipts))
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

fn get_total_fees(receipts: &[DatabaseReceipt]) -> U256 {
    let mut fees_reward = U256::from_str("0x0").unwrap();

    for receipt in receipts {
        let reward = receipt
            .gas_used
            .unwrap()
            .mul(receipt.effective_gas_price.unwrap());

        fees_reward = fees_reward.add(reward);
    }

    fees_reward
}

pub fn get_block_reward(
    chain: u64,
    block: &DatabaseBlock,
    receipts: &[DatabaseReceipt],
) -> (U256, U256) {
    match chain {
        1 => calculate_ethereum_block_reward(block, receipts),
        56 => calculate_bsc_block_reward(block, receipts),
        137 => calculate_polygon_block_reward(block, receipts),
        _ => panic!("invalid chain"),
    }
}
