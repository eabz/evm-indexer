use dotenv::dotenv;
use evm_indexer::{config::IndexerConfig, db::Database};
use log::*;
use simple_logger::SimpleLogger;

#[tokio::main()]
async fn main() {
    dotenv().ok();

    SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()
        .unwrap();

    info!("Starting EVM Indexer");

    let config = IndexerConfig::new();

    let db = Database::new(&config).await;

    info!("Syncing chain: {}", config.chain_id);
}
