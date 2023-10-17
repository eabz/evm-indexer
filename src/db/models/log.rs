use clickhouse::Row;
use ethers::types::Log;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::{format_address, format_bytes, format_hash};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: String,
    pub block_number: u32,
    pub chain: u64,
    pub data: String,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub topic0: String,
    pub topic1: Option<String>,
    pub topic2: Option<String>,
    pub topic3: Option<String>,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseLog {
    pub fn from_rpc(
        log: &Log,
        chain: u64,
        timestamp: u32,
        block_number: &u32,
    ) -> Self {
        let topic0 = if log.topics.is_empty() {
            String::from("0x")
        } else {
            format_hash(log.topics[0])
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

        let transaction_log_index: Option<u16> =
            log.transaction_log_index.map(|transaction_log_index| {
                transaction_log_index.as_usize() as u16
            });

        Self {
            address: format_address(log.address),
            block_number: block_number.to_owned(),
            chain,
            data: format_bytes(&log.data),
            log_index: log.log_index.unwrap().as_usize() as u16,
            log_type: log.log_type.clone(),
            removed: log.removed.unwrap(),
            timestamp,
            topic0,
            topic1,
            topic2,
            topic3,
            transaction_hash: format_hash(log.transaction_hash.unwrap()),
            transaction_log_index,
        }
    }
}
