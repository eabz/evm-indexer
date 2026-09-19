use alloy::primitives::{Address, Bytes, Selector, B256, U256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::db::format::{
    SerAddress, SerB256, SerBytes, SerSelector, SerU256,
};

/// Values of `transactions.status` (NULL before Byzantium). The SQL
/// aggregates compare against these exact strings.
pub const STATUS_SUCCESS: &str = "success";
pub const STATUS_FAILURE: &str = "failure";

/// Row of `transactions`. Field names are the column names.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseTransaction {
    pub chain: u64,
    pub block_number: u64,
    pub transaction_index: u32,
    #[serde_as(as = "SerB256")]
    pub hash: B256,
    #[serde_as(as = "SerB256")]
    pub block_hash: B256,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub from: Address,
    /// NULL for contract creations.
    #[serde_as(as = "Option<SerAddress>")]
    pub to: Option<Address>,
    /// Zero address when the transaction did not create a contract.
    #[serde_as(as = "SerAddress")]
    pub contract_created: Address,
    #[serde_as(as = "SerU256")]
    pub value: U256,
    #[serde_as(as = "SerBytes")]
    pub input: Bytes,
    #[serde_as(as = "SerSelector")]
    pub method: Selector,
    pub nonce: u64,
    pub transaction_type: String,
    pub status: Option<String>,
    pub gas: u64,
    pub gas_used: u64,
    pub cumulative_gas_used: u64,
    #[serde_as(as = "Option<SerU256>")]
    pub gas_price: Option<U256>,
    #[serde_as(as = "SerU256")]
    pub effective_gas_price: U256,
    #[serde_as(as = "Option<SerU256>")]
    pub max_fee_per_gas: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub max_priority_fee_per_gas: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub base_fee_per_gas: Option<U256>,
    #[serde_as(as = "Vec<(SerAddress, Vec<SerB256>)>")]
    pub access_list: Vec<(Address, Vec<B256>)>,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseTransaction {
    /// The contract this transaction deployed, if any.
    pub fn created_contract(&self) -> Option<Address> {
        (!self.contract_created.is_zero()).then_some(self.contract_created)
    }
}
