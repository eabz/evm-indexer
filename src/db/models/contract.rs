use alloy::primitives::{Address, B256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::{
    trace::DatabaseTrace,
    transaction::{DatabaseTransaction, STATUS_FAILURE},
};
use crate::utils::format::{SerAddress, SerB256};

#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseContract {
    pub chain: u64,
    pub block_number: u64,
    #[serde_as(as = "SerAddress")]
    pub contract_address: Address,
    #[serde_as(as = "SerAddress")]
    pub creator: Address,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    pub timestamp: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseContract {
    /// A contract deployed directly by a transaction: the receipt carries
    /// a contract address and the transaction did not fail. (Nodes report
    /// the would-be address for failed deployments too.)
    pub fn from_transaction(
        transaction: &DatabaseTransaction,
    ) -> Option<Self> {
        let contract_address = transaction.created_contract()?;

        if transaction.status.as_deref() == Some(STATUS_FAILURE) {
            return None;
        }

        Some(Self {
            chain: transaction.chain,
            block_number: transaction.block_number,
            contract_address,
            creator: transaction.from,
            transaction_hash: transaction.hash,
            timestamp: transaction.timestamp,
            _version: 0,
        })
    }

    /// A contract deployed by a `create` trace (factories, CREATE2...).
    /// Failed creations have no result address and are skipped.
    ///
    /// This only looks at the trace itself. A create can also be rolled
    /// back by its surroundings (failed transaction, reverted parent
    /// call); the caller filters those, see `pipeline::transform`.
    pub fn from_trace(trace: &DatabaseTrace) -> Option<Self> {
        if trace.action_type != "create" || trace.failed() {
            return None;
        }

        Some(Self {
            chain: trace.chain,
            block_number: trace.block_number,
            contract_address: trace.address?,
            creator: trace.from?,
            transaction_hash: trace.transaction_hash()?,
            timestamp: trace.timestamp,
            _version: 0,
        })
    }
}
