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
        flush_windows, next_version, ranges::BlockRange, Database,
        FlushKey, FlushWindow,
    },
    pipeline::{
        lease::{Lease, LeaseOptions},
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
use tokio::sync::watch;

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

/// Where every row decoded from one chunk belongs: the `timestamp` of its
/// block (every module row carries its block's) and the block number, so
/// a chunk that spans more monthly partitions than one insert may touch
/// can be split the way a flush is (`db::flush_windows`).
type ChunkBlocks = Vec<(u32, u64)>;

/// The parts one chunk has to be written in: `(window, first block, last
/// block)`, oldest first. Normally one, [`FlushWindow::ALL`].
///
/// `--chunk-blocks` is a block count, so on a chain with a long block time
/// (or a very large value) one chunk can cover hundreds of UTC months,
/// which ClickHouse refuses with code 252
/// (`max_partitions_per_insert_block`). The backfill used to write every
/// chunk as one insert and fail there (review round 4, MINOR 17).
///
/// The block span is per part, so each part gets a deduplication token of
/// its own: the same token for two parts would make the server drop the
/// second one.
fn chunk_parts(blocks: &ChunkBlocks) -> Vec<(FlushWindow, (u64, u64))> {
    flush_windows(blocks.iter().map(|(timestamp, _)| *timestamp))
        .into_iter()
        .filter_map(|window| {
            let numbers = blocks
                .iter()
                .filter(|(timestamp, _)| window.holds(*timestamp))
                .map(|(_, number)| *number);

            let (low, high) = numbers.clone().min().zip(numbers.max())?;
            Some((window, (low, high)))
        })
        .collect()
}

/// Decodes `chunk` from the stored logs. `state` must see the chunks in
/// chain order.
async fn decode_chunk(
    db: &Database,
    enabled: EnabledModules,
    chunk: BlockRange,
    state: &mut DecodeState,
) -> Result<(ModuleRows, u64, ChunkBlocks)> {
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

    // Every decoded row carries the `timestamp` of the log it came from.
    let mut blocks: ChunkBlocks =
        logs.iter().map(|log| (log.timestamp, log.block_number)).collect();
    blocks.dedup();

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
        blocks,
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

    // The backfill IS a writer: it purges, bumps the chain's epoch and
    // rebuilds every aggregate. Two of them on the same chain and module
    // corrupt it exactly as two indexers would - both write a `reorgs`
    // row, and the lower-epoch rebuild ends up hidden by the higher floor
    // (review round 4, MINOR 16). A role of its own, so the
    // documented combination "a backfill next to a live `indexer run`"
    // keeps working.
    let (fatal, _taken_over) = watch::channel(None::<String>);
    let lease = Lease::acquire_as(
        db,
        &format!("backfill:{}", spec.name),
        LeaseOptions::default(),
        fatal,
    )
    .await?;

    let report = run_backfill(
        db,
        spec,
        enabled,
        from_block,
        to_block,
        chunk_blocks,
    )
    .await;

    lease.release().await;
    report
}

/// [`backfill`] with the lease already held.
async fn run_backfill(
    db: &Database,
    spec: &'static ModuleSpec,
    enabled: EnabledModules,
    from_block: u64,
    to_block: u64,
    chunk_blocks: u64,
) -> Result<BackfillReport> {
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
        let (decoded, count, _) =
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
        let (mut decoded, _, blocks) =
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

        // Oldest month first, one insert per part: a chunk of blocks can
        // still span more monthly partitions than one insert may touch.
        for (window, span) in chunk_parts(&blocks) {
            let key = FlushKey { chain: db.chain_id, span, version };

            decoded.store(db, &key, window).await.with_context(|| {
                format!("write the '{}' rows of {chunk}", spec.name)
            })?;
        }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::MAX_MONTHS_PER_FLUSH;

    /// Roughly one block per UTC month.
    fn monthly(count: u64) -> ChunkBlocks {
        let mut blocks = Vec::new();
        let mut timestamp = 1_000_000_000u32;
        for number in 0..count {
            blocks.push((timestamp, number));
            timestamp += 31 * 86_400;
        }
        blocks
    }

    /// A chunk of blocks is a block COUNT, so on a chain with a long block
    /// time it can span more monthly partitions than one insert may touch
    /// (ClickHouse code 252). It is written in parts, like a flush
    /// (review round 4, MINOR 17).
    #[test]
    fn a_chunk_that_spans_too_many_months_is_written_in_parts() {
        // The ordinary case: one insert, and the whole block span.
        let blocks = monthly(12);
        assert_eq!(
            chunk_parts(&blocks),
            vec![(FlushWindow::ALL, (0, 11))]
        );

        let blocks = monthly(MAX_MONTHS_PER_FLUSH as u64 * 2 + 5);
        let parts = chunk_parts(&blocks);
        assert!(parts.len() > 1, "{} months in one insert", blocks.len());

        // Every block lands in exactly one part, oldest first, and no two
        // parts share a deduplication token (the block span is the token).
        let mut covered = 0;
        let mut previous: Option<(u64, u64)> = None;
        for (window, span) in &parts {
            let in_part = blocks
                .iter()
                .filter(|(timestamp, _)| window.holds(*timestamp))
                .count();
            assert!(in_part > 0);
            covered += in_part;

            assert!(span.0 <= span.1);
            if let Some(before) = previous {
                assert!(before.1 < span.0, "{before:?} then {span:?}");
            }
            previous = Some(*span);
        }
        assert_eq!(covered, blocks.len());

        assert!(chunk_parts(&Vec::new()).is_empty());
    }
}
