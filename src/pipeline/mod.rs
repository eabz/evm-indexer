//! The indexing pipeline.
//!
//! ```text
//! HyperSync stream (ordered responses, whole blocks per response)
//!   -> transform (response -> rows, block timestamp joined by number)
//!   -> TokenResolver::resolve_new for the token contracts seen
//!   -> bounded channel (backpressure)
//!   -> writer: accumulate, flush on rows / interval / barrier
//!        flush = every table concurrently, `blocks` LAST (commit marker),
//!                then TokenResolver::mark_stored
//! ```

pub mod reorg;
#[cfg(test)]
mod sync_tests;
pub mod transform;
pub mod writer;

use crate::{
    configs::Config,
    db::{
        models::token::DatabaseToken,
        ranges::{BlockRange, MissingRanges},
        Database, RowBatch,
    },
    source::Source,
    tokens::{TokenResolver, TokenStandard},
};
use alloy::primitives::{Address, B256};
use anyhow::{bail, Context, Result};
use hypersync_client::net_types::RollbackGuard;
use log::{debug, info, warn};
use reorg::{ReorgDetector, ReorgEvidence};
use std::{collections::HashMap, future::Future, time::Duration};
use tokio::sync::mpsc::Receiver;
use transform::ResponseRows;
use writer::{Sink, Writer, WriterStopped};

/// One ordered response of a block stream: every block of
/// `[previous next_block, next_block)`.
#[derive(Debug)]
pub struct SourceResponse {
    /// Exclusive end of the blocks covered by this response.
    pub next_block: u64,
    pub data: ResponseRows,
    pub rollback_guard: Option<RollbackGuard>,
}

/// Where blocks come from (HyperSync in production).
pub trait BlockSource: Send + Sync + 'static {
    /// Exclusive upper bound of the blocks that can be streamed right now.
    fn head(&self) -> impl Future<Output = Result<u64>> + Send;

    /// Ordered responses covering exactly `range`.
    fn stream(
        &self,
        range: BlockRange,
    ) -> impl Future<Output = Result<Receiver<Result<SourceResponse>>>> + Send;
}

/// What the sync loop needs to know about already stored data
/// (ClickHouse in production).
pub trait Progress: Send + Sync + 'static {
    fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> impl Future<Output = Result<MissingRanges>> + Send;

    /// Hash of a stored canonical block, for reorg detection.
    fn block_hash(
        &self,
        number: u64,
    ) -> impl Future<Output = Result<Option<B256>>> + Send;
}

impl Progress for Database {
    async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<MissingRanges> {
        Database::missing_ranges(self, range).await
    }

    async fn block_hash(&self, number: u64) -> Result<Option<B256>> {
        match Database::block_hash(self, number).await? {
            Some(hash) => Ok(Some(
                hash.parse::<B256>().context("parse stored block hash")?,
            )),
            None => Ok(None),
        }
    }
}

/// How often the chain head is polled once caught up.
const HEAD_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cap of the backoff between failed passes (HyperSync / query errors).
const MAX_PASS_BACKOFF: Duration = Duration::from_secs(60);

/// Production sink: ClickHouse, then the token cache.
struct ClickhouseSink {
    db: Database,
    tokens: TokenResolver,
}

impl Sink for ClickhouseSink {
    async fn store(&self, batch: &RowBatch) -> Result<()> {
        self.db.store(batch).await?;

        if !batch.tokens.is_empty() {
            // Only now are the rows durable: persist them to the cache.
            self.tokens.mark_stored(&batch.tokens).await;
        }

        Ok(())
    }
}

/// The part of the configuration the sync loop cares about.
#[derive(Debug, Clone, Copy)]
struct SyncSettings {
    chain_id: u64,
    start_block: u64,
    /// Exclusive, 0 = follow the chain head.
    end_block: u64,
    /// Blocks to stay behind the chain head.
    confirmations: u64,
    new_blocks_only: bool,
}

struct Indexer<S: BlockSource, P: Progress> {
    settings: SyncSettings,
    progress: P,
    source: S,
    tokens: TokenResolver,
    writer: Writer,
    detector: ReorgDetector,
}

