use clickhouse::Row;
use ethers::abi::{ethabi, ParamType};
use primitive_types::{H256, U256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::format::{format_address, SerU256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC721Transfer {
    pub address: String,
    pub block_number: u32,
    pub chain: u64,
    pub from: String,
    #[serde_as(as = "SerU256")]
    pub id: U256,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub removed: bool,
    pub timestamp: u32,
    pub to: String,
    pub token_address: String,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseERC721Transfer {
    pub fn from_rpc(log: &DatabaseLog) -> Self {
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

        let id_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic3.clone().unwrap(),
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

        let id_tokens =
            ethabi::decode(&[ParamType::Uint(256)], id_bytes.as_bytes())
                .unwrap();

        let id = id_tokens.first().unwrap();

        Self {
            address: log.address.clone(),
            block_number: log.block_number,
            chain: log.chain,
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            id: id.to_owned().into_uint().unwrap(),
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
