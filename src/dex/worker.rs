//! Background pool resolver. Never on the commit path: the pipeline only
//! calls [`PoolWorker::discover`] (sync, bounded, drop-on-full) and the
//! worker writes `dex_pools` rows itself through a [`PoolSink`].
//!
//! Dropping is safe because of the DB driven backfill: every
//! `backfill_interval` the [`MissingPoolSource`] reports pool ids present
//! in `dex_swaps` / `dex_liquidity` without a `dex_pools` row, so an RPC
//! outage of any length heals itself.
//!
//! Same surface as `tokens::TokenWorker` (`spawn` / `discover` / `stats` /
//! `shutdown`) so the pipeline treats both uniformly.

use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use alloy::primitives::B256;
use futures::{future::BoxFuture, stream, StreamExt};
use log::{debug, info, warn};
use lru::LruCache;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

use crate::tokens::multicall::EthCaller;

use super::{
    models::DexPool,
    resolve::{resolve_pool, unresolved_pool, Resolution},
    PoolCandidate,
};

/// Where resolved pools are written, and what is already there.
pub trait PoolSink: Send + Sync + 'static {
    /// The subset of `pool_ids` that already has a `dex_pools` row (of any
    /// source). Keeps the worker from asking the RPC about pools whose
    /// creation event was indexed long ago:
    /// `SELECT pool_id FROM dex_pools WHERE chain = ? AND pool_id IN ?`.
    fn known_pools<'a>(
        &'a self,
        pool_ids: &'a [B256],
    ) -> BoxFuture<'a, anyhow::Result<HashSet<B256>>>;

    /// Inserts into `dex_pools`. Must be idempotent (ReplacingMergeTree)
    /// and must keep the rows' `_version` untouched.
    fn insert_pools<'a>(
        &'a self,
        rows: &'a [DexPool],
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Pools that traded but have no `dex_pools` row
/// ([`super::MISSING_POOLS_SQL`]).
pub trait MissingPoolSource: Send + Sync + 'static {
    fn missing_pools<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PoolCandidate>>>;
}

#[derive(Debug, Clone)]
pub struct PoolWorkerOptions {
    /// Candidates waiting for the worker; `discover` drops above this.
    pub queue_capacity: usize,
    /// Candidates resolved (and inserted) together.
    pub batch_size: usize,
    /// How long a batch waits for more candidates.
    pub batch_linger: Duration,
    /// Pools resolved concurrently (each takes 2 to ~20 `eth_call`s).
    pub concurrency: usize,
    pub backfill_interval: Duration,
    /// The interval backs off to this while nothing is missing.
    pub backfill_max_interval: Duration,
    pub backfill_limit: usize,
    /// Pool ids remembered as stored (or definitely not a pool).
    pub known_capacity: usize,
    /// Addresses without code are not asked again for this long (memory
    /// only).
    pub no_answer_ttl: Duration,
    pub no_answer_capacity: usize,
    /// Consecutive batches without a single RPC answer before pausing.
    pub breaker_threshold: u32,
    pub breaker_cooldown: Duration,
    pub breaker_max_cooldown: Duration,
    pub shutdown_grace: Duration,
}

impl Default for PoolWorkerOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 50_000,
            batch_size: 200,
            batch_linger: Duration::from_millis(250),
            concurrency: 8,
            backfill_interval: Duration::from_secs(60),
            backfill_max_interval: Duration::from_secs(600),
            backfill_limit: 2_000,
            known_capacity: 500_000,
            no_answer_ttl: Duration::from_secs(1_800),
            no_answer_capacity: 50_000,
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(600),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// Counters are monotonic since startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolWorkerStats {
    /// `false` when no RPC is configured (the worker is inert).
    pub enabled: bool,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub queued: u64,
    pub dropped: u64,
    /// Skipped because `dex_pools` already had the pool.
    pub already_known: u64,
    pub resolved: u64,
    /// Definitely not a pool (`source = 'unresolved'` rows).
    pub negative: u64,
    /// No code at the address (not cached persistently).
    pub codeless: u64,
    pub inserted: u64,
    pub insert_failures: u64,
    pub rpc_failures: u64,
    pub backfill_runs: u64,
    pub backfill_found: u64,
    pub backfill_failures: u64,
    pub breaker_open: bool,
}

