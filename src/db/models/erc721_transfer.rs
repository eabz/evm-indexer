use clickhouse::Row;
use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_number};

use super::log::DatabaseLog;

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC721Transfer {
    pub chain: u64,
    pub from: String,
    pub transaction_hash: String,
    pub log_index: U256,
    pub to: String,
    pub token: String,
    pub transaction_log_index: Option<U256>,
    pub id: U256,
    pub timestamp: u64,
}

impl DatabaseERC721Transfer {
    pub fn from_log(log: &DatabaseLog, chain: i64) -> Self {
        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[1].clone()).unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[2].clone()).unwrap();

        let id_bytes = array_bytes::hex_n_into::<String, H256, 32>(log.topics[3].clone()).unwrap();

        let from_address_tokens =
            ethabi::decode(&[ParamType::Address], from_address_bytes.as_bytes()).unwrap();

        let from_address = from_address_tokens.first().unwrap();

        let to_address_tokens =
            ethabi::decode(&[ParamType::Address], to_address_bytes.as_bytes()).unwrap();

        let to_address = to_address_tokens.first().unwrap();

        let id_tokens = ethabi::decode(&[ParamType::Uint(256)], id_bytes.as_bytes()).unwrap();

        let id = id_tokens.first().unwrap();

        Self {
            chain,
            from_address: format_address(from_address.to_owned().into_address().unwrap()),
            hash: log.hash.clone(),
            log_index: log.log_index,
            to_address: format_address(to_address.to_owned().into_address().unwrap()),
            token: log.address.clone(),
            transaction_log_index: log.transaction_log_index,
            id: format_number(id.to_owned().into_uint().unwrap()),
            timestamp: log.timestamp,
        }
    }
}
