use clickhouse::Row;
use ethers::types::TransactionReceipt;
use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::utils::format::{format_address, format_hash};

#[derive(Debug, Clone, Serialize_repr, Deserialize_repr, PartialEq, Eq)]
#[repr(u8)]
pub enum TransactionStatus {
    Reverted,
    Succeed,
    Pending,
}

impl TransactionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransactionStatus::Reverted => "reverted",
            TransactionStatus::Succeed => "succeed",
            TransactionStatus::Pending => "pending",
        }
    }
}

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseReceipt {
    pub contract_address: Option<String>,
    pub cumulative_gas_used: i64,
    pub effective_gas_price: Option<i64>,
    pub gas_used: i64,
    pub hash: String,
    pub status: TransactionStatus,
}

impl DatabaseReceipt {
    pub fn from_rpc(receipt: &TransactionReceipt) -> Self {
        let contract_address: Option<String> = match receipt.contract_address {
            None => None,
            Some(contract_address) => Some(format_address(contract_address)),
        };

        let status: TransactionStatus = match receipt.status {
            None => TransactionStatus::Succeed,
            Some(status) => {
                let status_number = status.as_u64() as i64;

                if status_number == 0 {
                    TransactionStatus::Reverted
                } else {
                    TransactionStatus::Succeed
                }
            }
        };

        let effective_gas_price: Option<i64> = match receipt.effective_gas_price {
            None => None,
            Some(effective_gas_price) => Some(effective_gas_price.as_u64() as i64),
        };

        Self {
            contract_address,
            cumulative_gas_used: receipt.cumulative_gas_used.as_u64() as i64,
            effective_gas_price,
            gas_used: receipt.gas_used.unwrap().as_u64() as i64,
            hash: format_hash(receipt.transaction_hash),
            status,
        }
    }
}
