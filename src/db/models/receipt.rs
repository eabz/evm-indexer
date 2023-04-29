use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::TransactionReceipt;
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    format_address, format_hash, opt_serialize_u256, serialize_u256,
};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseReceipt {
    pub chain: u64,
    pub contract_address: Option<String>,
    #[serde(with = "serialize_u256")]
    pub cumulative_gas_used: U256,
    #[serde(with = "opt_serialize_u256")]
    pub effective_gas_price: Option<U256>,
    #[serde(with = "opt_serialize_u256")]
    pub gas_used: Option<U256>,
    pub hash: String,
    pub status: u64,
}

impl DatabaseReceipt {
    pub fn from_rpc(receipt: &TransactionReceipt, chain: u64) -> Self {
        let contract_address: Option<String> =
            receipt.contract_address.map(format_address);

        let status: u64 = match receipt.status {
            Some(status) => status.as_u64(),
            None => 0,
        };

        Self {
            chain,
            contract_address,
            cumulative_gas_used: receipt.cumulative_gas_used,
            effective_gas_price: receipt.effective_gas_price,
            gas_used: receipt.gas_used,
            hash: format_hash(receipt.transaction_hash),
            status,
        }
    }
}
