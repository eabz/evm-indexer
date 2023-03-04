use std::collections::HashMap;

use crate::db::models::{
    block::DatabaseBlock, dex_trade::DatabaseDexTrade, erc1155_transfer::DatabaseERC1155Transfer,
    erc20_transfer::DatabaseERC20Transfer, erc721_transfer::DatabaseERC721Transfer,
    transaction::DatabaseTransaction,
};

#[derive(Debug, Clone)]
pub struct NativeTokenBalanceChange {
    pub address: String,
    pub balance_change: f64,
}

#[derive(Debug, Clone)]
pub struct ERC20TokenBalanceChange {
    pub token: String,
    pub address: String,
    pub balance_change: f64,
}

pub fn aggregate_data(
    blocks: &Vec<DatabaseBlock>,
    transactions: &Vec<DatabaseTransaction>,
    erc20_transfers: &Vec<DatabaseERC20Transfer>,
    erc721_transfers: &Vec<DatabaseERC721Transfer>,
    erc1155_transfers: &Vec<DatabaseERC1155Transfer>,
    dex_trades: &Vec<DatabaseDexTrade>,
) -> (
    HashMap<String, NativeTokenBalanceChange>,
    HashMap<(String, String), ERC20TokenBalanceChange>,
) {
    // first: calculate the rewards for each block and add it to the balance of the miner.
    let mut native_token_balance_changes: HashMap<String, NativeTokenBalanceChange> =
        HashMap::new();

    for block in blocks {
        // TODO: calculate real value
        let value_change = 0.0;

        let mut balance =
            get_native_balance_stored(&native_token_balance_changes, block.miner.clone());

        balance.balance_change += value_change;

        native_token_balance_changes.insert(block.miner.clone(), balance);
    }

    // second: aggregate balances for normal transfers for native tokens
    for transaction in transactions {
        let mut sender_balance = get_native_balance_stored(
            &native_token_balance_changes,
            transaction.from_address.clone(),
        );

        sender_balance.balance_change -= transaction.value;

        native_token_balance_changes.insert(transaction.from_address.clone(), sender_balance);

        let to_address = transaction.to_address.clone();

        if to_address.is_some() {
            let receiver = to_address.unwrap();

            let mut receiver_balance =
                get_native_balance_stored(&native_token_balance_changes, receiver.clone());

            receiver_balance.balance_change += transaction.value;

            native_token_balance_changes.insert(receiver.clone(), receiver_balance);
        }
    }

    // third: aggregate balances for all erc20 transfers
    let mut erc20_balance_changes: HashMap<(String, String), ERC20TokenBalanceChange> =
        HashMap::new();

    for transfer in erc20_transfers {
        let mut sender_balance = get_erc20_token_balance_stored(
            &erc20_balance_changes,
            transfer.token.clone(),
            transfer.from_address.clone(),
        );

        sender_balance.balance_change -= transfer.amount;

        erc20_balance_changes.insert(
            (transfer.token.clone(), transfer.from_address.clone()),
            sender_balance,
        );

        let mut receiver_balance = get_erc20_token_balance_stored(
            &erc20_balance_changes,
            transfer.token.clone(),
            transfer.to_address.clone(),
        );

        receiver_balance.balance_change -= transfer.amount;

        erc20_balance_changes.insert(
            (transfer.token.clone(), transfer.to_address.clone()),
            receiver_balance,
        );
    }

    // fourth: aggregate inventory for all erc721 transfers
    for transfer in erc721_transfers {}

    // five: aggregate inventory and balances for all erc1155 transfers
    for transfer in erc1155_transfers {}

    // six: aggregate all dex trades values.
    for trade in dex_trades {}

    return (native_token_balance_changes, erc20_balance_changes);
}

fn get_native_balance_stored(
    storage: &HashMap<String, NativeTokenBalanceChange>,
    address: String,
) -> NativeTokenBalanceChange {
    let stored_balance_change = storage.get(&address.clone());

    let balance_change: NativeTokenBalanceChange;

    if stored_balance_change.is_none() {
        balance_change = NativeTokenBalanceChange {
            address: address.clone(),
            balance_change: 0.0,
        };
    } else {
        balance_change = stored_balance_change.unwrap().to_owned();
    }

    return balance_change;
}

fn get_erc20_token_balance_stored(
    storage: &HashMap<(String, String), ERC20TokenBalanceChange>,
    token: String,
    address: String,
) -> ERC20TokenBalanceChange {
    let stored_balance_change = storage.get(&(token.clone(), address.clone()));

    let balance_change: ERC20TokenBalanceChange;

    if stored_balance_change.is_none() {
        balance_change = ERC20TokenBalanceChange {
            token: token.clone(),
            address: address.clone(),
            balance_change: 0.0,
        };
    } else {
        balance_change = stored_balance_change.unwrap().to_owned();
    }

    return balance_change;
}
