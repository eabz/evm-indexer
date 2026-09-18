use alloy::primitives::{Address, Bytes, B256, U256};
use anyhow::{Context, Result};
use clickhouse::Row;
use hypersync_client::{
    format::TransactionStatus, simple_types::Transaction,
};
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::utils::{
    convert::{
        address_to_alloy, data_to_bytes, hash_to_b256, quantity_to_u256,
        quantity_to_u32, sat_u16, sat_u32,
    },
    format::{SerAccessList, SerAddress, SerB256, SerBytes, SerU256},
};

#[serde_as]
#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseTransaction {
    #[serde_as(as = "SerAccessList")]
    pub access_list: Vec<(Address, Vec<B256>)>,
    pub base_fee_per_gas: Option<u64>,
    #[serde_as(as = "SerB256")]
    pub block_hash: B256,
    pub block_number: u32,
    pub chain: u64,
    #[serde_as(as = "Option<SerAddress>")]
    pub contract_created: Option<Address>,
    pub cumulative_gas_used: Option<u32>,
    #[serde_as(as = "Option<SerU256>")]
    pub effective_gas_price: Option<U256>,
    #[serde_as(as = "SerAddress")]
    pub from: Address,
    pub gas: u32,
    #[serde_as(as = "Option<SerU256>")]
    pub gas_price: Option<U256>,
    pub gas_used: Option<u32>,
    #[serde_as(as = "SerB256")]
    pub hash: B256,
    #[serde_as(as = "SerBytes")]
    pub input: Bytes,
    #[serde_as(as = "Option<SerU256>")]
    pub max_fee_per_gas: Option<U256>,
    #[serde_as(as = "Option<SerU256>")]
    pub max_priority_fee_per_gas: Option<U256>,
    pub method: String,
    pub nonce: u32,
    pub status: Option<String>,
    pub timestamp: u32,
    #[serde_as(as = "SerAddress")]
    pub to: Address,
    pub transaction_index: u16,
    pub transaction_type: String,
    #[serde_as(as = "SerU256")]
    pub value: U256,
}

