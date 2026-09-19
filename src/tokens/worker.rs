//! Token metadata off the commit path.
//!
//! The pipeline only calls [`TokenWorker::discover`] with the token
//! contracts a batch revealed: a synchronous, non blocking push into a
//! bounded queue. A background task resolves them ([`TokenResolver`]),
//! inserts the `tokens` rows itself through a [`TokenSink`] and only then
//! marks them in the cache. Nothing the RPC does (slow, down, rate
//! limited, gone for a week) can slow down or fail a flush.
//!
//! Nothing can be lost either, because the queue is only a fast path: a
//! periodic **backfill** asks the database ([`MissingTokenSource`]) which
//! token addresses are referenced by stored data but have no `tokens` row
//! and feeds them through the same path. Whatever was dropped (full
//! queue), failed (RPC or ClickHouse outage), or never seen by this
//! process at all (crash, restart, data indexed without `--rpc`) is healed
//! from there, however long the outage was.
//!
//! The backfill is also the authority over the caches: a token the
//! database keeps reporting as missing although memory / Redis believe it
//! is stored (a `tokens` table that was reset without flushing Redis, a
//! lost insert) is fetched again the second time it is reported.
//!
//! Reorgs need nothing: tokens are not block scoped. A token discovered in
//! a block that is later purged is resolved like any other (its row is
//! harmless and most likely needed again by the canonical fork); if its
//! contract does not exist on the canonical chain it simply has no code
//! and no backfill will ask for it.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use alloy::primitives::Address;
use futures::future::BoxFuture;
use log::{debug, error, info, warn};
use tokio::{
    sync::{watch, Notify},
    task::JoinHandle,
    time::Instant,
};

use super::{
    multicall::EthCaller, redact::redact_urls, ResolveMode, TokenResolver,
    TokenResolverOptions, TokenStandard,
};
use crate::db::models::token::DatabaseToken;

/// Where the worker stores the rows it resolved (ClickHouse `tokens`).
pub trait TokenSink: Send + Sync + 'static {
    /// Durably inserts `rows`. Inserting a token twice must be harmless
    /// (`tokens` is a `ReplacingMergeTree` keyed by `(chain, address)`).
    fn insert_tokens<'a>(
        &'a self,
        rows: &'a [DatabaseToken],
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// The database side of the backfill (an anti-join of the transfer tables
/// and `dex_pools` against `tokens`).
pub trait MissingTokenSource: Send + Sync + 'static {
    /// Up to `limit` token addresses referenced by stored data but absent
    /// from `tokens`.
    ///
    /// Addresses that cannot be resolved right now are reported again on
    /// the next call: prefer an order that does not always put the same
    /// addresses first (the worker does give up on them eventually, see
    /// [`TokenResolverOptions::empty_strikes`]).
    fn missing_tokens<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>>;
}

/// Tunables of the [`TokenWorker`].
#[derive(Debug, Clone)]
pub struct TokenWorkerOptions {
    /// Tokens waiting to be resolved; `discover` drops beyond that (the
    /// backfill finds them later). ~60 bytes each.
    pub queue_capacity: usize,
    /// Tokens resolved and inserted at once.
    pub batch_size: usize,
    /// How long a batch waits for more tokens before it is resolved.
    pub batch_linger: Duration,
    /// Retries of a failed insert (so `insert_retries + 1` attempts).
    pub insert_retries: u32,
    /// First retry delay, doubled on every further retry.
    pub insert_backoff: Duration,
    pub insert_max_backoff: Duration,
    /// Timeout of one insert attempt.
    pub insert_timeout: Duration,
    /// Pause after a batch the RPC could not (entirely) answer or the
    /// sink could not store, before its tokens are tried again.
    pub retry_delay: Duration,
    /// Time between two backfill queries. The first one runs at startup.
    pub backfill_interval: Duration,
    /// The interval doubles up to this while the backfill finds nothing.
    pub backfill_max_interval: Duration,
    /// `limit` of a backfill query. A query that comes back full is
    /// followed by the next one as soon as the queue is drained.
    pub backfill_limit: usize,
    pub backfill_timeout: Duration,
    /// How long `shutdown` lets an insert that is already running finish.
    pub shutdown_grace: Duration,
    pub resolver: TokenResolverOptions,
}

impl Default for TokenWorkerOptions {
    fn default() -> Self {
        Self {
            queue_capacity: 100_000,
            batch_size: 500,
            batch_linger: Duration::from_millis(250),
            insert_retries: 5,
            insert_backoff: Duration::from_secs(1),
            insert_max_backoff: Duration::from_secs(30),
            insert_timeout: Duration::from_secs(60),
            retry_delay: Duration::from_secs(5),
            backfill_interval: Duration::from_secs(60),
            backfill_max_interval: Duration::from_secs(600),
            backfill_limit: 5_000,
            backfill_timeout: Duration::from_secs(120),
            shutdown_grace: Duration::from_secs(5),
            resolver: TokenResolverOptions::default(),
        }
    }
}

/// Point in time numbers of the worker, for the metrics endpoint.
/// Counters are totals since startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenWorkerStats {
    /// `false`: no RPC configured, the worker does nothing.
    pub enabled: bool,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    /// Tokens accepted into the queue (from `discover` and the backfill).
    pub queued: u64,
    /// Tokens dropped because the queue was full.
    pub dropped: u64,
    /// Rows produced by the RPC (including the negative ones).
    pub resolved: u64,
    /// Rows without any metadata (reverts, garbage, no contract).
    pub negative: u64,
    /// Fetches that found no code at the address (retried later).
    pub codeless: u64,
    /// Rows durably inserted through the sink.
    pub inserted: u64,
    /// Batches the sink could not store even after the retries.
    pub insert_failures: u64,
    /// Tokens found in Redis / not found in Redis.
    pub cache_hits: u64,
    pub cache_misses: u64,
    /// Tokens the RPC could not be asked about (tried again later).
    pub rpc_failures: u64,
    pub backfill_runs: u64,
    /// Addresses the backfill reported as missing.
    pub backfill_found: u64,
    pub backfill_failures: u64,
    /// The RPC circuit breaker is open (or every endpoint serves another
    /// chain): nothing is being resolved right now.
    pub breaker_open: bool,
    pub endpoints_total: usize,
    pub endpoints_healthy: usize,
}

