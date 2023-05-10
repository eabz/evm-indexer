use clickhouse::Row;
use ethers::types::Block;
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::{
    format_address, format_bytes, format_bytes_slice, format_hash,
    format_nonce, SerU256,
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    #[serde_as(as = "SerU256")]
    pub base_block_reward: U256,
    pub base_fee_per_gas: Option<u64>,
    #[serde_as(as = "SerU256")]
    pub burned: U256,
    pub chain: u64,
    #[serde_as(as = "SerU256")]
    pub difficulty: U256,
    pub extra_data: String,
    pub gas_limit: u32,
    pub gas_used: u32,
    pub hash: String,
    pub is_uncle: bool,
    pub logs_bloom: String,
    pub miner: String,
    pub mix_hash: Option<String>,
    pub nonce: String,
    pub number: u32,
    pub parent_hash: String,
    pub receipts_root: String,
    pub sha3_uncles: String,
    pub size: u32,
    pub state_root: String,
    pub timestamp: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub total_difficulty: Option<U256>,
    #[serde_as(as = "SerU256")]
    pub total_fee_reward: U256,
    pub transactions: u16,
    pub transactions_root: String,
    pub uncles: Vec<String>,
    #[serde_as(as = "SerU256")]
    pub uncle_rewards: U256,
    pub withdrawals_root: Option<String>,
}

impl DatabaseBlock {
    pub fn from_rpc<T>(
        block: &Block<T>,
        chain: u64,
        is_uncle: bool,
    ) -> Self {
        let withdrawals_root: Option<String> =
            block.withdrawals_root.map(format_hash);

        let base_fee_per_gas: Option<u64> = block
            .base_fee_per_gas
            .map(|base_fee_per_gas| base_fee_per_gas.as_u64());

        Self {
            base_block_reward: U256::zero(),
            base_fee_per_gas,
            burned: U256::zero(),
            chain,
            difficulty: block.difficulty,
            extra_data: format_bytes(&block.extra_data),
            gas_limit: block.gas_limit.as_usize() as u32,
            gas_used: block.gas_used.as_usize() as u32,
            hash: format_hash(block.hash.unwrap()),
            is_uncle,
            logs_bloom: format_bytes_slice(
                block.logs_bloom.unwrap().as_bytes(),
            ),
            miner: format_address(block.author.unwrap()),
            mix_hash: block.mix_hash.map(format_hash),
            nonce: format_nonce(block.nonce.unwrap()),
            number: block.number.unwrap().as_usize() as u32,
            parent_hash: format_hash(block.parent_hash),
            receipts_root: format_hash(block.receipts_root),
            sha3_uncles: format_hash(block.uncles_hash),
            size: block.size.unwrap().as_usize() as u32,
            state_root: format_hash(block.state_root),
            timestamp: block.timestamp.as_usize() as u32,
            total_difficulty: block.total_difficulty,
            total_fee_reward: U256::zero(),
            transactions: block.transactions.len() as u16,
            transactions_root: format_hash(block.transactions_root),
            uncles: block
                .uncles
                .clone()
                .into_iter()
                .map(format_hash)
                .collect(),
            uncle_rewards: U256::zero(),
            withdrawals_root,
        }
    }

    pub fn add_rewards(
        &mut self,
        base_block_reward: U256,
        burned: U256,
        total_fee_reward: U256,
        uncle_rewards: U256,
    ) {
        self.base_block_reward = base_block_reward;
        self.burned = burned;
        self.total_fee_reward = total_fee_reward;
        self.uncle_rewards = uncle_rewards;
    }
}
