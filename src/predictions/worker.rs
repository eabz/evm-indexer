//! Background resolver of exchanges / pools (`prediction_venues`), the
//! mirror image of `dex::PoolWorker`.
//!
//! * **Off the commit path.** The pipeline only calls
//!   [`VenueWorker::discover`]: sync, non blocking, bounded, drop-on-full.
//! * **DB driven backfill.** [`MissingVenueSource`] (a query,
//!   [`super::MISSING_VENUES_SQL`]) periodically lists venues that traded
//!   but have no row: an RPC outage of any length, a dropped candidate or a
//!   restart heal themselves.
//! * **Negative caching.** A contract that answers "not a venue" gets a
//!   `source = 'unresolved'` row and is never asked again; a node that does
//!   not answer proves nothing and caches nothing (memory only, with a
//!   retry time).
//! * **Circuit breaker.** Batches in which no call got through pause the
//!   worker with an exponential cooldown; the queue keeps filling (and
//!   dropping) meanwhile, nothing blocks.
//!
//! There are only a handful of venues per chain, so everything here is
//! tiny compared with the pool worker - the shape is the same on purpose.

use std::{
    collections::HashSet,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use alloy::primitives::Address;
use futures::future::BoxFuture;
use log::{debug, info, warn};
use lru::LruCache;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};

use crate::tokens::multicall::EthCaller;

use super::{
    models::PredictionVenue,
    resolve::{resolve_venue, Resolution},
    VenueCandidate,
};

/// Where resolved venues are written, and what is already there.
pub trait VenueSink: Send + Sync + 'static {
    /// The subset of `exchanges` that already has a `prediction_venues`
    /// row (of any source):
    /// `SELECT exchange FROM prediction_venues WHERE chain = ? AND exchange IN ?`.
    fn known_venues<'a>(
        &'a self,
        exchanges: &'a [Address],
    ) -> BoxFuture<'a, anyhow::Result<HashSet<Address>>>;

    /// Inserts into `prediction_venues`. Must be idempotent
    /// (ReplacingMergeTree) and keep the rows' `_version` untouched.
    fn insert_venues<'a>(
        &'a self,
        rows: &'a [PredictionVenue],
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Venues that traded but have no `prediction_venues` row
/// ([`super::MISSING_VENUES_SQL`]).
pub trait MissingVenueSource: Send + Sync + 'static {
    fn missing_venues<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<VenueCandidate>>>;
}

#[derive(Debug, Clone)]
pub struct VenueWorkerOptions {
    /// Candidates waiting for the worker; `discover` drops above this.
    pub queue_capacity: usize,
    /// Candidates resolved (and inserted) together.
    pub batch_size: usize,
    /// How long a batch waits for more candidates.
    pub batch_linger: Duration,
    pub backfill_interval: Duration,
    /// The interval backs off to this while nothing is missing.
    pub backfill_max_interval: Duration,
    pub backfill_limit: usize,
    /// Exchanges remembered as stored (or definitely not a venue).
    pub known_capacity: usize,
    /// A venue the node could not answer for is not asked again for this
    /// long (memory only).
    pub retry_after: Duration,
    /// Consecutive batches without a single answer before pausing.
    pub breaker_threshold: u32,
    pub breaker_cooldown: Duration,
    pub breaker_max_cooldown: Duration,
    pub shutdown_grace: Duration,
}

impl Default for VenueWorkerOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 10_000,
            batch_size: 50,
            batch_linger: Duration::from_millis(250),
            backfill_interval: Duration::from_secs(60),
            backfill_max_interval: Duration::from_secs(600),
            backfill_limit: 500,
            known_capacity: 100_000,
            retry_after: Duration::from_secs(300),
            breaker_threshold: 3,
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(600),
            shutdown_grace: Duration::from_secs(5),
        }
    }
}

/// Counters are monotonic since startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VenueWorkerStats {
    /// `false` when no RPC is configured (the worker is inert).
    pub enabled: bool,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub queued: u64,
    pub dropped: u64,
    /// Skipped because `prediction_venues` already had the venue.
    pub already_known: u64,
    pub resolved: u64,
    /// Definitely not a venue (`source = 'unresolved'` rows).
    pub negative: u64,
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
    inserted: AtomicU64,
    insert_failures: AtomicU64,
    rpc_failures: AtomicU64,
    backfill_runs: AtomicU64,
    backfill_found: AtomicU64,
    backfill_failures: AtomicU64,
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

