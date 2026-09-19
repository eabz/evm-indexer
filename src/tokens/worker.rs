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
//! database reports as missing is fetched again whatever memory / Redis
//! believe (a `tokens` table that was reset without flushing Redis, a lost
//! insert), unless it was stored so recently that the query could not see
//! it yet. The listing is paged with a cursor, so tokens that cannot be
//! resolved for now never keep the others from being reported.
//!
//! "Nothing there" is never final either: blank rows are what a stale or
//! lying node makes of a good token, so the database is periodically asked
//! for them ([`MissingTokenSource::blank_tokens`]) and the ones that are
//! due are verified again; `tokens` is a `ReplacingMergeTree`, a row with
//! metadata replaces the blank one.
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
///
/// **Ordering.** Every listing must be ordered by address, ascending, and
/// `after` means `address > after`: the worker pages through the listing
/// with that cursor and starts over when a page comes back short. A
/// random or unstable order would make it skip tokens.
pub trait MissingTokenSource: Send + Sync + 'static {
    /// The first `limit` token addresses (by address) referenced by
    /// stored data but absent from `tokens`.
    fn missing_tokens<'a>(
        &'a self,
        limit: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>>;

    /// [`missing_tokens`](Self::missing_tokens) continued after a cursor.
    ///
    /// Implement it: the default only knows the first page, so tokens
    /// that cannot be resolved (and therefore stay in the listing) would
    /// eventually fill that page and hide everything behind them.
    fn missing_tokens_after<'a>(
        &'a self,
        after: Option<Address>,
        limit: usize,
    ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>> {
        match after {
            None => self.missing_tokens(limit),
            Some(_) => Box::pin(async { Ok(Vec::new()) }),
        }
    }

    /// Up to `limit` tokens (by address, after the cursor) whose stored
    /// row is blank (`name = '' AND symbol = '' AND decimals = 0`) and
    /// was written more than `older_than` ago (`_version`, the insert
    /// time). They are verified again: a stale node says "nothing there"
    /// about perfectly good tokens. The default lists nothing.
    fn blank_tokens<'a>(
        &'a self,
        after: Option<Address>,
        limit: usize,
        older_than: Duration,
    ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>> {
        let _ = (after, limit, older_than);
        Box::pin(async { Ok(Vec::new()) })
    }
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
    /// followed by the next page as soon as the queue has room.
    pub backfill_limit: usize,
    pub backfill_timeout: Duration,
    /// Never two database listings closer than this, whatever the state
    /// of the queue (they are heavy anti-joins).
    pub backfill_min_interval: Duration,
    /// Time between two passes over the blank rows of the database.
    pub blank_recheck_interval: Duration,
    /// Only blank rows written longer ago than this are listed.
    pub blank_recheck_age: Duration,
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
            backfill_min_interval: Duration::from_secs(5),
            blank_recheck_interval: Duration::from_secs(3_600),
            blank_recheck_age: Duration::from_secs(24 * 3_600),
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
    /// Tokens answered by one endpoint but not confirmed by a second,
    /// independent one: nothing was stored (tried again later).
    pub unconfirmed: u64,
    /// Blank rows that were verified again / that got metadata then.
    pub blank_rechecked: u64,
    pub blank_healed: u64,
    pub backfill_runs: u64,
    /// Addresses the backfill reported as missing.
    pub backfill_found: u64,
    pub backfill_failures: u64,
    /// The RPC circuit breaker is open (or every endpoint serves another
    /// chain): nothing is being resolved right now.
    pub breaker_open: bool,
    pub endpoints_total: usize,
    pub endpoints_healthy: usize,
    /// Endpoints banned for contradicting the others.
    pub endpoints_distrusted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Seen by the pipeline: the caches are trusted.
    Seen,
    /// Reported missing by the database: the caches are not asked.
    Missing,
    /// A blank row due to be verified again.
    Recheck,
}

