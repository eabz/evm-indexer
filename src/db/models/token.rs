use alloy::primitives::Address;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseToken {
    pub address: Address,
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
    pub r#type: String, // "ERC20", "ERC721", "ERC1155"
    pub chain: u64,
}
