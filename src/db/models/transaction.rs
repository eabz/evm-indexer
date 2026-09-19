use alloy::primitives::{Address, Bytes, Selector, B256, U256};
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
        quantity_to_u64, sat_u32,
    },
    format::{
        method_selector, SerAddress, SerB256, SerBytes, SerSelector,
        SerU256,
    },
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
        TransactionStatus::Success => STATUS_SUCCESS.to_string(),
        TransactionStatus::Failure => STATUS_FAILURE.to_string(),
    })
}

impl DatabaseTransaction {
    pub fn from_hypersync(
        transaction: &Transaction,
        chain: u64,
        timestamp: u32,
        base_fee_per_gas: Option<U256>,
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
                    base_fee.saturating_add(
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

        let opt_u64 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u64).unwrap_or_default()
        };

        Ok(Self {
            chain,
            block_number,
            transaction_index: sat_u32(
                transaction
                    .transaction_index
                    .map(u64::from)
                    .unwrap_or_default(),
            ),
            hash,
            block_hash: transaction
                .block_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            timestamp,
            from: transaction
                .from
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            // Contract creations have no recipient: NULL, the zero address
            // is a real (burn) recipient.
            to: transaction.to.as_ref().map(address_to_alloy),
            contract_created: transaction
                .contract_address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            value: transaction
                .value
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
            method: method_selector(&input),
            input,
            nonce: opt_u64(&transaction.nonce),
            transaction_type: transaction_type_label(
                transaction.type_.map(u8::from),
            ),
            status: transaction_status_label(transaction.status),
            gas: opt_u64(&transaction.gas),
            gas_used: opt_u64(&transaction.gas_used),
            cumulative_gas_used: opt_u64(&transaction.cumulative_gas_used),
            gas_price,
            effective_gas_price,
            max_fee_per_gas: transaction
                .max_fee_per_gas
                .as_ref()
                .map(quantity_to_u256),
            max_priority_fee_per_gas,
            base_fee_per_gas,
            access_list,
            epoch: 0,
            _version: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{
        Address as HsAddress, Data, Hash, Quantity, TransactionType, UInt,
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
        assert_eq!(row.to, None);
        assert_eq!(row.from, Address::ZERO);
        assert_eq!(row.contract_created, Address::ZERO);
        assert_eq!(row.created_contract(), None);
        assert_eq!(row.status, None);
        assert_eq!(row.transaction_type, "legacy");
        assert_eq!(row.method, Selector::ZERO);
        assert_eq!(row.effective_gas_price, U256::ZERO);
        assert_eq!(row.gas_price, None);
        assert_eq!(row.gas_used, 0);
        assert!(row.access_list.is_empty());
        assert_eq!(row._version, 0);
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
    fn wide_values_are_kept() {
        let mut transaction = minimal_transaction();
        transaction.gas = Some(Quantity::from(u64::MAX));
        transaction.nonce = Some(Quantity::from(u32::MAX as u64 + 5));
        transaction.transaction_index = Some(UInt::from(70_000u64));
        transaction.type_ = Some(TransactionType::from(4u8));
        transaction.status = Some(TransactionStatus::Success);
        transaction.input =
            Some(Data::from(vec![0xa9, 0x05, 0x9c, 0xbb, 0x01]));
        transaction.to = Some(HsAddress::from([0u8; 20]));
        transaction.contract_address = Some(HsAddress::from([7u8; 20]));

        let row =
            DatabaseTransaction::from_hypersync(&transaction, 1, 0, None)
                .unwrap();

        assert_eq!(row.gas, u64::MAX);
        assert_eq!(row.nonce, u32::MAX as u64 + 5);
        assert_eq!(row.transaction_index, 70_000);
        assert_eq!(row.transaction_type, "set_code");
        assert_eq!(row.status.as_deref(), Some(STATUS_SUCCESS));
        assert_eq!(row.method, Selector::from([0xa9, 0x05, 0x9c, 0xbb]));
        // The zero address is a recipient, not "no recipient".
        assert_eq!(row.to, Some(Address::ZERO));
        assert_eq!(row.created_contract(), Some(Address::repeat_byte(7)));
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
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(7u64));

        // 2. zero / missing -> gas price
        transaction.effective_gas_price = Some(Quantity::from(0u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(9u64));

        // 3. no gas price -> base fee + priority fee
        transaction.gas_price = None;
        transaction.max_priority_fee_per_gas = Some(Quantity::from(2u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(102u64));
    }
}
