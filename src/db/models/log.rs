use alloy::primitives::{Address, Bytes, B256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::simple_types::Log;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, sat_u16, sat_u32,
    },
    format::{SerAddress, SerB256, SerBytes},
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "SerBytes")]
    pub data: Bytes,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    #[serde_as(as = "Option<SerB256>")]
    pub topic0: Option<B256>,
    #[serde_as(as = "Option<SerB256>")]
    pub topic1: Option<B256>,
    #[serde_as(as = "Option<SerB256>")]
    pub topic2: Option<B256>,
    #[serde_as(as = "Option<SerB256>")]
    pub topic3: Option<B256>,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    /// NOTE: despite its name this column has always stored the index of
    /// the log's TRANSACTION inside the block (not the index of the log
    /// inside the transaction). Kept as is so existing data stays
    /// consistent; renaming it is a schema change.
    pub transaction_log_index: Option<u16>,
}

impl DatabaseLog {
    pub fn from_hypersync(
        log: &Log,
        chain: u64,
        timestamp: u32,
    ) -> Result<Self> {
        let block_number = log
            .block_number
            .map(u64::from)
            .context("log without a block number in response")?;

        let log_index =
            log.log_index.map(u64::from).with_context(|| {
                format!("log without a log index in block {block_number}")
            })?;

        let topic = |index: usize| {
            log.topics
                .get(index)
                .and_then(|topic| topic.as_ref())
                .map(hash_to_b256)
        };

        Ok(Self {
            address: log
                .address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            block_number: sat_u32(block_number),
            chain,
            data: log.data.as_ref().map(data_to_bytes).unwrap_or_default(),
            log_index: sat_u16(log_index),
            // Never reported by nodes for mined logs; stays NULL.
            log_type: None,
            // Only canonical blocks are streamed.
            removed: false,
            timestamp,
            topic0: topic(0),
            topic1: topic(1),
            topic2: topic(2),
            topic3: topic(3),
            transaction_hash: log
                .transaction_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            transaction_log_index: log
                .transaction_index
                .map(|index| sat_u16(u64::from(index))),
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A log row with the given topics / data for decoder tests.
    pub fn log_with(topics: &[B256], data: Vec<u8>) -> DatabaseLog {
        DatabaseLog {
            address: Address::repeat_byte(0xaa),
            block_number: 100,
            chain: 1,
            data: Bytes::from(data),
            log_index: 7,
            log_type: None,
            removed: false,
            timestamp: 1_700_000_000,
            topic0: topics.first().copied(),
            topic1: topics.get(1).copied(),
            topic2: topics.get(2).copied(),
            topic3: topics.get(3).copied(),
            transaction_hash: B256::repeat_byte(0x77),
            transaction_log_index: Some(3),
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
    use hypersync_client::format::{
        Address as HsAddress, Data, Hash, LogArgument, UInt,
    };

    fn hypersync_log() -> Log {
        let mut log = Log {
            block_number: Some(UInt::from(55u64)),
            log_index: Some(UInt::from(70_000u64)),
            transaction_index: Some(UInt::from(4u64)),
            transaction_hash: Some(Hash::from([3u8; 32])),
            address: Some(HsAddress::from([5u8; 20])),
            data: Some(Data::from(vec![1u8, 2, 3])),
            ..Default::default()
        };
        log.topics.push(Some(LogArgument::from([9u8; 32])));
        log.topics.push(None);
        log.topics.push(None);
        log.topics.push(None);
        log
    }

    #[test]
    fn converts_and_saturates() {
        let row =
            DatabaseLog::from_hypersync(&hypersync_log(), 1, 42).unwrap();

        assert_eq!(row.block_number, 55);
        // 70_000 does not fit the UInt16 column.
        assert_eq!(row.log_index, u16::MAX);
        assert_eq!(row.timestamp, 42);
        assert_eq!(row.topic0, Some(B256::repeat_byte(9)));
        assert_eq!(row.topic1, None);
        assert_eq!(row.transaction_log_index, Some(4));
        assert_eq!(row.log_type, None);
        assert!(!row.removed);
        assert_eq!(row.data, Bytes::from(vec![1u8, 2, 3]));
    }

    #[test]
    fn log_without_topics_or_data_is_fine() {
        let log = Log {
            block_number: Some(UInt::from(1u64)),
            log_index: Some(UInt::from(0u64)),
            ..Default::default()
        };

        let row = DatabaseLog::from_hypersync(&log, 1, 0).unwrap();

        assert_eq!(row.topic0, None);
        assert!(row.data.is_empty());
        assert_eq!(row.address, Address::ZERO);
    }

    #[test]
    fn missing_identity_is_an_error() {
        assert!(
            DatabaseLog::from_hypersync(&Log::default(), 1, 0).is_err()
        );
    }
}
