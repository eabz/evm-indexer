use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::format::{SerAddress, SerB256, SerU256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC1155Transfer {
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    #[serde_as(as = "Vec<SerU256>")]
    pub amounts: Vec<U256>,
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "SerAddress")]
    pub from: Address,
    #[serde_as(as = "Vec<SerU256>")]
    pub ids: Vec<U256>,
    pub log_index: u16,
    pub log_type: Option<String>,
    #[serde_as(as = "SerAddress")]
    pub operator: Address,
    pub removed: bool,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub to: Address,
    #[serde_as(as = "SerAddress")]
    pub token_address: Address,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseERC1155Transfer {
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        let topic0 = log.topic0?;
        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        let single = "0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62"
            .parse::<B256>()
            .unwrap();
        let batch = "0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb"
            .parse::<B256>()
            .unwrap();

        if topic0 != single && topic0 != batch {
            return None;
        }

        let operator = Address::from_word(topic1);
        let from = Address::from_word(topic2);
        let to = Address::from_word(topic3);

        let mut ids = Vec::new();
        let mut amounts = Vec::new();

        if topic0 == single {
            if log.data.len() >= 64 {
                let id = U256::from_be_slice(&log.data[0..32]);
                let amount = U256::from_be_slice(&log.data[32..64]);
                ids.push(id);
                amounts.push(amount);
            }
        } else {
            return None; // Batch decoding skipped for now
        }

        Some(Self {
            address: log.address,
            amounts,
            block_number: log.block_number,
            chain: log.chain,
            from,
            ids,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            operator,
            removed: log.removed,
            timestamp: log.timestamp,
            to,
            token_address: log.address,
            transaction_hash: log.transaction_hash,
            transaction_log_index: log.transaction_log_index,
        })
    }
}