#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    dropped: AtomicU64,
    already_known: AtomicU64,
    resolved: AtomicU64,
    negative: AtomicU64,
    codeless: AtomicU64,
    inserted: AtomicU64,
    insert_failures: AtomicU64,
    rpc_failures: AtomicU64,
    backfill_runs: AtomicU64,
    backfill_found: AtomicU64,
    backfill_failures: AtomicU64,
}

fn bump(counter: &AtomicU64, by: usize) {
    counter.fetch_add(by as u64, Ordering::Relaxed);
}

struct Memory {
    /// Pools with a `dex_pools` row.
    known: LruCache<B256, ()>,
    /// Queued or being resolved.
    pending: HashSet<B256>,
    /// Addresses without code -> when they may be asked again.
    no_answer: LruCache<B256, Instant>,
}

struct Shared {
    chain_id: u64,
    options: PoolWorkerOptions,
    counters: Counters,
    memory: Mutex<Memory>,
    breaker_open: AtomicBool,
}

impl Shared {
    fn memory(&self) -> MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Cheap, clonable handle of the background resolver.
#[derive(Clone)]
pub struct PoolWorker {
    shared: Arc<Shared>,
    /// `None` when the worker is inert (no RPC).
    queue: Option<mpsc::Sender<PoolCandidate>>,
    stop: watch::Sender<bool>,
}

fn capacity(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value.max(1)).unwrap_or(NonZeroUsize::MIN)
}

impl PoolWorker {
    /// Starts the worker. Sync, no I/O; must run inside a tokio runtime.
    /// `caller = None` (no `--rpc`) yields an inert worker: `discover` is a
    /// no-op and the task just waits for `shutdown`.
    ///
    /// Share the `Arc<dyn EthCaller>` of the token worker: endpoint
    /// failover and chain id checks live behind it.
    pub fn spawn(
        chain_id: u64,
        caller: Option<Arc<dyn EthCaller>>,
        sink: Arc<dyn PoolSink>,
        backfill: Option<Arc<dyn MissingPoolSource>>,
        options: PoolWorkerOptions,
    ) -> (PoolWorker, JoinHandle<()>) {
        let (stop, stopped) = watch::channel(false);

        let shared = Arc::new(Shared {
            chain_id,
            memory: Mutex::new(Memory {
                known: LruCache::new(capacity(options.known_capacity)),
                pending: HashSet::new(),
                no_answer: LruCache::new(capacity(
                    options.no_answer_capacity,
                )),
            }),
            counters: Counters::default(),
            breaker_open: AtomicBool::new(false),
            options,
        });

        let Some(caller) = caller else {
            info!("dex pool resolver disabled: no rpc configured");

            let mut stopped = stopped;
            let handle = tokio::spawn(async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            });

            return (PoolWorker { shared, queue: None, stop }, handle);
        };

        let (queue, receiver) =
            mpsc::channel(shared.options.queue_capacity.max(1));

        let task = Task {
            shared: shared.clone(),
            caller,
            sink,
            backfill,
            receiver,
            stopped,
            consecutive_failures: 0,
            cooldown: shared.options.breaker_cooldown,
            paused_until: None,
            backfill_wait: shared.options.backfill_interval,
        };

        let handle = tokio::spawn(task.run());

