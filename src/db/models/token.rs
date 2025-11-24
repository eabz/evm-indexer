use alloy::primitives::Address;
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::SerAddress;

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
