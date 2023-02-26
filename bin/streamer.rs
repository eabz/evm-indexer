use std::{thread::sleep, time::Duration};

use dotenv::dotenv;

use indexer::{config::config::Config, db::db::Database, rpc::rpc::Rpc};
use log::*;
use simple_logger::SimpleLogger;

#[tokio::main()]
async fn main() {
    dotenv().ok();

    let log = SimpleLogger::new().with_level(LevelFilter::Info);

    let mut config = Config::new();

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
        sleep(Duration::from_millis(500))
    }
}

/* async fn sync_chain(rpc: &Rpc, db: &Database, config: &Config) {
    let last_block = rpc.get_last_block().await.unwrap();

    let full_block_range = config.start_block..last_block;

    let mut indexed_blocks = db.get_indexed_blocks().await.unwrap();

    let db_state = DatabaseChainIndexedState {
        chain: config.chain.name.to_string(),
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

        for result in results {
            match result {
                Some((block, mut transactions, mut receipts, mut logs, mut contracts)) => {
                    db_blocks.push(block);
                    db_transactions.append(&mut transactions);
                    db_receipts.append(&mut receipts);
                    db_logs.append(&mut logs);
                    db_contracts.append(&mut contracts);
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
        )
        .await;

        for block in db_blocks.into_iter() {
            indexed_blocks.insert(block.number);
        }

        db.store_indexed_blocks(&indexed_blocks).await.unwrap();
    }
}
 */
