use dotenv::dotenv;
use futures::future::join_all;
use indexer::{
    chains::chains::Chain,
    configs::config::Config,
    db::{
        db::Database,
        models::{
            block::DatabaseBlock, chain_state::DatabaseChainIndexedState,
            contract::DatabaseContract, dex_trade::DatabaseDexTrade,
            erc1155_transfer::DatabaseERC1155Transfer, erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog, receipt::DatabaseReceipt,
            transaction::DatabaseTransaction,
        },
    },
    rpc::rpc::Rpc,
    utils::{
        aggregate::aggregate_data,
        events::{
            ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE, ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
            SWAPV3_EVENT_SIGNATURE, SWAP_EVENT_SIGNATURE, TRANSFER_EVENTS_SIGNATURE,
        },
        tokens::get_tokens,
    },
};
use log::*;
use simple_logger::SimpleLogger;
use std::{collections::HashSet, thread::sleep, time::Duration};

#[tokio::main()]
async fn main() {
    dotenv().ok();

    let log = SimpleLogger::new().with_level(LevelFilter::Info);

    let config = Config::new();

    if config.debug {
        log.with_level(LevelFilter::Debug).init().unwrap();
    } else {
        log.init().unwrap();
    }

    info!("Starting EVM Indexer.");

    info!("Syncing chain {}.", config.chain.name.clone());

    let rpc = Rpc::new(&config)
        .await
        .expect("Unable to start RPC client.");

    let db = Database::new(
        config.db_url.clone(),
        config.redis_url.clone(),
        config.agg_db_url.clone(),
        config.chain.clone(),
    )
    .await
    .expect("Unable to start DB connection.");

    let mut indexed_blocks = db.get_indexed_blocks().await.unwrap();

    loop {
        sync_chain(&rpc, &db, &config, &mut indexed_blocks).await;
        sleep(Duration::from_millis(500))
    }
}

async fn sync_chain(rpc: &Rpc, db: &Database, config: &Config, indexed_blocks: &mut HashSet<i64>) {
    let last_block = rpc.get_last_block().await.unwrap();

    let full_block_range: Vec<i64> = (config.start_block..last_block).collect();

    let db_state = DatabaseChainIndexedState {
        chain: config.chain.id,
        indexed_blocks_amount: indexed_blocks.len() as i64,
    };

    db.update_indexed_blocks_number(&db_state).await.unwrap();

    let missing_blocks: Vec<&i64> = full_block_range
        .iter()
        .filter(|block| !indexed_blocks.contains(&block))
        .collect();

    let total_missing_blocks = missing_blocks.len();

    info!("Syncing {} blocks.", total_missing_blocks);

    let missing_blocks_chunks = missing_blocks.chunks(config.batch_size);

    for missing_blocks_chunk in missing_blocks_chunks {
        let mut work = vec![];

        for block_number in missing_blocks_chunk {
            work.push(fetch_block(&rpc, &db, &block_number, &config.chain))
        }

        let results = join_all(work).await;
        let mut db_blocks: Vec<DatabaseBlock> = Vec::new();
        let mut db_transactions: Vec<DatabaseTransaction> = Vec::new();
        let mut db_receipts: Vec<DatabaseReceipt> = Vec::new();
        let mut db_logs: Vec<DatabaseLog> = Vec::new();
        let mut db_contracts: Vec<DatabaseContract> = Vec::new();
        let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> = Vec::new();
        let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> = Vec::new();
        let mut db_erc1155_transfers: Vec<DatabaseERC1155Transfer> = Vec::new();
        let mut db_dex_trade: Vec<DatabaseDexTrade> = Vec::new();

        for result in results {
            match result {
                Some((
                    block,
                    mut transactions,
                    mut receipts,
                    mut logs,
                    mut contracts,
                    mut erc20_transfers,
                    mut erc721_transfers,
                    mut erc1155_transfers,
                    mut dex_trades,
                )) => {
                    db_blocks.push(block);
                    db_transactions.append(&mut transactions);
                    db_receipts.append(&mut receipts);
                    db_logs.append(&mut logs);
                    db_contracts.append(&mut contracts);
                    db_erc20_transfers.append(&mut erc20_transfers);
                    db_erc721_transfers.append(&mut erc721_transfers);
                    db_erc1155_transfers.append(&mut erc1155_transfers);
                    db_dex_trade.append(&mut dex_trades)
                }
                None => continue,
            }
        }

        db.store_data(
            &db_blocks,
            &db_transactions,
            &db_receipts,
            &db_logs,
            &db_contracts,
            &db_erc20_transfers,
            &db_erc721_transfers,
            &db_erc1155_transfers,
            &db_dex_trade,
        )
        .await;

        for block in db_blocks.iter() {
            indexed_blocks.insert(block.number);
        }

        let indexed_blocks_vector: Vec<i64> = indexed_blocks.clone().into_iter().collect();

        db.store_indexed_blocks(&indexed_blocks_vector)
            .await
            .unwrap();

        let (
            db_native_balances,
            db_erc20_balances,
            db_erc721_owner_changes,
            db_erc1155_balances_changes,
            db_dex_minute_aggregates,
            db_dex_hourly_aggregates,
            db_dex_daily_aggregates,
        ) = aggregate_data(
            &db_blocks,
            &db_transactions,
            &db_erc20_transfers,
            &db_erc721_transfers,
            &db_erc1155_transfers,
            &db_dex_trade,
        );

        db.update_balances(
            &db_native_balances,
            &db_erc20_balances,
            &db_erc721_owner_changes,
            &db_erc1155_balances_changes,
        )
        .await
        .unwrap();

        db.update_dex_aggregates(
            &db_dex_minute_aggregates,
            &db_dex_hourly_aggregates,
            &db_dex_daily_aggregates,
        )
        .await
        .unwrap();
    }
}

