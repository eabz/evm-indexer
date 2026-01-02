use evm_indexer::config::IndexerConfig;
use log::*;
use simple_logger::SimpleLogger;

#[tokio::main()]
async fn main() {
    SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()
        .unwrap();

    info!("Starting EVM Indexer.");

    let config = IndexerConfig::new();

    info!("Syncing chain: {}.", config.chain_id);
}
