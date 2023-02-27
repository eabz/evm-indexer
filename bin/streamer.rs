use std::{thread::sleep, time::Duration};

use dotenv::dotenv;

use ethabi::{ethereum_types::H256, ParamType};
use ethers::utils::format_units;
use futures::future::join_all;
use indexer::{
    chains::chains::Chain,
    config::config::Config,
    db::{
        db::Database,
        models::{
            block::DatabaseBlock, chain_state::DatabaseChainIndexedState,
            contract::DatabaseContract, erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog, receipt::DatabaseReceipt,
            transaction::DatabaseTransaction,
        },
    },
    rpc::rpc::Rpc,
    utils::format::format_number,
};
use log::*;
use simple_logger::SimpleLogger;

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

    info!("Starting EVM Indexer Streaner.");

    let rpc = Rpc::new(&config)
        .await
        .expect("Unable to start RPC client.");

    let db = Database::new(
        config.db_url.clone(),
        config.redis_url.clone(),
        config.chain.clone(),
    )
    .await
    .expect("Unable to start DB connection.");

    loop {
        sync_chain(&rpc, &db, &config).await;
        sleep(Duration::from_millis(500))
    }
}

async fn sync_chain(rpc: &Rpc, db: &Database, config: &Config) {
    let last_block = rpc.get_last_block().await.unwrap();

    let full_block_range = config.start_block..last_block;

    let indexed_blocks = db.get_indexed_blocks().await.unwrap();

    let db_state = DatabaseChainIndexedState {
        chain: config.chain.id,
        indexed_blocks_amount: indexed_blocks.len() as i64,
    };

    db.update_indexed_blocks_number(&db_state).await.unwrap();

    let missing_blocks: Vec<i64> = full_block_range
        .into_iter()
        .filter(|block| !indexed_blocks.contains(block))
        .collect();

    let total_missing_blocks = missing_blocks.len();

    info!("Syncing {} blocks.", total_missing_blocks);

    let missing_blocks_chunks = missing_blocks.chunks(config.batch_size);

    for missing_blocks_chunk in missing_blocks_chunks {
        let mut work = vec![];

        for block_number in missing_blocks_chunk {
            work.push(fetch_block(&rpc, &block_number, &config.chain))
        }

        let results = join_all(work).await;

        let mut db_blocks: Vec<DatabaseBlock> = Vec::new();
        let mut db_transactions: Vec<DatabaseTransaction> = Vec::new();
        let mut db_receipts: Vec<DatabaseReceipt> = Vec::new();
        let mut db_logs: Vec<DatabaseLog> = Vec::new();
        let mut db_contracts: Vec<DatabaseContract> = Vec::new();
        let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> = Vec::new();
        let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> = Vec::new();

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
                )) => {
                    db_blocks.push(block);
                    db_transactions.append(&mut transactions);
                    db_receipts.append(&mut receipts);
                    db_logs.append(&mut logs);
                    db_contracts.append(&mut contracts);
                    db_erc20_transfers.append(&mut erc20_transfers);
                    db_erc721_transfers.append(&mut erc721_transfers)
                }
                None => continue,
            }
        }

        let stored_blocks: Vec<i64> = db_blocks.iter().map(|block| block.number).collect();

        db.store_indexed_blocks(&stored_blocks).await.unwrap();
    }
}

