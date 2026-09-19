//! The indexing pipeline.
//!
//! ```text
//! HyperSync stream (ordered responses, whole blocks per response)
//!   -> transform (response -> rows, block timestamp joined by number;
//!      decoder modules - DEX, predictions - over ALL logs of the response)
//!   -> reorg guard: do these blocks build on what is stored / streamed?
//!        no  -> fork-point search, purge_range, resume from the fork point
//!   -> token / pool / venue workers: non-blocking `discover` (never awaits)
//!   -> bounded channel (backpressure)
//!   -> writer: accumulate, flush on rows / interval / barrier
//!        flush = one `_version` + the chain's `epoch` on every row, every
//!                child table concurrently (module tables included),
//!                `blocks` LAST (commit marker), then `checkpoints`
//! ```
//!
//! Nothing on this path talks to an RPC endpoint: token, pool and venue
//! metadata is resolved by background workers (`workers`), which insert
//! their own rows and heal what they missed from the database.

pub mod backfill;
mod dedup;
pub mod lease;
pub mod modules;
pub mod store;
#[cfg(test)]
mod sync_tests;
pub mod transform;
pub mod verify;
pub mod workers;
pub mod writer;

#[cfg(test)]
mod acceptance;

use crate::{
    configs::Config,
    db::{
        ranges::{subtract_ranges, BlockRange, MissingRanges},
        Database, RowBatch,
    },
    metrics::{self, Metrics},
    reorg::{
        BlockHeader, CanonicalChain, DiscoveryCache, PurgeReason, Purger,
        ReorgConfig, ReorgError, ReorgGuard, StreamGuard, Verdict,
        WriterControl,
    },
    source::Source,
    tokens::{self, multicall::EthCaller},
    utils::convert::hash_to_b256,
};
use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use hypersync_client::net_types::RollbackGuard;
use lease::{Fence, Lease, LeaseOptions};
use log::{debug, error, info, warn};
use modules::{DecodeState, EnabledModules};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use store::{ClickhouseReorgStore, Scope};
use tokio::sync::{mpsc::Receiver, watch};
use transform::ResponseRows;
use workers::{Discovery, WorkerOptions, Workers};
use writer::{Sink, Writer, WriterHandle, WriterStopped};

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
}

impl Progress for Database {
    async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<MissingRanges> {
        Database::missing_ranges(self, range).await
    }
}

/// How often the chain head is polled once caught up.
const HEAD_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Cap of the backoff between failed passes (HyperSync / query errors).
const MAX_PASS_BACKOFF: Duration = Duration::from_secs(60);

/// A pass of at most this many blocks means "following the head".
const TIP_PASS_BLOCKS: u64 = 64;

/// `/readyz`: not ready when nothing happened for this long.
const READY_STALENESS: Duration = Duration::from_secs(120);

/// How long [`WriterGate::quiesce`] waits for the last flush to become
/// readable (ClickHouse has no read-your-writes; normally a few ms).
const VISIBILITY_ATTEMPTS: u32 = 200;
const VISIBILITY_DELAY: Duration = Duration::from_millis(25);

/// The last flush: highest block and the flush `_version`.
type LastFlush = Arc<Mutex<Option<(u64, u64)>>>;

/// Production sink: ClickHouse, then the workers' discovery.
struct ClickhouseSink {
    db: Database,
    discovery: Discovery,
    /// Asked before every flush: this process must not write once it lost
    /// the chain's lease, or once its own heartbeats stalled past the ttl
    /// (`pipeline::lease`, "Fencing").
    fence: Fence,
    last_flush: LastFlush,
    /// Block spans flushed with an epoch that was superseded WHILE the
    /// flush ran (a purge of another process, i.e. `indexer backfill`):
    /// the validity rule may hide their aggregate contributions, so the
    /// sync loop purges and streams them again.
    stale: Arc<Mutex<Vec<BlockRange>>>,
}

impl Sink for ClickhouseSink {
    /// The epoch in force: what this process adopted, or a newer one
    /// another process (`indexer backfill`) wrote into `reorgs`.
    async fn epoch(&self) -> Result<u32> {
        self.db.refresh_epoch().await
    }

