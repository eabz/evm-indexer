mod bsc;
mod ethereum;
mod polygon;

use std::collections::HashMap;

use primitive_types::{H160, U256};
use serde::{Deserialize, Serialize};

use crate::{
    chains::Chain,
    db::models::transaction::{
        DatabaseTransaction, TransactionStatus, TransactionType,
    },
    utils::format::format_address,
};

#[derive(Serialize, Deserialize, Debug)]
pub struct BalanceAllocation {
    balance: U256,
}

pub fn get_genesis_allocations(chain: Chain) -> Vec<DatabaseTransaction> {
    let mut transactions = Vec::new();

    let mut allocations = HashMap::new();

    if chain.id == 1 {
        allocations = ethereum::get_genesis_allocation();
    } else if chain.id == 56 {
        allocations = bsc::get_genesis_allocation();
    } else if chain.id == 137 {
        allocations = polygon::get_genesis_allocation();
    }

    for (i, (receiver, balance)) in allocations.iter().enumerate() {
        let transaction = DatabaseTransaction {
            access_list: Vec::new(),
            base_fee_per_gas: None,
            block_hash: chain.genesis_hash.to_owned(),
            block_number: 0,
            burned: None,
            chain: chain.id,
            contract_created: None,
            cumulative_gas_used: None,
            effective_gas_price: None,
            effective_transaction_fee: None,
            from: format_address(H160::zero()),
            gas: 0,
            gas_price: None,
            gas_used: None,
            hash: format!("{}_GENESIS_{}", chain.name.to_uppercase(), i),
            input: String::from("0x"),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            method: String::from("0x000000"),
            nonce: 0,
            timestamp: chain.genesis_timestamp,
            status: Some(TransactionStatus::Success),
            to: receiver.to_string(),
            transaction_index: 0,
            transaction_type: TransactionType::Legacy,
            value: balance.balance,
        };

        transactions.push(transaction);
    }

    transactions
}