async fn fetch_block(
    rpc: &Rpc,
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
)> {
    let block_data = rpc.get_block(block_number).await.unwrap();

    match block_data {
        Some((db_block, mut db_transactions)) => {
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
                for transaction in db_transactions.iter_mut() {
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

            let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> = Vec::new();
            let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> = Vec::new();

            let transfer_event = ethabi::Event {
                name: "Transfer".to_owned(),
                inputs: vec![
                    ethabi::EventParam {
                        name: "from".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "to".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "amount".to_owned(),
                        kind: ParamType::Uint(256),
                        indexed: false,
                    },
                ],
                anonymous: false,
            };

            let erc1155_transfer_single_event = ethabi::Event {
                name: "TransferSingle".to_owned(),
                inputs: vec![
                    ethabi::EventParam {
                        name: "operator".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "from".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "to".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "id".to_owned(),
                        kind: ParamType::Uint(256),
                        indexed: false,
                    },
                    ethabi::EventParam {
                        name: "value".to_owned(),
                        kind: ParamType::Uint(256),
                        indexed: false,
                    },
                ],
                anonymous: false,
            };

            let erc1155_transfer_batch_event = ethabi::Event {
                name: "TransferBatch".to_owned(),
                inputs: vec![
                    ethabi::EventParam {
                        name: "operator".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "from".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "to".to_owned(),
                        kind: ParamType::Address,
                        indexed: true,
                    },
                    ethabi::EventParam {
                        name: "ids".to_owned(),
                        kind: ParamType::Array(Box::new(ParamType::Uint(256))),
                        indexed: false,
                    },
                    ethabi::EventParam {
                        name: "values".to_owned(),
                        kind: ParamType::Array(Box::new(ParamType::Uint(256))),
                        indexed: false,
                    },
                ],
                anonymous: false,
            };

            for log in db_logs.iter() {
                if log.topics.len() < 3 {
                    continue;
                }

                // Check the first topic matches the erc20, erc721 or erc1155 signatures
                let topic_0 = log.topics[0].clone();

                if topic_0 == format!("{:?}", transfer_event.signature()) {
                    // Check if it is a erc20 or a erc721 based on the number of logs
                    // erc20 token transfer events have only 2 indexed values while erc721 have 3.

                    let from_address: String = match ethabi::decode(
                        &[ParamType::Address],
                        array_bytes::hex_n_into::<String, H256, 32>(log.topics[1].clone())
                            .unwrap()
                            .as_bytes(),
                    ) {
                        Ok(address) => {
                            if address.len() == 0 {
                                continue;
                            } else {
                                format!("{:?}", address[0].clone().into_address().unwrap())
                            }
                        }
                        Err(_) => continue,
                    };

                    let to_address = match ethabi::decode(
                        &[ParamType::Address],
                        array_bytes::hex_n_into::<String, H256, 32>(log.topics[2].clone())
                            .unwrap()
                            .as_bytes(),
                    ) {
                        Ok(address) => {
                            if address.len() == 0 {
                                continue;
                            } else {
                                format!("{:?}", address[0].clone().into_address().unwrap())
                            }
                        }
                        Err(_) => continue,
                    };

                    // Handle as ERC20
                    if log.topics.len() == 3 {
                        let value = match ethabi::decode(&[ParamType::Uint(256)], &log.data[..]) {
                            Ok(value) => {
                                if value.len() == 0 {
                                    continue;
                                } else {
                                    value[0].clone().into_uint().unwrap()
                                }
                            }
                            Err(_) => continue,
                        };

                        let db_erc20_transfer = DatabaseERC20Transfer {
                            chain: chain.id,
                            from_address: from_address.clone(),
                            hash: log.hash.clone(),
                            log_index: log.log_index,
                            to_address: to_address.clone(),
                            token: log.address.clone(),
                            transaction_log_index: log.transaction_log_index,
                            amount: format_units(value, 18).unwrap().parse::<f64>().unwrap(),
                            timestamp: log.timestamp,
                        };

                        db_erc20_transfers.push(db_erc20_transfer);
                    }

                    // Handle as ERC721
                    if log.topics.len() == 4 {
                        let id = match ethabi::decode(
                            &[ParamType::Uint(256)],
                            array_bytes::hex_n_into::<String, H256, 32>(log.topics[3].clone())
                                .unwrap()
                                .as_bytes(),
                        ) {
                            Ok(address) => {
                                if address.len() == 0 {
                                    continue;
                                } else {
                                    address[0].clone().into_uint().unwrap()
                                }
                            }
                            Err(_) => continue,
                        };

                        let db_erc721_transfer = DatabaseERC721Transfer {
                            chain: chain.id,
                            from_address,
                            hash: log.hash.clone(),
                            log_index: log.log_index,
                            to_address,
                            token: log.address.clone(),
                            transaction_log_index: log.transaction_log_index,
                            id: format_number(id),
                            timestamp: log.timestamp,
                        };

                        db_erc721_transfers.push(db_erc721_transfer)
                    }
                }

                if topic_0 == format!("{:?}", erc1155_transfer_single_event.signature()) {}

                if topic_0 == format!("{:?}", erc1155_transfer_batch_event.signature()) {}
            }

            info!(
                "Found erc20 transfers {} erc721 transfers {} transactions {} receipts {} logs {} and contracts {} for block {}.",
                db_erc20_transfers.len(),
                db_erc721_transfers.len(),
                total_block_transactions,
                db_receipts.len(),
                db_logs.len(),
                db_contracts.len(),
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
            ));
        }
        None => return None,
    }
}
