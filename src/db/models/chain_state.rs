use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseChainIndexedState {
    pub chain: u64,
    pub indexed_blocks_amount: u64,
}
