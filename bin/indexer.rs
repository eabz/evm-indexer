use anyhow::{Context, Result};
use evm_indexer::{
    configs::{Command, Config, MigrateConfig},
    db::migrate,
    pipeline,
};
use log::{error, info, LevelFilter};
use simple_logger::SimpleLogger;
use std::process::ExitCode;

/// Exit code of commands that exist but are not implemented yet.
const EXIT_NOT_IMPLEMENTED: u8 = 2;

fn main() -> ExitCode {
    // Parsed before the runtime exists: it scrubs blank environment
    // variables, which must not race with other threads.
    let command = Command::parse();

    match execute(command) {
        Ok(code) => code,
        Err(e) => {
            if log::max_level() == LevelFilter::Off {
                // The logger itself is what failed.
                eprintln!("Fatal: {e:#}");
            }
            error!("Fatal: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(command: Command) -> Result<ExitCode> {
    let level = if command.debug() {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
    };

    SimpleLogger::new()
        .with_level(level)
        .init()
        .context("initialize logger")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    match command {
        Command::Run(config) => runtime.block_on(run(*config))?,
        Command::Migrate(config) => {
            runtime.block_on(run_migrate(config))?
        }
        Command::Verify(config) => {
            // Placeholder, filled in by the pipeline: gap / consistency
            // verification over `blocks` and `checkpoints`.
            println!(
                "indexer verify (chain {}, blocks {}..{}): not implemented \
                 yet",
                config.chain_id, config.start_block, config.end_block
            );
            return Ok(ExitCode::from(EXIT_NOT_IMPLEMENTED));
        }
    }

    Ok(ExitCode::SUCCESS)
}

async fn run(config: Config) -> Result<()> {
    info!("Starting EVM Indexer.");
    info!("Syncing chain id {}.", config.chain_id);

    // Before the pipeline connects: the database itself may not exist yet.
    if config.no_migrate {
        info!("Skipping schema migrations (--no-migrate).");
    } else {
        migrate::run(&config.database_url)
            .await
            .context("apply schema migrations")?;
    }

    pipeline::run(config).await
}

async fn run_migrate(config: MigrateConfig) -> Result<()> {
    if config.dry_run {
        let status = migrate::status(&config.database_url).await?;

        if !status.database_exists {
            println!(
                "The database does not exist yet; it would be created."
            );
        }

        println!(
            "{} migration(s) applied, {} pending.",
            status.applied,
            status.pending.len()
        );
        for label in &status.pending {
            println!("pending: {label}");
        }

        return Ok(());
    }

    let report = migrate::run(&config.database_url).await?;

    info!(
        "Migrations done: {} applied now, {} applied before, {} applied \
         concurrently by another process.",
        report.applied.len(),
        report.already_applied,
        report.applied_elsewhere.len()
    );

    Ok(())
}
