use alloy::primitives::{Address, Bytes, B256};
use alloy::rpc::types::Log;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: Address,
    pub block_number: u32,
    pub chain: u64,
    pub data: Bytes,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub topic0: Option<B256>,
    pub topic1: Option<B256>,
    pub topic2: Option<B256>,
    pub topic3: Option<B256>,
    pub transaction_hash: B256,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseLog {
    pub fn from_rpc(
        log: &Log,
        chain: u64,
        timestamp: u32,
        block_number: &u32,
    ) -> Self {
        let topic0 = if log.topics().is_empty() {
            None
        } else {
            Some(log.topics()[0])
        };

        let topic1 = if log.topics().len() < 2 {
            None
        } else {
            Some(log.topics()[1])
        };

        let topic2 = if log.topics().len() < 3 {
            None
        } else {
            Some(log.topics()[2])
        };

        let topic3 = if log.topics().len() < 4 {
            None
        } else {
            Some(log.topics()[3])
        };

        let transaction_log_index = log
            .transaction_index
            .map(|transaction_log_index| transaction_log_index as u16);

        Self {
            address: log.address(),
            block_number: *block_number,
            chain,
            data: log.data().data.clone(),
            log_index: log.log_index.unwrap() as u16,
            log_type: None,
            removed: log.removed,
            timestamp,
            topic0,
            topic1,
            topic2,
            topic3,

            transaction_hash: log.transaction_hash.unwrap(),
            transaction_log_index,
        }
    }
}
