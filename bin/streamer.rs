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
