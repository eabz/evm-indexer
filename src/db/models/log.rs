use clickhouse::Row;
use ethers::types::Log;
use primitive_types::U256;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum TokenTransferType {
    Erc20 = 1,
    Erc721 = 2,
    Erc1155 = 3,
}

use crate::utils::format::{format_address, format_bytes, format_hash};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseLog {
    pub address: String,
    pub block_number: u32,
    pub chain: u64,
    pub data: String,
    pub dex_trade_maker: Option<String>,
    pub dex_trade_pair: Option<String>,
    pub dex_trade_receiver: Option<String>,
    pub dex_trade_token0_amount: Option<U256>,
    pub dex_trade_token1_amount: Option<U256>,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub token_transfer_amount: Option<U256>,
    pub token_transfer_amounts: Vec<U256>,
    pub token_transfer_from: Option<String>,
    pub token_transfer_id: Option<U256>,
    pub token_transfer_ids: Vec<U256>,
    pub token_transfer_operator: Option<String>,
    pub token_transfer_to: Option<String>,
    pub token_transfer_token_address: Option<String>,
    pub token_transfer_type: Option<TokenTransferType>,
    pub topic0: String,
    pub topic1: Option<String>,
    pub topic2: Option<String>,
    pub topic3: Option<String>,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseLog {
    pub fn from_rpc(
        log: &Log,
        chain: u64,
        timestamp: u32,
        block_number: &u32,
    ) -> Self {
        let topic0 = if log.topics.is_empty() {
            String::from("0x")
        } else {
            format_hash(log.topics[0])
        };

        let topics = log.topics.len();

        let topic1 = if topics < 2 {
            None
        } else {
            Some(format_hash(log.topics[1]))
        };

        let topic2 = if topics < 3 {
            None
        } else {
            Some(format_hash(log.topics[2]))
        };

        let topic3 = if topics < 4 {
            None
        } else {
            Some(format_hash(log.topics[3]))
        };

        let transaction_log_index: Option<u16> =
            log.transaction_log_index.map(|transaction_log_index| {
                transaction_log_index.as_u32() as u16
            });

        Self {
            address: format_address(log.address),
            block_number: block_number.to_owned(),
            chain,
            data: format_bytes(&log.data),
            dex_trade_maker: None,
            dex_trade_pair: None,
            dex_trade_receiver: None,
            dex_trade_token0_amount: None,
            dex_trade_token1_amount: None,
            log_index: log.log_index.unwrap().as_u32() as u16,
            log_type: log.log_type.clone(),
            removed: log.removed.unwrap(),
            timestamp,
            token_transfer_amount: None,
            token_transfer_amounts: Vec::new(),
            token_transfer_from: None,
            token_transfer_id: None,
            token_transfer_ids: Vec::new(),
            token_transfer_operator: None,
            token_transfer_to: None,
            token_transfer_token_address: None,
            token_transfer_type: None,
            topic0,
            topic1,
            topic2,
            topic3,
            transaction_hash: format_hash(log.transaction_hash.unwrap()),
            transaction_log_index,
        }
    }
}
