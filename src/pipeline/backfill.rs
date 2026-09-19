//! `indexer backfill --module <m>`: decode a module's rows again from the
//! STORED `logs` (joined with `transactions` for `tx_from` / `tx_to`),
//! through the same module seam and store path as the live pipeline. A new
//! event family or a decoder fix never needs a re-sync of the chain.
//!
//! # Why it does not double count, and why it is safe next to a live indexer
//!
//! Rows are keyed positionally, so writing a row again replaces it. The
//! aggregates are the problem: a materialized view ADDS whatever an insert
//! brings, so re-inserting a swap would count it twice. The only mechanism
//! the design has for "take contributions back" is the purge primitive
//! (tombstones + a new epoch + bucket repair), so the backfill IS a purge,
//! restricted to the module's tables (`PurgeReason::Redecode`):
//!
//! 1. **Scan** (read only): every chunk of the range is decoded and
//!    compared with what is stored. Nothing differs => nothing is written,
//!    the epoch does not move, the aggregates are untouched. This makes the
//!    command idempotent and cheap to run "just in case".
//! 2. **Purge** the smallest block range holding every difference: the
//!    module's rows are tombstoned (`blocks`, checkpoints and every other
//!    table stay), a `reorgs` row bumps the chain's epoch, and the
//!    aggregates of EVERY module are rebuilt from the first affected day -
//!    the validity rule is per chain, so rebuilding only this module's
//!    would silently zero the others. The module's own aggregates leave the
//!    purged range out; everybody else's count it (their rows stay).
//! 3. **Re-insert** the decoded rows of that range, chunk by chunk, stamped
//!    with the new epoch: they flow through the materialized views once.
//!
//! A crash anywhere is healed by running the command again: the scan finds
//! what is still missing. A live `indexer run` on the same chain notices the
//! new epoch before its next flush (and purges + re-indexes a flush that
//! raced with it, see `pipeline::ClickhouseSink`). While step 2 to 3 run,
//! readers see the module's rows of that range missing, never doubled.

use crate::{
    core::{models::log::DatabaseLog, RowBatch},
    db::format::{SerAddress, SerB256, SerU256},
    db::{
        next_version, ranges::BlockRange, Database, FlushKey, FlushWindow,
    },
    pipeline::{
        modules::{
            self, DecodeState, EnabledModules, ModuleRows, ModuleSpec,
        },
        store::{ClickhouseReorgStore, Scope},
    },
    reorg::{NoHooks, PurgeReason, Purger, WriterControl},
};
use alloy::primitives::{Address, B256, U256};
use anyhow::{bail, Context, Result};
use clickhouse::Row;
use futures::future::BoxFuture;
use log::info;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    pub module: &'static str,
    pub range: BlockRange,
    pub chunks: u64,
    pub logs: u64,
    /// Block range that was purged and written again (`None`: the stored
    /// rows already equal what the decoder produces).
    pub rewritten: Option<BlockRange>,
    pub rows_tombstoned: u64,
    pub rows_written: u64,
    /// The chain's epoch afterwards.
    pub epoch: u32,
}

#[serde_with::serde_as]
#[derive(Debug, Row, Deserialize)]
struct OriginRow {
    #[serde_as(as = "SerB256")]
    hash: B256,
    #[serde_as(as = "SerAddress")]
    from: Address,
    #[serde_as(as = "Option<SerAddress>")]
    to: Option<Address>,
    #[serde_as(as = "SerU256")]
    value: U256,
}

/// The backfill has no writer to quiesce; it only adopts the epoch.
pub(crate) struct EpochOnly(Database);

impl EpochOnly {
    pub(crate) fn new(db: Database) -> Self {
        Self(db)
    }
}

