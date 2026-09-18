use alloy::primitives::{Address, Bytes, B256, U256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::simple_types::Trace;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, quantity_to_u256,
        quantity_to_u32, sat_u16, sat_u32,
    },
    format::{SerAddress, SerB256, SerBytes, SerU256},
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTrace {
    pub action_type: String,
    #[serde_as(as = "Option<SerAddress>")]
    pub address: Option<Address>,
    #[serde_as(as = "Option<SerAddress>")]
    pub author: Option<Address>,
    #[serde_as(as = "Option<SerU256>")]
    pub balance: Option<U256>,
    #[serde_as(as = "SerB256")]
    pub block_hash: B256,
    pub block_number: u32,
    pub call_type: Option<String>,
    pub chain: u64,
    #[serde_as(as = "Option<SerBytes>")]
    pub code: Option<Bytes>,
    pub error: Option<String>,
    #[serde_as(as = "Option<SerAddress>")]
    pub from: Option<Address>,
    pub gas: Option<u32>,
    pub gas_used: Option<u32>,
    #[serde_as(as = "Option<SerBytes>")]
    pub init: Option<Bytes>,
    #[serde_as(as = "Option<SerBytes>")]
    pub input: Option<Bytes>,
    #[serde_as(as = "Option<SerBytes>")]
    pub output: Option<Bytes>,
    #[serde_as(as = "Option<SerAddress>")]
    pub refund_address: Option<Address>,
    pub reward_type: Option<String>,
    pub subtraces: u16,
    #[serde_as(as = "Option<SerAddress>")]
    pub to: Option<Address>,
    pub trace_address: Vec<u16>,
    #[serde_as(as = "Option<SerB256>")]
    pub transaction_hash: Option<B256>,
    pub transaction_position: Option<u16>,
    #[serde_as(as = "Option<SerU256>")]
    pub value: Option<U256>,
}

impl DatabaseTrace {
    /// HyperSync serves parity style traces already flattened
    /// (action + result in one record), which maps 1:1 to the table.
    ///
    /// - `address`: result address of a `create` (the deployed contract).
    /// - `suicide`: parity reports the destroyed contract as
    ///   `action.address` (HyperSync `action_address`); the table has
    ///   always stored it in `from`.
    pub fn from_hypersync(trace: &Trace, chain: u64) -> Result<Self> {
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

        Ok(Self {
            action_type,
            address,
            author: trace.author.as_ref().map(address_to_alloy),
            balance: trace.balance.as_ref().map(quantity_to_u256),
            block_hash: trace
                .block_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            block_number: sat_u32(block_number),
            call_type: trace.call_type.clone(),
            chain,
            code: trace.code.as_ref().map(data_to_bytes),
            error: trace.error.clone(),
            from,
            gas: trace.gas.as_ref().map(quantity_to_u32),
            gas_used: trace.gas_used.as_ref().map(quantity_to_u32),
            init: trace.init.as_ref().map(data_to_bytes),
            input: trace.input.as_ref().map(data_to_bytes),
            output: trace.output.as_ref().map(data_to_bytes),
            refund_address: trace
                .refund_address
                .as_ref()
                .map(address_to_alloy),
            reward_type: trace.reward_type.clone(),
            subtraces: sat_u16(trace.subtraces.unwrap_or_default()),
            to: trace.to.as_ref().map(address_to_alloy),
            trace_address: trace
                .trace_address
                .as_ref()
                .map(|path| path.iter().map(|v| sat_u16(*v)).collect())
                .unwrap_or_default(),
            transaction_hash: trace
                .transaction_hash
                .as_ref()
                .map(hash_to_b256),
            transaction_position: trace.transaction_position.map(sat_u16),
            value: trace.value.as_ref().map(quantity_to_u256),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{Address as HsAddress, Quantity};

    #[test]
    fn gas_above_u32_saturates() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("call".to_string()),
            gas: Some(Quantity::from(u64::MAX)),
            gas_used: Some(Quantity::from(u32::MAX as u64 + 1)),
            subtraces: Some(100_000),
            trace_address: Some(vec![1, 70_000]),
            transaction_position: Some(66_000),
            ..Default::default()
        };

        let row = DatabaseTrace::from_hypersync(&trace, 1).unwrap();

        assert_eq!(row.gas, Some(u32::MAX));
        assert_eq!(row.gas_used, Some(u32::MAX));
        assert_eq!(row.subtraces, u16::MAX);
        assert_eq!(row.trace_address, vec![1, u16::MAX]);
        assert_eq!(row.transaction_position, Some(u16::MAX));
    }

    #[test]
    fn suicide_action_address_lands_in_from() {
        let trace = Trace {
            block_number: Some(1),
            type_: Some("suicide".to_string()),
            action_address: Some(HsAddress::from([9u8; 20])),
            ..Default::default()
        };

        let row = DatabaseTrace::from_hypersync(&trace, 1).unwrap();

        assert_eq!(row.from, Some(Address::repeat_byte(9)));
        assert_eq!(row.address, None);
    }

    #[test]
    fn missing_block_number_is_an_error() {
        assert!(
            DatabaseTrace::from_hypersync(&Trace::default(), 1).is_err()
        );
    }
}