/// EIP-2718 transaction type -> the label stored in `transaction_type`.
/// Unknown types keep their numeric value instead of being mislabeled.
pub fn transaction_type_label(transaction_type: Option<u8>) -> String {
    match transaction_type {
        None | Some(0) => "legacy".to_string(),
        Some(1) => "access_list".to_string(),
        Some(2) => "eip_1559".to_string(),
        Some(3) => "blob".to_string(),
        Some(4) => "set_code".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Receipt status -> the label stored in `status`. Pre-Byzantium receipts
/// have no status, those stay NULL.
pub fn transaction_status_label(
    status: Option<TransactionStatus>,
) -> Option<String> {
    status.map(|status| match status {
        TransactionStatus::Success => "success".to_string(),
        TransactionStatus::Failure => "failure".to_string(),
    })
}

/// First four bytes of the input as `0x` hex, `0x00000000` when absent.
pub fn method_selector(input: &[u8]) -> String {
    match input.get(..4) {
        Some(selector) => format!("0x{}", hex::encode(selector)),
        None => String::from("0x00000000"),
    }
}

impl DatabaseTransaction {
    pub fn from_hypersync(
        transaction: &Transaction,
        chain: u64,
        timestamp: u32,
        base_fee_per_gas: Option<u64>,
    ) -> Result<Self> {
        let hash = transaction
            .hash
            .as_ref()
            .map(hash_to_b256)
            .context("transaction without a hash in response")?;

        let block_number =
            transaction.block_number.map(u64::from).with_context(
                || format!("transaction {hash:?} without a block number"),
            )?;

        let access_list: Vec<(Address, Vec<B256>)> = transaction
            .access_list
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item.address
                                .as_ref()
                                .map(address_to_alloy)
                                .unwrap_or_default(),
                            item.storage_keys
                                .as_ref()
                                .map(|keys| {
                                    keys.iter().map(hash_to_b256).collect()
                                })
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let gas_price =
            transaction.gas_price.as_ref().map(quantity_to_u256);

        let max_priority_fee_per_gas = transaction
            .max_priority_fee_per_gas
            .as_ref()
            .map(quantity_to_u256);

        // Same fallback chain the RPC based indexer used when the receipt
        // did not report an effective gas price.
        let effective_gas_price = transaction
            .effective_gas_price
            .as_ref()
            .map(quantity_to_u256)
            .filter(|price| !price.is_zero())
            .or(gas_price)
            .or_else(|| {
                base_fee_per_gas.map(|base_fee| {
                    U256::from(base_fee).saturating_add(
                        max_priority_fee_per_gas.unwrap_or_default(),
                    )
                })
            })
            .unwrap_or_default();

        let input = transaction
            .input
            .as_ref()
            .map(data_to_bytes)
            .unwrap_or_default();

        Ok(Self {
            access_list,
            base_fee_per_gas,
            block_hash: transaction
                .block_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            block_number: sat_u32(block_number),
            chain,
            contract_created: transaction
                .contract_address
                .as_ref()
                .map(address_to_alloy),
            cumulative_gas_used: transaction
                .cumulative_gas_used
                .as_ref()
                .map(quantity_to_u32),
            effective_gas_price: Some(effective_gas_price),
            from: transaction
                .from
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            gas: transaction
                .gas
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            gas_price,
            gas_used: transaction.gas_used.as_ref().map(quantity_to_u32),
            hash,
            method: method_selector(&input),
            input,
            max_fee_per_gas: transaction
                .max_fee_per_gas
                .as_ref()
                .map(quantity_to_u256),
            max_priority_fee_per_gas,
            nonce: transaction
                .nonce
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            status: transaction_status_label(transaction.status),
            timestamp,
            // Contract creations have no recipient; the column is not
            // nullable and has always stored the zero address for them.
            to: transaction
                .to
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            transaction_index: sat_u16(
                transaction
                    .transaction_index
                    .map(u64::from)
                    .unwrap_or_default(),
            ),
            transaction_type: transaction_type_label(
                transaction.type_.map(u8::from),
            ),
            value: transaction
                .value
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{
        Hash, Quantity, TransactionType, UInt,
    };

    #[test]
    fn transaction_type_labels() {
        assert_eq!(transaction_type_label(None), "legacy");
        assert_eq!(transaction_type_label(Some(0)), "legacy");
        assert_eq!(transaction_type_label(Some(1)), "access_list");
        assert_eq!(transaction_type_label(Some(2)), "eip_1559");
        assert_eq!(transaction_type_label(Some(3)), "blob");
        assert_eq!(transaction_type_label(Some(4)), "set_code");
        // Unknown (e.g. OP deposit 0x7e) keeps the numeric value.
        assert_eq!(transaction_type_label(Some(126)), "126");
    }

    #[test]
    fn transaction_status_labels() {
        assert_eq!(
            transaction_status_label(Some(TransactionStatus::Success)),
            Some("success".to_string())
        );
        assert_eq!(
            transaction_status_label(Some(TransactionStatus::Failure)),
            Some("failure".to_string())
        );
        assert_eq!(transaction_status_label(None), None);
    }

    #[test]
    fn method_selector_handles_short_input() {
        assert_eq!(method_selector(&[]), "0x00000000");
        assert_eq!(method_selector(&[1, 2, 3]), "0x00000000");
        assert_eq!(
            method_selector(&[0xa9, 0x05, 0x9c, 0xbb, 0xff]),
            "0xa9059cbb"
        );
    }

    fn minimal_transaction() -> Transaction {
        Transaction {
            hash: Some(Hash::from([1u8; 32])),
            block_number: Some(UInt::from(10u64)),
            ..Default::default()
        }
    }

    #[test]
    fn sparse_transaction_uses_defaults_and_never_panics() {
        let row = DatabaseTransaction::from_hypersync(
            &minimal_transaction(),
            1,
            1_700_000_000,
            None,
        )
        .unwrap();

        assert_eq!(row.block_number, 10);
        assert_eq!(row.to, Address::ZERO);
        assert_eq!(row.from, Address::ZERO);
        assert_eq!(row.status, None);
        assert_eq!(row.transaction_type, "legacy");
        assert_eq!(row.method, "0x00000000");
        assert_eq!(row.effective_gas_price, Some(U256::ZERO));
        assert!(row.access_list.is_empty());
    }

    #[test]
    fn missing_identity_fields_are_errors_not_panics() {
        let transaction = Transaction::default();
        assert!(DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            None
        )
        .is_err());
    }

    #[test]
    fn wide_values_saturate() {
        let mut transaction = minimal_transaction();
        transaction.gas = Some(Quantity::from(u64::MAX));
        transaction.nonce = Some(Quantity::from(u32::MAX as u64 + 5));
        transaction.transaction_index = Some(UInt::from(70_000u64));
        transaction.type_ = Some(TransactionType::from(4u8));
        transaction.status = Some(TransactionStatus::Success);

        let row =
            DatabaseTransaction::from_hypersync(&transaction, 1, 0, None)
                .unwrap();

        assert_eq!(row.gas, u32::MAX);
        assert_eq!(row.nonce, u32::MAX);
        assert_eq!(row.transaction_index, u16::MAX);
        assert_eq!(row.transaction_type, "set_code");
        assert_eq!(row.status.as_deref(), Some("success"));
    }

    #[test]
    fn effective_gas_price_fallbacks() {
        // 1. reported by the receipt
        let mut transaction = minimal_transaction();
        transaction.effective_gas_price = Some(Quantity::from(7u64));
        transaction.gas_price = Some(Quantity::from(9u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(100),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, Some(U256::from(7u64)));

        // 2. zero / missing -> gas price
        transaction.effective_gas_price = Some(Quantity::from(0u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(100),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, Some(U256::from(9u64)));

        // 3. no gas price -> base fee + priority fee
        transaction.gas_price = None;
        transaction.max_priority_fee_per_gas = Some(Quantity::from(2u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(100),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, Some(U256::from(102u64)));
    }
}
