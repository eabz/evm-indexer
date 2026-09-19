use anyhow::{Context, Result};
use evm_indexer::{
    configs::{
        BackfillConfig, Command, Config, FleetConfig, MigrateConfig,
        VerifyConfig,
    },
    db::{migrate, Database},
    fleet, pipeline,
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
        Command::Fleet(config) => runtime.block_on(run_fleet(*config))?,
    }

    Ok(ExitCode::SUCCESS)
}

/// Is this chain the Solana one? The only family switch in the binary.
///
/// One id, compared once, rather than a `chains` lookup: the registry row
/// is written BY the run that is starting, so it can not be the thing that
/// decides which pipeline to start.
fn is_solana(chain_id: u64) -> bool {
    chain_id == pipeline::solana::SOLANA_CHAIN_ID
}

async fn run(config: Config) -> Result<()> {
    let solana = is_solana(config.chain_id);

    info!("Starting EVM Indexer.");
    if solana {
        info!(
            "Syncing Solana (chain id {}): slots, not blocks.",
            config.chain_id
        );
    } else {
        info!("Syncing chain id {}.", config.chain_id);
    }

    // Before the pipeline connects: the database itself may not exist yet.
    if config.no_migrate {
        info!("Skipping schema migrations (--no-migrate).");
    } else {
        migrate::run(&config.database_url)
            .await
            .context("apply schema migrations")?;
    }

    if solana {
        pipeline::solana::run(config).await
    } else {
        pipeline::run(config).await
    }
}

/// `indexer fleet`: one process, many chains, plus the control panel
/// (docs/design.md section 15). `indexer run` is untouched by it.
async fn run_fleet(config: FleetConfig) -> Result<()> {
    info!("Starting EVM Indexer.");
    info!("{}", fleet::describe(&config));

    fleet::run(config).await
}

/// Read only. Exit code 0 = consistent, 1 = problems found.
///
/// Solana gets its own checks: "every block number has a row" would report
/// every skipped slot as a gap for ever (`pipeline::solana_verify`).
async fn run_verify(config: VerifyConfig) -> Result<ExitCode> {
    let db = Database::new(&config.database_url, config.chain_id).await?;

    let consistent = if is_solana(config.chain_id) {
        // The coverage promise, in the same words as everywhere else
        // (docs/design.md section 16). Printed here rather than inside the
        // Solana report because slots have no timestamps to date the head
        // with, so the line is the floor and the tiled head and nothing
        // that would need a second query.
        if let Ok(Some(coverage)) =
            evm_indexer::coverage::store::coverage(&db).await
        {
            println!(
                "{}",
                evm_indexer::coverage::store::sentence(
                    &coverage, None, None
                )
            );
        }

        let report = pipeline::solana::verify(
            &db,
            config.start_block,
            config.end_block,
        )
        .await?;

        println!("{report}");
        report.is_consistent()
    } else {
        let report = pipeline::verify::verify(
            &db,
            config.start_block,
            config.end_block,
        )
        .await?;

        println!("{report}");
        report.is_consistent()
    };

    Ok(if consistent {
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
