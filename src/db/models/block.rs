use alloy::primitives::{Address, Bytes, B256, B64, U256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::simple_types::Block;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, nonce_to_b64,
        quantity_to_u256, quantity_to_u32, quantity_to_u64, sat_u32,
    },
    format::{SerAddress, SerB256, SerB64, SerBytes, SerU256},
};

/// Row of `blocks`. Field names are the column names.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    pub chain: u64,
    pub number: u64,
    #[serde_as(as = "SerB256")]
    pub hash: B256,
    #[serde_as(as = "SerB256")]
    pub parent_hash: B256,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub miner: Address,
    /// NULL before London.
    #[serde_as(as = "Option<SerU256>")]
    pub base_fee_per_gas: Option<U256>,
    #[serde_as(as = "SerU256")]
    pub difficulty: U256,
    /// Zero when not reported.
    #[serde_as(as = "SerU256")]
    pub total_difficulty: U256,
    #[serde_as(as = "SerBytes")]
    pub extra_data: Bytes,
    pub gas_limit: u64,
    pub gas_used: u64,
    /// Zero bytes when absent.
    #[serde_as(as = "SerB256")]
    pub mix_hash: B256,
    #[serde_as(as = "SerB64")]
    pub nonce: B64,
    #[serde_as(as = "SerB256")]
    pub receipts_root: B256,
    #[serde_as(as = "SerB256")]
    pub sha3_uncles: B256,
    pub size: u64,
    #[serde_as(as = "SerB256")]
    pub state_root: B256,
    pub transactions: u32,
    #[serde_as(as = "SerB256")]
    pub transactions_root: B256,
    #[serde_as(as = "Vec<SerB256>")]
    pub uncles: Vec<B256>,
    /// Zero bytes before Shanghai.
    #[serde_as(as = "SerB256")]
    pub withdrawals_root: B256,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseBlock {
    /// `transactions` is the number of transactions of the block (HyperSync
    /// blocks do not carry the transaction list, the caller counts them).
    ///
    /// `number` and `hash` identify the row and are the commit marker, so
    /// they are the only fields that are an error when missing. Everything
    /// else falls back to a zero value / NULL.
    pub fn from_hypersync(
        block: &Block,
        chain: u64,
        transactions: u64,
    ) -> Result<Self> {
        let number =
            block.number.context("block without a number in response")?;

        let hash =
            block.hash.as_ref().map(hash_to_b256).with_context(|| {
                format!("block {number} without a hash")
            })?;

        let opt_hash = |h: &Option<_>| {
            h.as_ref().map(hash_to_b256).unwrap_or_default()
        };

        let opt_u64 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u64).unwrap_or_default()
        };

        let opt_u256 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u256).unwrap_or_default()
        };

        Ok(Self {
            chain,
            number,
            hash,
            parent_hash: opt_hash(&block.parent_hash),
            timestamp: block
                .timestamp
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            miner: block
                .miner
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            base_fee_per_gas: block
                .base_fee_per_gas
                .as_ref()
                .map(quantity_to_u256),
            difficulty: opt_u256(&block.difficulty),
            total_difficulty: opt_u256(&block.total_difficulty),
            extra_data: block
                .extra_data
                .as_ref()
                .map(data_to_bytes)
                .unwrap_or_default(),
            gas_limit: opt_u64(&block.gas_limit),
            gas_used: opt_u64(&block.gas_used),
            mix_hash: opt_hash(&block.mix_hash),
            nonce: block
                .nonce
                .as_ref()
                .map(nonce_to_b64)
                .unwrap_or_default(),
            receipts_root: opt_hash(&block.receipts_root),
            sha3_uncles: opt_hash(&block.sha3_uncles),
            size: opt_u64(&block.size),
            state_root: opt_hash(&block.state_root),
            transactions: sat_u32(transactions),
            transactions_root: opt_hash(&block.transactions_root),
            uncles: block
                .uncles
                .as_ref()
                .map(|uncles| uncles.iter().map(hash_to_b256).collect())
                .unwrap_or_default(),
            withdrawals_root: opt_hash(&block.withdrawals_root),
            epoch: 0,
            _version: 0,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A block row with only the identity / chain linkage set.
    pub fn block_row(number: u64, hash: u8, parent: u8) -> DatabaseBlock {
        DatabaseBlock {
            chain: 1,
            number,
            hash: B256::repeat_byte(hash),
            parent_hash: B256::repeat_byte(parent),
            timestamp: 0,
            miner: Address::ZERO,
            base_fee_per_gas: None,
            difficulty: U256::ZERO,
            total_difficulty: U256::ZERO,
            extra_data: Bytes::new(),
            gas_limit: 0,
            gas_used: 0,
            mix_hash: B256::ZERO,
            nonce: B64::ZERO,
            receipts_root: B256::ZERO,
            sha3_uncles: B256::ZERO,
            size: 0,
            state_root: B256::ZERO,
            transactions: 0,
            transactions_root: B256::ZERO,
            uncles: vec![],
            withdrawals_root: B256::ZERO,
            epoch: 0,
            _version: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{Hash, Quantity};

    fn minimal() -> Block {
        Block {
            number: Some(19_000_000),
            hash: Some(Hash::from([1u8; 32])),
            ..Default::default()
        }
    }

    #[test]
    fn sparse_block_uses_defaults() {
        let row = DatabaseBlock::from_hypersync(&minimal(), 1, 3).unwrap();

        assert_eq!(row.number, 19_000_000);
        assert_eq!(row.transactions, 3);
        assert_eq!(row.base_fee_per_gas, None);
        assert_eq!(row.total_difficulty, U256::ZERO);
        assert_eq!(row.difficulty, U256::ZERO);
        assert_eq!(row.nonce, B64::ZERO);
        assert_eq!(row.mix_hash, B256::ZERO);
        assert_eq!(row.withdrawals_root, B256::ZERO);
        assert!(row.uncles.is_empty());
        assert_eq!(row._version, 0);
    }

    #[test]
    fn number_and_hash_are_required() {
        assert!(DatabaseBlock::from_hypersync(&Block::default(), 1, 0)
            .is_err());

        let mut block = minimal();
        block.hash = None;
        assert!(DatabaseBlock::from_hypersync(&block, 1, 0).is_err());
    }

    #[test]
    fn wide_values_are_kept() {
        let mut block = minimal();
        block.number = Some(u64::MAX);
        block.gas_limit = Some(Quantity::from(u64::MAX));
        block.gas_used = Some(Quantity::from(u32::MAX as u64 + 1));
        block.size = Some(Quantity::from(u32::MAX as u64 + 2));
        // 9 bytes: more than the UInt64 the old schema saturated at.
        block.base_fee_per_gas = Some(Quantity::from(vec![1u8; 9]));

        let row =
            DatabaseBlock::from_hypersync(&block, 1, 100_000).unwrap();

        assert_eq!(row.number, u64::MAX);
        assert_eq!(row.gas_limit, u64::MAX);
        assert_eq!(row.gas_used, u32::MAX as u64 + 1);
        assert_eq!(row.size, u32::MAX as u64 + 2);
        assert_eq!(
            row.base_fee_per_gas,
            Some(U256::from_be_slice(&[1u8; 9]))
        );
        assert_eq!(row.transactions, 100_000);
    }

    #[test]
    fn values_wider_than_the_column_saturate_instead_of_panicking() {
        let mut block = minimal();
        block.size = Some(Quantity::from(vec![1u8; 20]));
        block.timestamp = Some(Quantity::from(u64::MAX));

        let row = DatabaseBlock::from_hypersync(&block, 1, 0).unwrap();

        assert_eq!(row.size, u64::MAX);
        assert_eq!(row.timestamp, u32::MAX);
    }
}
