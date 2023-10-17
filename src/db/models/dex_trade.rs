use clickhouse::Row;
use ethers::abi::{ethabi, ParamType};
use primitive_types::{H256, U256};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::format::{decode_bytes, format_address, SerU256};

use super::log::DatabaseLog;

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseDexTrade {
    pub address: String,
    pub block_number: u32,
    pub chain: u64,
    pub log_index: u16,
    pub log_type: Option<String>,
    pub maker: String,
    pub pair: String,
    pub receiver: String,
    pub removed: bool,
    pub timestamp: u32,
    #[serde_as(as = "SerU256")]
    pub token0_amount: U256,
    #[serde_as(as = "SerU256")]
    pub token1_amount: U256,
    pub transaction_hash: String,
    pub transaction_log_index: Option<u16>,
}

impl DatabaseDexTrade {
    pub fn from_v2_rpc(log: &DatabaseLog) -> Self {
        let maker_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic1.clone().unwrap(),
        )
        .unwrap();

        let receiver_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic2.clone().unwrap(),
        )
        .unwrap();

        let maker_tokens =
            ethabi::decode(&[ParamType::Address], maker_bytes.as_bytes())
                .unwrap();

        let maker = maker_tokens.first().unwrap();

        let receiver_tokens = ethabi::decode(
            &[ParamType::Address],
            receiver_bytes.as_bytes(),
        )
        .unwrap();

        let receiver = receiver_tokens.first().unwrap();

        let log_data = decode_bytes(log.data.clone());

        let values_tokens = ethabi::decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            &log_data[..],
        )
        .unwrap();

        let token0_amount =
            values_tokens[2].to_owned().into_uint().unwrap();

        let token1_amount =
            values_tokens[3].to_owned().into_uint().unwrap();

        Self {
            address: log.address.clone(),
            block_number: log.block_number,
            chain: log.chain,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            maker: format_address(
                maker.to_owned().into_address().unwrap(),
            ),
            pair: log.address.clone(),
            receiver: format_address(
                receiver.to_owned().into_address().unwrap(),
            ),
            removed: log.removed,
            timestamp: log.timestamp,
            token0_amount,
            token1_amount,
            transaction_hash: log.transaction_hash.clone(),
            transaction_log_index: log.transaction_log_index,
        }
    }

    pub fn from_v3_rpc(log: &DatabaseLog) -> Self {
        let maker_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic1.clone().unwrap(),
        )
        .unwrap();

        let receiver_bytes = array_bytes::hex_n_into::<String, H256, 32>(
            log.topic2.clone().unwrap(),
        )
        .unwrap();

        let maker_tokens =
            ethabi::decode(&[ParamType::Address], maker_bytes.as_bytes())
                .unwrap();

        let maker = maker_tokens.first().unwrap();

        let receiver_tokens = ethabi::decode(
            &[ParamType::Address],
            receiver_bytes.as_bytes(),
        )
        .unwrap();

        let receiver = receiver_tokens.first().unwrap();

        let log_data = decode_bytes(log.data.clone());

        let values_tokens = ethabi::decode(
            &[
                ParamType::Int(256),
                ParamType::Int(256),
                ParamType::Uint(160),
                ParamType::Uint(128),
                ParamType::Int(24),
            ],
            &log_data[..],
        )
        .unwrap();

        let token0_amount =
            values_tokens[0].to_owned().into_int().unwrap();

        let token1_amount =
            values_tokens[1].to_owned().into_int().unwrap();

        Self {
            address: log.address.clone(),
            block_number: log.block_number,
            chain: log.chain,
            log_index: log.log_index,
            log_type: log.log_type.clone(),
            maker: format_address(
                maker.to_owned().into_address().unwrap(),
            ),
            pair: log.address.clone(),
            receiver: format_address(
                receiver.to_owned().into_address().unwrap(),
            ),
            removed: log.removed,
            timestamp: log.timestamp,
            token0_amount,
            token1_amount,
            transaction_hash: log.transaction_hash.clone(),
            transaction_log_index: log.transaction_log_index,
        }
    }
}