#[derive(Debug, Clone, Copy)]
struct Queued {
    address: Address,
    standard: TokenStandard,
    /// Reported missing by the database although the caches know it.
    forced: bool,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Queued>,
    queued: HashSet<Address>,
    /// Waiting to be retried alone (see `Retries`): not accepted here.
    suspended: HashSet<Address>,
}

#[derive(Default)]
struct Counters {
    queued: AtomicU64,
    dropped: AtomicU64,
    inserted: AtomicU64,
    insert_failures: AtomicU64,
    backfill_runs: AtomicU64,
    backfill_found: AtomicU64,
    backfill_failures: AtomicU64,
}

struct Shared {
    resolver: TokenResolver,
    options: TokenWorkerOptions,
    queue: Mutex<Queue>,
    wake: Notify,
    accepting: AtomicBool,
    /// The queue is overflowing (reported once per episode).
    overflowing: AtomicBool,
    shutdown: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    counters: Counters,
}

/// Handle of the background token worker. Cheap to clone.
#[derive(Clone)]
pub struct TokenWorker {
    shared: Arc<Shared>,
}

impl Shared {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn queue_len(&self) -> usize {
        self.queue().items.len()
    }

    /// Pushes what fits, returns how many were accepted. Never waits.
    fn enqueue<I>(&self, tokens: I) -> usize
    where
        I: IntoIterator<Item = Queued>,
    {
        let capacity = self.options.queue_capacity.max(1);
        let mut accepted = 0u64;
        let mut dropped = 0u64;

        {
            let mut queue = self.queue();
            for token in tokens {
                if queue.queued.contains(&token.address)
                    || queue.suspended.contains(&token.address)
                {
                    continue;
                }
                if queue.items.len() >= capacity {
                    dropped += 1;
                    continue;
                }
                queue.queued.insert(token.address);
                queue.items.push_back(token);
                accepted += 1;
            }
        }

        if accepted > 0 {
            self.counters.queued.fetch_add(accepted, Ordering::Relaxed);
            self.wake.notify_one();
        }

        if dropped > 0 {
            self.counters.dropped.fetch_add(dropped, Ordering::Relaxed);
            if !self.overflowing.swap(true, Ordering::Relaxed) {
                warn!(
                    "The token metadata queue is full ({capacity}), new \
                     tokens are skipped for now and picked up later from \
                     the database"
                );
            }
        }

        accepted as usize
    }

    fn take_batch(&self) -> Vec<Queued> {
        let mut queue = self.queue();
        let count = self.options.batch_size.max(1).min(queue.items.len());
        let batch: Vec<Queued> = queue.items.drain(..count).collect();
        for token in &batch {
            queue.queued.remove(&token.address);
        }
        drop(queue);

        if !batch.is_empty() {
            self.overflowing.store(false, Ordering::Relaxed);
        }
        batch
    }
}

impl TokenWorker {
    /// Spawns the background task. `caller: None` => the worker is inert
    /// (`discover` is a no-op, logged once); its task still runs until
    /// `shutdown` so the handle can be awaited the same way.
    ///
    /// No I/O happens here: Redis is connected and the chain id verified
    /// by the task. Only an invalid `redis_url` is an error. Must be
    /// called from within a tokio runtime.
    ///
    /// The returned handle completes after [`shutdown`](Self::shutdown);
    /// the task never ends on its own (a panic of the sink or the
    /// backfill source is logged and the worker restarted).
    pub fn spawn(
        chain_id: u64,
        caller: Option<Arc<dyn EthCaller>>,
        redis_url: Option<&str>,
        sink: Arc<dyn TokenSink>,
        backfill: Option<Arc<dyn MissingTokenSource>>,
        options: TokenWorkerOptions,
    ) -> anyhow::Result<(Self, JoinHandle<()>)> {
        let resolver = TokenResolver::from_caller(
            chain_id,
            caller,
            redis_url,
            &options.resolver,
        )?;

        Ok(Self::spawn_with_resolver(resolver, sink, backfill, options))
    }

    /// [`spawn`](Self::spawn) over an already assembled resolver (tests,
    /// custom caches). `options.resolver` is not used.
    pub fn spawn_with_resolver(
        resolver: TokenResolver,
        sink: Arc<dyn TokenSink>,
        backfill: Option<Arc<dyn MissingTokenSource>>,
        options: TokenWorkerOptions,
    ) -> (Self, JoinHandle<()>) {
        let (shutdown, _) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        let enabled = resolver.is_enabled();

        let shared = Arc::new(Shared {
            resolver,
            options,
            queue: Mutex::new(Queue::default()),
            wake: Notify::new(),
            accepting: AtomicBool::new(enabled),
            overflowing: AtomicBool::new(false),
            shutdown,
            done,
            counters: Counters::default(),
        });

        if !enabled {
            info!(
                "No RPC configured: token metadata is disabled (the \
                 `tokens` table stays empty)"
            );
        }

        let handle = tokio::spawn(supervise(
            shared.clone(),
            sink,
            backfill,
            done_tx,
            enabled,
        ));

        (Self { shared }, handle)
    }

    /// Hands the token contracts seen in a batch to the worker.
    ///
    /// NEVER blocks and NEVER awaits I/O: tokens already known, in flight
    /// or queued are skipped in memory and the rest is pushed into the
    /// bounded queue. When the queue is full the tokens are dropped
    /// (counted) — safe, the backfill finds them in the database later.
    pub fn discover(&self, tokens: &HashMap<Address, TokenStandard>) {
        let shared = &*self.shared;

        if tokens.is_empty() || !shared.accepting.load(Ordering::Relaxed) {
            return;
        }

        let unknown = shared.resolver.filter_unknown(tokens);
        if unknown.is_empty() {
            return;
        }

        shared.enqueue(unknown.into_iter().map(|(address, standard)| {
            Queued { address, standard, forced: false }
        }));
    }

    pub fn stats(&self) -> TokenWorkerStats {
        let shared = &*self.shared;
        let counters = &shared.counters;
        let resolver = shared.resolver.stats();
        let health = shared.resolver.rpc_health().unwrap_or_default();
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);

