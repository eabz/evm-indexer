use std::{ops::Mul, str::FromStr};

use clickhouse::Row;
use ethabi::ethereum_types::U256;
use serde::{Deserialize, Serialize};

use crate::{chains::get_block_reward, utils::format::serialize_u256};

use super::{block::DatabaseBlock, receipt::DatabaseReceipt};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlockReward {
    #[serde(with = "serialize_u256")]
    pub base_block_reward: U256,
    #[serde(with = "serialize_u256")]
    pub burned: U256,
    pub chain: u64,
    pub hash: String,
    pub miner: String,
    #[serde(with = "serialize_u256")]
    pub total_fee_reward: U256,
}

impl DatabaseBlockReward {
    pub fn calculate(
        block: &DatabaseBlock,
        receipts: &[DatabaseReceipt],
        chain: u64,
    ) -> Self {
        let (base_block_reward, total_fee_reward) =
            get_block_reward(chain, block, receipts);

        let burned = match block.base_fee_per_gas {
            Some(base_fee_per_gas) => base_fee_per_gas.mul(block.gas_used),
            None => U256::from_str("0x0").unwrap(),
        };

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