    async fn store(&self, batch: &RowBatch) -> Result<()> {
        // Before anything is written: is this process still the one that
        // owns the chain? A failure here is final, like any failed flush.
        self.fence.check()?;

        self.db.store(batch).await?;

        if let Some((from, to)) = batch.block_span() {
            *self.last_flush.lock().unwrap() = Some((to, batch.version()));

            // Did the epoch move while the rows were on their way? Best
            // effort (a failed read is not a failed flush).
            match self.db.refresh_epoch().await {
                Ok(epoch) if epoch > batch.epoch() => {
                    warn!(
                        "Chain {}: the epoch moved from {} to {epoch} while                          blocks {from}..={to} were flushed; they will be                          purged and indexed again.",
                        self.db.chain_id,
                        batch.epoch()
                    );
                    self.stale
                        .lock()
                        .unwrap()
                        .push(BlockRange::new(from, to + 1));
                }
                Ok(_) => {}
                Err(e) => debug!("Epoch re-read after the flush: {e:#}"),
            }
        }

        // Only now are the rows durable. Never awaits.
        self.discovery.stored(&batch.modules);

        Ok(())
    }
}

/// What the reorg logic needs from the writer.
struct WriterGate {
    writer: WriterHandle,
    /// A purge rewrites history: it is fenced like a flush.
    fence: Fence,
    /// Resolves once the last flush can be read back.
    visible: Box<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>,
    adopt: Box<dyn Fn(u32) + Send + Sync>,
}

impl WriterControl for WriterGate {
    fn quiesce(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Step 0 of every purge: a process that lost the chain must
            // not tombstone rows or bump the epoch either.
            self.fence.check()?;
            self.writer.barrier().await?;
            (self.visible)().await
        })
    }

    fn adopt_epoch(&self, epoch: u32) {
        (self.adopt)(epoch);
    }
}

/// Polls until the `blocks` row of the last flush is readable.
async fn wait_until_visible(db: Database, last: LastFlush) -> Result<()> {
    let Some((number, version)) = *last.lock().unwrap() else {
        return Ok(());
    };

    // No FINAL: the exact row version this process wrote.
    let sql = format!(
        "SELECT toUInt64(count()) FROM blocks WHERE chain = {} \
         AND number = {number} AND _version = {version}",
        db.chain_id
    );

    for _ in 0..VISIBILITY_ATTEMPTS {
        let rows: u64 = db
            .db
            .query(&sql)
            .fetch_one()
            .await
            .context("read back the last flush")?;

        if rows > 0 {
            return Ok(());
        }

        tokio::time::sleep(VISIBILITY_DELAY).await;
    }

    bail!(
        "block {number} of the last flush (version {version}) can not be \
         read back after {:?}",
        VISIBILITY_DELAY * VISIBILITY_ATTEMPTS
    )
}

/// The workers' caches are keyed by address, not by block: a token or a
/// pool discovered on an abandoned fork is still a contract worth a
/// metadata row, and the database backfill finds whatever is missing
/// after the range is streamed again. Nothing to evict.
struct NoBlockKeyedCaches;

impl DiscoveryCache for NoBlockKeyedCaches {
    fn evict_range(&self, _from: u64, _to: Option<u64>) {}
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
    /// Minimum time between two commits while following the head: fewer,
    /// larger inserts (every insert of a flush is a synchronous ClickHouse
    /// part, and 50+ chains share the server).
    tip_interval: Duration,
}

struct Indexer<S: BlockSource, P: Progress> {
    settings: SyncSettings,
    progress: P,
    source: S,
    writer: Writer,
    guard: ReorgGuard,
    modules: EnabledModules,
    decode_state: DecodeState,
    /// `None` in tests without workers.
    discovery: Option<Discovery>,
    metrics: Metrics,
    /// Ranges this process committed (and did not purge since). A gap
    /// listing that reports one of them is a stale read (no
    /// read-your-writes), not a gap.
    committed: Vec<BlockRange>,
    /// See [`ClickhouseSink::stale`].
    stale: Arc<Mutex<Vec<BlockRange>>>,
    /// Shares the epoch memory with the guard's purger.
    purger: Purger,
}

