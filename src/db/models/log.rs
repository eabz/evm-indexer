use clickhouse::Row;
use ethers::types::Log;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_hash};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: String,
    pub chain: i64,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    pub hash: String,
    pub log_index: i32,
    pub log_type: Option<String>,
    pub removed: bool,
    pub topics: Vec<String>,
    pub transaction_log_index: Option<i32>,
    pub timestamp: i64,
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
            data: log.data.to_vec(),
            hash: format_hash(log.transaction_hash.unwrap()),
            removed: log.removed.unwrap(),
            log_index: log.log_index.unwrap().as_u32() as i32,
            log_type,
            transaction_log_index,
            timestamp,
        }
    }
}