struct Memory {
    known: LruCache<Address, ()>,
    pending: HashSet<Address>,
    retry_at: LruCache<Address, Instant>,
}

struct Shared {
    chain_id: u64,
    memory: Mutex<Memory>,
    counters: Counters,
    breaker_open: AtomicBool,
    options: VenueWorkerOptions,
}

impl Shared {
    fn memory(&self) -> MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Cheap, clonable handle of the background resolver.
#[derive(Clone)]
pub struct VenueWorker {
    shared: Arc<Shared>,
    /// `None` when the worker is inert (no RPC).
    queue: Option<mpsc::Sender<VenueCandidate>>,
    stop: watch::Sender<bool>,
}

fn capacity(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value.max(1)).unwrap_or(NonZeroUsize::MIN)
}

impl VenueWorker {
    /// Starts the worker. Sync, no I/O; must run inside a tokio runtime.
    /// `caller = None` (`--rpc none`) yields an inert worker: `discover`
    /// is a no-op and the task just waits for `shutdown`.
    ///
    /// Share the `Arc<dyn EthCaller>` of the token worker: endpoint
    /// failover and chain id checks live behind it.
    pub fn spawn(
        chain_id: u64,
        caller: Option<Arc<dyn EthCaller>>,
        sink: Arc<dyn VenueSink>,
        backfill: Option<Arc<dyn MissingVenueSource>>,
        options: VenueWorkerOptions,
    ) -> (VenueWorker, JoinHandle<()>) {
        let (stop, stopped) = watch::channel(false);

        let shared = Arc::new(Shared {
            chain_id,
            memory: Mutex::new(Memory {
                known: LruCache::new(capacity(options.known_capacity)),
                pending: HashSet::new(),
                retry_at: LruCache::new(capacity(options.known_capacity)),
            }),
            counters: Counters::default(),
            breaker_open: AtomicBool::new(false),
            options,
        });

        let Some(caller) = caller else {
            info!("prediction venue resolver disabled: no rpc configured");

            let mut stopped = stopped;
            let handle = tokio::spawn(async move {
                let _ = stopped.wait_for(|stop| *stop).await;
            });

            return (VenueWorker { shared, queue: None, stop }, handle);
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
            backfill_wait: shared.options.backfill_interval,
        };

        let handle = tokio::spawn(task.run());

        (VenueWorker { shared, queue: Some(queue), stop }, handle)
    }

    /// Queues venues for resolution. Never blocks or awaits; deduplicates
    /// against known / queued venues; drops (counted) when the queue is
    /// full - the backfill finds dropped venues again.
    pub fn discover(&self, venues: &[VenueCandidate]) {
        let Some(queue) = &self.queue else {
            return;
        };

        let now = Instant::now();
        let mut memory = self.shared.memory();

        for candidate in venues {
            let exchange = candidate.exchange;

            if memory.known.contains(&exchange)
                || memory.pending.contains(&exchange)
                || memory
                    .retry_at
                    .peek(&exchange)
                    .is_some_and(|until| *until > now)
            {
                continue;
            }

            match queue.try_send(*candidate) {
                Ok(()) => {
                    memory.pending.insert(exchange);
                    bump(&self.shared.counters.queued, 1);
                }
                Err(_) => bump(&self.shared.counters.dropped, 1),
            }
        }
    }

    /// Venues known to be stored: they will never be asked over RPC.
    pub fn mark_known<I: IntoIterator<Item = Address>>(&self, exchanges: I) {
        let mut memory = self.shared.memory();
        for exchange in exchanges {
            memory.known.put(exchange, ());
        }
    }

    pub fn stats(&self) -> VenueWorkerStats {
        let counters = &self.shared.counters;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);

        let (queue_depth, queue_capacity) =
            self.queue.as_ref().map_or((0, 0), |queue| {
                (queue.max_capacity() - queue.capacity(), queue.max_capacity())
            });

