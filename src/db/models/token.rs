use alloy::primitives::Address;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::SerAddress;

/// Row of `tokens`. Field names are the column names.
///
/// No `_version` field on purpose: token metadata is written by the token
/// worker outside of the block flushes, so `tokens._version` is assigned
/// by the server (`DEFAULT` now, in ms) and the column is simply not part
/// of the insert.
#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseToken {
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub r#type: String, // "ERC20", "ERC721", "ERC1155"
    pub chain: u64,
}
