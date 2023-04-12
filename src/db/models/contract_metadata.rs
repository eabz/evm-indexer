use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseContractMetadata {
    pub abi: String,
    pub chain: i64,
    pub contract_address: String,
    pub name: String,
}
