use alloy::primitives::{Address, Bytes, B256, B64, U256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::simple_types::Block;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, nonce_to_b64,
        quantity_to_u256, quantity_to_u32, quantity_to_u64, sat_u16,
        sat_u32,
    },
    format::{SerAddress, SerB256, SerB64, SerBytes, SerU256, SerVecB256},
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseBlock {
    pub base_fee_per_gas: Option<u64>,
    pub chain: u64,
    #[serde_as(as = "SerU256")]
    pub difficulty: U256,
    #[serde_as(as = "SerBytes")]
    pub extra_data: Bytes,
    pub gas_limit: u32,
    pub gas_used: u32,
    #[serde_as(as = "SerB256")]
    pub hash: B256,
    pub is_uncle: bool,
    /// Kept as raw bytes (serialized as 0x-hex exactly like before) so a
    /// chain with a non standard bloom size can never fail a conversion.
    #[serde_as(as = "SerBytes")]
    pub logs_bloom: Bytes,
    #[serde_as(as = "SerAddress")]
    pub miner: Address,
    #[serde_as(as = "Option<SerB256>")]
    pub mix_hash: Option<B256>,
    #[serde_as(as = "SerB64")]
    pub nonce: B64,
    pub number: u32,
    #[serde_as(as = "SerB256")]
    pub parent_hash: B256,
    #[serde_as(as = "SerB256")]
    pub receipts_root: B256,
    #[serde_as(as = "SerB256")]
    pub sha3_uncles: B256,
    pub size: u32,
    #[serde_as(as = "SerB256")]
    pub state_root: B256,
    pub timestamp: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub total_difficulty: Option<U256>,
    pub transactions: u16,
    #[serde_as(as = "SerB256")]
    pub transactions_root: B256,
    #[serde_as(as = "SerVecB256")]
    pub uncles: Vec<B256>,
    #[serde_as(as = "Option<SerB256>")]
    pub withdrawals_root: Option<B256>,
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

        Ok(Self {
            base_fee_per_gas: block
                .base_fee_per_gas
                .as_ref()
                .map(quantity_to_u64),
            chain,
            difficulty: block
                .difficulty
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
            extra_data: block
                .extra_data
                .as_ref()
                .map(data_to_bytes)
                .unwrap_or_default(),
            gas_limit: block
                .gas_limit
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            gas_used: block
                .gas_used
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            hash,
            // HyperSync does not serve uncle bodies.
            is_uncle: false,
            logs_bloom: block
                .logs_bloom
                .as_ref()
                .map(data_to_bytes)
                .unwrap_or_default(),
            miner: block
                .miner
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            mix_hash: block.mix_hash.as_ref().map(hash_to_b256),
            nonce: block
                .nonce
                .as_ref()
                .map(nonce_to_b64)
                .unwrap_or_default(),
            number: sat_u32(number),
            parent_hash: opt_hash(&block.parent_hash),
            receipts_root: opt_hash(&block.receipts_root),
            sha3_uncles: opt_hash(&block.sha3_uncles),
            size: block
                .size
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            state_root: opt_hash(&block.state_root),
            timestamp: block
                .timestamp
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            total_difficulty: block
                .total_difficulty
                .as_ref()
                .map(quantity_to_u256),
            transactions: sat_u16(transactions),
            transactions_root: opt_hash(&block.transactions_root),
            uncles: block
                .uncles
                .as_ref()
                .map(|uncles| uncles.iter().map(hash_to_b256).collect())
                .unwrap_or_default(),
            withdrawals_root: block
                .withdrawals_root
                .as_ref()
                .map(hash_to_b256),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{Data, Hash, Quantity};

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
        assert!(!row.is_uncle);
        assert_eq!(row.base_fee_per_gas, None);
        assert_eq!(row.total_difficulty, None);
        assert_eq!(row.difficulty, U256::ZERO);
        assert_eq!(row.nonce, B64::ZERO);
        assert!(row.uncles.is_empty());
        assert!(row.logs_bloom.is_empty());
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
    fn wide_values_saturate_instead_of_panicking() {
        let mut block = minimal();
        block.number = Some(u64::MAX);
        block.gas_limit = Some(Quantity::from(u64::MAX));
        block.gas_used = Some(Quantity::from(u32::MAX as u64 + 1));
        block.size = Some(Quantity::from(vec![1u8; 20]));
        block.timestamp = Some(Quantity::from(u64::MAX));
        block.base_fee_per_gas = Some(Quantity::from(vec![1u8; 9]));

        let row =
            DatabaseBlock::from_hypersync(&block, 1, 100_000).unwrap();

        assert_eq!(row.number, u32::MAX);
        assert_eq!(row.gas_limit, u32::MAX);
        assert_eq!(row.gas_used, u32::MAX);
        assert_eq!(row.size, u32::MAX);
        assert_eq!(row.timestamp, u32::MAX);
        assert_eq!(row.base_fee_per_gas, Some(u64::MAX));
        assert_eq!(row.transactions, u16::MAX);
    }

    #[test]
    fn bloom_of_any_length_is_accepted() {
        let mut block = minimal();
        block.logs_bloom = Some(Data::from(vec![0xab; 3]));

        let row = DatabaseBlock::from_hypersync(&block, 1, 0).unwrap();
        assert_eq!(row.logs_bloom, Bytes::from(vec![0xab; 3]));
    }
}
