use alloy::primitives::{Address, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::db::format::{SerAddress, SerU256};

#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub chain: u64,
    pub block_number: u64,
    pub withdrawal_index: u64,
    pub validator_index: u64,
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub timestamp: u32,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}
