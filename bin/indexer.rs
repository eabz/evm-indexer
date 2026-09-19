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

/// Lowers the coverage floor to `from_block`, but only when everything
/// between there and the old floor is actually stored and gap-free
/// (docs/design.md section 16).
///
/// This is the ONLY way the floor moves earlier, and it is deliberately
/// a check rather than a claim: the floor is what this database promises,
/// so it may only follow the data, never lead it. A range that is not
/// complete leaves the floor exactly where it was and says why.
///
/// Note what this does NOT do: it does not fetch anything. `indexer
/// backfill` re-decodes logs this database already has, so the floor moves
/// down only over blocks that are already stored - which is the case an
/// operator actually hits, having indexed deeper with an older version or
/// with an explicit `--start-block` before the floor was ever written.
async fn lower_the_floor_if_earned(
    db: &Database,
    from_block: u64,
) -> Result<()> {
    use evm_indexer::coverage::store;

    let Some(floor) = store::stored(db).await? else { return Ok(()) };
    if from_block >= floor.block {
        return Ok(());
    }

    let report =
        pipeline::verify::verify(db, from_block, floor.block).await?;

    if !report.gaps.is_empty() {
        let missing: u64 = report.gaps.iter().map(|gap| gap.len()).sum();
        println!(
            "The coverage floor stays at block {} ({}). Blocks \
             [{from_block}, {}) are not all stored - {missing} are \
             missing - and the floor is a promise, so it only ever follows \
             the data. `indexer verify --start-block {from_block} \
             --end-block {}` lists the holes.",
            floor.block,
            floor.date(),
            floor.block,
            floor.block
        );
        return Ok(());
    }

    let timestamp = pipeline::verify::block_timestamp(db, from_block)
        .await
        .unwrap_or(0);

    store::lower_to(
        db,
        // No lease: this writes one row of `chain_coverage`, which the
        // engine resolves in favour of the LOWEST block whatever else is
        // writing (see the migration header).
        &evm_indexer::pipeline::lease::Fence::open(),
        store::Floor {
            block: from_block,
            timestamp,
            reason: store::Reason::Backfill,
        },
    )
    .await?;

    Ok(())
}

/// `indexer backfill --module predictions --registry-only`.
///
/// Reads the blocks below this chain's coverage floor, filtered to the
/// operator's own trusted addresses, and stores what a market needs to be
/// describable: its metadata, its question, its outcome tokens and the
/// split / merge / redeem events open interest is made of. No trade below
/// the floor is stored, on purpose (docs/design.md section 16).
///
/// The same pass `indexer run` starts by itself, run on demand and in the
/// foreground so the operator watches it finish.
async fn run_registry_history(
    db: &Database,
    config: &BackfillConfig,
) -> Result<()> {
    use evm_indexer::predictions::history;

    if config.module != "predictions" {
        anyhow::bail!(
            "--registry-only is only for --module predictions. It reads \
             the blocks below the coverage floor for the market metadata \
             and the split/merge/redeem events open interest is made of, \
             which no other module needs."
        );
    }

    let Some(floor) = evm_indexer::coverage::store::stored(db).await?
    else {
        anyhow::bail!(
            "chain {} has no coverage floor yet, so there is nothing \
             below it to read. Run `indexer run` once first.",
            config.chain_id
        );
    };

    // The pass READS FROM THE SOURCE - these logs are below the floor, so
    // this database has never had them - which is what makes it different
    // from every other backfill.
    let token = std::env::var("ENVIO_API_TOKEN").unwrap_or_default();
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!(
            "this pass reads blocks below the coverage floor from the \
             source, so it needs ENVIO_API_TOKEN in the environment. \
             (Every other `indexer backfill` re-decodes logs this \
             database already has and needs no token.)"
        );
    }

    // By default the pass goes as far down as the source will serve these
    // addresses; it remembers how far it got in `prediction_history`.
    let source = evm_indexer::source::evm::Source::new(
        config.chain_id,
        None,
        token,
    )?;

    let report = history::run(
        db,
        // No lease is taken: the pass writes only below the floor, where
        // the live indexer never writes, and a second copy of it is
        // idempotent rather than harmful.
        &evm_indexer::pipeline::lease::Fence::open(),
        &source,
        floor.block,
        config.chunk_blocks.max(history::CHUNK_BLOCKS),
    )
    .await?;

    println!("{report}");
    Ok(())
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
                    &coverage,
                    evm_indexer::coverage::store::unit_of(config.chain_id),
                    None,
                    None,
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

    // `--registry-only` is a different job from every other backfill: the
    // rest re-decode logs this database already has, and this one fetches
    // logs from BELOW the coverage floor that it has never had
    // (docs/design.md section 16).
    if config.registry_only {
        return run_registry_history(&db, &config).await;
    }

    let report = pipeline::backfill::backfill(
        &db,
        &config.module,
        config.from_block,
        config.to_block,
        config.chunk_blocks,
    )
    .await?;

    // A backfill that reached below the coverage floor may have made the
    // promise bigger - but only if the older range really is complete
    // (docs/design.md section 16). Checked, never assumed.
    lower_the_floor_if_earned(&db, config.from_block).await?;

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
