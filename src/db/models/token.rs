use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseToken {
    pub chain: u64,
    pub token: String,
    pub name: String,
    pub symbol: String,
    pub decimals: u64,           // Only for ERC20 tokens
    pub token0: Option<String>,  // Only for ERC20 from UniswapV2 Pairs
    pub token1: Option<String>,  // Only for ERC20 from UniswapV2 Pairs
    pub factory: Option<String>, // Only for ERC20 from UniswapV2 Pairs
}
