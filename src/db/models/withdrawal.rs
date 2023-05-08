use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Withdrawal;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, serialize_u256};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub address: String,
    #[serde(with = "serialize_u256")]
    pub amount: U256,
    pub block_number: u64,
    pub chain: u64,
    pub timestamp: u64,
    pub validator_index: u64,
    pub withdrawal_index: u64,
}

impl DatabaseWithdrawal {
    pub fn from_rpc(
        withdrawal: &Withdrawal,
        chain: u64,
        block_number: u64,
        timestamp: u64,
    ) -> Self {
        Self {
            address: format_address(withdrawal.address),
            amount: withdrawal.amount,
            block_number,
            chain,
            timestamp,
            validator_index: withdrawal.validator_index.as_u64(),
            withdrawal_index: withdrawal.index.as_u64(),
        }
    }
}
