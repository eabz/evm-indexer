use clickhouse::Row;
use ethers::abi::{ethabi, ParamType};
use primitive_types::{H256, U256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::log::DatabaseLog;
use crate::utils::format::{format_address, SerU256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC1155Transfer {
    pub address: String,
    #[serde_as(as = "Vec<SerU256>")]
    pub amounts: Vec<U256>,
    pub block_number: u32,
    pub chain: u64,
    pub from: String,
    #[serde_as(as = "Vec<SerU256>")]
    pub ids: Vec<U256>,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub operator: String,
    pub removed: bool,
    pub timestamp: u32,
    pub to: String,
    pub token_address: String,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseERC1155Transfer {
    pub fn from_single_rpc(
        log: &DatabaseLog,
        id: U256,
        amount: U256,
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
            address: log.address.clone(),
            amounts: vec![amount],
            block_number: log.block_number,
            chain: log.chain,
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            ids: vec![id],
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            operator: format_address(
                operator.to_owned().into_address().unwrap(),
            ),
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

    pub fn from_batch_rpc(
        log: &DatabaseLog,
        ids: Vec<U256>,
        amounts: Vec<U256>,
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
            address: log.address.clone(),
            amounts,
            block_number: log.block_number,
            chain: log.chain,
            from: format_address(
                from_address.to_owned().into_address().unwrap(),
            ),
            ids,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            operator: format_address(
                operator.to_owned().into_address().unwrap(),
            ),
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
