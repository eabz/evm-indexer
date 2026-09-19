use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::{
    db::format::{SerAddress, SerB256, SerU256},
    utils::events::TRANSFER_EVENT_SIGNATURE,
};

/// Row of `erc20_transfers`. Field names are the column names.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseERC20Transfer {
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
    pub amount: U256,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseERC20Transfer {
    /// `Transfer(address indexed from, address indexed to, uint256 value)`:
    /// exactly three topics and the amount as the first data word.
    ///
    /// Log data is attacker controlled. Only the first 32 bytes are read
    /// (extra bytes are ignored) and a log with less than one full word is
    /// not a decodable ERC20 transfer, so it is skipped (`None`).
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        if log.topic0? != TRANSFER_EVENT_SIGNATURE {
            return None;
        }

        let topic1 = log.topic1?;
        let topic2 = log.topic2?;

        if log.topic3.is_some() {
            return None;
        }

        let amount = U256::from_be_slice(log.data.get(..32)?);

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
            amount,
            epoch: 0,
            _version: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::{
        address_topic, log_with, word,
    };

    fn topics() -> Vec<B256> {
        vec![TRANSFER_EVENT_SIGNATURE, address_topic(1), address_topic(2)]
    }

    #[test]
    fn decodes_a_standard_transfer() {
        let log = log_with(&topics(), word(1_000));
        let transfer = DatabaseERC20Transfer::from_log(&log).unwrap();

        assert_eq!(transfer.from, Address::repeat_byte(1));
        assert_eq!(transfer.to, Address::repeat_byte(2));
        assert_eq!(transfer.amount, U256::from(1_000u64));
        assert_eq!(transfer.token_address, log.address);
        assert_eq!(transfer.log_index, 7);
        assert_eq!(transfer.transaction_index, 3);
    }

    #[test]
    fn data_longer_than_one_word_does_not_panic() {
        // A hostile token can emit Transfer with arbitrary extra data.
        let mut data = word(5);
        data.extend_from_slice(&[0xff; 100]);

        let transfer =
            DatabaseERC20Transfer::from_log(&log_with(&topics(), data))
                .unwrap();

        // Only the first word is the amount.
        assert_eq!(transfer.amount, U256::from(5u64));
    }

    #[test]
    fn max_amount_is_kept() {
        let transfer = DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![0xff; 32],
        ))
        .unwrap();
        assert_eq!(transfer.amount, U256::MAX);
    }

    #[test]
    fn short_or_empty_data_is_skipped() {
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![]
        ))
        .is_none());
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![1; 31]
        ))
        .is_none());
    }

    #[test]
    fn other_shapes_are_not_erc20() {
        // ERC721: four topics.
        let mut four = topics();
        four.push(B256::repeat_byte(9));
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &four,
            word(1)
        ))
        .is_none());

        // Too few topics.
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics()[..2],
            word(1)
        ))
        .is_none());

        // Another event.
        let mut other = topics();
        other[0] = B256::repeat_byte(1);
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &other,
            word(1)
        ))
        .is_none());
    }
}
