use clickhouse::Row;
use ethers::abi::{ethabi, ParamType};
use primitive_types::{H256, U256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::format::{decode_bytes, format_address, SerU256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC20Transfer {
    pub address: String,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub block_number: u32,
    pub chain: u64,
    pub from: String,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub to: String,
    pub token_address: String,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseERC20Transfer {
    pub fn from_rpc(log: &DatabaseLog) -> Self {
        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(
                log.topic0.clone(),
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

        Self {
            address: log.address.clone(),
            amount: value.to_owned().into_uint().unwrap(),
            block_number: log.block_number,
            chain: log.chain,
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            removed: log.removed,
            timestamp: log.timestamp,
            to: format_address(
                to_address.to_owned().into_address().unwrap(),
            ),
            token_address: log.address.clone(),
            transaction_hash: log.transaction_hash.clone(),
            transaction_log_index: log.transaction_log_index,
        }
    }
}
