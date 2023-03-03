use ethabi::{ethereum_types::H256, ParamType};
use field_count::FieldCount;

use crate::utils::format::{format_address, format_number};

use super::log::DatabaseLog;

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
