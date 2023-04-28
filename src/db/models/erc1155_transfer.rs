use clickhouse::Row;
use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    format_address, opt_serialize_u256, serialize_u256,
};

use super::log::DatabaseLog;

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC1155Transfer {
    pub chain: u64,
    pub operator: String,
    pub from: String,
    pub transaction_hash: String,
    #[serde(with = "serialize_u256")]
    pub log_index: U256,
    pub to: String,
    pub token: String,
    #[serde(with = "opt_serialize_u256")]
    pub transaction_log_index: Option<U256>,
    #[serde(with = "serialize_u256")]
    pub id: U256,
    #[serde(with = "serialize_u256")]
    pub value: U256,
    pub timestamp: u64,
}

impl DatabaseERC1155Transfer {
    pub fn from_log(
        log: &DatabaseLog,
        chain: u64,
        id: U256,
        value: U256,
    ) -> Self {
        let operator_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic1.clone().unwrap(),
        )
        .unwrap();

        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                log.topic2.clone().unwrap(),
            )
            .unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                log.topic3.clone().unwrap(),
            )
            .unwrap();

        let operator_tokens = ethabi::decode(
            &[ParamType::Address],
            operator_bytes.as_bytes(),
        )
        .unwrap();

        let operator = operator_tokens.first().unwrap();

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

        Self {
            chain,
            operator: format_address(
                operator.to_owned().into_address().unwrap(),
            ),
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            transaction_hash: log.hash.clone(),
            log_index: log.log_index,
            to: format_address(
                to_address.to_owned().into_address().unwrap(),
            ),
            token: log.address.clone(),
            transaction_log_index: log.transaction_log_index,
            id,
            value,
            timestamp: log.timestamp,
        }
    }
}
