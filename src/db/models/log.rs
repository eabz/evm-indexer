use clickhouse::Row;
use ethabi::ethereum_types::U256;
use ethers::types::Log;
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    format_address, format_bytes, format_hash, opt_serialize_u256,
    serialize_u256,
};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: String,
    pub chain: u64,
    pub data: String,
    #[serde(with = "serialize_u256")]
    pub log_index: U256,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub topic0: Option<String>,
    pub topic1: Option<String>,
    pub topic2: Option<String>,
    pub topic3: Option<String>,
    pub transaction_hash: String,
    #[serde(with = "opt_serialize_u256")]
    pub transaction_log_index: Option<U256>,
}

impl DatabaseLog {
    pub fn from_rpc(log: &Log, chain: u64, timestamp: u32) -> Self {
        let topic0 = if log.topics.is_empty() {
            None
        } else {
            Some(format_hash(log.topics[0]))
        };

        let topics = log.topics.len();

        let topic1 = if topics < 2 {
            None
        } else {
            Some(format_hash(log.topics[1]))
        };

        let topic2 = if topics < 3 {
            None
        } else {
            Some(format_hash(log.topics[2]))
        };

        let topic3 = if topics < 4 {
            None
        } else {
            Some(format_hash(log.topics[3]))
        };

        Self {
            address: format_address(log.address),
            chain,
            data: format_bytes(&log.data),
            log_index: log.log_index.unwrap(),
            log_type: log.log_type.clone(),
            removed: log.removed.unwrap(),
            timestamp,
            topic0,
            topic1,
            topic2,
            topic3,
            transaction_hash: format_hash(log.transaction_hash.unwrap()),
            transaction_log_index: log.transaction_log_index,
        }
    }
}
