use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::{
    events::TRANSFER_EVENT_SIGNATURE,
    format::{SerAddress, SerB256, SerU256},
};

/// Row of `erc721_transfers`. Field names are the column names.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseERC721Transfer {
    pub chain: u64,
    pub block_number: u64,
    pub log_index: u32,
    pub transaction_index: u32,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub token_address: Address,
    #[serde_as(as = "SerAddress")]
    pub from: Address,
    #[serde_as(as = "SerAddress")]
    pub to: Address,
    #[serde_as(as = "SerU256")]
    pub id: U256,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
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
            chain: log.chain,
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_index: log.transaction_index,
            transaction_hash: log.transaction_hash,
            timestamp: log.timestamp,
            token_address: log.address,
            from: Address::from_word(topic1),
            to: Address::from_word(topic2),
            id: U256::from_be_bytes(topic3.0),
            epoch: 0,
            _version: 0,
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
    fn token_id_zero_is_a_transfer() {
        // The fourth topic is all zero bytes but PRESENT.
        let topics = [
            TRANSFER_EVENT_SIGNATURE,
            address_topic(1),
            address_topic(2),
            B256::ZERO,
        ];

        let transfer =
            DatabaseERC721Transfer::from_log(&log_with(&topics, vec![]))
                .unwrap();

        assert_eq!(transfer.id, U256::ZERO);
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
