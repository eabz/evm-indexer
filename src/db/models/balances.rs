use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct AggDatabaseNativeBalance {
    pub owner: String,
    pub balance: f64,
    pub chain: i64,
    pub received: i32,
    pub sent: i32,
}

#[derive(Serialize, Deserialize)]
pub struct AggDatabaseERC20Balance {
    pub token: String,
    pub owner: String,
    pub balance: f64,
    pub chain: i64,
    pub received: i32,
    pub sent: i32,
}

#[derive(Serialize, Deserialize)]
pub struct AggDatabaseERC721TokenOwner {
    pub token: String,
    pub owner: String,
    pub id: String,
    pub chain: i64,
    pub transactions: i32,
}

#[derive(Serialize, Deserialize)]
pub struct AggDatabaseERC1155Balance {
    pub token: String,
    pub owner: String,
    pub balance: f64,
    pub id: String,
    pub chain: i64,
    pub received: i32,
    pub sent: i32,
}
