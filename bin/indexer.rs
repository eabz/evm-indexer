use anyhow::{Context, Result};
use evm_indexer::{
    configs::{
        BackfillConfig, Command, Config, MigrateConfig, VerifyConfig,
    },
    db::{migrate, Database},
    pipeline,
};
use log::{error, info, LevelFilter};
use simple_logger::SimpleLogger;
use std::process::ExitCode;

/// Exit code of `indexer verify` when it found problems.
const EXIT_PROBLEMS_FOUND: u8 = 1;

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
            return runtime.block_on(run_verify(config));
        }
        Command::Backfill(config) => {
            runtime.block_on(run_backfill(config))?
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

/// Read only. Exit code 0 = consistent, 1 = problems found.
async fn run_verify(config: VerifyConfig) -> Result<ExitCode> {
    let db = Database::new(&config.database_url, config.chain_id).await?;

    let report = pipeline::verify::verify(
        &db,
        config.start_block,
        config.end_block,
    )
    .await?;

    println!("{report}");

    Ok(if report.is_consistent() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_PROBLEMS_FOUND)
    })
}

async fn run_backfill(config: BackfillConfig) -> Result<()> {
    let db = Database::new(&config.database_url, config.chain_id).await?;

    let report = pipeline::backfill::backfill(
        &db,
        &config.module,
        config.from_block,
        config.to_block,
        config.chunk_blocks,
    )
    .await?;

    match report.rewritten {
        None => println!(
            "{}: blocks {} already match the stored logs ({} logs \
             checked). Nothing was written.",
            report.module, report.range, report.logs
        ),
        Some(range) => println!(
            "{}: blocks {range} re-decoded from the stored logs: {} rows \
             replaced by {}. Epoch is now {}.",
            report.module,
            report.rows_tombstoned,
            report.rows_written,
            report.epoch
        ),
    }

    Ok(())
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
