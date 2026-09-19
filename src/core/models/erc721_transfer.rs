use alloy::primitives::{Address, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::db::format::{SerAddress, SerB256, SerU256};

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
