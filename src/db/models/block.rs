use clickhouse::Row;
use ethers::{
    types::{Block, Transaction},
    utils::format_units,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BlockStatus {
    Unfinalized,
    Secure,
    Finalized,
}

impl BlockStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            BlockStatus::Unfinalized => "unfinalized",
            BlockStatus::Secure => "secure",
            BlockStatus::Finalized => "finalized",
        }
    }
}

use crate::utils::format::{format_address, format_hash, format_nonce, format_number};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    pub base_fee_per_gas: Option<f64>,
    pub chain: i64,
    pub difficulty: String,
    pub extra_data: Vec<u8>,
    pub gas_limit: i64,
    pub gas_used: i64,
    pub hash: String,
    pub logs_bloom: Vec<u8>,
    pub miner: String,
    pub mix_hash: String,
    pub nonce: String,
    pub number: i64,
    pub parent_hash: String,
    pub receipts_root: String,
    pub sha3_uncles: String,
    pub size: i32,
    pub state_root: String,
    pub status: BlockStatus,
    pub timestamp: i64,
    pub total_difficulty: String,
    pub transactions: i32,
    pub transactions_root: String,
    pub uncles: Vec<String>,
}

impl DatabaseBlock {
    pub fn from_rpc(block: &Block<Transaction>, chain: i64) -> Self {
        let base_fee_per_gas: Option<f64> = match block.base_fee_per_gas {
            None => None,
            Some(base_fee_per_gas) => Some(
                format_units(base_fee_per_gas, 18)
                    .unwrap()
                    .parse::<f64>()
                    .unwrap(),
            ),
        };

        Self {
            base_fee_per_gas,
            chain,
            difficulty: format_number(block.difficulty),
            extra_data: block.extra_data.to_vec(),
            gas_limit: block.gas_limit.as_u64() as i64,
            gas_used: block.gas_used.as_u64() as i64,
            hash: format_hash(block.hash.unwrap()),
            logs_bloom: block.logs_bloom.unwrap().as_bytes().to_vec(),
            miner: format_address(block.author.unwrap()),
            mix_hash: format_hash(block.mix_hash.unwrap()),
            nonce: format_nonce(block.nonce.unwrap()),
            number: block.number.unwrap().as_u64() as i64,
            parent_hash: format_hash(block.parent_hash),
            receipts_root: format_hash(block.receipts_root),
            sha3_uncles: format_hash(block.uncles_hash),
            size: block.size.unwrap().as_u32() as i32,
            status: BlockStatus::Unfinalized,
            state_root: format_hash(block.state_root),
            timestamp: block.timestamp.as_u64() as i64,
            transactions_root: format_hash(block.transactions_root),
            total_difficulty: format_number(block.total_difficulty.unwrap()),
            transactions: block.transactions.len() as i32,
            uncles: block
                .uncles
                .clone()
                .into_iter()
                .map(|uncle| format_hash(uncle))
                .collect(),
        }
    }
}