        TokenWorkerStats {
            enabled: shared.resolver.is_enabled(),
            queue_depth: shared.queue_len(),
            queue_capacity: shared.options.queue_capacity.max(1),
            queued: load(&counters.queued),
            dropped: load(&counters.dropped),
            resolved: resolver.resolved,
            negative: resolver.negative,
            codeless: resolver.codeless,
            inserted: load(&counters.inserted),
            insert_failures: load(&counters.insert_failures),
            cache_hits: resolver.cache_hits,
            cache_misses: resolver.cache_misses,
            rpc_failures: resolver.rpc_failures,
            backfill_runs: load(&counters.backfill_runs),
            backfill_found: load(&counters.backfill_found),
            backfill_failures: load(&counters.backfill_failures),
            breaker_open: resolver.breaker_open,
            endpoints_total: health.endpoints_total,
            endpoints_healthy: health.endpoints_healthy,
        }
    }

    /// Stops accepting tokens and stops the task: an RPC fetch in flight
    /// is abandoned (its claims are released), an insert already running
    /// gets `shutdown_grace` to finish, the rest of the queue is dropped
    /// (it is all in the database for the next run's backfill). Returns
    /// once the task is gone, promptly in any case.
    pub async fn shutdown(&self) {
        let shared = &*self.shared;

        shared.accepting.store(false, Ordering::Relaxed);
        shared.shutdown.send_replace(true);
        shared.wake.notify_one();

        let mut done = shared.done.clone();
        let patience =
            shared.options.shutdown_grace + Duration::from_secs(1);
        // An error means the task is gone already, which is fine too.
        let _ =
            tokio::time::timeout(patience, done.wait_for(|done| *done))
                .await;
    }
}

/// Runs the worker and restarts it if it panics (the sink and the
/// backfill source are foreign code), until shutdown.
async fn supervise(
    shared: Arc<Shared>,
    sink: Arc<dyn TokenSink>,
    backfill: Option<Arc<dyn MissingTokenSource>>,
    done: watch::Sender<bool>,
    enabled: bool,
) {
    let mut stop = shared.shutdown.subscribe();

    if !enabled {
        let _ = stopped(&mut stop).await;
        done.send_replace(true);
        return;
    }

    loop {
        let run = tokio::spawn(run(
            shared.clone(),
            sink.clone(),
            backfill.clone(),
        ));

        match run.await {
            Ok(()) => break,
            Err(failure) if failure.is_panic() => {
                error!(
                    "The token metadata worker panicked, restarting it \
                     in {:?}",
                    shared.options.retry_delay
                );
                if sleep_or_stop(&mut stop, shared.options.retry_delay)
                    .await
                {
                    break;
                }
            }
            // The runtime is shutting down.
            Err(_) => break,
        }
    }

    done.send_replace(true);
}

/// Completes once shutdown is requested (or the worker handle is gone,
/// which amounts to the same).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    // The returned guard must not outlive this statement: it is not
    // `Send`.
    let _ = stop.wait_for(|stop| *stop).await;
}

