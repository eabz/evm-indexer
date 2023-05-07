use std::collections::HashMap;

use ethabi::ethereum_types::U256;
use ethers::types::{Block, Transaction};
use serde::{Deserialize, Serialize};

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
    block: &Block<Transaction>,
) -> (U256, U256, U256) {
    (U256([0, 0, 0, 0]), U256([0, 0, 0, 0]), U256([0, 0, 0, 0]))
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
    block: &Block<Transaction>,
) -> (U256, U256, U256) {
    (U256([0, 0, 0, 0]), U256([0, 0, 0, 0]), U256([0, 0, 0, 0]))
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
    block: &Block<Transaction>,
) -> (U256, U256, U256) {
    (U256([0, 0, 0, 0]), U256([0, 0, 0, 0]), U256([0, 0, 0, 0]))
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

pub fn get_block_reward(
    chain: u64,
    block: &Block<Transaction>,
) -> (U256, U256, U256) {
    match chain {
        1 => calculate_ethereum_block_reward(block),
        56 => calculate_bsc_block_reward(block),
        137 => calculate_polygon_block_reward(block),
        _ => panic!("invalid chain"),
    }
}