/// What [`run_with`] needs besides the configuration: everything that
/// talks to the outside world, so the acceptance tests can run the REAL
/// pipeline against an in-memory chain and a fake RPC.
pub struct Runtime<S: BlockSource> {
    pub source: S,
    /// The chain as the source sees it now, for the fork-point search.
    pub canonical: Arc<dyn CanonicalChain>,
    /// The shared RPC backend of the workers, as built by
    /// `tokens::build_caller*` from `--rpc` (`None` = `--rpc none`).
    pub caller: Option<Arc<dyn EthCaller>>,
    pub workers: WorkerOptions,
    pub lease: LeaseOptions,
    /// Resolves when the process should stop (SIGINT / SIGTERM).
    pub shutdown: BoxFuture<'static, ()>,
}

/// Runs the indexer. Returns `Ok` when `--end-block` was reached or a
/// shutdown signal arrived, `Err` on a fatal error (startup failure or a
/// flush that failed after all its retries).
pub async fn run(config: Config) -> Result<()> {
    let source = Source::new(
        config.chain_id,
        config.hypersync_url.as_deref(),
        &config.hypersync_token,
    )?;

    // The default endpoint is derived from the chain id; only a custom url
    // can point at the wrong chain.
    if config.hypersync_url.is_some() {
        source.verify_chain_id(config.chain_id).await?;
    }

    // `--rpc` unset / blank = `auto` (public endpoints, discovered in the
    // background: this never blocks or fails the start), `none` = no RPC.
    let caller = tokens::build_caller_shared(
        config.chain_id,
        config.rpc_url.as_deref(),
        config.redis_url.as_deref(),
    )
    .await
    .context("set up the RPC endpoints (--rpc)")?;

    let runtime = Runtime {
        canonical: Arc::new(source.clone()),
        source,
        caller,
        workers: WorkerOptions::default(),
        lease: LeaseOptions::default(),
        shutdown: Box::pin(shutdown_signal()),
    };

    run_with(config, runtime).await
}