/// `true` when shutdown was requested before `duration` elapsed.
async fn sleep_or_stop(
    stop: &mut watch::Receiver<bool>,
    duration: Duration,
) -> bool {
    tokio::select! {
        biased;
        _ = stopped(stop) => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

struct Backfill {
    source: Arc<dyn MissingTokenSource>,
    next_at: Instant,
    interval: Duration,
    /// `dropped` counter when the last query ran.
    dropped_seen: u64,
    /// Reported missing last time although the caches knew them.
    suspects: HashSet<Address>,
    failing: bool,
}

/// Tokens the RPC repeatedly could not answer, kept away from everybody
/// else.
///
/// Batches are resolved with shared Multicall3 requests, so one token that
/// makes a node time out (or answer nonsense) fails its neighbours too,
/// trips the RPC breaker and would do so again on every retry. From the
/// second failure on a token is therefore retried *alone*, after the
/// shared queue had its turn, with a doubling delay: the innocent ones
/// resolve right away and the culprit ends up costing one request per
/// hour.
///
/// An outage looks the same from here (everything fails), so the delay
/// only grows past a few minutes for a token that failed although the RPC
/// demonstrably worked for others since its previous failure. After an
/// outage of any length everything is resolved within minutes.
#[derive(Default)]
struct Retries {
    failures: HashMap<Address, Failure>,
    delayed: Vec<(Instant, Queued)>,
    /// Batches that got rows out of the RPC so far.
    successes: u64,
}

#[derive(Clone, Copy, Default)]
struct Failure {
    count: u32,
    /// `Retries::successes` when it failed last.
    successes_seen: u64,
    /// Failed while the RPC worked for others: the token is the problem.
    proven: bool,
}

/// Failures before a token is only ever resolved alone.
const SOLO_AFTER_FAILURES: u32 = 2;
/// Tokens retried alone per round, so the shared queue keeps its turn.
const SOLO_PER_ROUND: usize = 16;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(300);
const MAX_PROVEN_RETRY_DELAY: Duration = Duration::from_secs(3_600);
/// Beyond that the tokens are forgotten (the backfill knows them).
const MAX_TRACKED_RETRIES: usize = 10_000;

impl Retries {
    /// Book keeping after a batch: returns the tokens that go back to the
    /// shared queue, the others wait here to be tried alone.
    fn record(
        &mut self,
        shared: &Shared,
        batch: &[Queued],
        processed: &Processed,
    ) -> Vec<Queued> {
        if processed.progress {
            self.successes += 1;
        }

        if !self.failures.is_empty() {
            let failed: HashSet<Address> = processed
                .unresolved
                .iter()
                .map(|token| token.address)
                .collect();
            for token in batch {
                if !failed.contains(&token.address) {
                    self.failures.remove(&token.address);
                }
            }
        }

        if self.failures.len() + processed.unresolved.len()
            > MAX_TRACKED_RETRIES
        {
            self.failures.clear();
        }

        let now = Instant::now();
        let mut requeue = Vec::new();

        for token in &processed.unresolved {
            let failure = {
                let failure =
                    self.failures.entry(token.address).or_default();
                failure.proven |= failure.count > 0
                    && self.successes > failure.successes_seen;
                failure.count = failure.count.saturating_add(1);
                failure.successes_seen = self.successes;
                *failure
            };

            if failure.count < SOLO_AFTER_FAILURES {
                requeue.push(*token);
                continue;
            }

            if self.delayed.len() >= MAX_TRACKED_RETRIES {
                continue;
            }

            let doublings = (failure.count - SOLO_AFTER_FAILURES).min(16);
            let delay = shared
                .options
                .retry_delay
                .saturating_mul(2u32.saturating_pow(doublings))
                .min(if failure.proven {
                    MAX_PROVEN_RETRY_DELAY
                } else {
                    MAX_RETRY_DELAY
                });

            shared.queue().suspended.insert(token.address);
            self.delayed.push((now + delay, *token));
        }

        requeue
    }

    /// Up to `limit` tokens whose delay is over, the most overdue first.
    fn take_due(&mut self, shared: &Shared, limit: usize) -> Vec<Queued> {
        if self.delayed.is_empty() {
            return Vec::new();
        }

        let now = Instant::now();
        self.delayed.sort_by_key(|(at, _)| *at);
        let due = self
            .delayed
            .iter()
            .take(limit)
            .take_while(|(at, _)| *at <= now)
            .count();

        let mut queue = shared.queue();
        self.delayed
            .drain(..due)
            .map(|(_, token)| {
                queue.suspended.remove(&token.address);
                token
            })
            .collect()
    }

    /// Puts a token back untouched (it was not its turn after all).
    fn put_back(&mut self, shared: &Shared, token: Queued) {
        shared.queue().suspended.insert(token.address);
        self.delayed.push((Instant::now(), token));
    }

    fn next_due(&self) -> Option<Instant> {
        self.delayed.iter().map(|(at, _)| *at).min()
    }
}

async fn run(
    shared: Arc<Shared>,
    sink: Arc<dyn TokenSink>,
    backfill: Option<Arc<dyn MissingTokenSource>>,
) {
    let options = &shared.options;
    let resolver = &shared.resolver;
    let mut stop = shared.shutdown.subscribe();

    let mut backfill = backfill.map(|source| Backfill {
        source,
        // Right away: heal whatever previous runs left behind.
        next_at: Instant::now(),
        interval: options.backfill_interval,
        dropped_seen: 0,
        suspects: HashSet::new(),
        failing: false,
    });

    let mut retries = Retries::default();
    let mut wrong_chain_logged = false;

    // A restart after a panic: nothing is suspended any more.
    shared.queue().suspended.clear();

    loop {
        if *stop.borrow() {
            return;
        }

        // RPC paused: keep everything queued (the queue is bounded and
        // what it drops is backfilled) and wait for the next probe.
        if !resolver.rpc_available() {
            if resolver.wrong_chain() && !wrong_chain_logged {
                wrong_chain_logged = true;
                error!(
                    "Token metadata stays disabled for this run: the RPC \
                     serves another chain"
                );
            }

            let wait = match resolver.rpc_retry_at() {
                Some(at) => at.saturating_duration_since(Instant::now()),
                None => options.retry_delay,
            }
            .max(Duration::from_millis(100));

            if sleep_or_stop(&mut stop, wait).await {
                return;
            }
            continue;
        }

        if let Some(backfill) = &mut backfill {
            let due = Instant::now() >= backfill.next_at;
            let room = shared.queue_len() * 2
                <= shared.options.queue_capacity.max(1);

            if due && room {
                tokio::select! {
                    biased;
                    _ = stopped(&mut stop) => return,
                    _ = run_backfill(&shared, backfill) => {}
                }
            }
        }

        let mut worked = false;
        let mut troubled = false;

        // 1. The shared queue.
        if shared.queue_len() > 0 {
            // Let a batch fill up a little.
            if shared.queue_len() < options.batch_size
                && sleep_or_stop(&mut stop, options.batch_linger).await
            {
                return;
            }

            let batch = shared.take_batch();
            let processed = process(&shared, &*sink, &batch).await;
            if processed.stop {
                return;
            }

            worked = true;
            troubled |= processed.troubled();
            shared.enqueue(retries.record(&shared, &batch, &processed));
            shared.enqueue(processed.not_stored);
        }

        // 2. Tokens that failed before, one at a time.
        for token in retries.take_due(&shared, SOLO_PER_ROUND) {
            if !resolver.rpc_available() {
                // Not its fault: back to the waiting list as it was.
                retries.put_back(&shared, token);
                continue;
            }

            let batch = [token];
            let processed = process(&shared, &*sink, &batch).await;
            if processed.stop {
                return;
            }

            worked = true;
            shared.enqueue(retries.record(&shared, &batch, &processed));
            shared.enqueue(processed.not_stored);
        }

        if troubled {
            // Something is wrong with the RPC or the database: no hot
            // loop on the shared queue.
            if sleep_or_stop(&mut stop, options.retry_delay).await {
                return;
            }
        } else if !worked {
            // Idle: wait for tokens, the next backfill / retry, shutdown.
            let next = [
                backfill.as_ref().map(|backfill| backfill.next_at),
                retries.next_due(),
            ]
            .into_iter()
            .flatten()
            .min();

            tokio::select! {
                biased;
                _ = stopped(&mut stop) => return,
                _ = shared.wake.notified() => {}
                _ = async {
                    match next {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
            }
        }
    }
}

#[derive(Default)]
struct Processed {
    /// The RPC could not be asked about these.
    unresolved: Vec<Queued>,
    /// Resolved, but the sink could not store them.
    not_stored: Vec<Queued>,
    /// The RPC produced rows: it works.
    progress: bool,
    /// Shutdown was requested meanwhile.
    stop: bool,
}

impl Processed {
    fn troubled(&self) -> bool {
        !self.unresolved.is_empty() || !self.not_stored.is_empty()
    }
}

async fn process(
    shared: &Shared,
    sink: &dyn TokenSink,
    batch: &[Queued],
) -> Processed {
    let resolver = &shared.resolver;
    let mut stop = shared.shutdown.subscribe();
    let mut rows = Vec::new();
    let mut processed = Processed::default();

    for (mode, forced) in
        [(ResolveMode::Normal, false), (ResolveMode::Forced, true)]
    {
        let tokens: HashMap<Address, TokenStandard> = batch
            .iter()
            .filter(|token| token.forced == forced)
            .map(|token| (token.address, token.standard))
            .collect();

        if tokens.is_empty() {
            continue;
        }

        let resolution = tokio::select! {
            biased;
            // Abandoning the fetch releases the claims it holds; rows
            // resolved so far are given back as well.
            _ = stopped(&mut stop) => {
                resolver.release(&rows);
                processed.stop = true;
                return processed;
            }
            resolution = resolver.resolve(&tokens, mode) => resolution,
        };
        rows.extend(resolution.rows);
        processed.unresolved.extend(
            resolution.unresolved.into_iter().map(
                |(address, standard)| Queued { address, standard, forced },
            ),
        );
    }

    if rows.is_empty() {
        return processed;
    }
    processed.progress = true;

    match insert(shared, sink, &rows).await {
        Inserted::Yes => {
            // Durable: only now may the caches know about them.
            resolver.mark_stored(&rows).await;
            shared
                .counters
                .inserted
                .fetch_add(rows.len() as u64, Ordering::Relaxed);
        }
        failed => {
            // Not stored: not marked, and free to be resolved again.
            resolver.release(&rows);
            shared
                .counters
                .insert_failures
                .fetch_add(1, Ordering::Relaxed);

            if matches!(failed, Inserted::Stopped) {
                processed.stop = true;
                return processed;
            }

            let failed: HashSet<Address> =
                rows.iter().map(|row| row.address).collect();
            processed.not_stored.extend(
                batch
                    .iter()
                    .filter(|token| failed.contains(&token.address))
                    .copied(),
            );
        }
    }

    processed
}

enum Inserted {
    Yes,
    /// Every attempt failed.
    No,
    /// Shutdown was requested before it could be stored.
    Stopped,
}

async fn insert(
    shared: &Shared,
    sink: &dyn TokenSink,
    rows: &[DatabaseToken],
) -> Inserted {
    let options = &shared.options;
    let mut stop = shared.shutdown.subscribe();
    let mut backoff = options.insert_backoff;

    for attempt in 0..=options.insert_retries {
        let stopping = *stop.borrow();
        // A shutdown lets the attempt that is running finish (within the
        // grace period) but starts no other.
        let limit = if stopping {
            options.shutdown_grace
        } else {
            options.insert_timeout
        };

        let insert = tokio::time::timeout(limit, sink.insert_tokens(rows));
        tokio::pin!(insert);

        let result = tokio::select! {
            result = &mut insert => result,
            _ = stopped(&mut stop), if !stopping => {
                tokio::time::timeout(options.shutdown_grace, &mut insert)
                    .await
                    .unwrap_or_else(Err)
            }
        };

        let error = match result {
            Ok(Ok(())) => return Inserted::Yes,
            Ok(Err(error)) => redact_urls(&format!("{error:#}")),
            Err(_) => format!("no answer within {limit:?}"),
        };

        if *stop.borrow() {
            debug!("Token rows not stored before shutdown: {error}");
            return Inserted::Stopped;
        }

        if attempt == options.insert_retries {
            warn!(
                "Unable to store {} token rows after {} attempts, they \
                 will be resolved again later: {error}",
                rows.len(),
                attempt + 1
            );
            break;
        }

        debug!(
            "Unable to store {} token rows (attempt {}), retrying in \
             {backoff:?}: {error}",
            rows.len(),
            attempt + 1
        );
        if sleep_or_stop(&mut stop, backoff).await {
            return Inserted::Stopped;
        }
        backoff =
            backoff.saturating_mul(2).min(options.insert_max_backoff);
    }

    Inserted::No
}

async fn run_backfill(shared: &Shared, backfill: &mut Backfill) {
    let options = &shared.options;
    let counters = &shared.counters;
    let limit = options.backfill_limit.max(1);

    counters.backfill_runs.fetch_add(1, Ordering::Relaxed);

    let found = tokio::time::timeout(
        options.backfill_timeout,
        backfill.source.missing_tokens(limit),
    )
    .await
    .unwrap_or_else(|_| {
        Err(anyhow::anyhow!(
            "no answer within {:?}",
            options.backfill_timeout
        ))
    });

    let found = match found {
        Ok(found) => {
            if std::mem::take(&mut backfill.failing) {
                info!("Token backfill query works again");
            }
            found
        }
        Err(error) => {
            counters.backfill_failures.fetch_add(1, Ordering::Relaxed);
            let error = redact_urls(&format!("{error:#}"));
            if !std::mem::replace(&mut backfill.failing, true) {
                warn!(
                    "Unable to query the tokens missing from the \
                     database, trying again later: {error}"
                );
            } else {
                debug!("Token backfill query still failing: {error}");
            }
            backfill.next_at = Instant::now() + backfill.interval;
            return;
        }
    };

    counters
        .backfill_found
        .fetch_add(found.len() as u64, Ordering::Relaxed);

    // What memory thinks of them. Tokens it would resolve anyway go the
    // normal way (Redis may still know them: then they come back as
    // suspects next time). Tokens it believes stored are suspects: once
    // is a race with an insert, twice is a cache that is wrong.
    let reported: HashMap<Address, TokenStandard> =
        found.iter().copied().collect();
    let unknown: HashSet<Address> = shared
        .resolver
        .filter_unknown(&reported)
        .into_iter()
        .map(|(address, _)| address)
        .collect();

    let mut suspects = HashSet::new();
    let mut tokens = Vec::with_capacity(found.len());

    for (address, standard) in &reported {
        if unknown.contains(address) {
            tokens.push(Queued {
                address: *address,
                standard: *standard,
                forced: false,
            });
        } else if !shared.resolver.is_known(address) {
            // In flight or recently seen without code: being handled.
        } else if backfill.suspects.contains(address) {
            tokens.push(Queued {
                address: *address,
                standard: *standard,
                forced: true,
            });
        } else {
            suspects.insert(*address);
        }
    }

    let forced = tokens.iter().filter(|token| token.forced).count();
    let wanted = tokens.len();
    backfill.suspects = suspects;

    let queued = shared.enqueue(tokens);

    if queued > 0 {
        info!(
            "Token backfill: {} tokens referenced by stored data have no \
             metadata yet, {queued} queued ({forced} of them known to the \
             cache but not to the database)",
            found.len()
        );
    }

    let dropped = counters.dropped.load(Ordering::Relaxed);
    let dropped_since = dropped != backfill.dropped_seen;
    backfill.dropped_seen = dropped;

    let now = Instant::now();
    if queued > 0 && (found.len() >= limit || queued < wanted) {
        // There is more (a full answer, or more than the queue takes):
        // come back as soon as this is worked off. Never when nothing
        // could be queued, that would be a busy loop on the database.
        backfill.interval = options.backfill_interval;
        backfill.next_at = now + options.batch_linger;
    } else if !found.is_empty() || dropped_since {
        backfill.interval = options.backfill_interval;
        backfill.next_at = now + backfill.interval;
    } else {
        // Nothing missing: look less often.
        backfill.next_at = now + backfill.interval;
        backfill.interval = backfill
            .interval
            .saturating_mul(2)
            .min(options.backfill_max_interval)
            .max(options.backfill_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{
        cache::TokenCache,
        multicall::{
            testing::{fast_options, FakeChain, FakeToken},
            CallError, MetadataFetcher,
        },
    };
    use alloy::primitives::Bytes;
    use std::sync::atomic::AtomicUsize;

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    fn batch(numbers: &[u64]) -> HashMap<Address, TokenStandard> {
        numbers.iter().map(|n| (addr(*n), TokenStandard::Erc20)).collect()
    }

    /// ClickHouse stand-in: the `tokens` table plus the transfers that
    /// reference token addresses (for the anti-join).
    #[derive(Default)]
    struct FakeDb {
        tokens: Mutex<Vec<DatabaseToken>>,
        referenced: Mutex<Vec<(Address, TokenStandard)>>,
        failures_left: AtomicUsize,
        down: AtomicBool,
        hang: AtomicBool,
        insert_attempts: AtomicUsize,
        backfill_queries: AtomicUsize,
        /// What the resolver knew at insert time: must be nothing.
        marked_before_insert: AtomicBool,
        resolver: Mutex<Option<TokenResolver>>,
    }

    impl FakeDb {
        fn stored(&self) -> Vec<Address> {
            let mut stored: Vec<Address> = self
                .tokens
                .lock()
                .unwrap()
                .iter()
                .map(|row| row.address)
                .collect();
            stored.sort();
            stored
        }

        fn reference(&self, numbers: &[u64]) {
            self.referenced.lock().unwrap().extend(
                numbers.iter().map(|n| (addr(*n), TokenStandard::Erc20)),
            );
        }
    }

    impl TokenSink for FakeDb {
        fn insert_tokens<'a>(
            &'a self,
            rows: &'a [DatabaseToken],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.insert_attempts.fetch_add(1, Ordering::SeqCst);

                if self.hang.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }

                if let Some(resolver) = &*self.resolver.lock().unwrap() {
                    if rows
                        .iter()
                        .any(|row| resolver.is_known(&row.address))
                    {
                        self.marked_before_insert
                            .store(true, Ordering::SeqCst);
                    }
                }

                let failing = self.down.load(Ordering::SeqCst)
                    || self
                        .failures_left
                        .fetch_update(
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                            |n| n.checked_sub(1),
                        )
                        .is_ok();
                if failing {
                    anyhow::bail!(
                        "clickhouse at http://user:hunter2@db:8123 is down"
                    );
                }

                self.tokens.lock().unwrap().extend_from_slice(rows);
                Ok(())
            })
        }
    }

    impl MissingTokenSource for FakeDb {
        fn missing_tokens<'a>(
            &'a self,
            limit: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>>
        {
            Box::pin(async move {
                self.backfill_queries.fetch_add(1, Ordering::SeqCst);
                let stored = self.stored();
                Ok(self
                    .referenced
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(address, _)| !stored.contains(address))
                    .take(limit)
                    .copied()
                    .collect())
            })
        }
    }

    /// A node that never answers.
    struct Stuck;

    impl EthCaller for Stuck {
        fn call(
            &self,
            _to: Address,
            _data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            Box::pin(std::future::pending())
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            Box::pin(async { Ok(1) })
        }
    }

    fn options() -> TokenWorkerOptions {
        TokenWorkerOptions {
            resolver: TokenResolverOptions {
                fetch: fast_options(),
                ..TokenResolverOptions::default()
            },
            ..TokenWorkerOptions::default()
        }
    }

    fn resolver(
        caller: Arc<dyn EthCaller>,
        cache: Option<Arc<dyn TokenCache>>,
    ) -> TokenResolver {
        TokenResolver::from_parts(
            1,
            Some(
                MetadataFetcher::new(caller, fast_options())
                    .expect_chain_id(1),
            ),
            cache,
            &options().resolver,
        )
    }

    struct Setup {
        chain: Arc<FakeChain>,
        db: Arc<FakeDb>,
        worker: TokenWorker,
        handle: JoinHandle<()>,
        resolver: TokenResolver,
    }

    fn setup(with_backfill: bool, options: TokenWorkerOptions) -> Setup {
        let chain = FakeChain::new();
        for n in 0..100u64 {
            chain.add(addr(n), FakeToken::erc20("Token", "TKN", 18));
        }
        let db = Arc::new(FakeDb::default());
        let resolver = resolver(chain.clone(), None);
        *db.resolver.lock().unwrap() = Some(resolver.clone());

        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver.clone(),
            db.clone(),
            with_backfill
                .then(|| db.clone() as Arc<dyn MissingTokenSource>),
            options,
        );

        Setup { chain, db, worker, handle, resolver }
    }

    /// Lets the worker run for `seconds` of (paused) time.
    async fn settle(seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn rows_are_inserted_once_and_marked_after_the_insert() {
        let s = setup(false, options());

        s.worker.discover(&batch(&[1, 2, 3]));
        // Seen again by the next batches while still queued / in flight.
        s.worker.discover(&batch(&[2, 3, 4]));
        settle(2).await;
        s.worker.discover(&batch(&[1, 2, 3, 4]));
        settle(2).await;

        assert_eq!(
            s.db.stored(),
            vec![addr(1), addr(2), addr(3), addr(4)]
        );
        assert!(!s.db.marked_before_insert.load(Ordering::SeqCst));
        for n in 1..=4 {
            assert!(s.resolver.is_known(&addr(n)));
            assert_eq!(s.chain.token_calls(&addr(n)), 3, "token {n}");
        }

        let stats = s.worker.stats();
        assert!(stats.enabled);
        assert_eq!(stats.queue_depth, 0);
        assert_eq!(stats.queued, 4);
        assert_eq!(stats.resolved, 4);
        assert_eq!(stats.inserted, 4);
        assert_eq!(stats.dropped, 0);
        assert!(!stats.breaker_open);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn discover_never_blocks_when_the_queue_is_full() {
        let db = Arc::new(FakeDb::default());
        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(Arc::new(Stuck), None),
            db.clone(),
            None,
            TokenWorkerOptions {
                queue_capacity: 2,
                batch_size: 2,
                ..options()
            },
        );

        // The worker takes the first two and gets stuck on the node.
        worker.discover(&batch(&[1, 2]));
        settle(1).await;
        assert_eq!(worker.stats().queue_depth, 0);

        // `discover` is synchronous: if it returns, it did not wait. The
        // clock is paused and nothing else runs meanwhile, so it cannot
        // have been helped by the (stuck) worker either.
        let before = Instant::now();
        let all: Vec<u64> = (10..1_010).collect();
        worker.discover(&batch(&all));
        worker.discover(&batch(&all));
        assert_eq!(Instant::now(), before);

        let stats = worker.stats();
        assert_eq!(stats.queue_depth, 2);
        assert_eq!(stats.queued, 4);
        assert_eq!(stats.dropped, 998 + 998);
        assert!(db.stored().is_empty());

        // Still stuck a long time later, still not blocking anybody.
        settle(3_600).await;
        worker.discover(&batch(&[5_000]));
        assert_eq!(worker.stats().queue_depth, 2);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn sink_failure_is_not_marked_and_is_retried() {
        let s = setup(false, options());

        // Two failed attempts, then it works: same rows, no refetch.
        s.db.failures_left.store(2, Ordering::SeqCst);
        s.worker.discover(&batch(&[1]));
        settle(10).await;
        assert_eq!(s.db.stored(), vec![addr(1)]);
        assert_eq!(s.db.insert_attempts.load(Ordering::SeqCst), 3);
        assert_eq!(s.chain.token_calls(&addr(1)), 3);

        // ClickHouse down for good: every attempt fails.
        s.db.down.store(true, Ordering::SeqCst);
        s.worker.discover(&batch(&[2]));
        settle(40).await;
        assert!(s.worker.stats().insert_failures >= 1);
        assert_eq!(s.db.stored(), vec![addr(1)]);
        // Not marked anywhere, so it can be resolved again.
        assert!(!s.resolver.is_known(&addr(2)));

        // Back: the worker retried on its own, no new sighting needed.
        s.db.down.store(false, Ordering::SeqCst);
        settle(60).await;
        assert_eq!(s.db.stored(), vec![addr(1), addr(2)]);
        assert!(s.resolver.is_known(&addr(2)));
        assert!(!s.db.marked_before_insert.load(Ordering::SeqCst));

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn backfill_heals_dropped_tokens() {
        let s = setup(
            true,
            TokenWorkerOptions {
                queue_capacity: 4,
                batch_size: 2,
                ..options()
            },
        );
        settle(1).await;
        // The startup backfill ran and found nothing.
        assert_eq!(s.db.backfill_queries.load(Ordering::SeqCst), 1);

        // 20 tokens are stored with their transfers, the queue takes 4.
        let all: Vec<u64> = (1..=20).collect();
        s.db.reference(&all);
        s.worker.discover(&batch(&all));
        assert_eq!(s.worker.stats().dropped, 16);

        settle(10).await;
        assert_eq!(s.db.stored().len(), 4);

        // The next backfill ticks find the rest in the database.
        settle(300).await;
        assert_eq!(s.db.stored().len(), 20);
        assert_eq!(s.db.tokens.lock().unwrap().len(), 20, "no duplicates");
        for n in all {
            assert_eq!(s.chain.token_calls(&addr(n)), 3, "token {n}");
        }

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn backfill_heals_a_long_rpc_outage_and_data_never_seen_live() {
        let s = setup(true, options());
        s.chain.offline.store(true, Ordering::SeqCst);

        // Seen live during the outage, and indexed by a previous run.
        s.db.reference(&[1, 2, 3, 4, 5]);
        s.worker.discover(&batch(&[1, 2]));
        settle(6 * 3_600).await;
        assert!(s.db.stored().is_empty());
        assert!(s.worker.stats().rpc_failures > 0);

        s.chain.offline.store(false, Ordering::SeqCst);
        settle(900).await;
        assert_eq!(s.db.stored().len(), 5);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn backfill_overrides_a_cache_that_wrongly_knows_a_token() {
        let s = setup(true, options());

        // The cache believes token 7 is stored; the database lost it
        // (table reset without flushing Redis, lost insert...).
        let row = DatabaseToken {
            address: addr(7),
            name: "Token".into(),
            symbol: "TKN".into(),
            decimals: 18,
            r#type: "ERC20".into(),
            chain: 1,
        };
        s.resolver.mark_stored(&[row]).await;
        s.db.reference(&[7]);

        s.worker.discover(&batch(&[7]));
        settle(5).await;
        assert!(s.db.stored().is_empty(), "the cache is trusted at first");

        // Reported missing twice in a row: fetched again and stored.
        settle(200).await;
        assert_eq!(s.db.stored(), vec![addr(7)]);
        assert_eq!(s.db.tokens.lock().unwrap().len(), 1);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn backfill_is_skipped_while_the_rpc_breaker_is_open() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 18));
        chain.offline.store(true, Ordering::SeqCst);
        let db = Arc::new(FakeDb::default());
        db.reference(&[1]);

        let fetch = crate::tokens::multicall::FetchOptions {
            breaker_cooldown: Duration::from_secs(3_600),
            breaker_max_cooldown: Duration::from_secs(3_600),
            ..fast_options()
        };
        let resolver = TokenResolver::from_parts(
            1,
            Some(
                MetadataFetcher::new(chain.clone(), fetch)
                    .expect_chain_id(1),
            ),
            None,
            &options().resolver,
        );
        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver,
            db.clone(),
            Some(db.clone()),
            options(),
        );

        settle(1_800).await;
        assert!(worker.stats().breaker_open);
        // The startup query only; none while the breaker is open.
        assert_eq!(db.backfill_queries.load(Ordering::SeqCst), 1);
        assert_eq!(worker.stats().queue_depth, 1, "kept, not lost");

        chain.offline.store(false, Ordering::SeqCst);
        settle(3_600).await;
        assert_eq!(db.stored(), vec![addr(1)]);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn purged_block_tokens_are_resolved_harmlessly() {
        let s = setup(true, options());

        // Token 1 exists on both forks; token 500 was deployed in the
        // purged block only: no code on the canonical chain, and nothing
        // stored references it any more.
        s.worker.discover(&batch(&[1, 500]));
        settle(5).await;
        assert_eq!(s.db.stored(), vec![addr(1)]);
        assert_eq!(s.worker.stats().codeless, 1);
        assert_eq!(s.worker.stats().queue_depth, 0);

        // Nobody asks for it again: no loop, no row, no error.
        let calls = s.chain.token_calls(&addr(500));
        settle(24 * 3_600).await;
        assert_eq!(s.chain.token_calls(&addr(500)), calls);
        assert_eq!(s.db.stored(), vec![addr(1)]);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn codeless_addresses_do_not_starve_the_backfill() {
        let s = setup(
            true,
            TokenWorkerOptions { backfill_limit: 2, ..options() },
        );
        // Two self-destructed contracts come first in every answer.
        s.db.reference(&[500, 501, 1, 2]);

        settle(3 * 3_600).await;
        assert_eq!(
            s.db.stored(),
            vec![addr(1), addr(2), addr(500), addr(501)]
        );
        let blank =
            s.db.tokens
                .lock()
                .unwrap()
                .iter()
                .find(|row| row.address == addr(500))
                .cloned();
        assert!(blank.is_some_and(|row| row.name.is_empty()));

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_is_prompt_even_when_everything_hangs() {
        // Stuck on the RPC.
        let db = Arc::new(FakeDb::default());
        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(Arc::new(Stuck), None),
            db.clone(),
            Some(db.clone()),
            options(),
        );
        worker.discover(&batch(&[1, 2, 3]));
        settle(5).await;

        let started = Instant::now();
        worker.shutdown().await;
        handle.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));

        // No more work is accepted.
        worker.discover(&batch(&[9]));
        assert_eq!(worker.stats().queue_depth, 0);
        // Idempotent.
        worker.shutdown().await;

        // Stuck on ClickHouse: the running insert gets the grace period.
        let s = setup(false, options());
        s.db.hang.store(true, Ordering::SeqCst);
        s.worker.discover(&batch(&[1]));
        settle(5).await;
        assert_eq!(s.db.insert_attempts.load(Ordering::SeqCst), 1);

        let started = Instant::now();
        s.worker.shutdown().await;
        s.handle.await.unwrap();
        assert!(started.elapsed() <= Duration::from_secs(6));
        // The claim was given back: nothing pretends it is stored.
        assert!(!s.resolver.is_known(&addr(1)));

        // Idle worker.
        let s = setup(true, options());
        settle(5).await;
        let started = Instant::now();
        s.worker.shutdown().await;
        s.handle.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_caller_the_worker_is_inert() {
        let db = Arc::new(FakeDb::default());
        db.reference(&[1]);
        let (worker, handle) = TokenWorker::spawn(
            1,
            None,
            Some("redis://127.0.0.1:1/"),
            db.clone(),
            Some(db.clone()),
            TokenWorkerOptions::default(),
        )
        .unwrap();

        worker.discover(&batch(&[1, 2, 3]));
        settle(600).await;

        let stats = worker.stats();
        assert!(!stats.enabled);
        assert_eq!(
            (stats.queue_depth, stats.queued, stats.dropped),
            (0, 0, 0)
        );
        assert_eq!(db.backfill_queries.load(Ordering::SeqCst), 0);
        assert!(!handle.is_finished());

        worker.shutdown().await;
        handle.await.unwrap();

        // An invalid Redis URL is the only startup error.
        assert!(TokenWorker::spawn(
            1,
            Some(FakeChain::new() as Arc<dyn EthCaller>),
            Some("not a url"),
            db.clone(),
            None,
            TokenWorkerOptions::default(),
        )
        .is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_sink_does_not_kill_the_worker() {
        struct Panicky {
            inner: Arc<FakeDb>,
            panics_left: AtomicUsize,
        }

        impl TokenSink for Panicky {
            fn insert_tokens<'a>(
                &'a self,
                rows: &'a [DatabaseToken],
            ) -> BoxFuture<'a, anyhow::Result<()>> {
                let panic = self
                    .panics_left
                    .fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |n| n.checked_sub(1),
                    )
                    .is_ok();
                if panic {
                    panic!("sink bug");
                }
                self.inner.insert_tokens(rows)
            }
        }

        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 18));
        let db = Arc::new(FakeDb::default());
        db.reference(&[1]);
        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(chain.clone(), None),
            Arc::new(Panicky {
                inner: db.clone(),
                panics_left: AtomicUsize::new(1),
            }),
            Some(db.clone()),
            options(),
        );

        settle(3_600).await;
        assert_eq!(db.stored(), vec![addr(1)]);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    /// A node that cannot answer anything involving one token.
    struct Poisoned {
        chain: Arc<FakeChain>,
        poison: Address,
        poisoned_requests: AtomicUsize,
    }

    impl EthCaller for Poisoned {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            let needle = self.poison.as_slice();
            let involved = to == self.poison
                || data.windows(needle.len()).any(|w| w == needle);

            if involved {
                self.poisoned_requests.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async {
                    Err(CallError::Transient("upstream timeout".into()))
                });
            }
            self.chain.call(to, data)
        }

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            self.chain.chain_id()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_token_the_rpc_chokes_on_does_not_stall_the_others() {
        let chain = FakeChain::new();
        for n in 1..=10u64 {
            chain.add(addr(n), FakeToken::erc20("Token", "TKN", 18));
        }
        let node = Arc::new(Poisoned {
            chain: chain.clone(),
            poison: addr(66),
            poisoned_requests: AtomicUsize::new(0),
        });
        let db = Arc::new(FakeDb::default());
        let all: Vec<u64> = (1..=10).chain([66]).collect();
        db.reference(&all);

        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(node.clone(), None),
            db.clone(),
            Some(db.clone()),
            options(),
        );

        // All 11 share one Multicall3 request that can never succeed.
        worker.discover(&batch(&all));
        settle(120).await;
        let good: Vec<Address> = (1..=10).map(addr).collect();
        assert_eq!(db.stored(), good);

        // The culprit is retried alone, less and less often, and the
        // backfill reporting it every minute does not change that.
        let early = node.poisoned_requests.load(Ordering::SeqCst);
        settle(24 * 3_600).await;
        let late = node.poisoned_requests.load(Ordering::SeqCst);
        // 4 attempts (retries) per try, about one try per hour.
        assert!(late - early <= 4 * 40, "{early} -> {late}");
        assert_eq!(db.stored(), good);

        // A new token is not held back by it.
        chain.add(addr(11), FakeToken::erc20("Token", "TKN", 18));
        worker.discover(&batch(&[11]));
        settle(5).await;
        assert_eq!(db.stored().len(), 11);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn the_handle_is_usable_from_spawned_tasks() {
        fn assert_send<T: Send>(value: T) -> T {
            value
        }

        let s = setup(false, options());
        let worker = assert_send(s.worker.clone());
        let tokens = batch(&[1]);

        tokio::spawn(async move {
            worker.discover(&tokens);
            let _ = worker.stats();
        })
        .await
        .unwrap();

        let worker = s.worker.clone();
        tokio::spawn(assert_send(async move { worker.shutdown().await }))
            .await
            .unwrap();
        s.handle.await.unwrap();
    }
}
