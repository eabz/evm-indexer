use field_count::FieldCount;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TradeType {
    Buy,
    Sell,
}

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseDexTrade {
    pub chain: i64,
    pub maker: String,
    pub hash: String,
    pub log_index: i32,
    pub receiver: String,
    pub token_in: String,
    pub token_amount_in: f64,
    pub token_out: String,
    pub token_amount_out: f64,
    pub usd_value: f64,
    pub swap_rate: f64,
    pub transaction_log_index: Option<i32>,
    pub timestamp: i32,
    pub trade_type: TradeType,
}
