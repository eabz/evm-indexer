use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseERC721Transfer {
    pub chain: i64,
    pub from_address: String,
    pub hash: String,
    pub log_index: i32,
    pub to_address: String,
    pub token: String,
    pub transaction_log_index: Option<i32>,
    pub id: String,
    pub timestamp: i64,
}
