use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::{
    events::TRANSFER_EVENT_SIGNATURE,
    format::{SerAddress, SerB256, SerU256},
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC721Transfer {
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "SerAddress")]
    pub from: Address,
    #[serde_as(as = "SerU256")]
    pub id: U256,
    pub log_index: u16,
    pub log_type: Option<String>,
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

impl DatabaseERC721Transfer {
    /// `Transfer(address indexed from, address indexed to, uint256 indexed
    /// tokenId)`: four topics, no data needed.
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        if log.topic0? != TRANSFER_EVENT_SIGNATURE {
            return None;
        }

        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        Some(Self {
            address: log.address,
            block_number: log.block_number,
            chain: log.chain,
            from: Address::from_word(topic1),
            id: U256::from_be_bytes(topic3.0),
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            removed: log.removed,
            timestamp: log.timestamp,
            to: Address::from_word(topic2),
            token_address: log.address,
            transaction_hash: log.transaction_hash,
            transaction_log_index: log.transaction_log_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::{address_topic, log_with};

    #[test]
    fn decodes_token_id_from_the_fourth_topic() {
        let topics = [
            TRANSFER_EVENT_SIGNATURE,
            address_topic(1),
            address_topic(2),
            B256::from(U256::from(4242u64)),
        ];

        let transfer =
            DatabaseERC721Transfer::from_log(&log_with(&topics, vec![]))
                .unwrap();

        assert_eq!(transfer.from, Address::repeat_byte(1));
        assert_eq!(transfer.to, Address::repeat_byte(2));
        assert_eq!(transfer.id, U256::from(4242u64));
    }

    #[test]
    fn three_topics_is_not_erc721() {
        let topics =
            [TRANSFER_EVENT_SIGNATURE, address_topic(1), address_topic(2)];

        assert!(DatabaseERC721Transfer::from_log(&log_with(
            &topics,
            vec![]
        ))
        .is_none());
    }
}
