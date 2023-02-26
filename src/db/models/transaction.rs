use ethers::{
    types::{Transaction, H160},
    utils::format_units,
};
use field_count::FieldCount;

use crate::utils::format::{byte4_from_input, format_address, format_bytes, format_hash};

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseTransaction {
    pub block_hash: String,
    pub block_number: i64,
    pub chain: i64,
    pub from_address: String,
    pub gas: i64,
    pub gas_price: i64,
    pub hash: String,
    pub input: Vec<u8>,
    pub max_fee_per_gas: i64,
    pub max_priority_fee_per_gas: i64,
    pub method: String,
    pub nonce: i32,
    pub timestamp: i64,
    pub to_address: String,
    pub transaction_index: i8,
    pub transaction_type: i8,
    pub value: f64,
}

impl DatabaseTransaction {
    pub fn from_rpc(transaction: Transaction, chain: i64, timestamp: i64) -> Self {
        let max_fee_per_gas: i64 = match transaction.max_fee_per_gas {
            None => 0,
            Some(gas_price) => gas_price.as_u64() as i64,
        };

        let max_priority_fee_per_gas: i64 = match transaction.max_priority_fee_per_gas {
            None => 0,
            Some(gas_price) => gas_price.as_u64() as i64,
        };

        let to_address: String = match transaction.to {
            None => format_address(H160::zero()),
            Some(to) => format_address(to),
        };

        let block_number: i64 = match transaction.block_number {
            None => 0,
            Some(block_number) => block_number.as_u64() as i64,
        };

        let block_hash: String = match transaction.block_hash {
            None => String::from("0"),
            Some(block_hash) => format_hash(block_hash),
        };

        let gas_price: i64 = match transaction.gas_price {
            None => 0,
            Some(gas_price) => gas_price.as_u64() as i64,
        };

        let transaction_type: i8 = match transaction.transaction_type {
            None => 0,
            Some(transaction_type) => transaction_type.as_u32() as i8,
        };

        let transaction_index: i8 = match transaction.transaction_index {
            None => 0,
            Some(transaction_index) => transaction_index.as_u32() as i8,
        };

        Self {
            block_hash,
            block_number,
            chain: chain.to_owned(),
            from_address: format_address(transaction.from),
            gas: transaction.gas.as_u64() as i64,
            gas_price,
            max_priority_fee_per_gas,
            max_fee_per_gas,
            hash: format_hash(transaction.hash),
            method: format!(
                "0x{}",
                hex::encode(byte4_from_input(&format_bytes(&transaction.input)))
            ),
            input: transaction.input.to_vec(),
            nonce: transaction.nonce.as_u32() as i32,
            timestamp,
            to_address,
            transaction_index,
            transaction_type,
            value: format_units(transaction.value, 18)
                .unwrap()
                .parse::<f64>()
                .unwrap(),
        }
    }
}