        (PoolWorker { shared, queue: Some(queue), stop }, handle)
    }

    /// Queues pools for resolution. Never blocks or awaits; deduplicates
    /// against known / queued pools; drops (counted) when the queue is
    /// full - the backfill finds dropped pools again.
    pub fn discover(&self, pools: &[PoolCandidate]) {
        let Some(queue) = &self.queue else {
            return;
        };

        let now = Instant::now();
        let mut memory = self.shared.memory();

        for candidate in pools {
            let id = candidate.pool_id;

            if !candidate.protocol.resolvable_by_rpc()
                || memory.known.contains(&id)
                || memory.pending.contains(&id)
                || memory
                    .no_answer
                    .peek(&id)
                    .is_some_and(|until| *until > now)
            {
                continue;
            }

            match queue.try_send(*candidate) {
                Ok(()) => {
                    memory.pending.insert(id);
                    bump(&self.shared.counters.queued, 1);
                }
                Err(_) => bump(&self.shared.counters.dropped, 1),
            }
        }
    }

    /// Pools the pipeline stored itself (creation events): they will never
    /// be asked over RPC.
    pub fn mark_known<I: IntoIterator<Item = B256>>(&self, pool_ids: I) {
        let mut memory = self.shared.memory();
        for id in pool_ids {
            memory.known.put(id, ());
        }
    }

    pub fn stats(&self) -> PoolWorkerStats {
        let counters = &self.shared.counters;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);

        let (queue_depth, queue_capacity) =
            self.queue.as_ref().map_or((0, 0), |queue| {
                (
                    queue.max_capacity() - queue.capacity(),
                    queue.max_capacity(),
                )
            });

        PoolWorkerStats {
            enabled: self.queue.is_some(),
            queue_depth,
            queue_capacity,
            queued: load(&counters.queued),
            dropped: load(&counters.dropped),
            already_known: load(&counters.already_known),
            resolved: load(&counters.resolved),
            negative: load(&counters.negative),
            codeless: load(&counters.codeless),
            inserted: load(&counters.inserted),
            insert_failures: load(&counters.insert_failures),
            rpc_failures: load(&counters.rpc_failures),
            backfill_runs: load(&counters.backfill_runs),
            backfill_found: load(&counters.backfill_found),
            backfill_failures: load(&counters.backfill_failures),
            breaker_open: self.shared.breaker_open.load(Ordering::Relaxed),
        }
    }

    /// Asks the worker to stop. Idempotent; the `JoinHandle` returned by
    /// [`PoolWorker::spawn`] completes within `shutdown_grace`.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
    }
}

struct Task {
    shared: Arc<Shared>,
    caller: Arc<dyn EthCaller>,
    sink: Arc<dyn PoolSink>,
    backfill: Option<Arc<dyn MissingPoolSource>>,
    receiver: mpsc::Receiver<PoolCandidate>,
    stopped: watch::Receiver<bool>,
    consecutive_failures: u32,
    cooldown: Duration,
    paused_until: Option<Instant>,
    backfill_wait: Duration,
}

impl Task {
    async fn run(mut self) {
        let mut stopped = self.stopped.clone();
        // First backfill right away: it is what heals a previous outage.
        let mut next_backfill = tokio::time::Instant::now();

        loop {
            tokio::select! {
                biased;

                _ = async { let _ = stopped.wait_for(|stop| *stop).await; } => break,

                first = self.receiver.recv() => {
                    let Some(first) = first else { break };
                    let batch = self.collect(first).await;
                    self.guarded(batch, true).await;
                }

                _ = tokio::time::sleep_until(next_backfill), if self.backfill.is_some() => {
                    let wait = self.run_backfill().await;
                    next_backfill = tokio::time::Instant::now() + wait;
                }
            }
        }

        debug!("dex pool resolver stopped");
    }

    /// `first` plus whatever arrives within `batch_linger`.
    async fn collect(
        &mut self,
        first: PoolCandidate,
    ) -> Vec<PoolCandidate> {
        let options = &self.shared.options;
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + options.batch_linger;

        while batch.len() < options.batch_size {
            match tokio::time::timeout_at(deadline, self.receiver.recv())
                .await
            {
                Ok(Some(candidate)) => batch.push(candidate),
                _ => break,
            }
        }

        batch
    }

    /// Runs a batch, but gives up at shutdown (bounded by the grace).
    async fn guarded(
        &mut self,
        batch: Vec<PoolCandidate>,
        check_store: bool,
    ) {
        let grace = self.shared.options.shutdown_grace;
        let mut stopped = self.stopped.clone();
        let ids: Vec<B256> =
            batch.iter().map(|candidate| candidate.pool_id).collect();

        tokio::select! {
            _ = self.process(batch, check_store) => {}
            _ = async {
                let _ = stopped.wait_for(|stop| *stop).await;
                tokio::time::sleep(grace).await;
            } => {}
        }

        let mut memory = self.shared.memory();
        for id in ids {
            memory.pending.remove(&id);
        }
    }

