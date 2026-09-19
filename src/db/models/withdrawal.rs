use alloy::primitives::{Address, U256};
use clickhouse::Row;
use hypersync_client::format::Withdrawal;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::{
    db::format::{SerAddress, SerU256},
    utils::convert::{
        address_to_alloy, quantity_to_u256, quantity_to_u64,
    },
};

#[serde_as]
#[derive(Debug, Clone, PartialEq, Row, Serialize, Deserialize)]
pub struct DatabaseWithdrawal {
    pub chain: u64,
    pub block_number: u64,
    pub withdrawal_index: u64,
    pub validator_index: u64,
    #[serde_as(as = "SerAddress")]
    pub address: Address,
    #[serde_as(as = "SerU256")]
    pub amount: U256,
    pub timestamp: u32,
    /// The chain's purge generation, stamped once per flush, see
    /// `RowBatch::set_epoch`.
    pub epoch: u32,
    /// Stamped once per flush, see `RowBatch::set_version`.
    pub _version: u64,
}

impl DatabaseWithdrawal {
    pub fn from_hypersync(
        withdrawal: &Withdrawal,
        chain: u64,
        block_number: u64,
        timestamp: u32,
    ) -> Self {
        Self {
            address: withdrawal
                .address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            amount: withdrawal
                .amount
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
            block_number,
            chain,
            timestamp,
            validator_index: withdrawal
                .validator_index
                .as_ref()
                .map(quantity_to_u64)
                .unwrap_or_default(),
            withdrawal_index: withdrawal
                .index
                .as_ref()
                .map(quantity_to_u64)
                .unwrap_or_default(),
            epoch: 0,
            _version: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypersync_client::format::{Address as HsAddress, Quantity};

    #[test]
    fn validator_and_withdrawal_index_are_distinct_fields() {
        let withdrawal = Withdrawal {
            index: Some(Quantity::from(1_000u64)),
            validator_index: Some(Quantity::from(42u64)),
            address: Some(HsAddress::from([7u8; 20])),
            amount: Some(Quantity::from(32_000_000_000u64)),
        };

        let row =
            DatabaseWithdrawal::from_hypersync(&withdrawal, 1, 17, 99);

        assert_eq!(row.withdrawal_index, 1_000);
        assert_eq!(row.validator_index, 42);
        assert_eq!(row.amount, U256::from(32_000_000_000u64));
        assert_eq!(row.address, Address::repeat_byte(7));
        assert_eq!(row.block_number, 17);
        assert_eq!(row.timestamp, 99);
    }

    #[test]
    fn empty_withdrawal_defaults() {
        let row = DatabaseWithdrawal::from_hypersync(
            &Withdrawal::default(),
            1,
            1,
            1,
        );
        assert_eq!(row.validator_index, 0);
        assert_eq!(row.amount, U256::ZERO);
    }
}
