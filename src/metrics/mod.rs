//! Observability (design §7): a cheap clonable [`Metrics`] handle with
//! typed recording methods, a Prometheus text exposition and the
//! `/metrics`, `/healthz` and `/readyz` endpoints ([`serve`]).
//!
//! Hand-rolled on atomics and a `tokio` `TcpListener`: no metrics or HTTP
//! crate is involved, so none can leak into the rest of the indexer. See
//! `README.md` next to this file for the metric reference.
//!
//! ```no_run
//! # use evm_indexer::metrics::{self, Metrics};
//! # use std::time::Duration;
//! # async fn example() -> anyhow::Result<()> {
//! let metrics = Metrics::new(1, Duration::from_secs(120));
//! let server = metrics::bind("127.0.0.1:9100".parse()?, metrics.clone())
//!     .await?;
//! tokio::spawn(server.run(std::future::pending()));
//!
//! metrics.set_head(19_000_000);
//! metrics.rows_inserted("blocks", 250);
//! metrics.set_ready(true);
//! # Ok(())
//! # }
//! ```

mod encode;
mod primitives;
mod server;

#[cfg(test)]
mod tests;

pub use server::{bind, bind_exposition, serve, Server};

use encode::{Encoder, Kind};
use primitives::{Counter, Gauge, Histogram, LabeledCounter};
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Upper bounds (seconds) of `flush_duration_seconds`. A flush is one big
/// multi-table insert whose retries back off for minutes.
const FLUSH_BUCKETS: &[f64] = &[
    0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
    120.0, 300.0,
];

/// Upper bounds (seconds) of `purge_duration_seconds`: tombstone inserts
/// (`INSERT .. SELECT .. FINAL`) over every block scoped table plus the
/// bucket repair of the aggregates. Nothing is ever deleted.
const PURGE_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0];

/// The Solana-only series (`indexer run --chain solana`), as plain
/// numbers. Everything an EVM chain also has - head, indexed height, lag
/// in heights AND in seconds, rows per table, flush latency - comes from
/// the shared gauges above and is NOT repeated here.
///
/// **Lag is published in seconds as well as in heights, and the seconds
/// are the number to look at.** A Solana slot is 0.27 s and an Ethereum
/// block is 12 s, so one `lag_blocks` panel across both families is
/// meaningless and would be read wrong on the first bad day
/// (docs/solana-research.md §11.5). `lag_seconds` already exists for every
/// chain; the Solana loop feeds it from the `block_time` of the last
/// committed slot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SolanaStats {
    /// Metered HyperSync queries sent in the last 60 seconds. The free
    /// budget is 30 per 60 s per endpoint and the follower aims at 25, so
    /// this is the number that says how much room is left.
    pub queries_last_minute: u64,
    /// Metered queries since the process started.
    pub queries_total: u64,
    /// Requests the server's own `x-ratelimit-*` headers last said were
    /// left in the window (`remaining / cost`, because `remaining` counts
    /// budget units). `None` until a response carried them.
    pub ratelimit_requests_left: Option<u64>,
    /// Swaps decoded and handed to the writer since the process started.
    pub swaps_stored: u64,
    /// Slots inside served windows that produced no block. NORMAL on
    /// Solana; worth watching because every rows/day estimate assumes the
    /// rate stays near zero.
    pub skipped_slots: u64,
}

/// State of a background resolver (token metadata, DEX pools), as plain
/// numbers. The pipeline maps the workers' own statistics into this, so
/// neither side depends on the other.
///
/// `queue_depth`, `breaker_open`, `endpoints_total` and
/// `endpoints_healthy` are the current state; everything else is a total
/// since the worker started (exposed as counters).
///
/// **`resolved` and `negative` are DISJOINT here**: `resolved` counts
/// answers WITH metadata only, `negative` the "checked, nothing there"
/// ones, so `resolved + negative` is the number of addresses answered.
/// (`tokens::TokenWorkerStats::resolved` INCLUDES the negative rows: the
/// pipeline subtracts them when it maps the stats, see
/// `pipeline::workers`. `dex::PoolWorkerStats` is already disjoint.)
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStatsSnapshot {
    /// Addresses waiting to be resolved.
    pub queue_depth: u64,
    /// Resolved WITH metadata (negatives not included).
    pub resolved: u64,
    /// Resolved to "nothing there" (reverts, garbage).
    pub negative: u64,
    /// No contract code at the address (asked again later, no row).
    pub codeless: u64,
    /// Discoveries dropped because the queue was full. Not a loss by
    /// itself: the database backfill finds them again.
    pub dropped: u64,
    /// Rows durably inserted by the worker.
    pub inserted: u64,
    /// Batches the worker could not store even after its retries. The
    /// rows are found again by the backfill; a growing number means the
    /// database rejects them.
    pub insert_failures: u64,
    /// Addresses the RPC could not be asked about (tried again later).
    pub rpc_failures: u64,
    /// Addresses the database backfill reported as missing: what the live
    /// path lost (drops, RPC outages, restarts) and the backfill healed.
    pub backfill_found: u64,
    /// Backfill queries that failed.
    pub backfill_failures: u64,
    /// Answers of an untrusted (public) endpoint no second provider
    /// confirmed: nothing was stored, asked again later.
    pub unconfirmed: u64,
    /// Blank rows verified again / replaced by real metadata: a stale
    /// node said "nothing there" about a good token.
    pub blank_rechecked: u64,
    pub blank_healed: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// True when every RPC endpoint's circuit breaker is open.
    pub breaker_open: bool,
    /// RPC endpoints configured or discovered.
    pub endpoints_total: u64,
    /// RPC endpoints currently considered healthy.
    pub endpoints_healthy: u64,
    /// RPC endpoints caught giving answers the others contradict.
    pub endpoints_distrusted: u64,
}