/// [`run`] over explicit backends.
pub async fn run_with<S: BlockSource>(
    config: Config,
    runtime: Runtime<S>,
) -> Result<()> {
    let metrics = match config.metrics_addr {
        Some(_) => Metrics::new(config.chain_id, READY_STALENESS),
        None => Metrics::disabled(),
    };

    let (stop_metrics, metrics_stopped) = watch::channel(false);

    if let Some(addr) = config.metrics_addr {
        // Fails fast: a port that is taken is a configuration error.
        let server = metrics::bind(addr, metrics.clone())
            .await
            .with_context(|| format!("bind --metrics-addr {addr}"))?;
        info!(
            "Serving metrics on http://{}/metrics.",
            server.local_addr()?
        );

        let mut stopped = metrics_stopped.clone();
        tokio::spawn(async move {
            let shutdown = async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            };
            if let Err(e) = server.run(shutdown).await {
                error!("Metrics server stopped: {e:#}");
            }
        });
    }

    let db = Database::new(&config.database_url, config.chain_id)
        .await?
        .with_metrics(metrics.clone());

    // One process per chain, before anything is written.
    let (fatal_tx, mut fatal) = watch::channel(None::<String>);
    let lease = Lease::acquire(&db, runtime.lease, fatal_tx).await?;

    let enabled = EnabledModules {
        dex: config.dex,
        predictions: config.predictions,
        launchpads: config.launchpads,
        // MODULE: <module>: config.<module>,
    };

    info!(
        "Modules: DEX {}, prediction markets {}, launchpads {}. \
         RPC metadata: {}.",
        if enabled.dex { "on" } else { "off (--no-dex)" },
        if enabled.predictions { "on" } else { "off (--no-predictions)" },
        if enabled.launchpads { "on" } else { "off (--no-launchpads)" },
        if runtime.caller.is_some() { "on" } else { "off (--rpc none)" },
        // MODULE: one placeholder and one arm in the line above.
    );

    // Checkpoints say where a previous run got to; `blocks` stays the
    // truth: the first pass verifies the whole range with the gap query.
    //
    // They are NOT used as the cursor, although docs/design.md section 3
    // says "resume = max contiguous to_block". Starting the cursor at the
    // resume point would skip the first pass's inspection of everything
    // below it - and that inspection is the ONLY thing that finds the
    // orphan children of a flush that died before its `blocks` insert
    // (`ReorgGuard::begin_pass`). A checkpoint is written after `blocks`,
    // so it cannot claim such a range; but a purge that died after
    // tombstoning children and before its `reorgs` row can leave one
    // below it. The gap query over `blocks` costs one indexed read per
    // pass and answers the same question without that hole, so the
    // checkpoints stay what they are: an index for operators and for
    // `indexer verify`, and the log line below.
    let resume =
        verify::resume_point(&db, config.start_block).await.unwrap_or(0);
    if resume > config.start_block {
        info!(
            "Checkpoints cover blocks [{}, {resume}) without a hole.",
            config.start_block
        );
    }

    // `_version`s must never go back, whatever this host's clock says.
    db.seed_version(&modules::versioned_tables()).await?;

    // The epoch of a chain survives restarts in `reorgs`.
    db.set_epoch(db.current_epoch().await?);

    let workers = Workers::spawn(
        &db,
        runtime.caller,
        config.redis_url.as_deref(),
        enabled,
        metrics.clone(),
        runtime.workers,
    )?;
    let discovery = workers.discovery();

    let last_flush = LastFlush::default();
    let stale = Arc::new(Mutex::new(Vec::new()));

    let fence = lease.fence();

    let writer = Writer::spawn_with_metrics(
        ClickhouseSink {
            db: db.clone(),
            discovery: discovery.clone(),
            fence: fence.clone(),
            last_flush: last_flush.clone(),
            stale: stale.clone(),
        },
        config.flush_rows,
        Duration::from_millis(config.flush_interval_ms),
        metrics.clone(),
    );

    let gate = WriterGate {
        writer: writer.handle(),
        fence,
        visible: {
            let db = db.clone();
            Box::new(move || {
                Box::pin(wait_until_visible(
                    db.clone(),
                    last_flush.clone(),
                ))
            })
        },
        adopt: {
            let db = db.clone();
            let discovery = discovery.clone();
            Box::new(move |epoch| {
                db.set_epoch(epoch);
                discovery.set_epoch(epoch);
            })
        },
    };

    let purger = Purger::new(
        Arc::new(ClickhouseReorgStore::new(db.clone(), Scope::Chain)),
        Arc::new(gate),
        Arc::new(NoBlockKeyedCaches),
        Arc::new(metrics.clone()),
    );

    let mut reorg_config =
        ReorgConfig::new(config.chain_id, config.start_block);
    reorg_config.max_reorg_depth = config.max_reorg_depth;

    let decode_state = DecodeState {
        registries: modules::known_registries(&db, enabled).await?,
    };

    let mut indexer = Indexer {
        settings: SyncSettings {
            chain_id: config.chain_id,
            start_block: config.start_block,
            end_block: config.end_block,
            confirmations: config.confirmations,
            new_blocks_only: config.new_blocks_only,
            tip_interval: Duration::from_millis(
                config.flush_interval_ms.saturating_mul(2),
            ),
        },
        progress: db.clone(),
        source: runtime.source,
        writer,
        guard: ReorgGuard::new(
            reorg_config,
            runtime.canonical,
            purger.clone(),
        ),
        purger,
        modules: enabled,
        decode_state,
        discovery: Some(discovery),
        metrics: metrics.clone(),
        committed: Vec::new(),
        stale,
    };

    // 1. Stop the stream ...
    let result = tokio::select! {
        result = indexer.sync() => result,
        _ = runtime.shutdown => {
            info!("Shutdown requested, flushing buffered rows.");
            Ok(())
        }
        reason = async {
            match fatal.wait_for(|reason| reason.is_some()).await {
                Ok(reason) => reason.clone().unwrap_or_default(),
                // The lease task ended without a verdict: never fatal.
                Err(_) => std::future::pending().await,
            }
        } => Err(anyhow::anyhow!(reason)),
    };

    metrics.set_ready(false);

    // 2. ... final flush (if the writer died its error is the root cause,
    //    the sync loop only sees "writer stopped") ...
    let flushed = indexer.writer.shutdown().await;

    // 3. ... then the workers, then the lease and the metrics endpoint.
    workers.shutdown().await;
    lease.release().await;
    let _ = stop_metrics.send(true);

    match (result, flushed) {
        (_, Err(e)) => Err(e),
        (result, Ok(())) => result,
    }
}