        VenueWorkerStats {
            enabled: self.queue.is_some(),
            queue_depth,
            queue_capacity,
            queued: load(&counters.queued),
            dropped: load(&counters.dropped),
            already_known: load(&counters.already_known),
            resolved: load(&counters.resolved),
            negative: load(&counters.negative),
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
    /// [`VenueWorker::spawn`] completes within `shutdown_grace`.
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
    }
}

struct Task {
    shared: Arc<Shared>,
    caller: Arc<dyn EthCaller>,
    sink: Arc<dyn VenueSink>,
    backfill: Option<Arc<dyn MissingVenueSource>>,
    receiver: mpsc::Receiver<VenueCandidate>,
    stopped: watch::Receiver<bool>,
    consecutive_failures: u32,
    cooldown: Duration,
    backfill_wait: Duration,
}

impl Task {
    async fn run(mut self) {
        let mut stopped = self.stopped.clone();
        // First backfill right away: it is what heals a previous outage.
        let mut next_backfill = Instant::now();

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
                    next_backfill = Instant::now() + wait;
                }
            }
        }

        debug!("prediction venue resolver stopped");
    }

    /// `first` plus whatever arrives within `batch_linger`.
    async fn collect(&mut self, first: VenueCandidate) -> Vec<VenueCandidate> {
        let options = &self.shared.options;
        let mut batch = vec![first];
        let deadline = Instant::now() + options.batch_linger;

        while batch.len() < options.batch_size {
            match tokio::time::timeout_at(deadline, self.receiver.recv()).await
            {
                Ok(Some(candidate)) => batch.push(candidate),
                _ => break,
            }
        }

        batch
    }

    /// Runs a batch, but gives up at shutdown (bounded by the grace).
    async fn guarded(&mut self, batch: Vec<VenueCandidate>, check_store: bool) {
        let grace = self.shared.options.shutdown_grace;
        let mut stopped = self.stopped.clone();
        let exchanges: Vec<Address> =
            batch.iter().map(|candidate| candidate.exchange).collect();

        tokio::select! {
            _ = self.process(batch, check_store) => {}
            _ = async {
                let _ = stopped.wait_for(|stop| *stop).await;
                tokio::time::sleep(grace).await;
            } => {}
        }

        // Whatever happened, nothing of this batch is pending any more.
        let mut memory = self.shared.memory();
        for exchange in exchanges {
            memory.pending.remove(&exchange);
        }
    }

    async fn process(&mut self, batch: Vec<VenueCandidate>, check_store: bool) {
        let shared = self.shared.clone();
        let counters = &shared.counters;

        // The circuit breaker: wait out the cooldown first.
        if shared.breaker_open.load(Ordering::Relaxed) {
            tokio::time::sleep(self.cooldown).await;
        }

        let mut batch = batch;

        if check_store {
            let exchanges: Vec<Address> =
                batch.iter().map(|candidate| candidate.exchange).collect();

            match self.sink.known_venues(&exchanges).await {
                Ok(known) => {
                    bump(&counters.already_known, known.len() as u64);
                    batch.retain(|candidate| !known.contains(&candidate.exchange));
                    shared.memory().known.extend_known(known);
                }
                Err(error) => {
                    warn!("prediction venues: lookup failed: {error:#}");
                }
            }
        }

        if batch.is_empty() {
            return;
        }

        let mut rows = Vec::new();
        let mut retries = Vec::new();

        for candidate in &batch {
            match resolve_venue(self.caller.as_ref(), shared.chain_id, candidate)
                .await
            {
                Resolution::Resolved(row) => {
                    bump(&counters.resolved, 1);
                    rows.push(row);
                }
                Resolution::NotAVenue(row) => {
                    bump(&counters.negative, 1);
                    rows.push(row);
                }
                Resolution::Retry => {
                    bump(&counters.rpc_failures, 1);
                    retries.push(candidate.exchange);
                }
            }
        }

        // Breaker bookkeeping: a batch where nothing got through.
        if rows.is_empty() && !retries.is_empty() {
            self.consecutive_failures += 1;
            if self.consecutive_failures >= shared.options.breaker_threshold {
                if shared.breaker_open.swap(true, Ordering::Relaxed) {
                    self.cooldown = (self.cooldown * 2)
                        .min(shared.options.breaker_max_cooldown);
                }
                warn!(
                    "prediction venues: rpc unavailable, pausing {:?}",
                    self.cooldown
                );
            }
        } else {
            self.consecutive_failures = 0;
            self.cooldown = shared.options.breaker_cooldown;
            shared.breaker_open.store(false, Ordering::Relaxed);
        }

        {
            let until = Instant::now() + shared.options.retry_after;
            let mut memory = shared.memory();
            for exchange in retries {
                memory.retry_at.put(exchange, until);
            }
        }

        if rows.is_empty() {
            return;
        }

        match self.sink.insert_venues(&rows).await {
            Ok(()) => {
                bump(&counters.inserted, rows.len() as u64);
                let mut memory = shared.memory();
                for row in &rows {
                    memory.known.put(row.exchange, ());
                }
            }
            Err(error) => {
                // Not remembered: the backfill finds them again.
                bump(&counters.insert_failures, 1);
                warn!("prediction venues: insert failed: {error:#}");
            }
        }
    }

    /// One backfill round; returns how long to wait for the next one.
    async fn run_backfill(&mut self) -> Duration {
        let Some(source) = self.backfill.clone() else {
            return self.shared.options.backfill_max_interval;
        };

        let shared = self.shared.clone();
        let options = &shared.options;
        bump(&shared.counters.backfill_runs, 1);

        let missing = match source.missing_venues(options.backfill_limit).await {
            Ok(missing) => missing,
            Err(error) => {
                bump(&shared.counters.backfill_failures, 1);
                warn!("prediction venues: backfill query failed: {error:#}");
                return self.backfill_wait;
            }
        };

        let now = Instant::now();
        let todo: Vec<VenueCandidate> = {
            let memory = shared.memory();
            missing
                .into_iter()
                .filter(|candidate| {
                    !memory.pending.contains(&candidate.exchange)
                        && !memory
                            .retry_at
                            .peek(&candidate.exchange)
                            .is_some_and(|until| *until > now)
                })
                .collect()
        };

        if todo.is_empty() {
            // Nothing missing: look less often.
            self.backfill_wait =
                (self.backfill_wait * 2).min(options.backfill_max_interval);
            return self.backfill_wait;
        }

        bump(&shared.counters.backfill_found, todo.len() as u64);
        self.backfill_wait = options.backfill_interval;

        for chunk in todo.chunks(options.batch_size.max(1)) {
            if *self.stopped.borrow() {
                break;
            }
            // The query already excluded stored venues.
            self.guarded(chunk.to_vec(), false).await;
        }

        self.backfill_wait
    }
}

