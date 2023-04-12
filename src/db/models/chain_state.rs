use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseChainIndexedState {
    pub chain: i64,
    pub indexed_blocks_amount: i64,
}