/// What [`Metrics::snapshot`] hands the control panel: the live numbers of
/// one chain, already measured for `/metrics`. `None` means "never set",
/// which the panel prints as a dash rather than as a zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub head_block: Option<u64>,
    pub indexed_block: Option<u64>,
    pub head_timestamp: Option<u64>,
    pub indexed_timestamp: Option<u64>,
    /// Duration of the most recent flush.
    pub last_flush_ms: Option<u64>,
    /// Unix milliseconds of the most recent SUCCESSFUL flush.
    pub last_flush_ok_unix_ms: Option<u64>,
    pub flushes_ok: u64,
    pub flushes_failed: u64,
    pub reorgs: u64,
    /// Deepest reorg since the process started; 0 = none.
    pub reorg_max_depth: u64,
    /// Addresses waiting in each background resolver, when it reported.
    pub token_queue: Option<u64>,
    pub pool_queue: Option<u64>,
    pub venue_queue: Option<u64>,
    /// Solana only.
    pub solana: Option<SolanaStats>,
}

/// Argument of [`Metrics::set_token_stats`].
pub type TokenStatsSnapshot = WorkerStatsSnapshot;
/// Argument of [`Metrics::set_pool_stats`].
pub type PoolStatsSnapshot = WorkerStatsSnapshot;
/// Argument of [`Metrics::set_venue_stats`].
pub type VenueStatsSnapshot = WorkerStatsSnapshot;

/// What the HTTP endpoint asks for. One chain's [`Metrics`] implements it,
/// and so does a whole fleet (`fleet::metrics`), which is the only reason
/// it exists: `indexer fleet` serves ONE `/metrics` covering every chain
/// (docs/design.md section 15) and `indexer run`'s output is unchanged.
pub trait Exposition: Send + Sync + 'static {
    /// Prometheus text exposition (format 0.0.4).
    fn render(&self) -> String;

    /// What `/readyz` answers; the string is the reason it is not ready.
    fn readiness(&self) -> Result<(), String>;
}

impl Exposition for Metrics {
    fn render(&self) -> String {
        Metrics::render(self)
    }

    fn readiness(&self) -> Result<(), String> {
        Metrics::readiness(self)
    }
}

/// Handle to the indexer's metrics. Cloning is one `Arc` increment; every
/// recording method is a handful of atomic operations and never blocks,
/// allocates (after a label's first use) or fails. A
/// [`disabled`](Self::disabled) handle costs one branch per call.
#[derive(Clone, Default)]
pub struct Metrics {
    inner: Option<Arc<Inner>>,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Some(inner) => write!(f, "Metrics(chain {})", inner.chain),
            None => f.write_str("Metrics(disabled)"),
        }
    }
}

struct BuildInfo {
    version: String,
    commit: String,
}

#[derive(Default)]
struct WorkerStats {
    /// Absent from the exposition until the first snapshot arrives, so a
    /// disabled worker (`--no-dex`, `--rpc none`) has no series.
    seen: AtomicBool,
    queue_depth: AtomicU64,
    resolved: Counter,
    negative: Counter,
    codeless: Counter,
    dropped: Counter,
    inserted: Counter,
    insert_failures: Counter,
    rpc_failures: Counter,
    backfill_found: Counter,
    backfill_failures: Counter,
    unconfirmed: Counter,
    blank_rechecked: Counter,
    blank_healed: Counter,
    cache_hits: Counter,
    cache_misses: Counter,
    breaker_open: AtomicBool,
    endpoints_total: AtomicU64,
    endpoints_healthy: AtomicU64,
    endpoints_distrusted: AtomicU64,
}

