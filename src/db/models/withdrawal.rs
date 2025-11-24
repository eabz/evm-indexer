use alloy::primitives::{Address, U256};
use alloy::rpc::types::Withdrawal;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::SerU256;

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub address: Address,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub block_number: u32,
    pub chain: u64,
    pub timestamp: u32,
    pub validator_index: u64,
    pub withdrawal_index: u64,
}

impl DatabaseWithdrawal {
    pub fn from_rpc(
        withdrawal: &Withdrawal,
        chain: u64,
        block_number: u32,
        timestamp: u32,
    ) -> Self {
        Self {
            address: withdrawal.address,
            amount: U256::from(withdrawal.amount),
            block_number,
            chain,
            timestamp,
            validator_index: withdrawal.index,
            withdrawal_index: withdrawal.index,
        }
    }
}
