use alloy::primitives::{Address, Bytes, B256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::db::format::{SerAddress, SerB256, SerBytes, SerTopic};

/// Row of `logs`. Field names are the column names.
///
/// The topic columns are NOT nullable: an absent topic is stored as 32 zero
/// bytes. In Rust they stay `Option`, because decoders need to tell "three
/// topics" (ERC20 `Transfer`) from "four topics, the last one zero" (ERC721
/// `Transfer` of token id 0). `topic_count` keeps that distinction in SQL.
///
/// Reading a row back goes through [`StoredLog`], which uses `topic_count`
/// to restore the `Option`s exactly.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
#[serde(from = "StoredLog")]
pub struct DatabaseLog {
    pub chain: u64,
    pub block_number: u64,
    pub log_index: u32,
    /// Index of the log's transaction inside the block.
    pub transaction_index: u32,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    /// Number of topics (0-4), i.e. the position of the first `None`.
    pub topic_count: u8,
    #[serde_as(as = "SerTopic")]
    pub topic0: Option<B256>,
    #[serde_as(as = "SerTopic")]
    pub topic1: Option<B256>,
    #[serde_as(as = "SerTopic")]
    pub topic2: Option<B256>,
    #[serde_as(as = "SerTopic")]
    pub topic3: Option<B256>,
    #[serde_as(as = "SerBytes")]
    pub data: Bytes,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

/// A `logs` row as stored: four non nullable topics.
#[serde_as]
#[derive(Deserialize)]
struct StoredLog {
    chain: u64,
    block_number: u64,
    log_index: u32,
    transaction_index: u32,
    #[serde_as(as = "SerB256")]
    transaction_hash: B256,
    timestamp: u32,
    #[serde_as(as = "SerAddress")]
    address: Address,
    topic_count: u8,
    #[serde_as(as = "SerB256")]
    topic0: B256,
    #[serde_as(as = "SerB256")]
    topic1: B256,
    #[serde_as(as = "SerB256")]
    topic2: B256,
    #[serde_as(as = "SerB256")]
    topic3: B256,
    #[serde_as(as = "SerBytes")]
    data: Bytes,
    epoch: u32,
    _version: u64,
}

impl From<StoredLog> for DatabaseLog {
    fn from(log: StoredLog) -> Self {
        let topic = |index: u8, topic: B256| {
            (index < log.topic_count).then_some(topic)
        };

        Self {
            chain: log.chain,
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_index: log.transaction_index,
            transaction_hash: log.transaction_hash,
            timestamp: log.timestamp,
            address: log.address,
            topic_count: log.topic_count,
            topic0: topic(0, log.topic0),
            topic1: topic(1, log.topic1),
            topic2: topic(2, log.topic2),
            topic3: topic(3, log.topic3),
            data: log.data,
            epoch: log.epoch,
            _version: log._version,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A log row with the given topics / data for decoder tests.
    pub fn log_with(topics: &[B256], data: Vec<u8>) -> DatabaseLog {
        DatabaseLog {
            chain: 1,
            block_number: 100,
            log_index: 7,
            transaction_index: 3,
            transaction_hash: B256::repeat_byte(0x77),
            timestamp: 1_700_000_000,
            address: Address::repeat_byte(0xaa),
            topic_count: topics.len().min(4) as u8,
            topic0: topics.first().copied(),
            topic1: topics.get(1).copied(),
            topic2: topics.get(2).copied(),
            topic3: topics.get(3).copied(),
            data: Bytes::from(data),
            epoch: 0,
            _version: 0,
        }
    }

    /// 32 byte big endian word.
    pub fn word(value: u64) -> Vec<u8> {
        alloy::primitives::U256::from(value).to_be_bytes::<32>().to_vec()
    }

    pub fn address_topic(byte: u8) -> B256 {
        Address::repeat_byte(byte).into_word()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_rows_restore_the_options_from_topic_count() {
        let mut log = test_support::log_with(
            &[B256::repeat_byte(1), B256::ZERO, B256::repeat_byte(3)],
            vec![1, 2],
        );
        log._version = 9;

        let json = serde_json::to_string(&log).unwrap();
        let back: DatabaseLog = serde_json::from_str(&json).unwrap();

        assert_eq!(back, log);
        assert_eq!(back.topic1, Some(B256::ZERO));
        assert_eq!(back.topic3, None);
    }
}
