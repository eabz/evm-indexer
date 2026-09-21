use alloy::primitives::{Address, Bytes, B256, B64, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::db::format::{SerAddress, SerB256, SerB64, SerBytes, SerU256};

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