pub(crate) async fn shutdown_signal() {
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
/// newest `confirmations` blocks are left alone: a reorg no deeper than
/// that never touches a stored block. An explicit `end_block` (exclusive)
/// caps the result.
fn target_block(head: u64, confirmations: u64, end_block: u64) -> u64 {
    let confirmed = head.saturating_sub(confirmations);

    if end_block > 0 {
        confirmed.min(end_block)
    } else {
        confirmed
    }
}

/// Errors no retry can fix: a flush that failed for good, a reorg deeper
/// than `--max-reorg-depth`, another network under this chain id, ...
fn is_fatal(error: &anyhow::Error) -> bool {
    WriterStopped::is_cause_of(error)
        || error
            .chain()
            .filter_map(|cause| cause.downcast_ref::<ReorgError>())
            .any(ReorgError::is_fatal)
}

/// Adds `range` to the ordered, merged list of committed ranges.
fn remember(committed: &mut Vec<BlockRange>, range: BlockRange) {
    if range.is_empty() {
        return;
    }

    committed.push(range);
    committed.sort_unstable_by_key(|r| r.from);

    let mut merged: Vec<BlockRange> = Vec::with_capacity(committed.len());
    for range in committed.drain(..) {
        match merged.last_mut() {
            Some(last) if range.from <= last.to => {
                last.to = last.to.max(range.to)
            }
            _ => merged.push(range),
        }
    }

    *committed = merged;
}

/// How a pass ended.
enum PassOutcome {
    /// Everything below this block is stored.
    Covered(u64),
    /// A rollback happened: continue from this block.
    RolledBack(u64),
}

impl<S: BlockSource, P: Progress> Indexer<S, P> {
    async fn sync(&mut self) -> Result<()> {
        let end_block = self.settings.end_block;
        let confirmations = self.settings.confirmations;

        // Startup errors are fatal (bad token, wrong url...). Later ones
        // are retried forever.
        let mut head = self.source.head().await?;
        self.saw_head(head);

        // The writer stamps the epoch the chain is at.
        let epoch = self.guard.startup().await?;
        if epoch > 0 {
            info!("Chain {}: epoch {epoch}.", self.settings.chain_id);
        }

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
        let mut last_tip_commit: Option<tokio::time::Instant> = None;

        loop {
            let target = target_block(head, confirmations, end_block);

            let at_tip = target.saturating_sub(cursor) <= TIP_PASS_BLOCKS;
            let paced = at_tip
                && end_block == 0
                && last_tip_commit.is_some_and(|at| {
                    at.elapsed() < self.settings.tip_interval
                });

            if target > cursor && !paced {
                match self.pass(BlockRange::new(cursor, target)).await {
                    Ok(PassOutcome::Covered(covered_until)) => {
                        failures = 0;
                        cursor = covered_until;
                        self.metrics.set_ready(true);
                        if at_tip {
                            last_tip_commit =
                                Some(tokio::time::Instant::now());
                        }
                    }
                    Ok(PassOutcome::RolledBack(fork_point)) => {
                        failures = 0;
                        cursor = cursor.min(fork_point);
                        continue;
                    }
                    // Fatal, never retried.
                    Err(e) if is_fatal(&e) => return Err(e),
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        self.metrics.stream_error();
                        let wait = pass_backoff(failures);
                        warn!("Sync pass failed: {e:#}. Retrying in {wait:?}.");
                        tokio::time::sleep(wait).await;
                    }
                }
            } else if target <= cursor {
                self.metrics.set_ready(true);
            }

            if end_block > 0 && cursor >= end_block {
                info!("Finished syncing up to block {end_block} (exclusive).");
                return Ok(());
            }

            if cursor >= target || paced {
                tokio::time::sleep(HEAD_POLL_INTERVAL).await;
            }

            match self.source.head().await {
                // The head never moves backwards for our purposes.
                Ok(new_head) => {
                    head = head.max(new_head);
                    self.saw_head(head);
                }
                Err(e) => warn!("Could not fetch the chain head: {e:#}"),
            }
        }
    }

    /// Every successful head poll: the idle-time sign of life of
    /// `/readyz`, and what lets the RPC layer reject nodes that lag.
    fn saw_head(&self, head: u64) {
        self.metrics.set_head(head.saturating_sub(1));
        if let Some(discovery) = &self.discovery {
            discovery.set_head(head.saturating_sub(1));
        }
    }

    /// Indexes every missing block of `range`.
    async fn pass(&mut self, range: BlockRange) -> Result<PassOutcome> {
        if let Some(from) = self.purge_stale_flushes().await? {
            return Ok(PassOutcome::RolledBack(from));
        }

        let mut missing = self.progress.missing_ranges(range).await?;

        // No read-your-writes: what this process committed is not a gap,
        // whatever a lagging read says.
        missing.ranges = subtract_ranges(&missing.ranges, &self.committed);

        // Gap healing (first inspection of a range since startup only):
        // orphan children of a flush that died before its `blocks` rows
        // are purged BEFORE the range is streamed, or the aggregates
        // would count them twice.
        let gaps: Vec<(u64, u64)> =
            missing.ranges.iter().map(|r| (r.from, r.to)).collect();

        // `covered_until`, NOT `range.to`: when the gap listing hit
        // `MAX_GAPS_PER_PASS` it only accounts for the blocks below the
        // last gap it returned. Telling the guard "everything up to
        // range.to was inspected" would move its `healed_until` past gaps
        // it never saw, and those ranges would never be orphan-checked
        // again - their orphans stay live, the blocks are re-streamed on
        // top, and the aggregates count both.
        let healed =
            self.guard.begin_pass(&gaps, missing.covered_until).await?;
        if healed > 0 {
            info!(
                "Chain {}: healed {healed} gap range(s) left by an \
                 interrupted write.",
                self.settings.chain_id
            );
        }

        if missing.ranges.is_empty() {
            return Ok(PassOutcome::Covered(missing.covered_until));
        }

        let blocks: u64 = missing.ranges.iter().map(BlockRange::len).sum();

        if blocks > 1 {
            info!(
                "Syncing {blocks} missing blocks in {} range(s) of {range}.",
                missing.ranges.len()
            );
        }

        let mut streamed = Ok(None);
        let mut delivered: Vec<BlockRange> = Vec::new();

        for missing_range in &missing.ranges {
            let mut reached = missing_range.from;

            streamed = self
                .stream_range(*missing_range, range.to, &mut reached)
                .await;

            delivered.push(BlockRange::new(missing_range.from, reached));

            if !matches!(streamed, Ok(None)) {
                break;
            }
        }

        // Commit point, also after a failed stream: whatever was delivered
        // is stored now, so the next pass sees it and only asks for the
        // rest (no duplicates, no lost work).
        let flushed = self.writer.barrier().await;

        // The writer's failure wins: it is fatal, a stream error is not.
        flushed?;

        for range in delivered {
            remember(&mut self.committed, range);
        }

        match streamed? {
            Some(rollback) => Ok(rollback),
            None => Ok(PassOutcome::Covered(missing.covered_until)),
        }
    }

    /// Streams `range`. `Ok(Some(..))` when a rollback ended the pass.
    /// `reached` is the block up to which rows went to the writer.
    async fn stream_range(
        &mut self,
        range: BlockRange,
        pass_end: u64,
        reached: &mut u64,
    ) -> Result<Option<PassOutcome>> {
        debug!("Streaming {range}.");

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
            let enabled = self.modules;
            let data = response.data;
            let mut state = std::mem::take(&mut self.decode_state);

            // CPU bound: keep it off the async workers. The decode state
            // travels with it and comes back, error or not.
            let (transformed, state) =
                tokio::task::spawn_blocking(move || {
                    let transformed = transform::transform_with(
                        chain, &data, covered, enabled, &mut state,
                    );
                    (transformed, state)
                })
                .await
                .context("transform task panicked")?;

            self.decode_state = state;
            let transformed = transformed?;

            // BEFORE the rows go to the writer: do they build on what is
            // stored / was streamed? A failing lookup is an error (the
            // pass is retried), never "no evidence".
            let headers: Vec<BlockHeader> = transformed
                .rows
                .blocks
                .iter()
                .map(|block| BlockHeader {
                    number: block.number,
                    hash: block.hash,
                    parent_hash: block.parent_hash,
                    timestamp: block.timestamp,
                })
                .collect();

            let stream_guard =
                response.rollback_guard.as_ref().map(|guard| {
                    StreamGuard {
                        first_block: guard.first_block_number,
                        first_parent_hash: hash_to_b256(
                            &guard.first_parent_hash,
                        ),
                    }
                });

            let verdict = self
                .guard
                .observe(&headers, stream_guard.as_ref())
                .await?;

            if let Verdict::Rollback(rollback) = verdict {
                // The response is dropped: it belongs to the new fork and
                // is streamed again from the fork point.
                drop(responses);
                return self.roll_back(rollback).await.map(Some);
            }

            // Never awaits, never blocks: bounded queues, drop-on-full.
            if let Some(discovery) = &self.discovery {
                discovery.tokens_seen(&transformed.tokens_seen);
            }

            self.writer.send(transformed.rows).await?;

            cursor = next_block;
            *reached = cursor;
        }

        if cursor != range.to {
            bail!("HyperSync stream for {range} ended early at {cursor}");
        }

        // A gap below stored blocks was filled: is the block right above
        // it still canonical?
        if range.to < pass_end {
            if let Verdict::Rollback(rollback) =
                self.guard.check_join(range.to).await?
            {
                return self.roll_back(rollback).await.map(Some);
            }
        }

        Ok(None)
    }

    async fn roll_back(
        &mut self,
        rollback: crate::reorg::Rollback,
    ) -> Result<PassOutcome> {
        let started = std::time::Instant::now();
        let report = self.guard.repair(&rollback).await?;

        self.forget_committed(rollback.fork_point, rollback.purge_to);

        warn!(
            "Chain {}: rolled back blocks [{}, {}): {} blocks, {} rows, \
             {} checkpoints tombstoned, aggregates rebuilt over unix time \
             [{}, {}); epoch is now {} ({:?}). Resuming from block {}.",
            self.settings.chain_id,
            rollback.fork_point,
            rollback
                .purge_to
                .map_or("head".to_string(), |to| to.to_string()),
            report.blocks_tombstoned,
            report.children_tombstoned,
            report.checkpoints_tombstoned,
            report.from_ts.unwrap_or_default(),
            report.to_ts.unwrap_or_default(),
            report.epoch,
            started.elapsed(),
            rollback.fork_point,
        );

        Ok(PassOutcome::RolledBack(rollback.fork_point))
    }

    fn forget_committed(&mut self, from: u64, to: Option<u64>) {
        let purged = BlockRange::new(from, to.unwrap_or(u64::MAX));
        self.committed = subtract_ranges(&self.committed, &[purged]);
    }

    /// See [`ClickhouseSink::stale`]. Returns the lowest purged block.
    async fn purge_stale_flushes(&mut self) -> Result<Option<u64>> {
        let stale: Vec<BlockRange> =
            std::mem::take(&mut *self.stale.lock().unwrap());

        let mut lowest = None;

        for range in stale {
            self.purger
                .purge_range(
                    self.settings.chain_id,
                    range.from,
                    Some(range.to),
                    PurgeReason::GapHeal,
                )
                .await?;
            self.forget_committed(range.from, Some(range.to));
            lowest = Some(
                lowest.map_or(range.from, |low: u64| low.min(range.from)),
            );
        }

        Ok(lowest)
    }
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
