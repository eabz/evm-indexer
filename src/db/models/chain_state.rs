use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseChainIndexedState {
    pub chain: i64,
    pub indexed_blocks_amount: i64,
}
