use std::collections::HashMap;

use clickhouse::Row;
use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    decode_bytes, format_address, opt_serialize_u256, serialize_u256,
};

use super::{log::DatabaseLog, token::DatabaseToken};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseDexTrade {
    pub chain: u64,
    pub maker: String,
    pub transaction_hash: String,
    #[serde(with = "serialize_u256")]
    pub log_index: U256,
    pub receiver: String,
    pub token0: String,
    pub token1: String,
    pub pair_address: String,
    pub factory: String,
    #[serde(with = "serialize_u256")]
    pub token0_amount: U256,
    #[serde(with = "serialize_u256")]
    pub token1_amount: U256,
    #[serde(with = "opt_serialize_u256")]
    pub transaction_log_index: Option<U256>,
    pub timestamp: u64,
}

impl DatabaseDexTrade {
    pub fn from_v2_log(
        log: &DatabaseLog,
        chain: u64,
        tokens_metadata: &HashMap<String, DatabaseToken>,
    ) -> Self {
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

        let token0_out = values_tokens[2].to_owned().into_uint().unwrap();

        let token1_out = values_tokens[3].to_owned().into_uint().unwrap();

        let pair_address = log.address.clone();

        let pair_metadata = tokens_metadata.get(&pair_address).unwrap();

        Self {
            chain,
            maker: format_address(
                maker.to_owned().into_address().unwrap(),
            ),
            transaction_hash: log.transaction_hash.clone(),
            log_index: log.log_index,
            receiver: format_address(
                receiver.to_owned().into_address().unwrap(),
            ),
            token0_amount: token0_out,
            token1_amount: token1_out,
            transaction_log_index: log.transaction_log_index,
            timestamp: log.timestamp,
            token0: pair_metadata.token0.clone().unwrap(),
            token1: pair_metadata.token1.clone().unwrap(),
            pair_address,
            factory: pair_metadata.factory.clone().unwrap(),
        }
    }

    pub fn from_v3_log(
        log: &DatabaseLog,
        chain: u64,
        tokens_metadata: &HashMap<String, DatabaseToken>,
    ) -> Self {
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

        let pair_address = log.address.clone();

        let pair_metadata = tokens_metadata.get(&pair_address).unwrap();

        Self {
            chain,
            maker: format_address(
                maker.to_owned().into_address().unwrap(),
            ),
            transaction_hash: log.transaction_hash.clone(),
            log_index: log.log_index,
            receiver: format_address(
                receiver.to_owned().into_address().unwrap(),
            ),
            token0_amount,
            token1_amount,
            transaction_log_index: log.transaction_log_index,
            timestamp: log.timestamp,

            token0: pair_metadata.token0.clone().unwrap(),
            token1: pair_metadata.token1.clone().unwrap(),
            pair_address,
            factory: pair_metadata.factory.clone().unwrap(),
        }
    }
}
