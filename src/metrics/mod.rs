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

pub use server::{bind, serve, Server};

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

/// Upper bounds (seconds) of `purge_duration_seconds`: synchronous
/// lightweight deletes over every table plus the bucket repair.
const PURGE_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0];

/// State of a background resolver (token metadata, DEX pools), as plain
/// numbers. The pipeline maps the workers' own statistics into this, so
/// neither side depends on the other.
///
/// `resolved`, `negative`, `dropped`, `cache_hits` and `cache_misses` are
/// totals since the worker started (exposed as counters); `queue_depth`,
/// `breaker_open` and `endpoints_healthy` are the current state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkerStatsSnapshot {
    /// Addresses waiting to be resolved.
    pub queue_depth: u64,
    /// Resolved with metadata.
    pub resolved: u64,
    /// Resolved to "nothing there" (reverts, garbage).
    pub negative: u64,
    /// Discoveries dropped because the queue was full.
    pub dropped: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// True when every RPC endpoint's circuit breaker is open.
    pub breaker_open: bool,
    /// RPC endpoints currently considered healthy.
    pub endpoints_healthy: u64,
}

/// Argument of [`Metrics::set_token_stats`].
pub type TokenStatsSnapshot = WorkerStatsSnapshot;
/// Argument of [`Metrics::set_pool_stats`].
pub type PoolStatsSnapshot = WorkerStatsSnapshot;

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
    /// disabled worker (no `--dex`) has no series.
    seen: AtomicBool,
    queue_depth: AtomicU64,
    resolved: Counter,
    negative: Counter,
    dropped: Counter,
    cache_hits: Counter,
    cache_misses: Counter,
    breaker_open: AtomicBool,
    endpoints_healthy: AtomicU64,
}

impl WorkerStats {
    fn set(&self, stats: WorkerStatsSnapshot) {
        self.queue_depth.store(stats.queue_depth, Relaxed);
        self.resolved.set(stats.resolved);
        self.negative.set(stats.negative);
        self.dropped.set(stats.dropped);
        self.cache_hits.set(stats.cache_hits);
        self.cache_misses.set(stats.cache_misses);
        self.breaker_open.store(stats.breaker_open, Relaxed);
        self.endpoints_healthy.store(stats.endpoints_healthy, Relaxed);
        self.seen.store(true, Relaxed);
    }

    fn get(&self) -> Option<WorkerStatsSnapshot> {
        self.seen.load(Relaxed).then(|| WorkerStatsSnapshot {
            queue_depth: self.queue_depth.load(Relaxed),
            resolved: self.resolved.get(),
            negative: self.negative.get(),
            dropped: self.dropped.get(),
            cache_hits: self.cache_hits.get(),
            cache_misses: self.cache_misses.get(),
            breaker_open: self.breaker_open.load(Relaxed),
            endpoints_healthy: self.endpoints_healthy.load(Relaxed),
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
    purge_duration: Histogram,
    purged_blocks: Counter,

    tokens: WorkerStats,
    pools: WorkerStats,
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
                purge_duration: Histogram::new(PURGE_BUCKETS),
                purged_blocks: Counter::default(),
                tokens: WorkerStats::default(),
                pools: WorkerStats::default(),
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

    /// Startup (migrations, gap healing, first head poll) is complete.
    /// Necessary for `/readyz`, not sufficient: see [`Self::readiness`].
    pub fn set_ready(&self, ready: bool) {
        let Some(inner) = &self.inner else { return };

        inner.ready.store(ready, Relaxed);
    }

    /// What `/readyz` answers: `Ok` when the ready flag is set and the
    /// last successful flush or head poll is at most `ready_staleness`
    /// old, otherwise a one-line reason.
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

        let last_sign_of_life = inner
            .last_flush_ok_ms
            .load(Relaxed)
            .max(inner.last_head_poll_ms.load(Relaxed));

        if last_sign_of_life == 0 {
            return Err("not ready: no successful flush or head poll yet"
                .to_string());
        }

        let age_ms = now_ms.saturating_sub(last_sign_of_life);
        let limit_ms = inner.ready_staleness.as_millis();

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

        e.finish()
    }
}

/// One family per field, one series per worker that reported.
fn render_workers(e: &mut Encoder, inner: &Inner) {
    let workers: Vec<(&str, WorkerStatsSnapshot)> =
        [("tokens", &inner.tokens), ("pools", &inner.pools)]
            .into_iter()
            .filter_map(|(name, stats)| Some((name, stats.get()?)))
            .collect();

    if workers.is_empty() {
        return;
    }

    type Field = fn(&WorkerStatsSnapshot) -> u64;

    let families: [(&str, &str, Kind, Field); 8] = [
        (
            "resolver_queue_depth",
            "Addresses waiting to be resolved by a background worker.",
            Kind::Gauge,
            |s| s.queue_depth,
        ),
        (
            "resolver_resolved_total",
            "Addresses resolved with metadata.",
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
            "resolver_dropped_total",
            "Discoveries dropped because the queue was full.",
            Kind::Counter,
            |s| s.dropped,
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
            "resolver_endpoints_healthy",
            "RPC endpoints currently considered healthy.",
            Kind::Gauge,
            |s| s.endpoints_healthy,
        ),
    ];

    for (name, help, kind, field) in families {
        e.family(name, help, kind);
        for (worker, stats) in &workers {
            e.sample(name, &[("worker", worker)], field(stats));
        }
    }
}
