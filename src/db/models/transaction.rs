use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::rpc::types::{Transaction, TransactionReceipt};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::serde_as;

use crate::utils::format::{byte4_from_input, format_bytes, SerU256};

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq)]
#[repr(u8)]
pub enum TransactionType {
    Legacy = 0,
    AccessList = 1,
    Eip1559 = 2,
    Blob = 3,
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
    pub access_list: Vec<(Address, Vec<B256>)>,
    pub base_fee_per_gas: Option<u64>,
    pub block_hash: B256,
    pub block_number: u32,
    pub chain: u64,
    pub contract_created: Option<Address>,
    pub cumulative_gas_used: Option<u32>,
    #[serde_as(as = "Option<SerU256>")]
    pub effective_gas_price: Option<U256>,
    pub from: Address,
    pub gas: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub gas_price: Option<U256>,
    pub gas_used: Option<u32>,
    pub hash: B256,
    pub input: Bytes,
    #[serde_as(as = "Option<SerU256>")]
    pub max_fee_per_gas: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub max_priority_fee_per_gas: Option<U256>,
    pub method: String,
    pub nonce: u32,
    pub status: Option<TransactionStatus>,
    pub timestamp: u32,
    pub to: Address,
    pub transaction_index: u16,
    pub transaction_type: TransactionType,
    #[serde_as(as = "SerU256")]
    pub value: U256,
}

impl DatabaseTransaction {
    pub fn from_rpc(
        transaction: &Transaction,
        receipt: &TransactionReceipt,
        chain: u64,
        timestamp: u32,
        base_fee_per_gas: Option<u64>,
    ) -> Self {
        let to = transaction.to.unwrap_or(Address::ZERO);

        let transaction_type: TransactionType =
            match transaction.transaction_type {
                Some(transaction_type) => match transaction_type.into() {
                    0 => TransactionType::Legacy,
                    1 => TransactionType::AccessList,
                    2 => TransactionType::Eip1559,
                    3 => TransactionType::Blob,
                    _ => TransactionType::Legacy,
                },
                None => TransactionType::Legacy,
            };

        let access_list: Vec<(Address, Vec<B256>)> =
            match transaction.access_list.to_owned() {
                Some(access_list_items) => {
                    let mut access_list: Vec<(Address, Vec<B256>)> =
                        Vec::new();

                    for item in access_list_items.0 {
                        access_list.push((item.address, item.storage_keys))
                    }

                    access_list
                }
                None => Vec::new(),
            };

        let status = if receipt.status() {
            Some(TransactionStatus::Success)
        } else {
            Some(TransactionStatus::Failure)
        };

        let effective_gas_price = match receipt.effective_gas_price {
            0 => match transaction.gas_price {
                Some(gas_price) => U256::from(gas_price),
                None => match base_fee_per_gas {
                    Some(base_fee_per_gas) => {
                        let max_priority_fee_per_gas = transaction
                            .max_priority_fee_per_gas
                            .map(U256::from)
                            .unwrap_or_default();

                        U256::from(base_fee_per_gas)
                            + max_priority_fee_per_gas
                    }
                    None => U256::ZERO,
                },
            },
            _ => U256::from(receipt.effective_gas_price),
        };

        Self {
            access_list,
            base_fee_per_gas,
            block_hash: transaction.block_hash.unwrap(),
            block_number: transaction.block_number.unwrap() as u32,
            chain,
            contract_created: receipt.contract_address,
            cumulative_gas_used: Some(match &receipt.inner {
                alloy::consensus::ReceiptEnvelope::Legacy(r) => {
                    r.receipt.cumulative_gas_used
                }
                alloy::consensus::ReceiptEnvelope::Eip2930(r) => {
                    r.receipt.cumulative_gas_used
                }
                alloy::consensus::ReceiptEnvelope::Eip1559(r) => {
                    r.receipt.cumulative_gas_used
                }
                alloy::consensus::ReceiptEnvelope::Eip4844(r) => {
                    r.receipt.cumulative_gas_used
                }
                _ => 0,
            } as u32),
            effective_gas_price: Some(effective_gas_price),
            from: transaction.from,
            gas: transaction.gas as u32,
            gas_price: transaction.gas_price.map(|p| U256::from(p)),
            gas_used: Some(receipt.gas_used as u32),
            hash: transaction.hash,
            input: transaction.input.clone(),
            max_fee_per_gas: transaction
                .max_fee_per_gas
                .map(|p| U256::from(p)),
            max_priority_fee_per_gas: transaction
                .max_priority_fee_per_gas
                .map(|p| U256::from(p)),
            method: format!(
                "0x{}",
                hex::encode(byte4_from_input(&format_bytes(
                    &transaction.input
                )))
            ),
            nonce: transaction.nonce as u32,
            status,
            timestamp,
            to,
            transaction_index: transaction.transaction_index.unwrap()
                as u16,
            transaction_type,
            value: transaction.value,
        }
    }
}