async fn fetch_block(
    rpc: &Rpc,
    db: &Database,
    block_number: &i64,
    chain: &Chain,
) -> Option<(
    DatabaseBlock,
    Vec<DatabaseTransaction>,
    Vec<DatabaseReceipt>,
    Vec<DatabaseLog>,
    Vec<DatabaseContract>,
    Vec<DatabaseERC20Transfer>,
    Vec<DatabaseERC721Transfer>,
    Vec<DatabaseERC1155Transfer>,
    Vec<DatabaseDexTrade>,
)> {
    let block_data = rpc.get_block(block_number).await.unwrap();

    match block_data {
        Some((db_block, db_transactions)) => {
            let total_block_transactions = db_transactions.len();

            // Make sure all the transactions are correctly formatted.
            if db_block.transactions != total_block_transactions as i32 {
                warn!(
                    "Missing {} transactions for block {}.",
                    db_block.transactions - total_block_transactions as i32,
                    db_block.number
                );
                return None;
            }

            let mut db_receipts: Vec<DatabaseReceipt> = Vec::new();
            let mut db_logs: Vec<DatabaseLog> = Vec::new();
            let mut db_contracts: Vec<DatabaseContract> = Vec::new();

            if chain.supports_blocks_receipts {
                let receipts_data = rpc
                    .get_block_receipts(block_number, db_block.timestamp)
                    .await
                    .unwrap();
                match receipts_data {
                    Some((mut receipts, mut logs, mut contracts)) => {
                        db_receipts.append(&mut receipts);
                        db_logs.append(&mut logs);
                        db_contracts.append(&mut contracts);
                    }
                    None => return None,
                }
            } else {
                for transaction in db_transactions.iter() {
                    let receipt_data = rpc
                        .get_transaction_receipt(transaction.hash.clone(), transaction.timestamp)
                        .await
                        .unwrap();

                    match receipt_data {
                        Some((receipt, mut logs, contract)) => {
                            db_receipts.push(receipt);
                            db_logs.append(&mut logs);
                            match contract {
                                Some(contract) => db_contracts.push(contract),
                                None => continue,
                            }
                        }
                        None => continue,
                    }
                }
            }
            if total_block_transactions != db_receipts.len() {
                warn!(
                    "Missing receipts for block {}. Transactions {} receipts {}",
                    db_block.number,
                    total_block_transactions,
                    db_receipts.len()
                );
                return None;
            }

            let mut tokens_metadata_required: HashSet<String> = HashSet::new();

            // filter only logs with topic
            let logs_scan: Vec<&DatabaseLog> =
                db_logs.iter().filter(|log| log.topics.len() > 0).collect();

            // insert all the tokens from the logs to metadata check
            for log in logs_scan.iter() {
                let topic_0 = log.topics.first().unwrap();

                if topic_0 == TRANSFER_EVENTS_SIGNATURE
                    || topic_0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
                    || topic_0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
                    || topic_0 == SWAPV3_EVENT_SIGNATURE
                    || topic_0 == SWAP_EVENT_SIGNATURE
                {
                    tokens_metadata_required.insert(log.address.clone());
                }
            }

            let tokens_data = get_tokens(db, rpc, &tokens_metadata_required).await;

            let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> = Vec::new();
            let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> = Vec::new();
            let mut db_erc1155_transfers: Vec<DatabaseERC1155Transfer> = Vec::new();
            let mut db_dex_trades: Vec<DatabaseDexTrade> = Vec::new();

            for log in logs_scan.iter() {
                // Check the first topic matches the erc20, erc721, erc1155 or a swap signatures
                let topic0 = log.topics[0].clone();

                if topic0 == TRANSFER_EVENTS_SIGNATURE {
                    // Check if it is a erc20 or a erc721 based on the number of logs

                    // erc20 token transfer events have 2 indexed values.
                    if log.topics.len() == 3 {
                        let decimals = tokens_data.get(&log.address).unwrap().decimals;

                        let db_erc20_transfer =
                            DatabaseERC20Transfer::from_log(&log, chain.id, decimals as usize);

                        db_erc20_transfers.push(db_erc20_transfer);
                    }

                    // erc721 token transfer events have 3 indexed values.
                    if log.topics.len() == 4 {
                        let db_erc721_transfer = DatabaseERC721Transfer::from_log(log, chain.id);

                        db_erc721_transfers.push(db_erc721_transfer);
                    }
                }

                if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE {
                    let db_erc1155_transfer =
                        DatabaseERC1155Transfer::from_log(&log, chain.id, false);

                    db_erc1155_transfers.push(db_erc1155_transfer)
                }

                if topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE {
                    let db_erc1155_transfer =
                        DatabaseERC1155Transfer::from_log(&log, chain.id, true);

                    db_erc1155_transfers.push(db_erc1155_transfer)
                }

                if topic0 == SWAP_EVENT_SIGNATURE {
                    let token = log.address.clone();

                    let pair_data = tokens_data.get(&token).unwrap();

                    let token0_decimals = tokens_data
                        .get(&pair_data.token0.clone().unwrap())
                        .unwrap()
                        .decimals;

                    let token1_decimals = tokens_data
                        .get(&pair_data.token1.clone().unwrap())
                        .unwrap()
                        .decimals;

                    let db_dex_trade = DatabaseDexTrade::from_v2_log(
                        &log,
                        chain.id,
                        pair_data,
                        token0_decimals as usize,
                        token1_decimals as usize,
                    );

                    db_dex_trades.push(db_dex_trade);
                }

                if topic0 == SWAPV3_EVENT_SIGNATURE {
                    let token = log.address.clone();

                    let pair_data = tokens_data.get(&token).unwrap();

                    let token0_decimals = tokens_data
                        .get(&pair_data.token0.clone().unwrap())
                        .unwrap()
                        .decimals;

                    let token1_decimals = tokens_data
                        .get(&pair_data.token1.clone().unwrap())
                        .unwrap()
                        .decimals;

                    let db_dex_trade = DatabaseDexTrade::from_v3_log(
                        &log,
                        chain.id,
                        &pair_data,
                        token0_decimals as usize,
                        token1_decimals as usize,
                    );

                    db_dex_trades.push(db_dex_trade);
                }
            }

            info!(
                "Found: txs ({}) receipts ({}) logs ({}) contracts ({}) transfers erc20 ({}) erc721 ({}) erc1155 ({}) trades ({}) for block {}.",
                total_block_transactions,
                db_receipts.len(),
                db_logs.len(),
                db_contracts.len(),
                db_erc20_transfers.len(),
                db_erc721_transfers.len(),
                db_erc1155_transfers.len(),
                db_dex_trades.len(),
                block_number
            );

            return Some((
                db_block,
                db_transactions,
                db_receipts,
                db_logs,
                db_contracts,
                db_erc20_transfers,
                db_erc721_transfers,
                db_erc1155_transfers,
                db_dex_trades,
            ));
        }
        None => return None,
    }
}