    /// Returns how long to wait before the next run: the interval, doubled
    /// (up to `backfill_max_interval`) while nothing is missing, and no
    /// wait at all when the answer was truncated by `backfill_limit` and
    /// this run stored something.
    async fn run_backfill(&mut self) -> Duration {
        let interval = self
            .shared
            .options
            .backfill_interval
            .max(Duration::from_millis(10));

        let Some(source) = self.backfill.clone() else {
            return interval;
        };

        if self.breaker_is_open() {
            return interval;
        }

        let shared = self.shared.clone();
        let counters = &shared.counters;
        bump(&counters.backfill_runs, 1);

        let missing = match source
            .missing_pools(self.shared.options.backfill_limit)
            .await
        {
            Ok(missing) => missing,
            Err(error) => {
                bump(&counters.backfill_failures, 1);
                warn!("dex pool backfill query failed: {error:#}");
                return interval;
            }
        };

        bump(&counters.backfill_found, missing.len());

        let limit = self.shared.options.backfill_limit;
        let wait = if missing.is_empty() {
            self.backfill_wait = (self.backfill_wait.max(interval) * 2)
                .min(
                    self.shared
                        .options
                        .backfill_max_interval
                        .max(interval),
                );
            self.backfill_wait
        } else {
            self.backfill_wait = interval;
            interval
        };
        let truncated = missing.len() >= limit.max(1);
        let inserted_before = counters.inserted.load(Ordering::Relaxed);

        let now = Instant::now();
        let mut fresh: Vec<PoolCandidate> = Vec::new();
        {
            let mut memory = self.shared.memory();

            for candidate in missing {
                let id = candidate.pool_id;

                if !candidate.protocol.resolvable_by_rpc()
                    || memory.pending.contains(&id)
                    || memory
                        .no_answer
                        .peek(&id)
                        .is_some_and(|until| *until > now)
                {
                    continue;
                }

                // The store says it is missing: forget stale claims.
                memory.known.pop(&id);
                fresh.push(candidate);
            }
        }

        let batch_size = self.shared.options.batch_size.max(1);

        for chunk in fresh.chunks(batch_size) {
            if *self.stopped.borrow() || self.breaker_is_open() {
                break;
            }
            // Reported missing by the store itself: no need to ask again.
            self.guarded(chunk.to_vec(), false).await;
        }

        // More is waiting behind the limit: go on right away, but only
        // while rows are actually being written (codeless addresses and
        // RPC failures keep a full answer full).
        let progressed =
            counters.inserted.load(Ordering::Relaxed) > inserted_before;

        if truncated && progressed {
            Duration::from_millis(10)
        } else {
            wait
        }
    }

    fn breaker_is_open(&mut self) -> bool {
        match self.paused_until {
            Some(until) if Instant::now() < until => true,
            Some(_) => {
                // Cooldown over: the next batch probes the RPC.
                self.paused_until = None;
                self.shared.breaker_open.store(false, Ordering::Relaxed);
                false
            }
            None => false,
        }
    }

    async fn process(
        &mut self,
        batch: Vec<PoolCandidate>,
        check_store: bool,
    ) {
        let shared = self.shared.clone();
        let counters = &shared.counters;

        if self.breaker_is_open() {
            bump(&counters.dropped, batch.len());
            return;
        }

        let mut batch = batch;

        if check_store {
            let ids: Vec<B256> =
                batch.iter().map(|candidate| candidate.pool_id).collect();

            match self.sink.known_pools(&ids).await {
                Ok(known) => {
                    bump(&counters.already_known, known.len());
                    let mut memory = self.shared.memory();
                    for id in &known {
                        memory.known.put(*id, ());
                    }
                    batch.retain(|candidate| {
                        !known.contains(&candidate.pool_id)
                    });
                }
                Err(error) => {
                    // Without the check an RPC row could be requested for
                    // every known pool: leave the batch to the backfill.
                    bump(&counters.dropped, batch.len());
                    warn!("dex pool lookup failed: {error:#}");
                    return;
                }
            }
        }

        if batch.is_empty() {
            return;
        }

        let chain_id = self.shared.chain_id;
        let caller = self.caller.clone();
        let concurrency = self.shared.options.concurrency.max(1);

        let outcomes: Vec<(PoolCandidate, Resolution)> =
            stream::iter(batch.into_iter().map(|candidate| {
                let caller = caller.clone();
                async move {
                    let resolution = resolve_pool(
                        caller.as_ref(),
                        chain_id,
                        &candidate,
                    )
                    .await;
                    (candidate, resolution)
                }
            }))
            .buffer_unordered(concurrency)
            .collect()
            .await;

        let mut rows: Vec<DexPool> = Vec::new();
        let mut answered = 0usize;
        let mut failed = 0usize;
        let retry_at = Instant::now() + self.shared.options.no_answer_ttl;

        for (candidate, resolution) in outcomes {
            match resolution {
                Resolution::Resolved(pool) => {
                    answered += 1;
                    bump(&counters.resolved, 1);
                    rows.push(*pool);
                }
                Resolution::NotAPool => {
                    answered += 1;
                    bump(&counters.negative, 1);
                    rows.push(unresolved_pool(chain_id, &candidate));
                }
                Resolution::NoAnswer => {
                    answered += 1;
                    bump(&counters.codeless, 1);
                    self.shared
                        .memory()
                        .no_answer
                        .put(candidate.pool_id, retry_at);
                }
                Resolution::Retry(error) => {
                    failed += 1;
                    bump(&counters.rpc_failures, 1);
                    debug!(
                        "dex pool {} not resolved: {error}",
                        candidate.address
                    );
                }
            }
        }

        self.track_breaker(answered, failed);

        if rows.is_empty() {
            return;
        }

        match self.sink.insert_pools(&rows).await {
            Ok(()) => {
                bump(&counters.inserted, rows.len());
                let mut memory = self.shared.memory();
                for row in &rows {
                    memory.known.put(row.pool_id, ());
                }
            }
            Err(error) => {
                // Not marked known: the backfill presents them again.
                bump(&counters.insert_failures, 1);
                warn!(
                    "inserting {} dex pools failed: {error:#}",
                    rows.len()
                );
            }
        }
    }

