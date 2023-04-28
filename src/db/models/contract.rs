use clickhouse::Row;
use ethers::types::TransactionReceipt;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_hash};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseContract {
    pub block: u64,
    pub contract_address: String,
    pub chain: u64,
    pub creator: String,
    pub hash: String,
}

impl DatabaseContract {
    pub fn from_rpc(receipt: &TransactionReceipt, chain: u64) -> Self {
        Self {
            block: receipt.block_number.unwrap().as_u64(),
            chain: chain.to_owned(),
            contract_address: format_address(
                receipt.contract_address.unwrap(),
            ),
            creator: format_address(receipt.from),
            hash: format_hash(receipt.transaction_hash),
        }
    }
}
