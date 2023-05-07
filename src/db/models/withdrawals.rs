use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Withdrawal;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, serialize_u256};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub chain: u64,
    pub index: u64,
    pub validator_index: u64,
    pub address: String,
    #[serde(with = "serialize_u256")]
    pub amount: U256,
}

impl DatabaseWithdrawal {
    pub fn from_rpc(withdrawal: &Withdrawal, chain: u64) -> Self {
        Self {
            chain,
            index: withdrawal.index.as_u64(),
            validator_index: withdrawal.validator_index.as_u64(),
            address: format_address(withdrawal.address),
            amount: withdrawal.amount,
        }
    }
}
