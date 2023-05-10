use clickhouse::Row;
use ethers::types::TransactionReceipt;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_hash};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseContract {
    pub block_number: u32,
    pub chain: u64,
    pub contract_address: String,
    pub creator: String,
    pub transaction_hash: String,
}

impl DatabaseContract {
    pub fn from_rpc(receipt: &TransactionReceipt, chain: u64) -> Self {
        Self {
            block_number: receipt.block_number.unwrap().as_usize() as u32,
            chain,
            contract_address: format_address(
                receipt.contract_address.unwrap(),
            ),
            creator: format_address(receipt.from),
            transaction_hash: format_hash(receipt.transaction_hash),
        }
    }
}
