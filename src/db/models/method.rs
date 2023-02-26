use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseMethod {
    pub name: String,
    pub method: String,
}