#[derive(Debug, Clone, Copy)]
struct Queued {
    address: Address,
    standard: TokenStandard,
    kind: Kind,
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
    blank_rechecked: AtomicU64,
    blank_healed: AtomicU64,
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
            Queued { address, standard, kind: Kind::Seen }
        }));
    }

    /// Tells the worker how far the indexer has got (the highest block
    /// handed to the writer is fine). NEVER blocks: one atomic store. The
    /// RPC backend uses it to recognize stale nodes: a node behind the
    /// indexer cannot know the contracts it is asked about.
    pub fn set_head(&self, block: u64) {
        self.shared.resolver.set_head(block);
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
            unconfirmed: resolver.unconfirmed,
            blank_rechecked: load(&counters.blank_rechecked),
            blank_healed: load(&counters.blank_healed),
            backfill_runs: load(&counters.backfill_runs),
            backfill_found: load(&counters.backfill_found),
            backfill_failures: load(&counters.backfill_failures),
            breaker_open: resolver.breaker_open,
            endpoints_total: health.endpoints_total,
            endpoints_healthy: health.endpoints_healthy,
            endpoints_distrusted: health.endpoints_distrusted,
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
    /// Where the next page of the missing tokens listing starts.
    cursor: Option<Address>,
    /// Anything was queued since the cursor last started over.
    pass_found: bool,
    /// `dropped` counter when the last query ran.
    dropped_seen: u64,
    failing: bool,
    /// The pass over the blank rows.
    blank_next_at: Instant,
    blank_cursor: Option<Address>,
    /// Earliest time any database listing may run again.
    floor_at: Instant,
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
            // A blank row that could not be verified again stays what it
            // is; the next pass over the blank rows lists it again.
            if token.kind == Kind::Recheck {
                continue;
            }

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

    fn is_proven(&self, address: &Address) -> bool {
        self.failures.get(address).is_some_and(|failure| failure.proven)
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
        cursor: None,
        pass_found: false,
        dropped_seen: 0,
        failing: false,
        // Not at startup: the missing tokens go first.
        blank_next_at: Instant::now() + options.backfill_interval,
        blank_cursor: None,
        floor_at: Instant::now(),
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
            if !resolver.wrong_chain() {
                wrong_chain_logged = false;
            } else if !wrong_chain_logged {
                wrong_chain_logged = true;
                error!(
                    "Token metadata is disabled: the RPC serves another \
                     chain (it is asked again periodically)"
                );
            }

            let wait = match resolver
                .rpc_retry_at()
                .or_else(|| resolver.chain_recheck_at())
            {
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
            let now = Instant::now();
            let room = shared.queue_len() * 2
                <= shared.options.queue_capacity.max(1);

            if room && now >= backfill.floor_at {
                if now >= backfill.next_at {
                    tokio::select! {
                        biased;
                        _ = stopped(&mut stop) => return,
                        _ = run_backfill(&shared, backfill) => {}
                    }
                } else if now >= backfill.blank_next_at {
                    tokio::select! {
                        biased;
                        _ = stopped(&mut stop) => return,
                        _ = run_blank_recheck(&shared, backfill) => {}
                    }
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
            let processed = process(&shared, &*sink, &batch, false).await;
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

            // A token that fails while the RPC works for everybody else
            // is the problem itself: its failures open no breaker.
            let quiet = retries.is_proven(&token.address);
            let batch = [token];
            let processed = process(&shared, &*sink, &batch, quiet).await;
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
                backfill.as_ref().map(|backfill| {
                    backfill
                        .next_at
                        .min(backfill.blank_next_at)
                        .max(backfill.floor_at)
                }),
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
    quiet: bool,
) -> Processed {
    let resolver = &shared.resolver;
    let counters = &shared.counters;
    let mut stop = shared.shutdown.subscribe();
    let mut processed = Processed::default();

    for kind in [Kind::Seen, Kind::Missing, Kind::Recheck] {
        let tokens: HashMap<Address, TokenStandard> = batch
            .iter()
            .filter(|token| token.kind == kind)
            .map(|token| (token.address, token.standard))
            .collect();

        if tokens.is_empty() {
            continue;
        }

        let mode = match kind {
            Kind::Seen => ResolveMode::Normal,
            Kind::Missing | Kind::Recheck => ResolveMode::Forced,
        };

        let resolution = tokio::select! {
            biased;
            // Abandoning the fetch releases the claims it holds.
            _ = stopped(&mut stop) => {
                processed.stop = true;
                return processed;
            }
            resolution = resolver.resolve_with(&tokens, mode, quiet) => {
                resolution
            }
        };

        processed.unresolved.extend(
            resolution.unresolved.into_iter().map(
                |(address, standard)| Queued { address, standard, kind },
            ),
        );

        let rows = resolution.rows;
        if rows.is_empty() {
            continue;
        }
        processed.progress = true;

        match insert(shared, sink, &rows).await {
            Inserted::Yes => {
                // Durable: only now may the caches know about them. The
                // cache write is not worth delaying a shutdown for (the
                // rows are in the database, which is what counts).
                let mark = async {
                    if kind == Kind::Recheck {
                        resolver.mark_rechecked(&rows).await
                    } else {
                        resolver.mark_stored(&rows).await
                    }
                };
                tokio::select! {
                    biased;
                    _ = mark => {}
                    _ = stopped(&mut stop) => processed.stop = true,
                }

                counters
                    .inserted
                    .fetch_add(rows.len() as u64, Ordering::Relaxed);

                if kind == Kind::Recheck {
                    let healed = rows
                        .iter()
                        .filter(|row| !super::cache::is_blank(row))
                        .count();
                    counters
                        .blank_rechecked
                        .fetch_add(rows.len() as u64, Ordering::Relaxed);
                    counters
                        .blank_healed
                        .fetch_add(healed as u64, Ordering::Relaxed);
                    if healed > 0 {
                        info!(
                            "{healed} tokens that were stored without \
                             metadata have some now"
                        );
                    }
                }

                if processed.stop {
                    return processed;
                }
            }
            failed => {
                // Not stored: not marked, and free to be resolved again.
                resolver.release(&rows);
                counters.insert_failures.fetch_add(1, Ordering::Relaxed);

                if matches!(failed, Inserted::Stopped) {
                    processed.stop = true;
                    return processed;
                }

                let failed: HashSet<Address> =
                    rows.iter().map(|row| row.address).collect();
                processed.not_stored.extend(
                    batch
                        .iter()
                        .filter(|token| {
                            token.kind != Kind::Recheck
                                && failed.contains(&token.address)
                        })
                        .copied(),
                );
            }
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

/// Runs a database listing with the timeout and the failure bookkeeping
/// both passes share. `None`: it failed (already reported).
async fn listing(
    shared: &Shared,
    backfill: &mut Backfill,
    what: &str,
    query: BoxFuture<'_, anyhow::Result<Vec<(Address, TokenStandard)>>>,
) -> Option<Vec<(Address, TokenStandard)>> {
    let options = &shared.options;

    let found = tokio::time::timeout(options.backfill_timeout, query)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "no answer within {:?}",
                options.backfill_timeout
            ))
        });

    // Heavy queries: never back to back, whatever they found.
    backfill.floor_at = Instant::now() + options.backfill_min_interval;

    match found {
        Ok(found) => {
            if std::mem::take(&mut backfill.failing) {
                info!("Token backfill queries work again");
            }
            Some(found)
        }
        Err(error) => {
            shared
                .counters
                .backfill_failures
                .fetch_add(1, Ordering::Relaxed);
            let error = redact_urls(&format!("{error:#}"));
            if !std::mem::replace(&mut backfill.failing, true) {
                warn!(
                    "Unable to query the {what} of the database, trying \
                     again later: {error}"
                );
            } else {
                debug!("Token backfill query still failing: {error}");
            }
            None
        }
    }
}

async fn run_backfill(shared: &Shared, backfill: &mut Backfill) {
    let options = &shared.options;
    let counters = &shared.counters;
    let limit = options.backfill_limit.max(1);

    counters.backfill_runs.fetch_add(1, Ordering::Relaxed);

    let started = Instant::now();
    let source = backfill.source.clone();
    let query = source.missing_tokens_after(backfill.cursor, limit);

    let Some(found) =
        listing(shared, backfill, "tokens missing from", query).await
    else {
        backfill.next_at = Instant::now() + backfill.interval;
        return;
    };

    counters
        .backfill_found
        .fetch_add(found.len() as u64, Ordering::Relaxed);

    // The database is the authority: what it lacks is fetched whatever
    // the caches believe, except what is being worked on, what has no
    // code for now, and what was stored too recently for this very query
    // to have seen it.
    let tokens: Vec<Queued> = shared
        .resolver
        .missing_in_database(&found, started)
        .into_iter()
        .map(|(address, standard)| Queued {
            address,
            standard,
            kind: Kind::Missing,
        })
        .collect();

    let wanted = tokens.len();
    let queued = shared.enqueue(tokens);

    if queued > 0 {
        info!(
            "Token backfill: {} tokens referenced by stored data have no \
             metadata yet, {queued} queued",
            found.len()
        );
    }

    let dropped = counters.dropped.load(Ordering::Relaxed);
    let dropped_since = dropped != backfill.dropped_seen;
    backfill.dropped_seen = dropped;
    backfill.pass_found |= queued > 0;

    let full_page = found.len() >= limit;
    let now = Instant::now();

    if queued < wanted {
        // The queue is full: the same page again once there is room.
        backfill.next_at = now;
    } else if full_page {
        // There is more: the next page as soon as there is room (and the
        // floor between two queries allows).
        backfill.cursor = found.last().map(|(address, _)| *address);
        backfill.next_at = now;
    } else {
        // End of the listing: start over later, less and less often
        // while there is nothing to do.
        let idle =
            !std::mem::take(&mut backfill.pass_found) && !dropped_since;
        backfill.cursor = None;

        if idle {
            backfill.next_at = now + backfill.interval;
            backfill.interval = backfill
                .interval
                .saturating_mul(2)
                .min(options.backfill_max_interval)
                .max(options.backfill_interval);
        } else {
            backfill.interval = options.backfill_interval;
            backfill.next_at = now + backfill.interval;
        }
    }
}

/// One page of the pass over the blank rows of the database: the ones
/// that are due (nobody vouches for them any more) are verified again.
async fn run_blank_recheck(shared: &Shared, backfill: &mut Backfill) {
    let options = &shared.options;
    let limit = options.backfill_limit.max(1);

    let source = backfill.source.clone();
    let query = source.blank_tokens(
        backfill.blank_cursor,
        limit,
        options.blank_recheck_age,
    );

    let Some(listed) =
        listing(shared, backfill, "blank token rows", query).await
    else {
        backfill.blank_next_at =
            Instant::now() + options.blank_recheck_interval;
        return;
    };

    let due = shared.resolver.blank_recheck_due(&listed).await;
    let wanted = due.len();

    let queued =
        shared.enqueue(due.into_iter().map(|(address, standard)| {
            Queued { address, standard, kind: Kind::Recheck }
        }));

    if queued > 0 {
        debug!("{queued} blank token rows are verified again");
    }

    let now = Instant::now();
    if queued < wanted {
        backfill.blank_next_at = now;
    } else if listed.len() >= limit {
        backfill.blank_cursor = listed.last().map(|(address, _)| *address);
        backfill.blank_next_at = now;
    } else {
        backfill.blank_cursor = None;
        backfill.blank_next_at = now + options.blank_recheck_interval;
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
        blank_queries: AtomicUsize,
        /// When every database listing (of either kind) ran.
        query_times: Mutex<Vec<Instant>>,
        /// Insert time of the latest row of an address (`_version`).
        written_at: Mutex<HashMap<Address, Instant>>,
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
            stored.dedup();
            stored
        }

        /// The row a `FINAL` read returns.
        fn latest(&self, address: Address) -> Option<DatabaseToken> {
            self.tokens
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|row| row.address == address)
                .cloned()
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

                let now = Instant::now();
                let mut written = self.written_at.lock().unwrap();
                for row in rows {
                    written.insert(row.address, now);
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
            self.missing_tokens_after(None, limit)
        }

        fn missing_tokens_after<'a>(
            &'a self,
            after: Option<Address>,
            limit: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>>
        {
            Box::pin(async move {
                self.backfill_queries.fetch_add(1, Ordering::SeqCst);
                self.query_times.lock().unwrap().push(Instant::now());

                let stored: HashSet<Address> = self
                    .tokens
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|row| row.address)
                    .collect();

                // ORDER BY address, address > after, LIMIT limit.
                let mut missing: Vec<(Address, TokenStandard)> = self
                    .referenced
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(address, _)| {
                        !stored.contains(address)
                            && after.is_none_or(|after| *address > after)
                    })
                    .copied()
                    .collect();
                missing.sort();
                missing.dedup();
                missing.truncate(limit);
                Ok(missing)
            })
        }

        fn blank_tokens<'a>(
            &'a self,
            after: Option<Address>,
            limit: usize,
            older_than: Duration,
        ) -> BoxFuture<'a, anyhow::Result<Vec<(Address, TokenStandard)>>>
        {
            Box::pin(async move {
                self.blank_queries.fetch_add(1, Ordering::SeqCst);
                self.query_times.lock().unwrap().push(Instant::now());
                let now = Instant::now();

                // The latest row of every address (ReplacingMergeTree).
                let mut latest: HashMap<Address, &DatabaseToken> =
                    HashMap::new();
                let tokens = self.tokens.lock().unwrap();
                for row in tokens.iter() {
                    latest.insert(row.address, row);
                }
                let written = self.written_at.lock().unwrap();

                let mut blank: Vec<(Address, TokenStandard)> = latest
                    .values()
                    .filter(|row| {
                        crate::tokens::cache::is_blank(row)
                            && after
                                .is_none_or(|after| row.address > after)
                            && written.get(&row.address).is_some_and(
                                |at| now.duration_since(*at) >= older_than,
                            )
                    })
                    .map(|row| (row.address, TokenStandard::Erc20))
                    .collect();
                blank.sort();
                blank.truncate(limit);
                Ok(blank)
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
    async fn a_proven_troublemaker_opens_no_circuit_breaker() {
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

        // A real breaker this time: 30 seconds, doubling.
        let fetch = crate::tokens::multicall::FetchOptions {
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(300),
            ..fast_options()
        };
        let resolver = TokenResolver::from_parts(
            1,
            Some(
                MetadataFetcher::new(node.clone(), fetch)
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

        // Telling the culprit from an outage does cost a few pauses...
        settle(1_800).await;
        assert_eq!(db.stored().len(), 10);

        // ...but once it is known, its hourly retries never again make
        // anybody else wait for a cool-down.
        for round in 0..6u64 {
            settle(3_000).await;
            assert!(!worker.stats().breaker_open, "round {round}");

            let fresh = 100 + round;
            chain.add(addr(fresh), FakeToken::erc20("Token", "TKN", 18));
            worker.discover(&batch(&[fresh]));
            settle(2).await;
            assert!(db.stored().contains(&addr(fresh)), "round {round}");
        }
        assert!(node.poisoned_requests.load(Ordering::SeqCst) > 8);

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

    // ---- hardening ----------------------------------------------------

    /// Redis that vouches for every token ("the `tokens` table was
    /// reset, Redis was not") and whose writes can be made to hang.
    #[derive(Default)]
    struct KnowsEverything {
        lookups: AtomicUsize,
        hang_writes: AtomicBool,
    }

    impl TokenCache for KnowsEverything {
        fn contains_many<'a>(
            &'a self,
            addresses: &'a [Address],
        ) -> BoxFuture<'a, anyhow::Result<Vec<bool>>> {
            Box::pin(async move {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                Ok(vec![true; addresses.len()])
            })
        }

        fn store_many<'a>(
            &'a self,
            _tokens: &'a [DatabaseToken],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.hang_writes.load(Ordering::SeqCst) {
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn healing_a_reset_tokens_table_makes_steady_progress() {
        const TOKENS: u64 = 20_000;

        let chain = FakeChain::new();
        let db = Arc::new(FakeDb::default());
        let all: Vec<u64> = (1..=TOKENS).collect();
        for n in &all {
            chain.add(addr(*n), FakeToken::erc20("Token", "TKN", 18));
        }
        // Everything is referenced by stored transfers, nothing has a
        // row, and Redis swears it is all stored.
        db.reference(&all);
        let cache = Arc::new(KnowsEverything::default());

        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(chain.clone(), Some(cache.clone())),
            db.clone(),
            Some(db.clone()),
            options(),
        );

        // A page of 5000 every 5 seconds (the floor between listings).
        settle(12).await;
        let healed = db.stored().len();
        assert!((10_000..=15_000).contains(&healed), "{healed}");

        settle(30).await;
        assert_eq!(db.stored().len(), TOKENS as usize);
        assert_eq!(db.tokens.lock().unwrap().len(), TOKENS as usize);

        // The database said "missing": Redis was not even asked.
        assert_eq!(cache.lookups.load(Ordering::SeqCst), 0);
        // No more listings than pages (+ the one that found the end),
        // and never two closer than the floor.
        let times = db.query_times.lock().unwrap().clone();
        assert!(times.len() <= 6, "{}", times.len());
        assert!(times
            .windows(2)
            .all(|w| w[1] - w[0] >= Duration::from_secs(5)));

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reset_tokens_table_is_healed_without_a_restart_too() {
        let s = setup(true, options());
        let all: Vec<u64> = (1..=50).collect();
        s.db.reference(&all);
        s.worker.discover(&batch(&all));
        settle(30).await;
        assert_eq!(s.db.stored().len(), 50);

        // TRUNCATE TABLE tokens. Memory knows all 50.
        s.db.tokens.lock().unwrap().clear();
        settle(300).await;
        assert_eq!(s.db.stored().len(), 50);
        assert_eq!(s.db.tokens.lock().unwrap().len(), 50, "healed once");

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unresolvable_tokens_do_not_hide_the_rest_of_the_listing() {
        // The first page of the listing is nothing but addresses that
        // cannot be resolved for now (no code yet).
        let s = setup(
            true,
            TokenWorkerOptions { backfill_limit: 5, ..options() },
        );
        let codeless: Vec<u64> = (500..510).collect();
        s.db.reference(&codeless);
        s.db.reference(&[600_000, 600_001]);
        s.chain.add(addr(600_000), FakeToken::erc20("Late", "LATE", 6));
        s.chain.add(addr(600_001), FakeToken::erc20("Late", "LATE", 6));

        settle(120).await;
        assert_eq!(s.db.stored(), vec![addr(600_000), addr(600_001)]);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn blank_rows_are_verified_again_and_replaced() {
        const DAY: u64 = 24 * 3_600;
        let s = setup(true, options());

        // Token 200 answers nothing when it is first seen (think of the
        // node being wrong about it); token 201 never will.
        s.chain.add(addr(200), FakeToken::reverting());
        s.chain.add(addr(201), FakeToken::reverting());
        s.db.reference(&[200, 201]);
        s.worker.discover(&batch(&[200, 201]));
        settle(30).await;
        assert!(s
            .db
            .latest(addr(200))
            .is_some_and(|row| row.name.is_empty()));
        let first_calls = s.chain.token_calls(&addr(201));

        // Nothing is rechecked while the blank is fresh.
        s.chain.add(addr(200), FakeToken::erc20("Real Token", "REAL", 8));
        settle(DAY / 2).await;
        assert_eq!(s.chain.token_calls(&addr(201)), first_calls);
        assert!(s
            .db
            .latest(addr(200))
            .is_some_and(|row| row.name.is_empty()));

        // A day later both are due: one now has metadata, and the
        // ReplacingMergeTree row says so.
        settle(DAY).await;
        let row = s.db.latest(addr(200)).unwrap();
        assert_eq!(
            (row.name.as_str(), row.symbol.as_str(), row.decimals),
            ("Real Token", "REAL", 8)
        );
        let stats = s.worker.stats();
        assert_eq!((stats.blank_rechecked, stats.blank_healed), (2, 1));

        // The other one is still blank: written again (the database
        // lists it by age) and vouched for longer, 7 days.
        let second_calls = s.chain.token_calls(&addr(201));
        assert!(second_calls > first_calls);
        assert!(s
            .db
            .latest(addr(201))
            .is_some_and(|row| row.name.is_empty()));
        settle(5 * DAY).await;
        assert_eq!(s.chain.token_calls(&addr(201)), second_calls);
        settle(3 * DAY).await;
        assert!(s.chain.token_calls(&addr(201)) > second_calls);
        assert_eq!(s.worker.stats().blank_rechecked, 3);

        // The healed one is left alone for good.
        let healed_calls = s.chain.token_calls(&addr(200));
        settle(30 * DAY).await;
        assert_eq!(s.chain.token_calls(&addr(200)), healed_calls);

        s.worker.shutdown().await;
        s.handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn set_head_reaches_the_rpc_backend_without_blocking() {
        use crate::tokens::endpoints::{
            EndpointOptions, EndpointSpec, MultiEndpointCaller,
        };

        let stale = FakeChain::new();
        stale.height.store(100, Ordering::SeqCst);
        let pool = Arc::new(MultiEndpointCaller::new(
            1,
            vec![EndpointSpec {
                key: "node".into(),
                label: "#1".into(),
                provider: "node".into(),
                caller: stale.clone(),
                discovered: false,
            }],
            EndpointOptions::default(),
        ));
        stale.add(addr(1), FakeToken::erc20("Token", "TKN", 18));
        let db = Arc::new(FakeDb::default());
        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(pool.clone(), None),
            db.clone(),
            None,
            options(),
        );

        // The indexer is at block 9000, the only node at block 100: it
        // cannot know the contracts it would be asked about.
        let before = Instant::now();
        worker.set_head(9_000);
        worker.set_head(8_000);
        assert_eq!(Instant::now(), before);

        worker.discover(&batch(&[1]));
        settle(30).await;
        assert!(db.stored().is_empty());
        assert!(stale.block_number_calls.load(Ordering::SeqCst) >= 1);

        // It catches up.
        stale.height.store(9_001, Ordering::SeqCst);
        settle(600).await;
        assert_eq!(db.stored(), vec![addr(1)]);

        worker.shutdown().await;
        handle.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_cache_write_does_not_delay_shutdown() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 18));
        let cache = Arc::new(KnowsEverything::default());
        cache.hang_writes.store(true, Ordering::SeqCst);
        let db = Arc::new(FakeDb::default());
        db.reference(&[1]);

        let (worker, handle) = TokenWorker::spawn_with_resolver(
            resolver(chain.clone(), Some(cache.clone())),
            db.clone(),
            Some(db.clone()),
            options(),
        );
        settle(30).await;
        // In the database, stuck on telling Redis.
        assert_eq!(db.stored(), vec![addr(1)]);

        let started = Instant::now();
        worker.shutdown().await;
        handle.await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
