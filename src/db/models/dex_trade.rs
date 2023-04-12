use chrono::{DateTime, NaiveDateTime, Utc};
use clickhouse::Row;
use ethabi::{ethereum_types::H256, ParamType};
use ethers::utils::format_units;
use serde::{Deserialize, Serialize};

use crate::utils::format::format_address;

use super::{log::DatabaseLog, token_detail::DatabaseTokenDetails};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradeType {
    Buy,
    Sell,
}

impl TradeType {
    pub fn as_str(&self) -> &'static str {
        match self {
            TradeType::Buy => "buy",
            TradeType::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseDexTrade {
    pub chain: i64,
    pub maker: String,
    pub hash: String,
    pub log_index: i32,
    pub receiver: String,
    pub token0: String,
    pub token1: String,
    pub pair_address: String,
    pub factory: String,
    pub token0_amount: f64,
    pub token1_amount: f64,
    pub swap_rate: f64,
    pub transaction_log_index: Option<i32>,
    pub timestamp: i64,
    pub trade_type: TradeType,
}

impl DatabaseDexTrade {
    pub fn from_v2_log(
        log: &DatabaseLog,
        chain: i64,
        pair_token: &DatabaseTokenDetails,
        token0_decimals: usize,
        token1_decimals: usize,
    ) -> Self {
        let maker_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[1].clone()).unwrap();

        let receiver_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[2].clone()).unwrap();

        let maker_tokens = ethabi::decode(&[ParamType::Address], maker_bytes.as_bytes()).unwrap();

        let maker = maker_tokens.first().unwrap();

        let receiver_tokens =
            ethabi::decode(&[ParamType::Address], receiver_bytes.as_bytes()).unwrap();

        let receiver = receiver_tokens.first().unwrap();

        let values_tokens = ethabi::decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            &log.data[..],
        )
        .unwrap();

        let token0_in = format_units(
            values_tokens[0].to_owned().into_uint().unwrap(),
            token0_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        let token0_out = format_units(
            values_tokens[1].to_owned().into_uint().unwrap(),
            token0_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        let token1_in = format_units(
            values_tokens[2].to_owned().into_uint().unwrap(),
            token1_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        let token1_out = format_units(
            values_tokens[3].to_owned().into_uint().unwrap(),
            token1_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        Self {
            chain,
            maker: format_address(maker.to_owned().into_address().unwrap()),
            hash: log.hash.clone(),
            log_index: log.log_index,
            receiver: format_address(receiver.to_owned().into_address().unwrap()),
            token0: pair_token.token0.clone().unwrap(),
            token1: pair_token.token1.clone().unwrap(),
            pair_address: pair_token.token.clone(),
            factory: pair_token.factory.clone().unwrap(),
            token0_amount: token0_in - token0_out,
            token1_amount: token1_in - token1_out,
            transaction_log_index: log.transaction_log_index,
            timestamp: log.timestamp,

            // TODO: trade type and swap rate
            trade_type: TradeType::Buy,
            swap_rate: 0.0,
        }
    }

    pub fn from_v3_log(
        log: &DatabaseLog,
        chain: i64,
        pair_token: &DatabaseTokenDetails,
        token0_decimals: usize,
        token1_decimals: usize,
    ) -> Self {
        let maker_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[1].clone()).unwrap();

        let receiver_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[2].clone()).unwrap();

        let maker_tokens = ethabi::decode(&[ParamType::Address], maker_bytes.as_bytes()).unwrap();

        let maker = maker_tokens.first().unwrap();

        let receiver_tokens =
            ethabi::decode(&[ParamType::Address], receiver_bytes.as_bytes()).unwrap();

        let receiver = receiver_tokens.first().unwrap();

        let values_tokens = ethabi::decode(
            &[
                ParamType::Int(256),
                ParamType::Int(256),
                ParamType::Uint(160),
                ParamType::Uint(128),
                ParamType::Int(24),
            ],
            &log.data[..],
        )
        .unwrap();

        let token0_amount = format_units(
            values_tokens[0].to_owned().into_int().unwrap(),
            token0_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        let token1_amount = format_units(
            values_tokens[1].to_owned().into_int().unwrap(),
            token1_decimals as usize,
        )
        .unwrap()
        .parse::<f64>()
        .unwrap();

        Self {
            chain,
            maker: format_address(maker.to_owned().into_address().unwrap()),
            hash: log.hash.clone(),
            log_index: log.log_index,
            receiver: format_address(receiver.to_owned().into_address().unwrap()),
            token0: pair_token.token0.clone().unwrap(),
            token1: pair_token.token1.clone().unwrap(),
            pair_address: pair_token.token.clone(),
            factory: pair_token.factory.clone().unwrap(),
            token0_amount,
            token1_amount,
            transaction_log_index: log.transaction_log_index,
            timestamp: log.timestamp,
            // TODO: trade type and swap rate
            trade_type: TradeType::Buy,
            swap_rate: 0.0,
        }
    }

    fn trade_time(&self) -> DateTime<Utc> {
        let date = NaiveDateTime::from_timestamp_opt(self.timestamp, 0).unwrap();

        let date_utc: DateTime<Utc> = DateTime::from_utc(date, Utc);

        return date_utc;
    }

    pub fn trade_time_minutes(&self) -> String {
        let date = self.trade_time();

        let date_string = date.format("%d-%m-%y-%H-%M");

        return date_string.to_string();
    }

    pub fn trade_time_hours(&self) -> String {
        let date = self.trade_time();

        let date_string = date.format("%d-%m-%y-%H");

        return date_string.to_string();
    }

    pub fn trade_time_days(&self) -> String {
        let date = self.trade_time();

        let date_string = date.format("%d-%m-%y");

        return date_string.to_string();
    }
}