impl WriterControl for EpochOnly {
    fn quiesce(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn adopt_epoch(&self, epoch: u32) {
        self.0.set_epoch(epoch);
    }
}

fn only(spec: &ModuleSpec) -> Result<EnabledModules> {
    let mut enabled = EnabledModules::none();
    match spec.name {
        "dex" => enabled.dex = true,
        "predictions" => enabled.predictions = true,
        "launchpads" => enabled.launchpads = true,
        // MODULE: "<module>" => enabled.<module> = true,
        other => bail!("module '{other}' can not be backfilled"),
    }
    Ok(enabled)
}

/// Decodes `chunk` from the stored logs. `state` must see the chunks in
/// chain order.
async fn decode_chunk(
    db: &Database,
    enabled: EnabledModules,
    chunk: BlockRange,
    state: &mut DecodeState,
) -> Result<(ModuleRows, u64)> {
    let client = db.db.clone().with_validation(false);

    let logs: Vec<DatabaseLog> = client
        .query(&format!(
            "SELECT ?fields FROM logs FINAL WHERE chain = {} AND \
             block_number >= {} AND block_number < {} \
             ORDER BY block_number, log_index",
            db.chain_id, chunk.from, chunk.to
        ))
        .fetch_all()
        .await
        .with_context(|| format!("read the stored logs of {chunk}"))?;

    let origins: Vec<OriginRow> = client
        .query(&format!(
            "SELECT hash, `from`, `to`, value FROM transactions FINAL \
             WHERE chain = {} AND block_number >= {} AND \
             block_number < {}",
            db.chain_id, chunk.from, chunk.to
        ))
        .fetch_all()
        .await
        .with_context(|| format!("read the transactions of {chunk}"))?;

    let origins: modules::TxOrigins = origins
        .into_iter()
        .map(|row| {
            (
                row.hash,
                modules::TxOrigin {
                    from: row.from,
                    to: row.to,
                    value: row.value,
                },
            )
        })
        .collect();

    let count = logs.len() as u64;
    let batch = RowBatch { logs, ..Default::default() };

    Ok((
        modules::decode_with_origins(
            enabled,
            db.chain_id,
            &batch,
            &origins,
            state,
        ),
        count,
    ))
}

/// Runs the backfill. `to_block` 0 = up to the highest indexed block.
pub async fn backfill(
    db: &Database,
    module: &str,
    from_block: u64,
    to_block: u64,
    chunk_blocks: u64,
) -> Result<BackfillReport> {
    let spec = modules::ALL_MODULES
        .iter()
        .copied()
        .find(|spec| spec.name == module)
        .with_context(|| format!("unknown module '{module}'"))?;
    let enabled = only(spec)?;
    let chunk_blocks = chunk_blocks.max(1);

    let end = if to_block > 0 {
        to_block
    } else {
        db.stored_head().await?.map_or(from_block, |head| head + 1)
    };
    let range = BlockRange::new(from_block, end.max(from_block));

    db.seed_version(&modules::versioned_tables()).await?;
    db.set_epoch(db.current_epoch().await?);

    let chunks_of = |range: BlockRange| {
        let mut chunks = Vec::new();
        let mut from = range.from;
        while from < range.to {
            let to = from.saturating_add(chunk_blocks).min(range.to);
            chunks.push(BlockRange::new(from, to));
            from = to;
        }
        chunks
    };

    // 1. Scan: what differs from what the decoder produces today?
    info!(
        "Backfill of '{}' for chain {}: comparing blocks {range} with the \
         stored logs.",
        spec.name, db.chain_id
    );

    let seed = modules::known_registries(db, enabled).await?;
    let mut state = DecodeState { registries: seed.clone() };
    let mut changed: Option<BlockRange> = None;
    let mut logs = 0;
    let mut chunks = 0;

    for chunk in chunks_of(range) {
        let (decoded, count) =
            decode_chunk(db, enabled, chunk, &mut state).await?;
        logs += count;
        chunks += 1;

        let stored = modules::stored_fingerprints(db, spec, chunk).await?;

        if decoded.fingerprints(spec.name) != stored {
            changed = Some(match changed {
                Some(hull) => BlockRange::new(hull.from, chunk.to),
                None => chunk,
            });
        }
    }

    let Some(hull) = changed else {
        info!(
            "Backfill of '{}': the stored rows already match ({chunks} \
             chunk(s), {logs} logs). Nothing written.",
            spec.name
        );
        return Ok(BackfillReport {
            module: spec.name,
            range,
            chunks,
            logs,
            rewritten: None,
            rows_tombstoned: 0,
            rows_written: 0,
            epoch: db.epoch(),
        });
    };

    // 2. Purge the module's rows of the hull (new epoch, every aggregate
    //    of the chain rebuilt).
    info!(
        "Backfill of '{}': rows differ within {hull}. Replacing them.",
        spec.name
    );

    let purger = Purger::new(
        Arc::new(ClickhouseReorgStore::new(
            db.clone(),
            Scope::Module(spec),
        )),
        Arc::new(EpochOnly::new(db.clone())),
        Arc::new(NoHooks),
        Arc::new(NoHooks),
    );

    let report = purger
        .purge_range(
            db.chain_id,
            hull.from,
            Some(hull.to),
            PurgeReason::Redecode,
        )
        .await?;

    db.set_epoch(report.epoch);

    // 3. Write the decoded rows of the hull, stamped with the new epoch.
    //    (Decoded again instead of kept in memory: the hull can be the
    //    whole chain.)
    let mut state = DecodeState { registries: seed };
    let mut rows_written = 0;

    for chunk in chunks_of(BlockRange::new(range.from, hull.to)) {
        // Below the hull: only to bring the decode state up to date.
        let (mut decoded, _) =
            decode_chunk(db, enabled, chunk, &mut state).await?;

        if chunk.to <= hull.from || decoded.is_empty() {
            continue;
        }

        // A live indexer on the same chain can purge between two chunks
        // (a tip reorg, a gap heal). Its `reorgs` row moves the epoch
        // floor from that day on and its rebuild has already run, so a
        // chunk still stamped with OUR epoch would be hidden by the
        // validity rule for ever - and unhealable, because `fingerprints`
        // zeroes `epoch`, so a re-run finds the stored rows equal and
        // writes nothing.
        //
        // Stopping here is safe and honest: nothing written so far is
        // wrong (it carries an epoch that was current when it was
        // written and the other purge rebuilt from the base rows), and
        // running the command again completes the job under the new
        // epoch.
        let epoch = db.refresh_epoch().await?;
        if epoch != report.epoch {
            bail!(
                "chain {}: another process purged this chain while the \
                 backfill of '{}' was writing (epoch {} -> {epoch}). \
                 Blocks {} to {} were written, the rest was not. Nothing \
                 is wrong with what is stored: run `indexer backfill \
                 --module {}` again to finish under the new epoch.",
                db.chain_id,
                spec.name,
                report.epoch,
                hull.from,
                chunk.from,
                spec.name
            );
        }

        let version = next_version();
        decoded.set_version(version);
        decoded.set_epoch(epoch);

        let key = FlushKey {
            chain: db.chain_id,
            span: (chunk.from, chunk.to - 1),
            version,
        };

        // The backfill writes one chunk of blocks at a time, never a
        // span of months, so it is always one part.
        decoded.store(db, &key, FlushWindow::ALL).await.with_context(
            || format!("write the '{}' rows of {chunk}", spec.name),
        )?;

        rows_written += decoded.rows() as u64;
    }

    info!(
        "Backfill of '{}' done: {} rows tombstoned, {rows_written} written \
         for blocks {hull}; epoch is now {}.",
        spec.name, report.children_tombstoned, report.epoch
    );

    Ok(BackfillReport {
        module: spec.name,
        range,
        chunks,
        logs,
        rewritten: Some(hull),
        rows_tombstoned: report.children_tombstoned,
        rows_written,
        epoch: report.epoch,
    })
}
