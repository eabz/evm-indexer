use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Log;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_bytes, format_hash};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: String,
    pub chain: u64,
    pub data: String,
    pub hash: String,
    pub log_index: U256,
    pub log_type: Option<String>,
    pub removed: bool,
    pub topic0: String,
    pub topic1: String,
    pub topic2: String,
    pub topic3: String,
    pub transaction_log_index: Option<U256>,
    pub timestamp: u64,
}

impl DatabaseLog {
    pub fn from_rpc(log: &Log, chain: i64, timestamp: i64) -> Self {
        let transaction_log_index = match log.transaction_log_index.clone() {
            None => None,
            Some(transaction_log_index) => Some(transaction_log_index.as_u32() as i32),
        };

        let log_type = match log.log_type.clone() {
            None => None,
            Some(log_type) => Some(log_type),
        };

        Self {
            address: format_address(log.address),
            chain,
            topics: log
                .topics
                .clone()
                .into_iter()
                .map(|topic| format_hash(topic))
                .collect(),
            data: format_bytes(&log.data),
            hash: format_hash(log.transaction_hash.unwrap()),
            removed: log.removed.unwrap(),
            log_index: log.log_index.unwrap().as_u32() as i32,
            log_type,
            transaction_log_index,
            timestamp,
        }
    }
}
