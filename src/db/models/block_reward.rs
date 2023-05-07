use clickhouse::Row;
use ethabi::ethereum_types::U256;
use serde::{Deserialize, Serialize};

use crate::chains::get_block_reward;

use super::{block::DatabaseBlock, receipt::DatabaseReceipt};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlockReward {
    pub base_block_reward: U256,
    pub burned: U256,
    pub chain: u64,
    pub hash: String,
    pub miner: String,
    pub total_fee_reward: U256,
}

impl DatabaseBlockReward {
    pub fn calculate(
        block: &DatabaseBlock,
        receipts: &Vec<DatabaseReceipt>,
        chain: u64,
    ) -> Self {
        let (base_block_reward, total_fee_reward, burned) =
            get_block_reward(chain, block, receipts);

        Self {
            base_block_reward,
            burned,
            chain,
            hash: block.hash.clone(),
            miner: block.miner.clone(),
            total_fee_reward,
        }
    }
}