/// Runs the indexer. Returns `Ok` when `--end-block` was reached or a
/// shutdown signal arrived, `Err` on a fatal error (startup failure or a
/// flush that failed after all its retries).
pub async fn run(config: Config) -> Result<()> {
    let db = Database::new(&config.database_url, config.chain_id).await?;

    let source = Source::new(
        config.chain_id,
        config.hypersync_url.as_deref(),
        &config.hypersync_token,
        config.traces,
    )?;

    // The default endpoint is derived from the chain id; only a custom url
    // can point at the wrong chain.
    if config.hypersync_url.is_some() {
        source.verify_chain_id(config.chain_id).await?;
    }

    let tokens = TokenResolver::new(
        config.chain_id,
        config.rpc_url.as_deref(),
        config.redis_url.as_deref(),
    )
    .await
    .context("create token resolver")?;

    let writer = Writer::spawn(
        ClickhouseSink { db: db.clone(), tokens: tokens.clone() },
        config.flush_rows,
        Duration::from_millis(config.flush_interval_ms),
    );

    let mut indexer = Indexer {
        settings: SyncSettings {
            chain_id: config.chain_id,
            start_block: config.start_block,
            end_block: config.end_block,
            confirmations: config.confirmations,
            new_blocks_only: config.new_blocks_only,
        },
        progress: db,
        source,
        tokens,
        writer,
        detector: ReorgDetector::default(),
    };

    let result = tokio::select! {
        result = indexer.sync() => result,
        _ = shutdown_signal() => {
            info!("Shutdown requested, flushing buffered rows.");
            Ok(())
        }
    };

    // Flushes what is buffered. If the writer died its error is the root
    // cause (the sync loop only sees "writer stopped").
    let flushed = indexer.writer.shutdown().await;

    match (result, flushed) {
        (_, Err(e)) => Err(e),
        (result, Ok(())) => result,
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn pass_backoff(failures: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_PASS_BACKOFF)
}

/// Exclusive block the indexer wants to reach right now.
///
/// `head` is the exclusive bound of the blocks the source can serve. The
/// newest `confirmations` blocks are left alone: reorgs are only detected,
/// never repaired, so a block must not be stored while it can still be
/// replaced. An explicit `end_block` (exclusive) caps the result.
fn target_block(head: u64, confirmations: u64, end_block: u64) -> u64 {
    let confirmed = head.saturating_sub(confirmations);

    if end_block > 0 {
        confirmed.min(end_block)
    } else {
        confirmed
    }
}

impl<S: BlockSource, P: Progress> Indexer<S, P> {
    async fn sync(&mut self) -> Result<()> {
        let end_block = self.settings.end_block;
        let confirmations = self.settings.confirmations;

        // Startup errors are fatal (bad token, wrong url...). Later ones
        // are retried forever.
        let mut head = self.source.head().await?;

        // Everything below the cursor is known to be stored.
        let mut cursor = if self.settings.new_blocks_only {
            // Same horizon the loop syncs to: nothing unconfirmed is stored.
            head.saturating_sub(confirmations)
        } else {
            self.settings.start_block
        };

        info!(
            "Chain head is {head}. Syncing from block {cursor}{}{}.",
            if end_block > 0 {
                format!(" up to, not including, block {end_block}")
            } else {
                " and following the chain head".to_string()
            },
            if confirmations > 0 {
                format!(", staying {confirmations} blocks behind the head")
            } else {
                String::new()
            }
        );

        let mut failures: u32 = 0;

        loop {
            let target = target_block(head, confirmations, end_block);

            if target > cursor {
                match self.pass(BlockRange::new(cursor, target)).await {
                    Ok(covered_until) => {
                        failures = 0;
                        cursor = covered_until;
                    }
                    // Fatal, never retried: a flush failed for good.
                    Err(e) if WriterStopped::is_cause_of(&e) => {
                        return Err(e)
                    }
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        let wait = pass_backoff(failures);
                        warn!("Sync pass failed: {e:#}. Retrying in {wait:?}.");
                        tokio::time::sleep(wait).await;
                    }
                }
            }

            if end_block > 0 && cursor >= end_block {
                info!("Finished syncing up to block {end_block} (exclusive).");
                return Ok(());
            }

            if cursor >= target {
                tokio::time::sleep(HEAD_POLL_INTERVAL).await;
            }

            match self.source.head().await {
                // The head never moves backwards for our purposes.
                Ok(new_head) => head = head.max(new_head),
                Err(e) => warn!("Could not fetch the chain head: {e:#}"),
            }
        }
    }

    /// Indexes every missing block of `range` and returns the block up to
    /// which the range is now known to be fully stored.
    async fn pass(&mut self, range: BlockRange) -> Result<u64> {
        let missing = self.progress.missing_ranges(range).await?;

        if missing.ranges.is_empty() {
            return Ok(missing.covered_until);
        }

        let blocks: u64 = missing.ranges.iter().map(BlockRange::len).sum();

        if blocks > 1 {
            info!(
                "Syncing {blocks} missing blocks in {} range(s) of {range}.",
                missing.ranges.len()
            );
        }

        let mut streamed = Ok(());

        for missing_range in &missing.ranges {
            streamed = self.stream_range(*missing_range).await;
            if streamed.is_err() {
                break;
            }
        }

        // Commit point, also after a failed stream: whatever was delivered
        // is stored now, so the next pass sees it and only asks for the
        // rest (no duplicates, no lost work).
        let flushed = self.writer.barrier().await;

        // The writer's failure wins: it is fatal, a stream error is not.
        flushed?;
        streamed?;

        Ok(missing.covered_until)
    }

    async fn stream_range(&mut self, range: BlockRange) -> Result<()> {
        debug!("Streaming {range}.");

        self.seed_detector(range.from).await;

        let mut responses = self.source.stream(range).await?;
        let mut cursor = range.from;

        while let Some(response) = responses.recv().await {
            let response = response.with_context(|| {
                format!("HyperSync stream for {range}")
            })?;

            let next_block = response.next_block;

            if next_block <= cursor || next_block > range.to {
                bail!(
                    "HyperSync returned next_block {next_block} while \
                     streaming {range} at block {cursor}"
                );
            }

            let covered = BlockRange::new(cursor, next_block);
            let chain = self.settings.chain_id;
            let data = response.data;

            // CPU bound: keep it off the async workers.
            let transformed = tokio::task::spawn_blocking(move || {
                transform::transform(chain, &data, covered)
            })
            .await
            .context("transform task panicked")??;

            if let Some(guard) = &response.rollback_guard {
                if let Some(evidence) = self.detector.check_guard(guard) {
                    warn_reorg(&evidence);
                }
            }

            for evidence in self.detector.observe(&transformed.rows.blocks)
            {
                warn_reorg(&evidence);
            }

            let mut rows = transformed.rows;
            rows.tokens =
                self.resolve_tokens(transformed.tokens_seen).await;

            self.writer.send(rows).await?;

            cursor = next_block;
        }

        if cursor != range.to {
            bail!("HyperSync stream for {range} ended early at {cursor}");
        }

        Ok(())
    }

    /// Makes sure the detector knows the hash of block `from - 1` when that
    /// block was stored by an earlier run. Best effort.
    async fn seed_detector(&mut self, from: u64) {
        if from == 0 || self.detector.knows_parent_of(from) {
            return;
        }

        match self.progress.block_hash(from - 1).await {
            Ok(Some(hash)) => self.detector.seed(from - 1, hash),
            Ok(None) => {}
            Err(e) => debug!("Could not load hash of {}: {e:#}", from - 1),
        }
    }

    /// Metadata rows for the tokens not known yet. The resolver keeps track
    /// of what is known / in flight (until `mark_stored` after the flush),
    /// so the same token is not resolved again for every batch.
    async fn resolve_tokens(
        &self,
        seen: HashMap<Address, TokenStandard>,
    ) -> Vec<DatabaseToken> {
        if seen.is_empty() {
            return Vec::new();
        }

        self.tokens.resolve_new(&seen).await
    }
}

