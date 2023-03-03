use ethers::{types::Transaction, utils::format_units};
use field_count::FieldCount;

use crate::utils::format::{byte4_from_input, format_address, format_bytes, format_hash};

#[derive(Debug, Clone, FieldCount)]
pub struct DatabaseTransaction {
    pub block_hash: String,
    pub block_number: i64,
    pub chain: i64,
    pub from_address: String,
    pub gas: i64,
    pub gas_price: Option<i64>,
    pub hash: String,
    pub input: Vec<u8>,
    pub max_fee_per_gas: Option<i64>,
    pub max_priority_fee_per_gas: Option<i64>,
    pub method: String,
    pub nonce: i32,
    pub timestamp: i64,
    pub to_address: Option<String>,
    pub transaction_index: i16,
    pub transaction_type: i16,
    pub value: f64,
}

impl DatabaseTransaction {
    pub fn from_rpc(transaction: Transaction, chain: i64, timestamp: i64) -> Self {
        let max_fee_per_gas: Option<i64> = match transaction.max_fee_per_gas {
            None => None,
            Some(max_fee_per_gas) => Some(max_fee_per_gas.as_u64() as i64),
        };

        let max_priority_fee_per_gas: Option<i64> = match transaction.max_priority_fee_per_gas {
            None => None,
            Some(max_priority_fee_per_gas) => Some(max_priority_fee_per_gas.as_u64() as i64),
        };

        let to_address: Option<String> = match transaction.to {
            None => None,
            Some(to) => Some(format_address(to)),
        };

        let gas_price: Option<i64> = match transaction.gas_price {
            None => None,
            Some(gas_price) => Some(gas_price.as_u64() as i64),
        };

        Self {
            block_hash: format_hash(transaction.block_hash.unwrap()),
            block_number: transaction.block_number.unwrap().as_u64() as i64,
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
            transaction_index: transaction.transaction_index.unwrap().as_u32() as i16,
            transaction_type: transaction.transaction_type.unwrap().as_u32() as i16,
            value: format_units(transaction.value, 18)
                .unwrap()
                .parse::<f64>()
                .unwrap(),
        }
    }
}