impl WorkerStats {
    fn set(&self, stats: WorkerStatsSnapshot) {
        self.queue_depth.store(stats.queue_depth, Relaxed);
        self.resolved.set(stats.resolved);
        self.negative.set(stats.negative);
        self.codeless.set(stats.codeless);
        self.dropped.set(stats.dropped);
        self.inserted.set(stats.inserted);
        self.insert_failures.set(stats.insert_failures);
        self.rpc_failures.set(stats.rpc_failures);
        self.backfill_found.set(stats.backfill_found);
        self.backfill_failures.set(stats.backfill_failures);
        self.unconfirmed.set(stats.unconfirmed);
        self.blank_rechecked.set(stats.blank_rechecked);
        self.blank_healed.set(stats.blank_healed);
        self.cache_hits.set(stats.cache_hits);
        self.cache_misses.set(stats.cache_misses);
        self.breaker_open.store(stats.breaker_open, Relaxed);
        self.endpoints_total.store(stats.endpoints_total, Relaxed);
        self.endpoints_healthy.store(stats.endpoints_healthy, Relaxed);
        self.endpoints_distrusted
            .store(stats.endpoints_distrusted, Relaxed);
        self.seen.store(true, Relaxed);
    }

    fn get(&self) -> Option<WorkerStatsSnapshot> {
        self.seen.load(Relaxed).then(|| WorkerStatsSnapshot {
            queue_depth: self.queue_depth.load(Relaxed),
            resolved: self.resolved.get(),
            negative: self.negative.get(),
            codeless: self.codeless.get(),
            dropped: self.dropped.get(),
            inserted: self.inserted.get(),
            insert_failures: self.insert_failures.get(),
            rpc_failures: self.rpc_failures.get(),
            backfill_found: self.backfill_found.get(),
            backfill_failures: self.backfill_failures.get(),
            unconfirmed: self.unconfirmed.get(),
            blank_rechecked: self.blank_rechecked.get(),
            blank_healed: self.blank_healed.get(),
            cache_hits: self.cache_hits.get(),
            cache_misses: self.cache_misses.get(),
            breaker_open: self.breaker_open.load(Relaxed),
            endpoints_total: self.endpoints_total.load(Relaxed),
            endpoints_healthy: self.endpoints_healthy.load(Relaxed),
            endpoints_distrusted: self.endpoints_distrusted.load(Relaxed),
        })
    }
}

struct Inner {
    chain: String,
    ready_staleness: Duration,
    start_time_secs: u64,
    build_info: Mutex<BuildInfo>,

    ready: AtomicBool,
    /// Unix ms of the last `set_head` / successful flush; 0 = never.
    last_head_poll_ms: AtomicU64,
    last_flush_ok_ms: AtomicU64,
    /// Unix ms the flush that is running right now started; 0 = none.
    flush_started_ms: AtomicU64,
    /// The most recent flush attempt failed.
    last_flush_failed: AtomicBool,

    head_block: Gauge,
    indexed_block: Gauge,
    head_timestamp: Gauge,
    indexed_timestamp: Gauge,

    rows_inserted: LabeledCounter,
    flushes_ok: Counter,
    flushes_failed: Counter,
    flush_duration: Histogram,
    last_flush_rows: Gauge,
    flush_retries: LabeledCounter,

    channel_len: Gauge,
    channel_capacity: Gauge,
    stream_errors: Counter,

    reorgs: Counter,
    reorg_blocks: Counter,
    reorg_last_depth: Gauge,
    /// Deepest reorg since the process started. The control panel shows it
    /// next to the count, and a histogram cannot answer "how bad did it
    /// get" without guessing inside a bucket.
    reorg_max_depth: Gauge,
    /// Milliseconds the most recent flush took. The histogram answers the
    /// distribution; the panel wants the last one, in plain numbers.
    last_flush_ms: Gauge,
    purge_duration: Histogram,
    purged_blocks: Counter,

    tokens: WorkerStats,
    pools: WorkerStats,
    venues: WorkerStats,

