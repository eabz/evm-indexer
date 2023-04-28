use clickhouse::Row;
use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    decode_bytes, format_address, opt_serialize_u256, serialize_u256,
};

use super::log::DatabaseLog;

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC20Transfer {
    #[serde(with = "serialize_u256")]
    pub amount: U256,
    pub chain: u64,
    pub from: String,
    pub transaction_hash: String,
    #[serde(with = "serialize_u256")]
    pub log_index: U256,
    pub to: String,
    pub token: String,
    #[serde(with = "opt_serialize_u256")]
    pub transaction_log_index: Option<U256>,
    pub timestamp: u64,
}

impl DatabaseERC20Transfer {
    pub fn from_log(log: &DatabaseLog, chain: u64) -> Self {
        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                log.topic1.clone().unwrap(),
            )
            .unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                log.topic2.clone().unwrap(),
            )
            .unwrap();

        let from_address_tokens = ethabi::decode(
            &[ParamType::Address],
            from_address_bytes.as_bytes(),
        )
        .unwrap();

        let from_address = from_address_tokens.first().unwrap();

        let to_address_tokens = ethabi::decode(
            &[ParamType::Address],
            to_address_bytes.as_bytes(),
        )
        .unwrap();

        let to_address = to_address_tokens.first().unwrap();

        let log_data = decode_bytes(log.data.clone());

        let value_tokens =
            ethabi::decode(&[ParamType::Uint(256)], &log_data[..])
                .unwrap();

        let value = value_tokens.first().unwrap();

        let token = log.address.clone();

        Self {
            chain,
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            transaction_hash: log.transaction_hash.clone(),
            log_index: log.log_index,
            to: format_address(
                to_address.to_owned().into_address().unwrap(),
            ),
            token,
            transaction_log_index: log.transaction_log_index,
            amount: value.to_owned().into_uint().unwrap(),
            timestamp: log.timestamp,
        }
    }
}
