use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseTokenDetails {
    pub chain: i64,
    pub token: String,
    pub name: String,
    pub symbol: String,
    pub decimals: i8,
}
