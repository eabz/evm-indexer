use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::format::SerU256;

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC721Transfer {
    pub address: Address,
    pub block_number: u32,
    pub chain: u64,
    pub from: Address,
    #[serde_as(as = "SerU256")]
    pub id: U256,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub to: Address,
    pub token_address: Address,
    pub transaction_hash: B256,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseERC721Transfer {
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        let topic0 = log.topic0?;
        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        if topic0
            != "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
                .parse::<B256>()
                .unwrap()
        {
            return None;
        }

        let from = Address::from_word(topic1);
        let to = Address::from_word(topic2);
        let id = U256::from_be_bytes(topic3.0);

        Some(Self {
            address: log.address,
            block_number: log.block_number,
            chain: log.chain,
            from,
            id,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            removed: log.removed,
            timestamp: log.timestamp,
            to,
            token_address: log.address,
            transaction_hash: log.transaction_hash,
            transaction_log_index: log.transaction_log_index,
        })
    }
}