    fn track_breaker(&mut self, answered: usize, failed: usize) {
        if answered > 0 || failed == 0 {
            self.consecutive_failures = 0;
            self.cooldown = self.shared.options.breaker_cooldown;
            return;
        }

        self.consecutive_failures += 1;

        if self.consecutive_failures
            >= self.shared.options.breaker_threshold.max(1)
        {
            warn!(
                "dex pool resolver: rpc unavailable, pausing for {:?}",
                self.cooldown
            );
            self.paused_until = Some(Instant::now() + self.cooldown);
            self.shared.breaker_open.store(true, Ordering::Relaxed);
            self.cooldown = (self.cooldown * 2)
                .min(self.shared.options.breaker_max_cooldown);
            self.consecutive_failures = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::{
        models::{pool_id_of, PoolSource, Protocol},
        resolve::test_support::FakeNode,
    };
    use alloy::primitives::Address;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct MemorySink {
        rows: Mutex<Vec<DexPool>>,
        known: Mutex<HashSet<B256>>,
        fail_inserts: AtomicBool,
        lookups: AtomicUsize,
    }

    impl MemorySink {
        fn rows(&self) -> Vec<DexPool> {
            self.rows.lock().unwrap().clone()
        }
    }

    impl PoolSink for MemorySink {
        fn known_pools<'a>(
            &'a self,
            pool_ids: &'a [B256],
        ) -> BoxFuture<'a, anyhow::Result<HashSet<B256>>> {
            Box::pin(async move {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                let known = self.known.lock().unwrap();
                Ok(pool_ids
                    .iter()
                    .filter(|id| known.contains(*id))
                    .copied()
                    .collect())
            })
        }

        fn insert_pools<'a>(
            &'a self,
            rows: &'a [DexPool],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.fail_inserts.load(Ordering::SeqCst) {
                    anyhow::bail!("clickhouse is down");
                }
                self.rows.lock().unwrap().extend_from_slice(rows);
                Ok(())
            })
        }
    }

    struct FixedBackfill {
        missing: Mutex<Vec<PoolCandidate>>,
        calls: AtomicUsize,
    }

    impl MissingPoolSource for FixedBackfill {
        fn missing_pools<'a>(
            &'a self,
            _limit: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<PoolCandidate>>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.missing.lock().unwrap().clone())
            })
        }
    }

    fn addr(byte: u8) -> Address {
        Address::repeat_byte(byte)
    }

    fn candidate(pool: Address, protocol: Protocol) -> PoolCandidate {
        PoolCandidate {
            pool_id: pool_id_of(pool),
            address: pool,
            protocol,
        }
    }

    fn fast() -> PoolWorkerOptions {
        PoolWorkerOptions {
            batch_linger: Duration::from_millis(5),
            backfill_interval: Duration::from_millis(20),
            backfill_max_interval: Duration::from_millis(40),
            breaker_cooldown: Duration::from_millis(40),
            breaker_max_cooldown: Duration::from_millis(80),
            breaker_threshold: 1,
            no_answer_ttl: Duration::from_millis(30),
            shutdown_grace: Duration::from_millis(50),
            ..PoolWorkerOptions::default()
        }
    }

    async fn until<F: Fn() -> bool>(what: &str, condition: F) {
        for _ in 0..400 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn stop(worker: &PoolWorker, handle: JoinHandle<()>) {
        worker.shutdown().await;
        worker.shutdown().await;
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("worker must stop")
            .unwrap();
    }

    #[tokio::test]
    async fn resolves_discovered_pools_once() {
        let node = Arc::new(FakeNode::default());
        node.pair(addr(1), addr(0xa), addr(0xb));
        let sink = Arc::new(MemorySink::default());

        let (worker, handle) = PoolWorker::spawn(
            7,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        let pools = [candidate(addr(1), Protocol::UniswapV2)];
        worker.discover(&pools);
        worker.discover(&pools);

        until("the pool row", || sink.rows().len() == 1).await;

        let row = &sink.rows()[0];
        assert_eq!(row.chain, 7);
        assert_eq!(row.source, PoolSource::Rpc);
        assert_eq!(row.tokens, vec![addr(0xa), addr(0xb)]);

        // Known now: no queueing, no RPC.
        let calls = node.calls();
        worker.discover(&pools);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(node.calls(), calls);

        let stats = worker.stats();
        assert!(stats.enabled);
        assert_eq!(stats.queued, 1);
        assert_eq!((stats.resolved, stats.inserted), (1, 1));

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn pools_already_stored_are_not_asked() {
        let node = Arc::new(FakeNode::default());
        node.pair(addr(1), addr(0xa), addr(0xb));
        let sink = Arc::new(MemorySink::default());
        sink.known.lock().unwrap().insert(pool_id_of(addr(1)));

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        worker.discover(&[candidate(addr(1), Protocol::UniswapV2)]);

        until("the lookup", || worker.stats().already_known == 1).await;
        assert_eq!(node.calls(), 0);
        assert!(sink.rows().is_empty());

        // Pools the pipeline stored itself are not even queued.
        worker.mark_known([pool_id_of(addr(2))]);
        worker.discover(&[candidate(addr(2), Protocol::UniswapV3)]);
        assert_eq!(worker.stats().queued, 1);

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn negative_results_are_stored_and_not_retried() {
        let node = Arc::new(FakeNode::default());
        // A contract that is not a pool.
        node.set(
            addr(1),
            crate::dex::resolve::FEE,
            None,
            crate::dex::resolve::test_support::Reply::Revert,
        );
        let sink = Arc::new(MemorySink::default());

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        let pools = [candidate(addr(1), Protocol::UniswapV2)];
        worker.discover(&pools);
        until("the negative row", || sink.rows().len() == 1).await;

        assert_eq!(sink.rows()[0].source, PoolSource::Unresolved);
        assert_eq!(sink.rows()[0]._version, 0);
        assert_eq!(worker.stats().negative, 1);

        let calls = node.calls();
        worker.discover(&pools);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(node.calls(), calls);

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn transient_errors_are_never_negatively_cached() {
        let node = Arc::new(FakeNode::default());
        node.pair(addr(1), addr(0xa), addr(0xb));
        node.offline.store(true, Ordering::SeqCst);
        let sink = Arc::new(MemorySink::default());

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        let pools = [candidate(addr(1), Protocol::UniswapV2)];
        worker.discover(&pools);

        until("the rpc failure", || worker.stats().rpc_failures == 1)
            .await;
        assert!(sink.rows().is_empty());
        until("the breaker", || worker.stats().breaker_open).await;

        // The node comes back: the same pool resolves after the cooldown.
        node.offline.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        worker.discover(&pools);

        until("the pool row", || sink.rows().len() == 1).await;
        assert_eq!(sink.rows()[0].source, PoolSource::Rpc);
        assert!(!worker.stats().breaker_open);

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn codeless_addresses_get_no_row_and_are_asked_again_later() {
        let node = Arc::new(FakeNode::default());
        let sink = Arc::new(MemorySink::default());

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        let pools = [candidate(addr(1), Protocol::UniswapV2)];
        worker.discover(&pools);
        until("the empty answer", || worker.stats().codeless == 1).await;
        assert!(sink.rows().is_empty());

        // Within the TTL: not asked.
        worker.discover(&pools);
        assert_eq!(worker.stats().queued, 1);

        // The node catches up.
        node.pair(addr(1), addr(0xa), addr(0xb));
        tokio::time::sleep(Duration::from_millis(40)).await;
        worker.discover(&pools);
        until("the pool row", || sink.rows().len() == 1).await;

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn backfill_heals_what_discover_dropped() {
        let node = Arc::new(FakeNode::default());
        node.pair(addr(1), addr(0xa), addr(0xb));
        node.coins(
            addr(2),
            crate::dex::resolve::COINS_UINT,
            &[addr(0xa), addr(0xb), addr(0xc)],
        );
        let sink = Arc::new(MemorySink::default());
        let backfill = Arc::new(FixedBackfill {
            missing: Mutex::new(vec![
                candidate(addr(1), Protocol::UniswapV3),
                candidate(addr(2), Protocol::Curve),
                // Never resolvable over RPC: ignored.
                candidate(addr(3), Protocol::UniswapV4),
            ]),
            calls: AtomicUsize::new(0),
        });

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            Some(backfill.clone()),
            fast(),
        );

        until("both rows", || sink.rows().len() == 2).await;
        backfill.missing.lock().unwrap().clear();

        let stats = worker.stats();
        assert!(stats.backfill_runs >= 1);
        assert_eq!(stats.backfill_found % 3, 0);
        // The store reported them missing itself: no lookup.
        assert_eq!(sink.lookups.load(Ordering::SeqCst), 0);

        let curve = sink
            .rows()
            .into_iter()
            .find(|row| row.protocol == Protocol::Curve)
            .unwrap();
        assert_eq!(curve.tokens.len(), 3);

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn failed_inserts_are_not_remembered() {
        let node = Arc::new(FakeNode::default());
        node.pair(addr(1), addr(0xa), addr(0xb));
        let sink = Arc::new(MemorySink::default());
        sink.fail_inserts.store(true, Ordering::SeqCst);

        let (worker, handle) = PoolWorker::spawn(
            1,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        let pools = [candidate(addr(1), Protocol::UniswapV2)];
        worker.discover(&pools);
        until("the failure", || worker.stats().insert_failures == 1).await;

        sink.fail_inserts.store(false, Ordering::SeqCst);
        until("the pool to leave the queue", || {
            worker.discover(&pools);
            sink.rows().len() == 1
        })
        .await;

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn discover_never_blocks_and_drops_when_full() {
        // Offline node + tiny queue: the worker is stuck on its first
        // batch while `discover` keeps being called.
        let node = Arc::new(FakeNode::default());
        let sink = Arc::new(MemorySink::default());
        sink.fail_inserts.store(true, Ordering::SeqCst);

        let options = PoolWorkerOptions {
            queue_capacity: 2,
            batch_size: 1,
            ..fast()
        };

        let (worker, handle) =
            PoolWorker::spawn(1, Some(node), sink, None, options);

        let many: Vec<PoolCandidate> = (1..=200u8)
            .map(|byte| candidate(addr(byte), Protocol::UniswapV2))
            .collect();

        let started = Instant::now();
        worker.discover(&many);
        assert!(started.elapsed() < Duration::from_millis(200));

        let stats = worker.stats();
        assert_eq!(stats.queued + stats.dropped, 200);
        assert!(stats.dropped >= 190, "{stats:?}");
        assert_eq!(stats.queue_capacity, 2);

        stop(&worker, handle).await;
    }

    #[tokio::test]
    async fn without_rpc_the_worker_is_inert() {
        let sink = Arc::new(MemorySink::default());
        let (worker, handle) =
            PoolWorker::spawn(1, None, sink.clone(), None, fast());

        worker.discover(&[candidate(addr(1), Protocol::UniswapV2)]);

        let stats = worker.stats();
        assert!(!stats.enabled);
        assert_eq!((stats.queued, stats.dropped), (0, 0));
        assert!(!handle.is_finished());

        stop(&worker, handle).await;
        assert!(sink.rows().is_empty());
    }
}
