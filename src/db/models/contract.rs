use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseContract {
    pub contract_address: String,
    pub chain: i64,
    pub creator: String,
    pub hash: String,
}
