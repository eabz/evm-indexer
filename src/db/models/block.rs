use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Block;
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    format_address, format_bytes, format_bytes_slice, format_hash,
    format_nonce, opt_serialize_u256, serialize_u256,
};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    #[serde(with = "opt_serialize_u256")]
    pub base_fee_per_gas: Option<U256>,
    pub chain: u64,
    #[serde(with = "serialize_u256")]
    pub difficulty: U256,
    pub extra_data: String,
    #[serde(with = "serialize_u256")]
    pub gas_limit: U256,
    #[serde(with = "serialize_u256")]
    pub gas_used: U256,
    pub hash: String,
    pub logs_bloom: String,
    pub miner: String,
    pub mix_hash: String,
    pub nonce: String,
    pub number: u64,
    pub parent_hash: String,
    pub receipts_root: String,
    pub sha3_uncles: String,
    #[serde(with = "opt_serialize_u256")]
    pub size: Option<U256>,
    pub state_root: String,
    pub timestamp: u64,
    #[serde(with = "opt_serialize_u256")]
    pub total_difficulty: Option<U256>,
    pub transactions: u64,
    pub transactions_root: String,
    pub uncles: Vec<String>,
    pub withdrawals_root: Option<String>,
}

impl DatabaseBlock {
    pub fn from_rpc<T>(block: &Block<T>, chain: u64) -> Self {
        let withdrawals_root: Option<String> =
            block.withdrawals_root.map(format_hash);

        Self {
            base_fee_per_gas: block.base_fee_per_gas,
            chain,
            difficulty: block.difficulty,
            extra_data: format_bytes(&block.extra_data),
            gas_limit: block.gas_limit,
            gas_used: block.gas_used,
            hash: format_hash(block.hash.unwrap()),
            logs_bloom: format_bytes_slice(
                block.logs_bloom.unwrap().as_bytes(),
            ),
            miner: format_address(block.author.unwrap()),
            mix_hash: format_hash(block.mix_hash.unwrap()),
            nonce: format_nonce(block.nonce.unwrap()),
            number: block.number.unwrap().as_u64(),
            parent_hash: format_hash(block.parent_hash),
            receipts_root: format_hash(block.receipts_root),
            sha3_uncles: format_hash(block.uncles_hash),
            size: block.size,
            state_root: format_hash(block.state_root),
            timestamp: block.timestamp.as_u64(),
            total_difficulty: block.total_difficulty,
            transactions: block.transactions.len() as u64,
            transactions_root: format_hash(block.transactions_root),
            uncles: block
                .uncles
                .clone()
                .into_iter()
                .map(format_hash)
                .collect(),
            withdrawals_root,
        }
    }
}
