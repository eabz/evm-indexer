use alloy::primitives::{Address, Bloom, Bytes, B256, B64, U256};
use alloy::rpc::types::Block;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::SerU256;

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    pub base_fee_per_gas: Option<u64>,
    pub chain: u64,
    #[serde_as(as = "SerU256")]
    pub difficulty: U256,
    pub extra_data: Bytes,
    pub gas_limit: u32,
    pub gas_used: u32,
    pub hash: B256,
    pub is_uncle: bool,
    pub logs_bloom: Bloom,
    pub miner: Address,
    pub mix_hash: Option<B256>,
    pub nonce: B64,
    pub number: u32,
    pub parent_hash: B256,
    pub receipts_root: B256,
    pub sha3_uncles: B256,
    pub size: u32,
    pub state_root: B256,
    pub timestamp: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub total_difficulty: Option<U256>,
    pub transactions: u16,
    pub transactions_root: B256,
    pub uncles: Vec<B256>,
    pub withdrawals_root: Option<B256>,
}

impl DatabaseBlock {
    pub fn from_rpc<T>(
        block: &Block<T>,
        chain: u64,
        is_uncle: bool,
    ) -> Self {
        Self {
            base_fee_per_gas: block
                .header
                .base_fee_per_gas
                .map(|v| v.try_into().unwrap()),
            chain,
            difficulty: block.header.difficulty,
            extra_data: block.header.extra_data.clone(),
            gas_limit: block.header.gas_limit.try_into().unwrap(),
            gas_used: block.header.gas_used.try_into().unwrap(),
            hash: block.header.hash.unwrap(),
            is_uncle,
            logs_bloom: block.header.logs_bloom,
            miner: block.header.miner,
            mix_hash: block.header.mix_hash,
            nonce: block.header.nonce.unwrap_or_default(),
            number: block.header.number.unwrap().try_into().unwrap(),
            parent_hash: block.header.parent_hash,
            receipts_root: block.header.receipts_root,
            sha3_uncles: block.header.uncles_hash,
            size: block.size.unwrap().try_into().unwrap(),
            state_root: block.header.state_root,
            timestamp: block.header.timestamp.try_into().unwrap(),
            total_difficulty: block.header.total_difficulty,
            transactions: block.transactions.len() as u16,
            transactions_root: block.header.transactions_root,
            uncles: block.uncles.clone(),
            withdrawals_root: block.header.withdrawals_root,
        }
    }
}
