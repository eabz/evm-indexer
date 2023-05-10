use clickhouse::Row;
use ethers::types::Withdrawal;
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::{format_address, SerU256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub address: String,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub block_number: u32,
    pub chain: u64,
    pub timestamp: u32,
    pub validator_index: u32,
    pub withdrawal_index: u32,
}

impl DatabaseWithdrawal {
    pub fn from_rpc(
        withdrawal: &Withdrawal,
        chain: u64,
        block_number: u32,
        timestamp: u32,
    ) -> Self {
        Self {
            address: format_address(withdrawal.address),
            amount: withdrawal.amount,
            block_number,
            chain,
            timestamp,
            validator_index: withdrawal.validator_index.as_usize() as u32,
            withdrawal_index: withdrawal.index.as_usize() as u32,
        }
    }
}
