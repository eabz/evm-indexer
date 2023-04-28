use dotenv::dotenv;
use evm_indexer::{
    configs::Config,
    db::{
        models::chain_state::DatabaseChainIndexedState, BlockFetchedData,
        Database,
    },
    rpc::Rpc,
};
use futures::future::join_all;
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

    info!("Syncing chain {}.", config.chain.name);

    let rpc =
        Rpc::new(&config).await.expect("Unable to start RPC client.");

    let db = Database::new(
        config.db_host.clone(),
        config.db_username.clone(),
        config.db_password.clone(),
        config.db_name.clone(),
        config.chain,
    )
    .await
    .expect("Unable to start DB connection.");

    let mut indexed_blocks = db.get_indexed_blocks().await.unwrap();

    loop {
        sync_chain(&rpc, &db, &config, &mut indexed_blocks).await;
        sleep(Duration::from_millis(500))
    }
}

async fn sync_chain(
    rpc: &Rpc,
    db: &Database,
    config: &Config,
    indexed_blocks: &mut HashSet<u64>,
) {
    let last_block = rpc.get_last_block().await.unwrap();

    let full_block_range: Vec<u64> =
        (config.start_block..last_block).collect();

    let db_state = DatabaseChainIndexedState {
        chain: config.chain.id,
        indexed_blocks_amount: indexed_blocks.len() as u64,
    };

    db.update_indexed_blocks_number(&db_state).await.unwrap();

    let missing_blocks: Vec<&u64> = full_block_range
        .iter()
        .filter(|block| !indexed_blocks.contains(block))
        .collect();

    let total_missing_blocks = missing_blocks.len();

    info!("Syncing {} blocks.", total_missing_blocks);

    let missing_blocks_chunks = missing_blocks.chunks(config.batch_size);

    for missing_blocks_chunk in missing_blocks_chunks {
        let mut work = vec![];

        for block_number in missing_blocks_chunk {
            work.push(rpc.fetch_block(db, block_number, &config.chain))
        }

        let results = join_all(work).await;

        let mut fetched_data = BlockFetchedData {
            blocks: Vec::new(),
            transactions: Vec::new(),
            receipts: Vec::new(),
            logs: Vec::new(),
            contracts: Vec::new(),
            erc20_transfers: Vec::new(),
            erc721_transfers: Vec::new(),
            erc1155_transfers: Vec::new(),
            dex_trades: Vec::new(),
        };

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
                    fetched_data.blocks.push(block);
                    fetched_data.transactions.append(&mut transactions);
                    fetched_data.receipts.append(&mut receipts);
                    fetched_data.logs.append(&mut logs);
                    fetched_data.contracts.append(&mut contracts);
                    fetched_data
                        .erc20_transfers
                        .append(&mut erc20_transfers);
                    fetched_data
                        .erc721_transfers
                        .append(&mut erc721_transfers);
                    fetched_data
                        .erc1155_transfers
                        .append(&mut erc1155_transfers);
                    fetched_data.dex_trades.append(&mut dex_trades)
                }
                None => continue,
            }
        }

        db.store_data(&fetched_data).await;

        for block in fetched_data.blocks.iter() {
            indexed_blocks.insert(block.number);
        }
    }
}