fn warn_reorg(evidence: &ReorgEvidence) {
    warn!(
        "REORG DETECTED at block {}: its parent hash is {:?} but block {} \
         was indexed with hash {:?}. Data at or below block {} may belong \
         to an abandoned fork. No automatic rewind is performed.",
        evidence.block_number,
        evidence.actual_parent,
        evidence.block_number.saturating_sub(1),
        evidence.expected_parent,
        evidence.block_number.saturating_sub(1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_follows_head_or_stops_at_end_block() {
        // No confirmations: the old behaviour.
        assert_eq!(target_block(100, 0, 0), 100);
        assert_eq!(target_block(100, 0, 50), 50);
        assert_eq!(target_block(100, 0, 500), 100);
    }

    #[test]
    fn target_stays_confirmations_behind_the_head() {
        assert_eq!(target_block(100, 12, 0), 88);
        assert_eq!(target_block(100, 100, 0), 0);
        // Young chain / huge setting: saturates, never underflows.
        assert_eq!(target_block(5, 12, 0), 0);
        assert_eq!(target_block(0, u64::MAX, 0), 0);
    }

    #[test]
    fn end_block_still_caps_a_confirmed_target() {
        // Far behind the head: the end block decides.
        assert_eq!(target_block(1_000, 12, 50), 50);
        // Near the head: confirmations decide until the head moves on.
        assert_eq!(target_block(55, 12, 50), 43);
        assert_eq!(target_block(62, 12, 50), 50);
        assert_eq!(target_block(1_000, 12, 50), 50);
    }

    #[test]
    fn pass_backoff_is_capped() {
        assert_eq!(pass_backoff(1), Duration::from_secs(1));
        assert_eq!(pass_backoff(3), Duration::from_secs(4));
        assert_eq!(pass_backoff(30), MAX_PASS_BACKOFF);
        assert_eq!(pass_backoff(u32::MAX), MAX_PASS_BACKOFF);
    }
}
