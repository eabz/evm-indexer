use evm_indexer::{
    configs::Config,
    db::{BlockFetchedData, Database, DatabaseTables},
    genesis::get_genesis_allocations,
    rpc::Rpc,
};
use futures::future::join_all;
use log::*;
use simple_logger::SimpleLogger;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main()]
async fn main() {
    let log = SimpleLogger::new().with_level(LevelFilter::Info);

    let config = Config::new();

    if config.debug {
        log.with_level(LevelFilter::Debug).init().unwrap();
    } else {
        log.init().unwrap();
    }

    info!("Starting EVM Indexer.");

    info!("Syncing {}.", config.chain.name);

    let rpc = Rpc::new(&config).await;

    let db = Database::new(
        config.db_host.clone(),
        config.db_username.clone(),
        config.db_password.clone(),
        config.db_name.clone(),
        config.chain.clone(),
    )
    .await;

    if config.ws_url.is_some() && config.end_block == 0
        || config.end_block == -1
    {
        tokio::spawn({
            let rpc: Rpc = rpc.clone();
            let db: Database = db.clone();

            async move {
                loop {
                    rpc.listen_blocks(&db).await;

                    sleep(Duration::from_millis(500)).await;
                }
            }
        });
    }

    loop {
        if !config.new_blocks_only {
            sync_chain(&rpc, &db, &config).await;
        }
        sleep(Duration::from_secs(30)).await;
    }
}

async fn sync_chain(rpc: &Rpc, db: &Database, config: &Config) {
    let mut indexed_blocks = db.get_indexed_blocks().await;

    // If there are no indexed blocks, insert the genesis transactions
    if indexed_blocks.is_empty() {
        let genesis_transactions =
            get_genesis_allocations(config.chain.clone());
        db.store_items(
            &genesis_transactions,
            DatabaseTables::Transactions.as_str(),
        )
        .await;
    }

    let last_block = if config.end_block != 0 {
        config.end_block as u32
    } else {
        rpc.get_last_block().await
    };

    let full_block_range: Vec<u32> =
        (config.start_block..last_block).collect();

    let missing_blocks: Vec<&u32> = full_block_range
        .iter()
        .filter(|block| !indexed_blocks.contains(block))
        .collect();

    let total_missing_blocks = missing_blocks.len();

    // If the program uses a block range and finishes shutdown gracefully
    if config.end_block != 0 && total_missing_blocks == 0 {
        info!("Finished syncing blocks");
        std::process::exit(0);
    }

    info!("Syncing {} blocks.", total_missing_blocks);

    let missing_blocks_chunks = missing_blocks.chunks(config.batch_size);

    for missing_blocks_chunk in missing_blocks_chunks {
        let mut work = vec![];

        for block_number in missing_blocks_chunk {
            work.push(rpc.fetch_block(block_number, &config.chain))
        }

        let results = join_all(work).await;

        let mut fetched_data = BlockFetchedData {
            blocks: Vec::new(),
            contracts: Vec::new(),
            logs: Vec::new(),
            traces: Vec::new(),
            transactions: Vec::new(),
            withdrawals: Vec::new(),
            erc20_transfers: Vec::new(),
            erc721_transfers: Vec::new(),
            erc1155_transfers: Vec::new(),
            dex_trades: Vec::new(),
        };

        for result in results {
            match result {
                Some((
                    mut blocks,
                    mut transactions,
                    mut logs,
                    mut contracts,
                    mut traces,
                    mut withdrawals,
                    mut erc20_transfers,
                    mut erc721_transfers,
                    mut erc1155_transfers,
                    mut dex_trades,
                )) => {
                    fetched_data.blocks.append(&mut blocks);
                    fetched_data.transactions.append(&mut transactions);
                    fetched_data.logs.append(&mut logs);
                    fetched_data.contracts.append(&mut contracts);
                    fetched_data.traces.append(&mut traces);
                    fetched_data.withdrawals.append(&mut withdrawals);
                    fetched_data
                        .erc20_transfers
                        .append(&mut erc20_transfers);
                    fetched_data
                        .erc721_transfers
                        .append(&mut erc721_transfers);
                    fetched_data
                        .erc1155_transfers
                        .append(&mut erc1155_transfers);
                    fetched_data.dex_trades.append(&mut dex_trades);
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
