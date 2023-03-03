use field_count::FieldCount;

#[derive(Debug, Clone, FieldCount, sqlx::FromRow)]
pub struct DatabaseTokenDetails {
    pub chain: i64,
    pub token: String,
    pub name: String,
    pub symbol: String,
    pub decimals: Option<i16>,  // Only for ERC20 tokens
    pub token0: Option<String>, // Only for ERC20 from UniswapV2 Pairs
    pub token1: Option<String>, // Only for ERC20 from UniswapV2 Pairs
}
