use alloy::primitives::{Address, Bytes, B256, U256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::simple_types::Trace;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, quantity_to_u256,
        quantity_to_u64, sat_u32,
    },
    format::{SerAddress, SerB256, SerBytes, SerU256},
};

/// `transaction_position` of block / uncle reward traces, which belong to
/// no transaction. The column is part of the sorting key, so it can not be
/// NULL. (`UInt32` maximum: no block holds that many transactions.)
pub const REWARD_TRANSACTION_POSITION: u32 = u32::MAX;

/// Row of `traces`. Field names are the column names.
///
/// The `Option` fields are the ones that do not apply to every action type
/// (`call` / `create` / `suicide` / `reward`) and are NULL when they don't.
#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseTrace {
    pub chain: u64,
    pub block_number: u64,
    /// [`REWARD_TRANSACTION_POSITION`] for reward traces.
    pub transaction_position: u32,
    pub trace_address: Vec<u32>,
    /// Zero bytes for reward traces, see [`Self::transaction_hash`].
    #[serde_as(as = "SerB256")]
    pub transaction_hash: B256,
    #[serde_as(as = "SerB256")]
    pub block_hash: B256,
    pub timestamp: u32,
    pub action_type: String,
    pub subtraces: u32,
    /// Empty when the trace did not fail, see [`Self::failed`].
    pub error: String,
    #[serde_as(as = "Option<SerAddress>")]
    pub from: Option<Address>,
    #[serde_as(as = "Option<SerAddress>")]
    pub to: Option<Address>,
    #[serde_as(as = "Option<SerU256>")]
    pub value: Option<U256>,
    pub gas: Option<u64>,
    pub gas_used: Option<u64>,
    pub call_type: Option<String>,
    #[serde_as(as = "Option<SerBytes>")]
    pub input: Option<Bytes>,
    #[serde_as(as = "Option<SerBytes>")]
    pub output: Option<Bytes>,
    #[serde_as(as = "Option<SerBytes>")]
    pub init: Option<Bytes>,
    #[serde_as(as = "Option<SerBytes>")]
    pub code: Option<Bytes>,
    #[serde_as(as = "Option<SerAddress>")]
    pub address: Option<Address>,
    #[serde_as(as = "Option<SerAddress>")]
    pub refund_address: Option<Address>,
    #[serde_as(as = "Option<SerU256>")]
    pub balance: Option<U256>,
    #[serde_as(as = "Option<SerAddress>")]
    pub author: Option<Address>,
    pub reward_type: Option<String>,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseTrace {
    /// HyperSync serves parity style traces already flattened
    /// (action + result in one record), which maps 1:1 to the table.
    ///
    /// - `address`: result address of a `create` (the deployed contract).
    /// - `suicide`: parity reports the destroyed contract as
    ///   `action.address` (HyperSync `action_address`); the table has
    ///   always stored it in `from`.
    /// - traces without a transaction (rewards) get the sentinel position
    ///   and a zero transaction hash.
    ///
    /// `transaction_index` is the index of the trace's transaction as known
    /// from the transactions of the same response. It is only used when
    /// the trace itself carries no position: the position is part of the
    /// sorting key, so guessing one would let traces replace each other.
    pub fn from_hypersync(
        trace: &Trace,
        chain: u64,
        timestamp: u32,
        transaction_index: Option<u32>,
    ) -> Result<Self> {
        let block_number = trace
            .block_number
            .context("trace without a block number in response")?;

        let action_type =
            trace.type_.clone().unwrap_or_else(|| "unknown".to_string());

        let action_address =
            trace.action_address.as_ref().map(address_to_alloy);

        let mut from = trace.from.as_ref().map(address_to_alloy);
        let mut address = trace.address.as_ref().map(address_to_alloy);

        if action_type == "suicide" {
            from = from.or(action_address);
        } else {
            address = address.or(action_address);
        }

        let transaction_hash =
            trace.transaction_hash.as_ref().map(hash_to_b256);

        let transaction_position =
            match (trace.transaction_position, transaction_hash) {
                (Some(position), _) => sat_u32(position),
                (None, None) => REWARD_TRANSACTION_POSITION,
                (None, Some(hash)) => {
                    transaction_index.with_context(|| {
                        format!(
                            "trace of transaction {hash:?} in block \
                         {block_number} without a transaction position"
                        )
                    })?
                }
            };

        Ok(Self {
            chain,
            block_number,
            transaction_position,
            trace_address: trace
                .trace_address
                .as_ref()
                .map(|path| path.iter().map(|v| sat_u32(*v)).collect())
                .unwrap_or_default(),
            transaction_hash: transaction_hash.unwrap_or_default(),
            block_hash: trace
                .block_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            timestamp,
            action_type,
            subtraces: sat_u32(trace.subtraces.unwrap_or_default()),
            error: trace.error.clone().unwrap_or_default(),
            from,
            to: trace.to.as_ref().map(address_to_alloy),
            value: trace.value.as_ref().map(quantity_to_u256),
            gas: trace.gas.as_ref().map(quantity_to_u64),
            gas_used: trace.gas_used.as_ref().map(quantity_to_u64),
            call_type: trace.call_type.clone(),
            input: trace.input.as_ref().map(data_to_bytes),
            output: trace.output.as_ref().map(data_to_bytes),
            init: trace.init.as_ref().map(data_to_bytes),
            code: trace.code.as_ref().map(data_to_bytes),
            address,
            refund_address: trace
                .refund_address
                .as_ref()
                .map(address_to_alloy),
            balance: trace.balance.as_ref().map(quantity_to_u256),
            author: trace.author.as_ref().map(address_to_alloy),
            reward_type: trace.reward_type.clone(),
            _version: 0,
        })
    }

    /// The transaction this trace belongs to, `None` for reward traces.
    pub fn transaction_hash(&self) -> Option<B256> {
        (!self.transaction_hash.is_zero()).then_some(self.transaction_hash)
    }

    pub fn failed(&self) -> bool {
        !self.error.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{Address as HsAddress, Hash, Quantity};

    #[test]
    fn wide_values_are_kept() {
        let trace = Trace {
            block_number: Some(u32::MAX as u64 + 1),
            type_: Some("call".to_string()),
            gas: Some(Quantity::from(u64::MAX)),
            gas_used: Some(Quantity::from(u32::MAX as u64 + 1)),
            subtraces: Some(100_000),
            trace_address: Some(vec![1, 70_000]),
            transaction_position: Some(66_000),
            transaction_hash: Some(Hash::from([4u8; 32])),
            ..Default::default()
        };

        let row =
            DatabaseTrace::from_hypersync(&trace, 1, 99, None).unwrap();

        assert_eq!(row.block_number, u32::MAX as u64 + 1);
        assert_eq!(row.gas, Some(u64::MAX));
        assert_eq!(row.gas_used, Some(u32::MAX as u64 + 1));
        assert_eq!(row.subtraces, 100_000);
        assert_eq!(row.trace_address, vec![1, 70_000]);
        assert_eq!(row.transaction_position, 66_000);
        assert_eq!(row.transaction_hash(), Some(B256::repeat_byte(4)));
        assert_eq!(row.timestamp, 99);
        assert!(!row.failed());
    }

    #[test]
    fn reward_traces_get_the_sentinel_position() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("reward".to_string()),
            reward_type: Some("block".to_string()),
            author: Some(HsAddress::from([8u8; 20])),
            value: Some(Quantity::from(2u64)),
            ..Default::default()
        };

        let row =
            DatabaseTrace::from_hypersync(&trace, 1, 0, None).unwrap();

        assert_eq!(row.transaction_position, REWARD_TRANSACTION_POSITION);
        assert_eq!(row.transaction_position, 4_294_967_295);
        assert_eq!(row.transaction_hash, B256::ZERO);
        assert_eq!(row.transaction_hash(), None);
        assert_eq!(row.author, Some(Address::repeat_byte(8)));
        // Does not apply to a reward: NULL, not a default.
        assert_eq!(row.gas, None);
        assert_eq!(row.from, None);
        assert_eq!(row.input, None);
    }

    #[test]
    fn errors_are_a_plain_string() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("call".to_string()),
            error: Some("Reverted".to_string()),
            ..Default::default()
        };

        let row =
            DatabaseTrace::from_hypersync(&trace, 1, 0, None).unwrap();
        assert!(row.failed());
        assert_eq!(row.error, "Reverted");
    }

    #[test]
    fn suicide_action_address_lands_in_from() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("suicide".to_string()),
            action_address: Some(HsAddress::from([9u8; 20])),
            ..Default::default()
        };

        let row =
            DatabaseTrace::from_hypersync(&trace, 1, 0, None).unwrap();

        assert_eq!(row.from, Some(Address::repeat_byte(9)));
        assert_eq!(row.address, None);
    }

    #[test]
    fn missing_block_number_is_an_error() {
        assert!(DatabaseTrace::from_hypersync(
            &Trace::default(),
            1,
            0,
            None
        )
        .is_err());
    }

    #[test]
    fn a_missing_position_is_taken_from_the_transaction_or_refused() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("call".to_string()),
            transaction_hash: Some(Hash::from([4u8; 32])),
            ..Default::default()
        };

        let row =
            DatabaseTrace::from_hypersync(&trace, 1, 0, Some(12)).unwrap();
        assert_eq!(row.transaction_position, 12);

        // Never guessed: it is part of the sorting key.
        assert!(DatabaseTrace::from_hypersync(&trace, 1, 0, None).is_err());
    }
}