    /// `indexer run --chain solana` only; absent from the exposition
    /// until the Solana loop reports once.
    solana: Mutex<Option<SolanaStats>>,
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl Metrics {
    /// Enabled handle. `chain_id` becomes the constant `chain` label.
    /// `/readyz` reports not-ready when neither a successful flush nor a
    /// head poll happened within `ready_staleness`.
    pub fn new(chain_id: u64, ready_staleness: Duration) -> Self {
        Self::with_start_time(
            chain_id.to_string(),
            ready_staleness,
            unix_ms() / 1000,
        )
    }

    fn with_start_time(
        chain: String,
        ready_staleness: Duration,
        start_time_secs: u64,
    ) -> Self {
        Self {
            inner: Some(Arc::new(Inner {
                chain,
                ready_staleness,
                start_time_secs,
                build_info: Mutex::new(BuildInfo {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    commit: "unknown".to_string(),
                }),
                ready: AtomicBool::new(false),
                last_head_poll_ms: AtomicU64::new(0),
                last_flush_ok_ms: AtomicU64::new(0),
                flush_started_ms: AtomicU64::new(0),
                last_flush_failed: AtomicBool::new(false),
                head_block: Gauge::default(),
                indexed_block: Gauge::default(),
                head_timestamp: Gauge::default(),
                indexed_timestamp: Gauge::default(),
                rows_inserted: LabeledCounter::default(),
                flushes_ok: Counter::default(),
                flushes_failed: Counter::default(),
                flush_duration: Histogram::new(FLUSH_BUCKETS),
                last_flush_rows: Gauge::default(),
                flush_retries: LabeledCounter::default(),
                channel_len: Gauge::default(),
                channel_capacity: Gauge::default(),
                stream_errors: Counter::default(),
                reorgs: Counter::default(),
                reorg_blocks: Counter::default(),
                reorg_last_depth: Gauge::default(),
                reorg_max_depth: Gauge::default(),
                last_flush_ms: Gauge::default(),
                purge_duration: Histogram::new(PURGE_BUCKETS),
                purged_blocks: Counter::default(),
                tokens: WorkerStats::default(),
                pools: WorkerStats::default(),
                venues: WorkerStats::default(),
                solana: Mutex::new(None),
            })),
        }
    }

    /// Handle that records nothing (`--metrics-addr` not given). Same as
    /// `Metrics::default()`.
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Labels of the `build_info` series. The version defaults to the
    /// crate version and the commit to `unknown`.
    pub fn build_info(&self, version: &str, commit: &str) {
        let Some(inner) = &self.inner else { return };

        let mut info =
            inner.build_info.lock().unwrap_or_else(|e| e.into_inner());
        info.version = version.to_string();
        info.commit = commit.to_string();
    }

    /// Chain head as reported by the source. Also counts as a sign of
    /// life for `/readyz`: call it on every successful head poll, even
    /// when the head did not move.
    pub fn set_head(&self, block: u64) {
        self.set_head_at(block, unix_ms());
    }

    fn set_head_at(&self, block: u64, now_ms: u64) {
        let Some(inner) = &self.inner else { return };

        inner.head_block.set(block);
        inner.last_head_poll_ms.store(now_ms, Relaxed);
    }

    /// Highest block durably stored. May move backwards after a reorg.
    pub fn set_indexed_height(&self, block: u64) {
        let Some(inner) = &self.inner else { return };

        inner.indexed_block.set(block);
    }

    /// Timestamp (unix seconds) of the head block, when the source knows
    /// it. Without it `lag_seconds` is measured against the wall clock.
    pub fn set_head_timestamp(&self, unix_seconds: u64) {
        let Some(inner) = &self.inner else { return };

        inner.head_timestamp.set(unix_seconds);
    }

    /// Timestamp (unix seconds) of the highest block durably stored.
    pub fn set_indexed_timestamp(&self, unix_seconds: u64) {
        let Some(inner) = &self.inner else { return };

        inner.indexed_timestamp.set(unix_seconds);
    }

    /// `n` rows were durably inserted into `table`.
    pub fn rows_inserted(&self, table: &'static str, n: u64) {
        let Some(inner) = &self.inner else { return };

        inner.rows_inserted.add(table, n);
    }

    /// A flush is starting (its insert retries can take minutes). Paired
    /// with [`Self::flush_observed`]; lets `/readyz` see a flush that is
    /// stuck retrying.
    pub fn flush_started(&self) {
        self.flush_started_at(unix_ms());
    }

    fn flush_started_at(&self, now_ms: u64) {
        let Some(inner) = &self.inner else { return };

        inner.flush_started_ms.store(now_ms.max(1), Relaxed);
    }

    /// One flush (all tables of a batch) finished after `duration`,
    /// retries included. Failed flushes are observed too.
    pub fn flush_observed(&self, duration: Duration, rows: u64, ok: bool) {
        self.flush_observed_at(duration, rows, ok, unix_ms());
    }

    fn flush_observed_at(
        &self,
        duration: Duration,
        rows: u64,
        ok: bool,
        now_ms: u64,
    ) {
        let Some(inner) = &self.inner else { return };

        inner.flush_duration.observe(duration);
        inner.last_flush_rows.set(rows);
        inner
            .last_flush_ms
            .set(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
        inner.flush_started_ms.store(0, Relaxed);
        inner.last_flush_failed.store(!ok, Relaxed);

        if ok {
            inner.flushes_ok.add(1);
            inner.last_flush_ok_ms.store(now_ms, Relaxed);
        } else {
            inner.flushes_failed.add(1);
        }
    }

    /// An insert into `table` failed and is being retried.
    pub fn flush_retry(&self, table: &'static str) {
        let Some(inner) = &self.inner else { return };

        inner.flush_retries.add(table, 1);
    }

    /// Batches queued between the transformer and the writer.
    pub fn channel_fill(&self, len: usize, capacity: usize) {
        let Some(inner) = &self.inner else { return };

        inner.channel_len.set(len as u64);
        inner.channel_capacity.set(capacity as u64);
    }

    /// A reorg was detected; `depth` = blocks between the fork point and
    /// the old head.
    pub fn reorg(&self, depth: u64) {
        let Some(inner) = &self.inner else { return };

        inner.reorgs.add(1);
        inner.reorg_blocks.add(depth);
        inner.reorg_last_depth.set(depth);
        if inner.reorg_max_depth.get().is_none_or(|max| depth > max) {
            inner.reorg_max_depth.set(depth);
        }
    }

    /// A `purge_range` (reorg rollback or gap healing) finished.
    pub fn purge_observed(&self, duration: Duration, blocks: u64) {
        let Some(inner) = &self.inner else { return };

        inner.purge_duration.observe(duration);
        inner.purged_blocks.add(blocks);
    }

    /// A sync pass failed (stream error) and will be retried.
    pub fn stream_error(&self) {
        let Some(inner) = &self.inner else { return };

        inner.stream_errors.add(1);
    }

    pub fn set_token_stats(&self, stats: TokenStatsSnapshot) {
        let Some(inner) = &self.inner else { return };

        inner.tokens.set(stats);
    }

    pub fn set_pool_stats(&self, stats: PoolStatsSnapshot) {
        let Some(inner) = &self.inner else { return };

        inner.pools.set(stats);
    }

    /// Prediction market venue resolver.
    pub fn set_venue_stats(&self, stats: VenueStatsSnapshot) {
        let Some(inner) = &self.inner else { return };

        inner.venues.set(stats);
    }

    /// The Solana-only series. Called by `pipeline::solana` once per loop
    /// turn; ignored on every other chain, which is why the series are
    /// absent from the exposition rather than zero there.
    pub fn set_solana_stats(&self, stats: SolanaStats) {
        let Some(inner) = &self.inner else { return };

        *inner.solana.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(stats);
    }

    /// Startup (migrations, gap healing, first head poll) is complete.
    /// Necessary for `/readyz`, not sufficient: see [`Self::readiness`].
    pub fn set_ready(&self, ready: bool) {
        let Some(inner) = &self.inner else { return };

        inner.ready.store(ready, Relaxed);
    }

    /// What `/readyz` answers, a one-line reason when not ready. Ready
    /// means ALL of:
    ///
    /// * the ready flag is set (startup completed);
    /// * the most recent flush attempt did not fail;
    /// * no flush has been running (retrying) for longer than
    ///   `ready_staleness`;
    /// * the last successful flush or head poll is at most
    ///   `ready_staleness` old.
    ///
    /// It answers "is this indexer serving fresh data", for load balancers
    /// and dashboards. It is NOT a liveness probe: do not restart the
    /// process on it (a ClickHouse outage makes it not-ready, and a
    /// restart loop would not help). Use `/healthz` for liveness.
    pub fn readiness(&self) -> Result<(), String> {
        self.readiness_at(unix_ms())
    }

    fn readiness_at(&self, now_ms: u64) -> Result<(), String> {
        let Some(inner) = &self.inner else {
            return Err("not ready: metrics are disabled".to_string());
        };

        if !inner.ready.load(Relaxed) {
            return Err("not ready: startup has not completed".to_string());
        }

        let limit_ms = inner.ready_staleness.as_millis();

        if inner.last_flush_failed.load(Relaxed) {
            return Err(
                "not ready: the most recent flush failed".to_string()
            );
        }

        let flush_started_ms = inner.flush_started_ms.load(Relaxed);
        if flush_started_ms != 0 {
            let running_ms = now_ms.saturating_sub(flush_started_ms);
            if u128::from(running_ms) > limit_ms {
                return Err(format!(
                    "not ready: a flush has been retrying for {}s (limit \
                     {}s)",
                    running_ms / 1000,
                    inner.ready_staleness.as_secs()
                ));
            }
        }

        let last_sign_of_life = inner
            .last_flush_ok_ms
            .load(Relaxed)
            .max(inner.last_head_poll_ms.load(Relaxed));

        if last_sign_of_life == 0 {
            return Err("not ready: no successful flush or head poll yet"
                .to_string());
        }

        let age_ms = now_ms.saturating_sub(last_sign_of_life);

        if u128::from(age_ms) > limit_ms {
            return Err(format!(
                "not ready: last successful flush or head poll was {}s ago \
                 (limit {}s)",
                age_ms / 1000,
                inner.ready_staleness.as_secs()
            ));
        }

        Ok(())
    }

    /// The numbers the control panel prints, read from what the pipeline
    /// already records. `None` for a disabled handle.
    ///
    /// This is the reason fleet mode needed almost no new instrumentation:
    /// everything below was already being measured for `/metrics`, so the
    /// panel reads the same atomics instead of adding a second counter to
    /// the hot path (docs/design.md section 15, `pipeline::status`).
    pub fn snapshot(&self) -> Option<Snapshot> {
        let inner = self.inner.as_ref()?;

        Some(Snapshot {
            head_block: inner.head_block.get(),
            indexed_block: inner.indexed_block.get(),
            head_timestamp: inner.head_timestamp.get(),
            indexed_timestamp: inner.indexed_timestamp.get(),
            last_flush_ms: inner.last_flush_ms.get(),
            last_flush_ok_unix_ms: match inner
                .last_flush_ok_ms
                .load(Relaxed)
            {
                0 => None,
                ms => Some(ms),
            },
            flushes_ok: inner.flushes_ok.get(),
            flushes_failed: inner.flushes_failed.get(),
            reorgs: inner.reorgs.get(),
            reorg_max_depth: inner.reorg_max_depth.get().unwrap_or(0),
            token_queue: inner.tokens.get().map(|s| s.queue_depth),
            pool_queue: inner.pools.get().map(|s| s.queue_depth),
            venue_queue: inner.venues.get().map(|s| s.queue_depth),
            solana: *inner
                .solana
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        })
    }

    /// Prometheus text exposition (format 0.0.4) of the current state.
    /// Empty for a disabled handle.
    pub fn render(&self) -> String {
        self.render_at(unix_ms())
    }

    fn render_at(&self, now_ms: u64) -> String {
        let Some(inner) = &self.inner else {
            return String::new();
        };

        let mut e = Encoder::new(&inner.chain);

        e.family(
            "build_info",
            "Build information; the value is always 1.",
            Kind::Gauge,
        );
        {
            let info =
                inner.build_info.lock().unwrap_or_else(|e| e.into_inner());
            e.sample(
                "build_info",
                &[("version", &info.version), ("commit", &info.commit)],
                1,
            );
        }

        e.scalar(
            "start_time_seconds",
            "Unix time the process started.",
            Kind::Gauge,
            inner.start_time_secs,
        );
        e.scalar(
            "ready",
            "1 when /readyz answers 200, else 0.",
            Kind::Gauge,
            u8::from(self.readiness_at(now_ms).is_ok()),
        );

        let head = inner.head_block.get();
        let indexed = inner.indexed_block.get();

        if let Some(head) = head {
            e.scalar(
                "head_block",
                "Chain head reported by the source.",
                Kind::Gauge,
                head,
            );
        }
        if let Some(indexed) = indexed {
            e.scalar(
                "indexed_block",
                "Highest block durably stored.",
                Kind::Gauge,
                indexed,
            );
        }
        if let (Some(head), Some(indexed)) = (head, indexed) {
            e.scalar(
                "lag_blocks",
                "Blocks between the chain head and the indexed block.",
                Kind::Gauge,
                head.saturating_sub(indexed),
            );
        }

        let head_ts = inner.head_timestamp.get();
        let indexed_ts = inner.indexed_timestamp.get();

        if let Some(head_ts) = head_ts {
            e.scalar(
                "head_timestamp_seconds",
                "Timestamp of the chain head block.",
                Kind::Gauge,
                head_ts,
            );
        }
        if let Some(indexed_ts) = indexed_ts {
            e.scalar(
                "indexed_timestamp_seconds",
                "Timestamp of the highest block durably stored.",
                Kind::Gauge,
                indexed_ts,
            );
            e.scalar(
                "lag_seconds",
                "Age of the indexed block relative to the head block, or \
                 to the wall clock when the head timestamp is unknown.",
                Kind::Gauge,
                head_ts
                    .unwrap_or(now_ms / 1000)
                    .saturating_sub(indexed_ts),
            );
        }

        let rows = inner.rows_inserted.snapshot();
        if !rows.is_empty() {
            e.family(
                "rows_inserted_total",
                "Rows durably inserted, per table.",
                Kind::Counter,
            );
            for (table, n) in rows {
                e.sample("rows_inserted_total", &[("table", table)], n);
            }
        }

        e.family(
            "flushes_total",
            "Flushes (one multi-table batch each), by result.",
            Kind::Counter,
        );
        e.sample(
            "flushes_total",
            &[("result", "ok")],
            inner.flushes_ok.get(),
        );
        e.sample(
            "flushes_total",
            &[("result", "error")],
            inner.flushes_failed.get(),
        );

        e.histogram(
            "flush_duration_seconds",
            "Duration of a flush, retries included.",
            &inner.flush_duration,
        );

        if let Some(rows) = inner.last_flush_rows.get() {
            e.scalar(
                "last_flush_rows",
                "Rows in the most recent flush.",
                Kind::Gauge,
                rows,
            );
        }

        let last_flush_ok_ms = inner.last_flush_ok_ms.load(Relaxed);
        if last_flush_ok_ms != 0 {
            e.scalar(
                "last_successful_flush_timestamp_seconds",
                "Unix time of the most recent successful flush.",
                Kind::Gauge,
                last_flush_ok_ms / 1000,
            );
        }

        let retries = inner.flush_retries.snapshot();
        if !retries.is_empty() {
            e.family(
                "flush_retries_total",
                "Inserts that failed and were retried, per table.",
                Kind::Counter,
            );
            for (table, n) in retries {
                e.sample("flush_retries_total", &[("table", table)], n);
            }
        }

        if let (Some(len), Some(capacity)) =
            (inner.channel_len.get(), inner.channel_capacity.get())
        {
            e.scalar(
                "channel_len",
                "Batches queued between the transformer and the writer.",
                Kind::Gauge,
                len,
            );
            e.scalar(
                "channel_capacity",
                "Capacity of the transformer to writer channel.",
                Kind::Gauge,
                capacity,
            );
        }

        e.scalar(
            "stream_errors_total",
            "Sync passes that failed and were retried.",
            Kind::Counter,
            inner.stream_errors.get(),
        );

        e.scalar(
            "reorgs_total",
            "Chain reorganizations detected.",
            Kind::Counter,
            inner.reorgs.get(),
        );
        e.scalar(
            "reorg_blocks_total",
            "Sum of the depths of all detected reorganizations.",
            Kind::Counter,
            inner.reorg_blocks.get(),
        );
        if let Some(depth) = inner.reorg_last_depth.get() {
            e.scalar(
                "reorg_last_depth",
                "Depth in blocks of the most recent reorganization.",
                Kind::Gauge,
                depth,
            );
        }
        // `reorg_max_depth` and `last_flush_ms` are recorded but NOT
        // exposed: the Prometheus output of `indexer run` is unchanged by
        // fleet mode, and both numbers are already answerable from the
        // series above (`reorg_last_depth` over time,
        // `flush_duration_seconds`). They exist for the control panel,
        // which shows one number rather than a query
        // ([`Metrics::snapshot`]).

        e.histogram(
            "purge_duration_seconds",
            "Duration of purge_range (reorg rollback or gap healing).",
            &inner.purge_duration,
        );
        e.scalar(
            "purged_blocks_total",
            "Blocks removed by purge_range.",
            Kind::Counter,
            inner.purged_blocks.get(),
        );

        render_workers(&mut e, inner);
        render_solana(&mut e, inner);

        e.finish()
    }
}

/// The Solana-only series, absent until the Solana loop reported once.
fn render_solana(e: &mut Encoder, inner: &Inner) {
    let Some(stats) =
        *inner.solana.lock().unwrap_or_else(|e| e.into_inner())
    else {
        return;
    };

    e.scalar(
        "hypersync_queries_last_minute",
        "Metered HyperSync queries sent in the last 60 seconds. The free \
         Solana budget is 30 per 60 seconds per endpoint.",
        Kind::Gauge,
        stats.queries_last_minute,
    );
    e.scalar(
        "hypersync_queries_total",
        "Metered HyperSync queries sent since the process started.",
        Kind::Counter,
        stats.queries_total,
    );
    if let Some(left) = stats.ratelimit_requests_left {
        e.scalar(
            "hypersync_ratelimit_requests_left",
            "Requests the server's x-ratelimit headers said were left in \
             the current window (remaining budget units divided by cost).",
            Kind::Gauge,
            left,
        );
    }
    e.scalar(
        "solana_swaps_total",
        "Swaps decoded and handed to the writer.",
        Kind::Counter,
        stats.swaps_stored,
    );
    e.scalar(
        "solana_skipped_slots_total",
        "Slots inside served windows that produced no block. Normal on \
         Solana; every rows-per-day estimate assumes it stays near zero.",
        Kind::Counter,
        stats.skipped_slots,
    );
}

/// One family per field, one series per worker that reported.
fn render_workers(e: &mut Encoder, inner: &Inner) {
    let workers: Vec<(&str, WorkerStatsSnapshot)> = [
        ("tokens", &inner.tokens),
        ("pools", &inner.pools),
        ("venues", &inner.venues),
    ]
    .into_iter()
    .filter_map(|(name, stats)| Some((name, stats.get()?)))
    .collect();

    if workers.is_empty() {
        return;
    }

    type Field = fn(&WorkerStatsSnapshot) -> u64;

    let families: [(&str, &str, Kind, Field); 19] = [
        (
            "resolver_queue_depth",
            "Addresses waiting to be resolved by a background worker.",
            Kind::Gauge,
            |s| s.queue_depth,
        ),
        (
            "resolver_resolved_total",
            "Addresses resolved WITH metadata (negatives not included).",
            Kind::Counter,
            |s| s.resolved,
        ),
        (
            "resolver_negative_total",
            "Addresses resolved to nothing (reverts, garbage).",
            Kind::Counter,
            |s| s.negative,
        ),
        (
            "resolver_codeless_total",
            "Addresses without contract code (asked again later).",
            Kind::Counter,
            |s| s.codeless,
        ),
        (
            "resolver_dropped_total",
            "Discoveries dropped because the queue was full.",
            Kind::Counter,
            |s| s.dropped,
        ),
        (
            "resolver_inserted_total",
            "Rows durably inserted by the worker.",
            Kind::Counter,
            |s| s.inserted,
        ),
        (
            "resolver_insert_failures_total",
            "Batches the worker could not store after its retries.",
            Kind::Counter,
            |s| s.insert_failures,
        ),
        (
            "resolver_rpc_failures_total",
            "Addresses the RPC could not be asked about.",
            Kind::Counter,
            |s| s.rpc_failures,
        ),
        (
            "resolver_backfill_found_total",
            "Addresses the database backfill reported as missing.",
            Kind::Counter,
            |s| s.backfill_found,
        ),
        (
            "resolver_backfill_failures_total",
            "Backfill queries that failed.",
            Kind::Counter,
            |s| s.backfill_failures,
        ),
        (
            "resolver_unconfirmed_total",
            "Answers of a public endpoint no second provider confirmed.",
            Kind::Counter,
            |s| s.unconfirmed,
        ),
        (
            "resolver_blank_rechecked_total",
            "Blank rows that were verified again.",
            Kind::Counter,
            |s| s.blank_rechecked,
        ),
        (
            "resolver_blank_healed_total",
            "Blank rows replaced by real metadata on a recheck.",
            Kind::Counter,
            |s| s.blank_healed,
        ),
        (
            "resolver_cache_hits_total",
            "Resolver cache hits.",
            Kind::Counter,
            |s| s.cache_hits,
        ),
        (
            "resolver_cache_misses_total",
            "Resolver cache misses.",
            Kind::Counter,
            |s| s.cache_misses,
        ),
        (
            "resolver_breaker_open",
            "1 when every RPC endpoint's circuit breaker is open.",
            Kind::Gauge,
            |s| u64::from(s.breaker_open),
        ),
        (
            "resolver_endpoints_total",
            "RPC endpoints configured or discovered.",
            Kind::Gauge,
            |s| s.endpoints_total,
        ),
        (
            "resolver_endpoints_healthy",
            "RPC endpoints currently considered healthy.",
            Kind::Gauge,
            |s| s.endpoints_healthy,
        ),
        (
            "resolver_endpoints_distrusted",
            "RPC endpoints caught contradicting the others.",
            Kind::Gauge,
            |s| s.endpoints_distrusted,
        ),
    ];

    for (name, help, kind, field) in families {
        e.family(name, help, kind);
        for (worker, stats) in &workers {
            e.sample(name, &[("worker", worker)], field(stats));
        }
    }
}
