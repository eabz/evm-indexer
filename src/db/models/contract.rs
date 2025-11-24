use alloy::primitives::{Address, B256};
use alloy::rpc::types::TransactionReceipt;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::{SerAddress, SerB256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseContract {
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "SerAddress")]
    pub contract_address: Address,
    #[serde_as(as = "SerAddress")]
    pub creator: Address,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
}

impl DatabaseContract {
    pub fn from_rpc(receipt: &TransactionReceipt, chain: u64) -> Self {
        Self {
            block_number: receipt.block_number.unwrap() as u32,
            chain,
            contract_address: receipt.contract_address.unwrap(),
            creator: receipt.from,
            transaction_hash: receipt.transaction_hash,
        }
    }
}
