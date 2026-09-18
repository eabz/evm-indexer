use anyhow::{Context, Result};
use evm_indexer::{configs::Config, pipeline};
use log::{error, info, LevelFilter};
use simple_logger::SimpleLogger;

fn main() -> Result<()> {
    // Parsed before the runtime exists: it scrubs blank environment
    // variables, which must not race with other threads.
    let config = Config::new();

    let level =
        if config.debug { LevelFilter::Debug } else { LevelFilter::Info };

    SimpleLogger::new()
        .with_level(level)
        .init()
        .context("initialize logger")?;

    info!("Starting EVM Indexer.");
    info!("Syncing chain id {}.", config.chain_id);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    let result = runtime.block_on(pipeline::run(config));

    if let Err(e) = &result {
        error!("Fatal: {e:#}");
    }

    result
}
