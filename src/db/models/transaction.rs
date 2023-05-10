use std::ops::Mul;

use clickhouse::Row;
use ethers::types::{Transaction, TransactionReceipt};
use primitive_types::{H160, U256};
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::serde_as;

use crate::utils::format::{
    byte4_from_input, format_address, format_bytes, format_hash, SerU256,
};

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum TransactionType {
    Legacy = 0,
    AccessList = 1,
    Eip1559 = 2,
}

#[serde_as]
#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum TransactionStatus {
    Unknown = 0,
    Failure = 1,
    Success = 2,
}

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTransaction {
    pub access_list: Vec<(String, Vec<String>)>,
    pub base_fee_per_gas: Option<u64>,
    pub block_hash: String,
    pub block_number: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub burned: Option<U256>,
    pub chain: u64,
    pub contract_created: Option<String>,
    pub cumulative_gas_used: Option<u32>,
    #[serde_as(as = "Option<SerU256>")]
    pub effective_gas_price: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub effective_transaction_fee: Option<U256>,
    pub from: String,
    pub gas: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub gas_price: Option<U256>,
    pub gas_used: Option<u32>,
    pub hash: String,
    pub input: String,
    #[serde_as(as = "Option<SerU256>")]
    pub max_fee_per_gas: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub max_priority_fee_per_gas: Option<U256>,
    pub method: String,
    pub nonce: u32,
    pub status: Option<TransactionStatus>,
    pub timestamp: u32,
    pub to: String,
    pub transaction_index: u16,
    pub transaction_type: TransactionType,
    #[serde_as(as = "SerU256")]
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

        let transaction_type: TransactionType =
            match transaction.transaction_type {
                Some(transaction_type) => {
                    if transaction_type.as_usize() == 0 {
                        TransactionType::AccessList
                    } else if transaction_type.as_usize() == 1 {
                        TransactionType::Eip1559
                    } else {
                        TransactionType::Legacy
                    }
                }
                None => TransactionType::Legacy,
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
            base_fee_per_gas: None,
            block_hash: format_hash(transaction.block_hash.unwrap()),
            block_number: transaction.block_number.unwrap().as_usize()
                as u32,
            burned: None,
            chain,
            contract_created: None,
            cumulative_gas_used: None,
            effective_gas_price: None,
            effective_transaction_fee: None,
            from: format_address(transaction.from),
            gas: transaction.gas.as_usize() as u32,
            gas_price: transaction.gas_price,
            gas_used: None,
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
            nonce: transaction.nonce.as_usize() as u32,
            status: None,
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

    pub fn add_receipt_data(
        &mut self,
        base_fee_per_gas: Option<u64>,
        receipt: &TransactionReceipt,
    ) {
        let gas_used = receipt.gas_used.unwrap();

        let effective_transaction_fee = receipt
            .gas_used
            .unwrap()
            .mul(receipt.effective_gas_price.unwrap());

        let status = match receipt.status {
            Some(status) => {
                if status.as_usize() == 0 {
                    TransactionStatus::Failure
                } else if status.as_usize() == 1 {
                    TransactionStatus::Success
                } else {
                    TransactionStatus::Unknown
                }
            }
            None => TransactionStatus::Unknown,
        };

        let burned = match base_fee_per_gas {
            Some(base_fee_per_gas) => {
                U256::from(base_fee_per_gas).mul(gas_used)
            }
            None => U256::zero(),
        };

        self.base_fee_per_gas = base_fee_per_gas;
        self.burned = Some(burned);
        self.contract_created =
            receipt.contract_address.map(format_address);
        self.cumulative_gas_used =
            Some(receipt.cumulative_gas_used.as_usize() as u32);
        self.effective_gas_price = receipt.effective_gas_price;
        self.effective_transaction_fee = Some(effective_transaction_fee);
        self.gas_used = Some(gas_used.as_usize() as u32);
        self.status = Some(status)
    }
}
