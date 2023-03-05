use ethers::types::TransactionReceipt;
use field_count::FieldCount;

use crate::utils::format::{format_address, format_hash};

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseContract {
    pub block: i64,
    pub contract_address: String,
    pub chain: i64,
    pub creator: String,
    pub hash: String,
}

impl DatabaseContract {
    pub fn from_rpc(receipt: &TransactionReceipt, chain: i64) -> Self {
        Self {
            block: receipt.block_number.unwrap().as_u64() as i64,
            chain: chain.to_owned(),
            contract_address: format_address(receipt.contract_address.unwrap()),
            creator: format_address(receipt.from),
            hash: format_hash(receipt.transaction_hash),
        }
    }
}
