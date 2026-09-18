use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::{
    events::{
        ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
        ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
    },
    format::{SerAddress, SerB256, SerU256},
};

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

const WORD: usize = 32;

/// Reads the ABI word starting at `offset`, `None` when out of bounds.
fn read_word(data: &[u8], offset: usize) -> Option<U256> {
    let end = offset.checked_add(WORD)?;
    data.get(offset..end).map(U256::from_be_slice)
}

/// Reads an ABI word that must be usable as an offset / length.
fn read_usize(data: &[u8], offset: usize) -> Option<usize> {
    usize::try_from(read_word(data, offset)?).ok()
}

/// Decodes a dynamic `uint256[]` whose head lives at `head_offset`.
fn read_u256_array(data: &[u8], head_offset: usize) -> Option<Vec<U256>> {
    let array_offset = read_usize(data, head_offset)?;
    let length = read_usize(data, array_offset)?;

    let first = array_offset.checked_add(WORD)?;
    let byte_length = length.checked_mul(WORD)?;
    let end = first.checked_add(byte_length)?;

    // Bounds check BEFORE allocating: `length` is attacker controlled.
    let items = data.get(first..end)?;

    let (words, _) = items.as_chunks::<WORD>();

    Some(words.iter().map(|word| U256::from_be_bytes(*word)).collect())
}

/// `TransferSingle` data: `(uint256 id, uint256 value)`.
fn decode_single(data: &[u8]) -> Option<(Vec<U256>, Vec<U256>)> {
    Some((vec![read_word(data, 0)?], vec![read_word(data, WORD)?]))
}

/// `TransferBatch` data: `(uint256[] ids, uint256[] values)`.
fn decode_batch(data: &[u8]) -> Option<(Vec<U256>, Vec<U256>)> {
    let ids = read_u256_array(data, 0)?;
    let amounts = read_u256_array(data, WORD)?;

    // EIP-1155 requires both arrays to have the same length.
    (ids.len() == amounts.len()).then_some((ids, amounts))
}

impl DatabaseERC1155Transfer {
    /// Decodes `TransferSingle` and `TransferBatch`. Malformed data returns
    /// `None`; nothing in here can panic on hostile input.
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        let topic0 = log.topic0?;
        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        let (ids, amounts) =
            if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE {
                decode_single(&log.data)?
            } else if topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE {
                decode_batch(&log.data)?
            } else {
                return None;
            };

        Some(Self {
            address: log.address,
            amounts,
            block_number: log.block_number,
            chain: log.chain,
            from: Address::from_word(topic2),
            ids,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            operator: Address::from_word(topic1),
            removed: log.removed,
            timestamp: log.timestamp,
            to: Address::from_word(topic3),
            token_address: log.address,
            transaction_hash: log.transaction_hash,
            transaction_log_index: log.transaction_log_index,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::{
        address_topic, log_with, word,
    };

    fn topics(signature: B256) -> Vec<B256> {
        vec![
            signature,
            address_topic(1),
            address_topic(2),
            address_topic(3),
        ]
    }

    /// ABI encoding of `(uint256[] ids, uint256[] values)`.
    fn encode_batch(ids: &[u64], values: &[u64]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(word(64));
        data.extend(word(64 + 32 + 32 * ids.len() as u64));
        data.extend(word(ids.len() as u64));
        ids.iter().for_each(|id| data.extend(word(*id)));
        data.extend(word(values.len() as u64));
        values.iter().for_each(|value| data.extend(word(*value)));
        data
    }

    fn u256s(values: &[u64]) -> Vec<U256> {
        values.iter().map(|v| U256::from(*v)).collect()
    }

    #[test]
    fn decodes_transfer_single() {
        let mut data = word(11);
        data.extend(word(500));

        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE),
            data,
        ))
        .unwrap();

        assert_eq!(transfer.operator, Address::repeat_byte(1));
        assert_eq!(transfer.from, Address::repeat_byte(2));
        assert_eq!(transfer.to, Address::repeat_byte(3));
        assert_eq!(transfer.ids, u256s(&[11]));
        assert_eq!(transfer.amounts, u256s(&[500]));
    }

    #[test]
    fn short_transfer_single_is_skipped() {
        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE),
            word(11),
        ))
        .is_none());
    }

    #[test]
    fn decodes_transfer_batch() {
        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE),
            encode_batch(&[1, 2, 3], &[10, 20, 30]),
        ))
        .unwrap();

        assert_eq!(transfer.ids, u256s(&[1, 2, 3]));
        assert_eq!(transfer.amounts, u256s(&[10, 20, 30]));
        assert_eq!(transfer.from, Address::repeat_byte(2));
    }

    #[test]
    fn decodes_an_empty_batch() {
        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE),
            encode_batch(&[], &[]),
        ))
        .unwrap();

        assert!(transfer.ids.is_empty());
        assert!(transfer.amounts.is_empty());
    }

    #[test]
    fn malformed_batches_return_none_and_never_panic() {
        let signature = topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE);
        let decode = |data: Vec<u8>| {
            DatabaseERC1155Transfer::from_log(&log_with(&signature, data))
        };

        // Empty / too short for the two offsets.
        assert!(decode(vec![]).is_none());
        assert!(decode(word(64)).is_none());

        // Offset pointing outside the data.
        let mut data = encode_batch(&[1], &[1]);
        data[..32].copy_from_slice(&word(10_000));
        assert!(decode(data).is_none());

        // Offset that does not even fit usize (2^256 - 1).
        let mut data = encode_batch(&[1], &[1]);
        data[..32].copy_from_slice(&[0xff; 32]);
        assert!(decode(data).is_none());

        // Length claiming far more items than the data holds: must not
        // allocate or overflow.
        let mut data = encode_batch(&[1], &[1]);
        data[64..96].copy_from_slice(&word(u64::MAX));
        assert!(decode(data).is_none());

        let mut data = encode_batch(&[1], &[1]);
        data[64..96].copy_from_slice(&[0xff; 32]);
        assert!(decode(data).is_none());

        // Truncated in the middle of the values.
        let mut data = encode_batch(&[1, 2], &[1, 2]);
        data.truncate(data.len() - 16);
        assert!(decode(data).is_none());

        // ids / values of different length.
        assert!(decode(encode_batch(&[1, 2], &[1])).is_none());
    }

    #[test]
    fn other_events_and_missing_topics_are_ignored() {
        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(B256::repeat_byte(1)),
            encode_batch(&[1], &[1]),
        ))
        .is_none());

        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE)[..3],
            encode_batch(&[1], &[1]),
        ))
        .is_none());
    }
}
