use clickhouse::Row;
use ethabi::ethereum_types::{H160, U256};
use ethers::types::Transaction;
use serde::{Deserialize, Serialize};

use crate::utils::format::{
    byte4_from_input, format_address, format_bytes, format_hash,
    opt_serialize_u256, serialize_u256,
};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTransaction {
    pub access_list: Vec<(String, Vec<String>)>,
    pub block_hash: String,
    pub block_number: u64,
    pub chain: u64,
    pub from: String,
    #[serde(with = "serialize_u256")]
    pub gas: U256,
    #[serde(with = "opt_serialize_u256")]
    pub gas_price: Option<U256>,
    pub hash: String,
    pub input: String,
    #[serde(with = "opt_serialize_u256")]
    pub max_fee_per_gas: Option<U256>,
    #[serde(with = "opt_serialize_u256")]
    pub max_priority_fee_per_gas: Option<U256>,
    pub method: String,
    #[serde(with = "serialize_u256")]
    pub nonce: U256,
    pub timestamp: u32,
    pub to: String,
    pub transaction_index: u16,
    pub transaction_type: u16,
    #[serde(with = "serialize_u256")]
    pub value: U256,
}

impl DatabaseTransaction {
    pub fn from_rpc(
        transaction: &Transaction,
        chain: u64,
        timestamp: u32,
    ) -> Self {
        let to: String = match transaction.to {
            Some(address) => format_address(address),
            None => format_address(H160::zero()),
        };

        let transaction_type: u16 = match transaction.transaction_type {
            Some(transaction_type) => transaction_type.as_u64() as u16,
            None => 0,
        };

        let access_list: Vec<(String, Vec<String>)> =
            match transaction.access_list.to_owned() {
                Some(access_list_items) => {
                    let mut access_list: Vec<(String, Vec<String>)> =
                        Vec::new();

                    for item in access_list_items.0 {
                        let keys: Vec<String> = item
                            .storage_keys
                            .into_iter()
                            .map(format_hash)
                            .collect();

                        access_list
                            .push((format_address(item.address), keys))
                    }

                    access_list
                }
                None => Vec::new(),
            };

        Self {
            access_list,
            block_hash: format_hash(transaction.block_hash.unwrap()),
            block_number: transaction.block_number.unwrap().as_u64(),
            chain,
            from: format_address(transaction.from),
            gas: transaction.gas,
            gas_price: transaction.gas_price,
            hash: format_hash(transaction.hash),
            input: format_bytes(&transaction.input),
            max_fee_per_gas: transaction.max_fee_per_gas,
            max_priority_fee_per_gas: transaction.max_priority_fee_per_gas,
            method: format!(
                "0x{}",
                hex::encode(byte4_from_input(&format_bytes(
                    &transaction.input
                )))
            ),
            nonce: transaction.nonce,
            timestamp,
            to,
            transaction_index: transaction
                .transaction_index
                .unwrap()
                .as_u64() as u16,
            transaction_type,
            value: transaction.value,
        }
    }
}