trait ExtendKnown {
    fn extend_known(&mut self, exchanges: HashSet<Address>);
}

impl ExtendKnown for LruCache<Address, ()> {
    fn extend_known(&mut self, exchanges: HashSet<Address>) {
        for exchange in exchanges {
            self.put(exchange, ());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::predictions::{
        models::{Protocol, RowSource},
        resolve::test_support::FakeNode,
    };

    #[derive(Default)]
    struct MemorySink {
        rows: Mutex<Vec<PredictionVenue>>,
    }

    impl MemorySink {
        fn rows(&self) -> Vec<PredictionVenue> {
            self.rows.lock().unwrap().clone()
        }
    }

    impl VenueSink for MemorySink {
        fn known_venues<'a>(
            &'a self,
            exchanges: &'a [Address],
        ) -> BoxFuture<'a, anyhow::Result<HashSet<Address>>> {
            Box::pin(async move {
                let rows = self.rows.lock().unwrap();
                Ok(exchanges
                    .iter()
                    .copied()
                    .filter(|exchange| {
                        rows.iter().any(|row| row.exchange == *exchange)
                    })
                    .collect())
            })
        }

        fn insert_venues<'a>(
            &'a self,
            rows: &'a [PredictionVenue],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.rows.lock().unwrap().extend_from_slice(rows);
                Ok(())
            })
        }
    }

    struct Missing(Vec<VenueCandidate>);

    impl MissingVenueSource for Missing {
        fn missing_venues<'a>(
            &'a self,
            _limit: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<VenueCandidate>>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn fast() -> VenueWorkerOptions {
        VenueWorkerOptions {
            batch_linger: Duration::from_millis(5),
            backfill_interval: Duration::from_millis(20),
            backfill_max_interval: Duration::from_millis(40),
            retry_after: Duration::from_millis(30),
            breaker_cooldown: Duration::from_millis(5),
            breaker_max_cooldown: Duration::from_millis(10),
            shutdown_grace: Duration::from_millis(20),
            ..VenueWorkerOptions::default()
        }
    }

    fn candidate(byte: u8) -> VenueCandidate {
        VenueCandidate {
            exchange: Address::repeat_byte(byte),
            protocol: Protocol::CtfExchange,
        }
    }

    async fn settle<F: Fn() -> bool>(done: F) {
        for _ in 0..400 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the worker did not settle");
    }

    #[tokio::test]
    async fn resolves_once_and_caches_negatives() {
        let node = Arc::new(FakeNode::default());
        node.exchange(
            Address::repeat_byte(1),
            Address::repeat_byte(0xc0),
            Address::repeat_byte(0xcf),
        );
        let sink = Arc::new(MemorySink::default());

        let (worker, handle) = VenueWorker::spawn(
            137,
            Some(node.clone()),
            sink.clone(),
            None,
            fast(),
        );

        worker.discover(&[candidate(1), candidate(1), candidate(2)]);
        settle(|| sink.rows().len() == 2).await;

        let rows = sink.rows();
        let venue = rows.iter().find(|row| row.exchange == Address::repeat_byte(1));
        assert_eq!(venue.unwrap().source, RowSource::Rpc);
        let stranger =
            rows.iter().find(|row| row.exchange == Address::repeat_byte(2));
        assert_eq!(stranger.unwrap().source, RowSource::Unresolved);

        // Known now: nothing is queued or asked again.
        let calls = node.calls();
        worker.discover(&[candidate(1), candidate(2)]);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(node.calls(), calls);

        let stats = worker.stats();
        assert!(stats.enabled);
        assert_eq!(stats.queued, 2);
        assert_eq!(stats.resolved, 1);
        assert_eq!(stats.negative, 1);
        assert_eq!(stats.inserted, 2);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn an_outage_caches_nothing_and_the_backfill_heals_it() {
        let node = Arc::new(FakeNode::default());
        node.exchange(
            Address::repeat_byte(1),
            Address::repeat_byte(0xc0),
            Address::repeat_byte(0xcf),
        );
        node.down.store(true, Ordering::Relaxed);

        let sink = Arc::new(MemorySink::default());
        let (worker, handle) = VenueWorker::spawn(
            137,
            Some(node.clone()),
            sink.clone(),
            Some(Arc::new(Missing(vec![candidate(1)]))),
            fast(),
        );

        settle(|| worker.stats().rpc_failures > 0).await;
        assert!(sink.rows().is_empty());

        node.down.store(false, Ordering::Relaxed);
        settle(|| sink.rows().len() == 1).await;
        assert_eq!(sink.rows()[0].source, RowSource::Rpc);
        assert!(worker.stats().backfill_found >= 1);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn without_rpc_the_worker_is_inert_and_a_full_queue_drops() {
        let sink = Arc::new(MemorySink::default());
        let (worker, handle) =
            VenueWorker::spawn(137, None, sink.clone(), None, fast());

        worker.discover(&[candidate(1)]);
        assert!(!worker.stats().enabled);
        assert_eq!(worker.stats().queued, 0);
        worker.shutdown().await;
        handle.await.unwrap();

        // A node that never answers keeps the single slot busy.
        let node = Arc::new(FakeNode::default());
        node.down.store(true, Ordering::Relaxed);
        let (worker, handle) = VenueWorker::spawn(
            137,
            Some(node),
            sink,
            None,
            VenueWorkerOptions { queue_capacity: 1, ..fast() },
        );

        let many: Vec<VenueCandidate> = (1..=50).map(candidate).collect();
        worker.discover(&many);
        let stats = worker.stats();
        assert!(stats.dropped > 0);
        assert_eq!(stats.queued + stats.dropped, 50);

        worker.shutdown().await;
        handle.await.unwrap();
    }
}
