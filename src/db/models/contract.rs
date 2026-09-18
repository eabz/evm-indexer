use alloy::primitives::{Address, B256};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use super::{trace::DatabaseTrace, transaction::DatabaseTransaction};
use crate::utils::format::{SerAddress, SerB256};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseContract {
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "SerAddress")]
    pub contract_address: Address,
    #[serde_as(as = "SerAddress")]
    pub creator: Address,
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
}

impl DatabaseContract {
    /// A contract deployed directly by a transaction: the receipt carries
    /// a contract address and the transaction did not fail. (Nodes report
    /// the would-be address for failed deployments too.)
    pub fn from_transaction(
        transaction: &DatabaseTransaction,
    ) -> Option<Self> {
        let contract_address = transaction.contract_created?;

        if transaction.status.as_deref() == Some("failure") {
            return None;
        }

        Some(Self {
            block_number: transaction.block_number,
            chain: transaction.chain,
            contract_address,
            creator: transaction.from,
            transaction_hash: transaction.hash,
        })
    }

    /// A contract deployed by a `create` trace (factories, CREATE2...).
    /// Failed creations have no result address and are skipped.
    ///
    /// This only looks at the trace itself. A create can also be rolled
    /// back by its surroundings (failed transaction, reverted parent
    /// call); the caller filters those, see `pipeline::transform`.
    pub fn from_trace(trace: &DatabaseTrace) -> Option<Self> {
        if trace.action_type != "create" || trace.error.is_some() {
            return None;
        }

        Some(Self {
            block_number: trace.block_number,
            chain: trace.chain,
            contract_address: trace.address?,
            creator: trace.from?,
            transaction_hash: trace.transaction_hash?,
        })
    }
}
