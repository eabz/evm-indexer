use clickhouse::Row;
use ethabi::{
    ethereum_types::{H256, U256},
    ParamType,
};
use ethers::utils::format_units;
use serde::{Deserialize, Serialize};

use crate::utils::format::{format_address, format_number};

use super::log::DatabaseLog;

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseERC1155Transfer {
    pub chain: i64,
    pub operator: String,
    pub from_address: String,
    pub hash: String,
    pub log_index: i32,
    pub to_address: String,
    pub token: String,
    pub transaction_log_index: Option<i32>,
    pub ids: Vec<String>,
    pub values: Vec<f64>,
    pub timestamp: i64,
}

impl DatabaseERC1155Transfer {
    pub fn from_log(log: &DatabaseLog, chain: i64, batch: bool) -> Self {
        let operator_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[1].clone()).unwrap();

        let from_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[2].clone()).unwrap();

        let to_address_bytes =
            array_bytes::hex_n_into::<String, H256, 32>(log.topics[3].clone()).unwrap();

        let operator_tokens =
            ethabi::decode(&[ParamType::Address], operator_bytes.as_bytes()).unwrap();

        let operator = operator_tokens.first().unwrap();

        let from_address_tokens =
            ethabi::decode(&[ParamType::Address], from_address_bytes.as_bytes()).unwrap();

        let from_address = from_address_tokens.first().unwrap();

        let to_address_tokens =
            ethabi::decode(&[ParamType::Address], to_address_bytes.as_bytes()).unwrap();

        let to_address = to_address_tokens.first().unwrap();

        let mut ids: Vec<String> = Vec::new();
        let mut values: Vec<f64> = Vec::new();

        if batch {
            let transfer_values = ethabi::decode(
                &[
                    ParamType::Array(Box::new(ParamType::Uint(256))),
                    ParamType::Array(Box::new(ParamType::Uint(256))),
                ],
                &log.data[..],
            )
            .unwrap();

            let mut transfer_ids: Vec<String> = transfer_values[0]
                .clone()
                .into_array()
                .unwrap()
                .iter()
                .map(|token| format_number(token.clone().into_uint().unwrap()))
                .collect();

            let transfer_values: Vec<U256> = transfer_values[1]
                .clone()
                .into_array()
                .unwrap()
                .iter()
                .map(|token| token.clone().into_uint().unwrap())
                .collect();

            let mut values_parsed: Vec<f64> = transfer_values
                .iter()
                .map(|value| match format_number(value.clone()).parse::<f64>() {
                    Ok(value) => value,
                    Err(_) => format_units(value.clone(), 18)
                        .unwrap()
                        .parse::<f64>()
                        .unwrap(),
                })
                .collect();

            ids.append(&mut transfer_ids);
            values.append(&mut values_parsed);
        } else {
            let transfer_values =
                ethabi::decode(&[ParamType::Uint(256), ParamType::Uint(256)], &log.data[..])
                    .unwrap();

            let id = format_number(transfer_values[0].clone().into_uint().unwrap());
            let value = transfer_values[1].clone().into_uint().unwrap();

            let value_parsed = match format_number(value).parse::<f64>() {
                Ok(value) => value,
                Err(_) => format_units(value, 18).unwrap().parse::<f64>().unwrap(),
            };

            ids.push(id);
            values.push(value_parsed)
        }

        Self {
            chain,
            operator: format_address(operator.to_owned().into_address().unwrap()),
            from_address: format_address(from_address.to_owned().into_address().unwrap()),
            hash: log.hash.clone(),
            log_index: log.log_index,
            to_address: format_address(to_address.to_owned().into_address().unwrap()),
            token: log.address.clone(),
            transaction_log_index: log.transaction_log_index,
            ids,
            values,
            timestamp: log.timestamp,
        }
    }
}
