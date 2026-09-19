//! `indexer run --chain solana`: the Solana sync loop.
//!
//! ```text
//! GET /height  (free, unmetered)   ->  head slot
//!   -> budget governor (never more than ~25 metered queries a minute)
//!   -> POST /query/arrow [cursor, target)  ->  the server truncates and
//!      answers with `next_slot`: that cursor, not our arithmetic, is what
//!      the checkpoint records
//!   -> svm::decode (pure)
//!   -> continuity: block_height chain + parent_slot/parent_blockhash
//!        break -> TRIPWIRE: stop loudly, write nothing further
//!   -> writer: children, then `sol_slots` LAST, then the checkpoint
//! ```
//!
//! # How this differs from the EVM loop, and why it is a separate loop
//!
//! | | EVM | Solana |
//! |---|---|---|
//! | commit marker | `blocks` | `sol_slots` |
//! | a gap is | a block number with no `blocks` row | a hole in the CHECKPOINT tiling. Skipped slots are normal and have no row |
//! | resume | the gap query over `blocks` | the checkpoint tiling, whose `to_block` is the server's `next_slot` |
//! | continuity | parent hash | `block_height` + 1 (immune to skipped slots) AND the parent chain |
//! | a mismatch | fork-point search, then purge and re-stream | there is no fork: Envio serves at ~finalized. It is a TRIPWIRE that stops the process |
//! | confirmations | tunable | 0, and any other value is refused |
//! | budget | none | ~25 metered queries a minute out of a measured 30 |
//!
//! The EVM sync loop is left exactly as it is; this file reuses everything
//! that is genuinely shared - the ClickHouse insert path and its
//! deduplication tokens, `db::next_version`, the epoch, `reorg::Purger`
//! (through `solana_store::SolanaReorgStore`), the one-process-per-chain
//! lease and its fence, and the metrics handle.

use crate::{
    configs::Config,
    db::{
        ranges::{contiguous_until, BlockRange},
        Database,
    },
    metrics::{self, Metrics, SolanaStats},
    pipeline::{
        lease::{Fence, Lease, LeaseOptions},
        shutdown_signal,
        solana_store::{SlotAnchor, SolanaReorgStore, COMMIT_MARKER},
        solana_verify,
        solana_writer::{
            ClickhouseSvmSink, LastFlush, SvmBatch, SvmWriter,
            SvmWriterHandle, SvmWriterStopped,
        },
    },
    reorg::{
        DiscoveryCache, PurgeReason, Purger, ReorgMetrics, ReorgStore,
        WriterControl,
    },
    source::solana::SolanaSource,
    svm::{self, SvmSlotBatch},
};
use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use log::{debug, error, info, warn};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::watch;

// ------------------------------------------------------------- constants

/// Solana's chain id in this project (docs/design.md §14): the Hyperlane
/// domain id. No standard integer id for Solana exists - CAIP-2 uses a
/// string, Wormhole's 1 collides with Ethereum mainnet, and SLIP-44 501 is
/// small enough to collide with a future EVM chain.
pub const SOLANA_CHAIN_ID: u64 = 1_399_811_149;

/// The name `--chain` accepts as an alias of [`SOLANA_CHAIN_ID`].
pub const SOLANA_CHAIN_NAME: &str = "solana";

/// First slot Envio's Solana HyperSync serves (2026-01-03 07:37 UTC,
/// docs/solana-research.md §4.4 / §11.3). A `--start-block` below it can
/// never be satisfied: the server answers empty WITHOUT advancing
/// `next_slot`, which is a stop condition and not a reason to spin, so the
/// only honest thing to do is refuse at startup.
pub const FIRST_SERVED_SLOT: u64 = 391_000_000;

/// Metered queries a minute the follower allows itself.
///
/// Measured budget is 30 per 60 s per endpoint (`x-ratelimit-limit:
/// 30000, 30000;w=60` with a flat `x-ratelimit-cost: 1000`,
/// docs/solana-research.md §11.1). 25 leaves headroom for retries, gap
/// heals and the verify sweep, and following the head needs 7-15.
///
/// It is a FLOOR on politeness, not a belief: every response's real
/// headers override it downwards through [`Budget::observe`].
pub const DEFAULT_MAX_QUERIES_PER_MINUTE: u32 = 25;

/// How often the free `/height` endpoint is polled while caught up.
const HEAD_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Target cadence at the head: slots accumulate for this long before a
/// metered query is spent on them. 4 s is the recommendation of
/// docs/solana-research.md §11.2 - ~15 slots a query, 15 queries a minute,
/// and cheaper cadences buy nothing because Envio's own 10-13 s ingest lag
/// dominates.
const TIP_CADENCE: Duration = Duration::from_secs(4);

/// A pass of at most this many slots means "following the head".
const TIP_PASS_SLOTS: u64 = 256;

/// Slots asked for in one metered query while backfilling. The server
/// truncates at its own ~5 s execution budget (30-66 slots measured); this
/// only has to be comfortably larger than that, because the CURSOR decides.
const BACKFILL_WINDOW_SLOTS: u64 = 10_000;

/// Cap of the backoff between failed passes.
const MAX_PASS_BACKOFF: Duration = Duration::from_secs(60);

/// `/readyz`: not ready when nothing happened for this long.
const READY_STALENESS: Duration = Duration::from_secs(120);

/// Checkpoints read per resume. Far more than a head follower ever has;
/// a long backfill that exceeds it simply re-inspects from further back.
const MAX_CHECKPOINTS: usize = 100_000;

/// Gap ranges a single pass streams.
const MAX_GAPS_PER_PASS: usize = 1_000;

/// How long [`SvmWriterGate::quiesce`] waits for the last flush to become
/// readable (ClickHouse has no read-your-writes; normally a few ms).
const VISIBILITY_ATTEMPTS: u32 = 200;
const VISIBILITY_DELAY: Duration = Duration::from_millis(25);

// ---------------------------------------------------------- the tripwire

/// Continuity broke on data the source serves at ~finalized. This is an
/// event that is not supposed to happen: the process stops and an operator
/// looks, rather than the pipeline quietly repairing a chart.
#[derive(Debug, Clone)]
pub struct Tripwire {
    pub chain: u64,
    pub slot: u64,
    pub detail: String,
}

impl std::fmt::Display for Tripwire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "chain {}: SOLANA CONTINUITY TRIPWIRE at slot {}. {}\n\
             \n\
             Nothing was written at or above that slot, and this process \
             stopped on purpose.\n\
             \n\
             Envio's Solana endpoint serves at (just behind) `finalized`, \
             which is why this pipeline runs with no fork-point search and \
             no confirmations. A break in the block_height / parent chain \
             therefore means one of:\n\
             \n\
               1. the endpoint is not the one the stored data came from \
             (check --hypersync-url), or the database holds another \
             network under chain id {};\n\
               2. Envio re-ingested the range from a different source, or \
             its commitment level changed (it is undocumented and the \
             product is labelled Beta);\n\
               3. a Solana cluster restart discarded rooted slots - twice \
             in five years, and it is announced.\n\
             \n\
             What to do: run `indexer verify --chain solana` to see how \
             far the stored data is consistent, then re-index from below \
             the reported slot. Do NOT simply restart: the same break will \
             be found again, which is the point.",
            self.chain, self.slot, self.detail, self.chain
        )
    }
}

impl std::error::Error for Tripwire {}

impl Tripwire {
    pub fn is_cause_of(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| cause.is::<Tripwire>())
    }
}

// ------------------------------------------------------------ the source

/// One served page: the slots and, above all, the server's cursor.
#[derive(Debug, Default)]
pub struct SlotPage {
    /// Exclusive end of what the server actually served. NEVER computed
    /// from the rows: a page whose slots were all skipped is empty and
    /// still advances the cursor.
    pub next_slot: u64,
    pub batches: Vec<SvmSlotBatch>,
    /// Rate-limit headers of this response, when the transport surfaced
    /// them.
    pub budget: BudgetInfo,
}

/// What the `x-ratelimit-*` headers of one response said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetInfo {
    pub limit: Option<u64>,
    pub remaining: Option<u64>,
    pub reset_secs: Option<u64>,
    pub cost: Option<u64>,
}

impl BudgetInfo {
    /// Requests left in this window, when the headers allow the division.
    pub fn requests_left(&self) -> Option<u64> {
        let cost = self.cost.filter(|cost| *cost > 0)?;
        Some(self.remaining? / cost)
    }
}

/// Where slots come from (Envio Solana HyperSync in production).
pub trait SlotSource: Send + Sync + 'static {
    /// The server's head slot. Must be FREE: on Envio it is `GET /height`,
    /// which carries no rate-limit headers at all and needs no token.
    fn head(&self) -> impl Future<Output = Result<u64>> + Send;

    /// One metered query for `[from, to)`. The server truncates where it
    /// likes and says so in `next_slot`.
    fn fetch(
        &self,
        from: u64,
        to: u64,
    ) -> impl Future<Output = Result<SlotPage>> + Send;
}

impl SlotSource for SolanaSource {
    async fn head(&self) -> Result<u64> {
        SolanaSource::head(self).await
    }

    async fn fetch(&self, from: u64, to: u64) -> Result<SlotPage> {
        let (batch, rate_limit) =
            SolanaSource::fetch_arrow(self, from, to).await?;

        Ok(SlotPage {
            next_slot: batch.next_slot,
            batches: batch.batches,
            budget: BudgetInfo {
                limit: rate_limit.limit,
                remaining: rate_limit.remaining,
                reset_secs: rate_limit.reset_secs,
                cost: rate_limit.cost,
            },
        })
    }
}

// ----------------------------------------------------------- the budget

/// Client-side governor of the metered query rate.
///
/// Two halves, and both are needed:
///
/// * a token bucket of `max_per_minute` over a sliding 60 s window, which
///   is what keeps us politely below the free tier's 30 even when the
///   server tells us nothing;
/// * [`Budget::observe`], which folds every response's real headers in and
///   sleeps out the window when the server says the budget is nearly gone.
///   The documented contract is that `remaining` counts BUDGET UNITS, not
///   requests, so it is divided by `cost` rather than compared with a
///   request count - and neither number is ever hard-coded.
pub struct Budget {
    max_per_minute: u32,
    /// Instants of the metered queries inside the last 60 s.
    recent: Mutex<std::collections::VecDeque<tokio::time::Instant>>,
    total: AtomicU64,
    remaining: AtomicU64,
}

/// `remaining` is absent from the headers.
const NO_REMAINING: u64 = u64::MAX;

impl Budget {
    pub fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute: max_per_minute.max(1),
            recent: Mutex::new(std::collections::VecDeque::new()),
            total: AtomicU64::new(0),
            remaining: AtomicU64::new(NO_REMAINING),
        }
    }

    /// Waits until a metered query may be sent, then counts it.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut recent = self.recent.lock().unwrap();
                let now = tokio::time::Instant::now();
                let window = Duration::from_secs(60);

                while recent
                    .front()
                    .is_some_and(|at| now.duration_since(*at) >= window)
                {
                    recent.pop_front();
                }

                if recent.len() < self.max_per_minute as usize {
                    recent.push_back(now);
                    self.total.fetch_add(1, Ordering::Relaxed);
                    return;
                }

                // The oldest query leaves the window in this long.
                recent
                    .front()
                    .map(|at| {
                        window.saturating_sub(now.duration_since(*at))
                    })
                    .unwrap_or(window)
            };

            debug!(
                "Solana query budget exhausted ({}/min), waiting {wait:?}.",
                self.max_per_minute
            );
            tokio::time::sleep(wait.max(Duration::from_millis(10))).await;
        }
    }

    /// Folds a response's headers in. Sleeps out the window when the server
    /// says fewer than two requests are left, so a 429 is never provoked.
    pub async fn observe(&self, info: BudgetInfo) {
        let Some(left) = info.requests_left() else {
            return;
        };

        self.remaining.store(left, Ordering::Relaxed);

        if left >= 2 {
            return;
        }

        let reset = info.reset_secs.unwrap_or(60).min(120);
        warn!(
            "Solana HyperSync says {left} request(s) are left in this \
             window; waiting {reset}s for it to reset. Lower the query \
             rate if this repeats."
        );
        tokio::time::sleep(Duration::from_secs(reset)).await;
    }

    /// Metered queries sent in the last 60 s.
    pub fn queries_last_minute(&self) -> u64 {
        let mut recent = self.recent.lock().unwrap();
        let now = tokio::time::Instant::now();
        while recent.front().is_some_and(|at| {
            now.duration_since(*at) >= Duration::from_secs(60)
        }) {
            recent.pop_front();
        }
        recent.len() as u64
    }

    pub fn total_queries(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Requests the server last said were left, `None` when it never said.
    pub fn remaining_requests(&self) -> Option<u64> {
        match self.remaining.load(Ordering::Relaxed) {
            NO_REMAINING => None,
            left => Some(left),
        }
    }
}

// -------------------------------------------------------- the continuity

/// The two witnesses of docs/solana-research.md §11.4.3, carried from slot
/// to slot and across a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Continuity {
    pub slot: u64,
    pub blockhash: [u8; 32],
    pub block_height: u64,
}

impl From<SlotAnchor> for Continuity {
    fn from(anchor: SlotAnchor) -> Self {
        Self {
            slot: anchor.block_number,
            blockhash: anchor.blockhash.0,
            block_height: anchor.block_height,
        }
    }
}

/// Checks `next` against its stored predecessor. `Ok(())` or the tripwire
/// detail.
///
/// **Skipped slots are normal, so `slot + 1` is never used.** The strong
/// witness is `block_height`: Solana counts PRODUCED BLOCKS, so it
/// increments by exactly 1 per block however many slots were skipped in
/// between. That is what detects a LOST block. The parent chain is what
/// detects a WRONG one, and the two together are what §11.4.3 calls
/// witnesses 2 and 3.
pub fn check_continuity(
    previous: Continuity,
    next: &SvmSlotBatch,
) -> std::result::Result<(), String> {
    if next.block_height != previous.block_height + 1 {
        return Err(format!(
            "block_height jumped from {} (slot {}) to {} (slot {}): \
             {} produced block(s) are missing between them. Solana's \
             block_height counts PRODUCED BLOCKS, so it increases by \
             exactly 1 per block no matter how many slots were skipped - \
             a jump means a block was lost, not that slots were skipped.",
            previous.block_height,
            previous.slot,
            next.block_height,
            next.slot,
            next.block_height.saturating_sub(previous.block_height + 1),
        ));
    }

    if next.parent_slot != previous.slot {
        return Err(format!(
            "slot {} names parent_slot {} but the slot stored below it is \
             {}. The block_height chain agrees, so this is a DIFFERENT \
             block at the same height.",
            next.slot, next.parent_slot, previous.slot
        ));
    }

    if next.parent_blockhash != previous.blockhash {
        return Err(format!(
            "slot {} names parent_blockhash {} but slot {} is stored with \
             blockhash {}.",
            next.slot,
            bs58::encode(next.parent_blockhash).into_string(),
            previous.slot,
            bs58::encode(previous.blockhash).into_string(),
        ));
    }

    Ok(())
}

// ----------------------------------------------------------- the runtime

/// Everything that talks to the outside world, so the acceptance tests can
/// run the REAL loop against an in-memory chain.
pub struct SolanaRuntime<S: SlotSource> {
    pub source: S,
    pub lease: LeaseOptions,
    /// Resolves when the process should stop (SIGINT / SIGTERM).
    pub shutdown: BoxFuture<'static, ()>,
    /// Metered queries a minute this process allows itself.
    pub max_queries_per_minute: u32,
}

impl<S: SlotSource> SolanaRuntime<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            lease: LeaseOptions::default(),
            shutdown: Box::pin(shutdown_signal()),
            max_queries_per_minute: DEFAULT_MAX_QUERIES_PER_MINUTE,
        }
    }
}

// --------------------------------------------------- flags that make no
//                                                      sense on Solana

/// Rejects or ignores the EVM-only flags, with a message that names the
/// Solana behaviour rather than just saying no.
///
/// The rule used to decide which is which: a flag whose value could make a
/// reader believe something about the data that is not true is REFUSED; a
/// flag that is merely inert is IGNORED with one line in the log.
pub fn check_flags(config: &Config) -> Result<()> {
    // REFUSED: `--confirmations N` on a source that already serves behind
    // `finalized` would cost freshness twice and buy nothing, and an
    // operator who set it believes they bought reorg safety.
    if config.confirmations != 0 {
        bail!(
            "--confirmations {} is refused on Solana. Envio serves Solana \
             at (just behind) `finalized` - measured 5-19 slots behind it \
             over six samples - so staying further back costs freshness \
             twice and protects against nothing. The continuity tripwire \
             (block_height + parent chain) is what guards this chain \
             instead, and it is always on. Drop the flag.",
            config.confirmations
        );
    }

    // REFUSED: the run would store slots and nothing else. `--no-dex` on
    // an EVM chain still indexes blocks, transactions and transfers;
    // Solana has no such tables by design (an analytics-only,
    // program-filtered pipeline), so the flag means "write nothing".
    if !config.dex {
        bail!(
            "--no-dex is refused on Solana. The Solana pipeline is \
             analytics-only and program-filtered (src/svm/README.md): \
             there is no chain-wide block, transaction or transfer table \
             to index, so switching the DEX decoder off would stream the \
             chain and store only empty slot headers. To index nothing, \
             do not run the chain."
        );
    }

    // REFUSED: a start below the served history can never be satisfied,
    // and the server's answer to it (empty, `next_slot` not advancing) is
    // indistinguishable from "caught up" unless it is caught here.
    if config.start_block != 0 && config.start_block < FIRST_SERVED_SLOT {
        bail!(
            "--start-block {} is below slot {FIRST_SERVED_SLOT}, the \
             first slot Envio's Solana endpoint serves (2026-01-03). A \
             query below it comes back empty WITHOUT advancing the \
             cursor, which a resume loop can not tell from 'caught up', \
             so this is refused rather than left to spin. Anything older \
             exists only in the Old Faithful archive and needs a second \
             ingest path (docs/solana-research.md §11.3.1). Use \
             --start-block {FIRST_SERVED_SLOT} or higher, or \
             --new-blocks-only to start at the head.",
            config.start_block
        );
    }

    Ok(())
}

/// One log line per flag that is accepted and does nothing. Not an error:
/// these are how a compose file written for the EVM chains looks, and
/// refusing them would make "add Solana to the fleet" a rewrite.
fn report_ignored_flags(config: &Config) {
    if config
        .rpc_url
        .as_deref()
        .is_some_and(|rpc| !rpc.eq_ignore_ascii_case("none"))
    {
        info!(
            "--rpc is ignored on Solana: token decimals arrive free on \
             every account_activity row, so nothing on this chain needs an \
             RPC call. Name, symbol and URI live in account state, which \
             HyperSync does not serve at all."
        );
    }

    if !config.predictions {
        info!(
            "--no-predictions is ignored on Solana: there is no Solana \
             prediction-market decoder, so the flag changes nothing."
        );
    }

    if !config.launchpads {
        info!(
            "--no-launchpads is ignored on Solana: Solana launchpads are \
             not decoded yet (the writer seam is in place for them)."
        );
    }

    if config.max_reorg_depth != crate::reorg::DEFAULT_MAX_REORG_DEPTH {
        info!(
            "--max-reorg-depth {} is ignored on Solana: there is no \
             fork-point search on this chain, because there is no fork to \
             find on finalized data. A continuity break stops the process \
             instead of being repaired silently.",
            config.max_reorg_depth
        );
    }

    if config.redis_url.is_some() {
        info!(
            "--redis is ignored on Solana: it caches EVM token metadata \
             lookups, and Solana makes none."
        );
    }
}

// ------------------------------------------------------- writer plumbing

/// The two things a purge needs from the Solana writer.
struct SvmWriterGate {
    writer: SvmWriterHandle,
    fence: Fence,
    visible: Box<dyn Fn() -> BoxFuture<'static, Result<()>> + Send + Sync>,
    adopt: Box<dyn Fn(u32) + Send + Sync>,
}

impl WriterControl for SvmWriterGate {
    fn quiesce(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.fence.check()?;
            self.writer.barrier().await?;
            (self.visible)().await
        })
    }

    fn adopt_epoch(&self, epoch: u32) {
        (self.adopt)(epoch);
    }
}

/// Polls until the `sol_slots` row of the last flush is readable.
async fn wait_until_visible(db: Database, last: LastFlush) -> Result<()> {
    let Some((slot, version)) = *last.lock().unwrap() else {
        return Ok(());
    };

    // No FINAL: the exact row version this process wrote.
    let sql = format!(
        "SELECT toUInt64(count()) FROM `{COMMIT_MARKER}` \
         WHERE chain = {} AND block_number = {slot} AND _version = {version}",
        db.chain_id
    );

    for _ in 0..VISIBILITY_ATTEMPTS {
        let rows: u64 = db
            .db
            .query(&sql)
            .fetch_one()
            .await
            .context("read back the last Solana flush")?;

        if rows > 0 {
            return Ok(());
        }

        tokio::time::sleep(VISIBILITY_DELAY).await;
    }

    bail!(
        "slot {slot} of the last flush (version {version}) can not be read \
         back after {:?}",
        VISIBILITY_DELAY * VISIBILITY_ATTEMPTS
    )
}

/// Solana has no block-keyed discovery caches: there is no token worker on
/// this chain, because decimals come with the data.
struct NoCaches;

impl DiscoveryCache for NoCaches {
    fn evict_range(&self, _from: u64, _to: Option<u64>) {}
}

// ------------------------------------------------------------- the loop

#[derive(Debug, Clone, Copy)]
struct SolanaSettings {
    chain: u64,
    start_slot: u64,
    /// Exclusive, 0 = follow the head.
    end_slot: u64,
    new_slots_only: bool,
}

struct SolanaIndexer<S: SlotSource> {
    settings: SolanaSettings,
    source: S,
    store: SolanaReorgStore,
    purger: Purger,
    writer: SvmWriter,
    budget: Arc<Budget>,
    metrics: Metrics,
    /// Windows this process committed (and did not purge since). A
    /// checkpoint listing that misses one is a stale read, not a gap.
    committed: Vec<BlockRange>,
    stale: Arc<Mutex<Vec<BlockRange>>>,
    /// Gap heals are only looked for on the first inspection of a range.
    healed_until: u64,
    /// Counters for the Solana-only metric series.
    swaps_stored: Arc<AtomicU64>,
    skipped_slots: Arc<AtomicU64>,
}

/// How a pass ended.
enum PassOutcome {
    /// Everything below this slot is stored or was asked for.
    Covered(u64),
    /// A stale-epoch purge happened: continue from this slot.
    Restart(u64),
}

/// Runs `indexer run --chain solana` against the real endpoint.
pub async fn run(config: Config) -> Result<()> {
    check_flags(&config)?;

    let source = SolanaSource::new(
        config.hypersync_url.as_deref(),
        &config.hypersync_token,
    )?;

    run_with(config, SolanaRuntime::new(source)).await
}

/// [`run`] over an explicit source, so the acceptance tests drive the real
/// loop with an in-memory chain and a real ClickHouse.
pub async fn run_with<S: SlotSource>(
    config: Config,
    runtime: SolanaRuntime<S>,
) -> Result<()> {
    check_flags(&config)?;
    report_ignored_flags(&config);

    let chain = config.chain_id;

    let metrics = match config.metrics_addr {
        Some(_) => Metrics::new(chain, READY_STALENESS),
        None => Metrics::disabled(),
    };

    let (stop_metrics, metrics_stopped) = watch::channel(false);

    if let Some(addr) = config.metrics_addr {
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

    let db = Database::new(&config.database_url, chain)
        .await?
        .with_metrics(metrics.clone());

    // The chain registry (docs/design.md §13), so a view or a UI knows to
    // print an identity column with base58Encode rather than as hex.
    // Idempotent: `chains` is a ReplacingMergeTree keyed on `chain`.
    db.db
        .query(svm::REGISTER_CHAIN_SQL)
        .execute()
        .await
        .context("register Solana in the `chains` registry")?;

    // One process per chain, before anything is written. The lease keys on
    // `chain` and knows nothing about EVM, so Solana gets the guarantee for
    // free - `a_second_solana_process_is_refused` proves it.
    let (fatal_tx, mut fatal) = watch::channel(None::<String>);
    let lease = Lease::acquire(&db, runtime.lease, fatal_tx).await?;

    // `_version`s must never go back, whatever this host's clock says.
    db.seed_version(&crate::pipeline::solana_store::versioned_tables())
        .await?;

    // The epoch of a chain survives restarts in `reorgs`.
    db.set_epoch(db.current_epoch().await?);

    let store = SolanaReorgStore::new(db.clone());
    let last_flush = LastFlush::default();
    let stale = Arc::new(Mutex::new(Vec::new()));
    let fence = lease.fence();

    let writer = SvmWriter::spawn(
        ClickhouseSvmSink {
            db: db.clone(),
            fence: fence.clone(),
            last_flush: last_flush.clone(),
            stale: stale.clone(),
        },
        config.flush_rows,
        Duration::from_millis(config.flush_interval_ms),
        metrics.clone(),
    );

    let gate = SvmWriterGate {
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
            Box::new(move |epoch| db.set_epoch(epoch))
        },
    };

    let purger = Purger::new(
        Arc::new(store.clone()),
        Arc::new(gate),
        Arc::new(NoCaches),
        Arc::new(metrics.clone()) as Arc<dyn ReorgMetrics>,
    );

    let start_slot = if config.start_block == 0 {
        FIRST_SERVED_SLOT
    } else {
        config.start_block
    };

    let budget = Arc::new(Budget::new(runtime.max_queries_per_minute));

    let mut indexer = SolanaIndexer {
        settings: SolanaSettings {
            chain,
            start_slot,
            end_slot: config.end_block,
            new_slots_only: config.new_blocks_only,
        },
        source: runtime.source,
        store,
        purger,
        writer,
        budget: budget.clone(),
        metrics: metrics.clone(),
        committed: Vec::new(),
        stale,
        healed_until: 0,
        swaps_stored: Arc::new(AtomicU64::new(0)),
        skipped_slots: Arc::new(AtomicU64::new(0)),
    };

    // 1. Stop the stream ...
    let result = tokio::select! {
        result = indexer.sync() => result,
        _ = runtime.shutdown => {
            info!("Shutdown requested, flushing buffered slots.");
            Ok(())
        }
        reason = async {
            match fatal.wait_for(|reason| reason.is_some()).await {
                Ok(reason) => reason.clone().unwrap_or_default(),
                Err(_) => std::future::pending().await,
            }
        } => Err(anyhow::anyhow!(reason)),
    };

    metrics.set_ready(false);

    // 2. ... then the final flush - EXCEPT after a tripwire, where the
    //    whole point is that nothing further reaches the database.
    let tripped = result.as_ref().err().is_some_and(Tripwire::is_cause_of);

    let flushed = if tripped {
        indexer.writer.abort().await;
        Ok(())
    } else {
        indexer.writer.shutdown().await
    };

    // 3. ... then the lease and the metrics endpoint.
    lease.release().await;
    let _ = stop_metrics.send(true);

    match (result, flushed) {
        (_, Err(e)) => Err(e),
        (result, Ok(())) => result,
    }
}

impl<S: SlotSource> SolanaIndexer<S> {
    async fn sync(&mut self) -> Result<()> {
        let chain = self.settings.chain;
        let end_slot = self.settings.end_slot;

        // Startup errors are fatal (bad token, wrong url ...). Later ones
        // are retried.
        let mut head = self.source.head().await?;
        self.saw_head(head);

        let epoch = self.purger.current_epoch(chain).await?;
        if epoch > 0 {
            info!("Chain {chain}: epoch {epoch}.");
        }

        let mut cursor = if self.settings.new_slots_only {
            head
        } else {
            self.resume_point().await?
        };

        if cursor > self.settings.start_slot {
            info!(
                "Checkpoints tile slots [{}, {cursor}) without a hole.",
                self.settings.start_slot
            );
        }

        info!(
            "Solana head is slot {head}. Syncing from slot {cursor}{}.",
            if end_slot > 0 {
                format!(" up to, not including, slot {end_slot}")
            } else {
                " and following the head".to_string()
            }
        );

        let mut failures: u32 = 0;
        let mut last_tip_commit: Option<tokio::time::Instant> = None;

        loop {
            let target = target_slot(head, end_slot);

            let at_tip = target.saturating_sub(cursor) <= TIP_PASS_SLOTS;
            // At the head, let slots accumulate for one cadence before a
            // metered query is spent on them.
            let paced = at_tip
                && end_slot == 0
                && last_tip_commit
                    .is_some_and(|at| at.elapsed() < TIP_CADENCE);

            if target > cursor && !paced {
                match self.pass(BlockRange::new(cursor, target)).await {
                    Ok(PassOutcome::Covered(covered)) => {
                        failures = 0;
                        cursor = covered;
                        self.metrics.set_ready(true);
                        if at_tip {
                            last_tip_commit =
                                Some(tokio::time::Instant::now());
                        }
                    }
                    Ok(PassOutcome::Restart(from)) => {
                        failures = 0;
                        cursor = cursor.min(from);
                        continue;
                    }
                    Err(e) if is_fatal(&e) => return Err(e),
                    Err(e) => {
                        failures = failures.saturating_add(1);
                        self.metrics.stream_error();
                        let wait = pass_backoff(failures);
                        warn!(
                            "Solana sync pass failed: {e:#}. Retrying in \
                             {wait:?}."
                        );
                        tokio::time::sleep(wait).await;
                    }
                }
            } else if target <= cursor {
                self.metrics.set_ready(true);
            }

            if end_slot > 0 && cursor >= end_slot {
                info!(
                    "Finished syncing up to slot {end_slot} (exclusive)."
                );
                return Ok(());
            }

            self.publish_stats().await;

            if cursor >= target || paced {
                tokio::time::sleep(HEAD_POLL_INTERVAL).await;
            }

            // FREE: `GET /height` carries no rate-limit headers at all and
            // needs no token, so discovering that nothing happened never
            // costs a metered query.
            match self.source.head().await {
                Ok(new_head) => {
                    head = head.max(new_head);
                    self.saw_head(head);
                }
                Err(e) => warn!("Could not fetch the Solana head: {e:#}"),
            }
        }
    }

    /// Witness 1: the slot up to which the live checkpoints tile
    /// `[start_slot, ..)` with no hole.
    async fn resume_point(&self) -> Result<u64> {
        let tiling = self
            .store
            .checkpoint_tiling(
                self.settings.chain,
                self.settings.start_slot,
                MAX_CHECKPOINTS,
            )
            .await?;

        Ok(contiguous_until(self.settings.start_slot, tiling))
    }

    /// The holes of the checkpoint tiling inside `range`.
    ///
    /// This REPLACES the EVM "every integer has a `blocks` row" query, and
    /// it has to: on Solana a slot with no row is usually a skipped slot,
    /// and that query would report every one of them as a gap for ever.
    async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<Vec<BlockRange>> {
        let tiling = self
            .store
            .checkpoint_tiling(
                self.settings.chain,
                range.from,
                MAX_CHECKPOINTS,
            )
            .await?;

        Ok(holes(range, &tiling, MAX_GAPS_PER_PASS))
    }

    fn saw_head(&self, head: u64) {
        self.metrics.set_head(head.saturating_sub(1));
    }

    async fn pass(&mut self, range: BlockRange) -> Result<PassOutcome> {
        if let Some(from) = self.purge_stale_flushes().await? {
            return Ok(PassOutcome::Restart(from));
        }

        let mut missing = self.missing_ranges(range).await?;

        // No read-your-writes: what this process committed is not a gap,
        // whatever a lagging read says.
        missing =
            crate::db::ranges::subtract_ranges(&missing, &self.committed);

        // Gap healing, on the FIRST inspection of a range only: a flush
        // that died between its children and its `sol_slots` insert left
        // orphans there, and streaming on top of them would make the
        // candles count both.
        self.heal_gaps(&missing, range.to).await?;

        if missing.is_empty() {
            return Ok(PassOutcome::Covered(range.to));
        }

        let slots: u64 = missing.iter().map(BlockRange::len).sum();
        if slots > TIP_PASS_SLOTS {
            info!(
                "Syncing {slots} slots in {} range(s) of {range}.",
                missing.len()
            );
        }

        let mut reached = range.from;
        let mut streamed = Ok(());

        for gap in &missing {
            streamed = self.stream_range(*gap, &mut reached).await;
            if streamed.is_err() {
                break;
            }
        }

        // A tripwire must not be followed by a commit of anything: the
        // rows below the break are fine, but we stop rather than leave a
        // half-inspected chain looking complete.
        if let Err(e) = &streamed {
            if Tripwire::is_cause_of(e) {
                return Err(streamed.unwrap_err());
            }
        }

        // Commit point, also after a failed stream: whatever was delivered
        // is stored now, so the next pass only asks for the rest.
        self.writer.barrier().await?;

        for gap in &missing {
            if gap.from < reached {
                remember(
                    &mut self.committed,
                    BlockRange::new(gap.from, reached.min(gap.to)),
                );
            }
        }

        streamed?;

        Ok(PassOutcome::Covered(range.to))
    }

    /// Purges the gap ranges that hold orphan children, once per range.
    async fn heal_gaps(
        &mut self,
        gaps: &[BlockRange],
        inspected_until: u64,
    ) -> Result<()> {
        let mut healed = 0;

        for gap in gaps {
            if gap.from < self.healed_until {
                continue;
            }

            // The same invariant the EVM path checks, through the same
            // trait: children at a slot with no live commit marker, not
            // already settled by a purge that finished.
            if !self
                .store
                .has_orphan_children(
                    self.settings.chain,
                    gap.from,
                    Some(gap.to),
                )
                .await?
            {
                continue;
            }

            warn!(
                "Chain {}: slots {gap} hold rows of a flush that died \
                 before its `{COMMIT_MARKER}` insert. Purging them before \
                 the range is streamed again.",
                self.settings.chain
            );

            self.purger
                .purge_range(
                    self.settings.chain,
                    gap.from,
                    Some(gap.to),
                    PurgeReason::GapHeal,
                )
                .await?;

            self.forget_committed(gap.from, Some(gap.to));
            healed += 1;
        }

        self.healed_until = self.healed_until.max(inspected_until);

        if healed > 0 {
            info!(
                "Chain {}: healed {healed} gap range(s) left by an \
                 interrupted write.",
                self.settings.chain
            );
        }

        Ok(())
    }

    /// Streams `range` one metered query at a time, following the server's
    /// cursor. `reached` is the slot up to which rows went to the writer.
    async fn stream_range(
        &mut self,
        range: BlockRange,
        reached: &mut u64,
    ) -> Result<()> {
        debug!("Streaming Solana slots {range}.");

        // The anchor the first served block must build on, when there IS
        // one: a live checkpoint ending exactly where this range starts
        // means every slot between the stored predecessor and `range.from`
        // was served and skipped, so the next produced block must be its
        // immediate successor.
        let mut previous = self.anchor_for(range.from).await?;
        let mut cursor = range.from;

        while cursor < range.to {
            // A backfill asks for a big window and lets the server
            // truncate; at the head `range.to` is the bound anyway.
            let want =
                range.to.min(cursor.saturating_add(BACKFILL_WINDOW_SLOTS));

            self.budget.acquire().await;
            let page =
                self.source.fetch(cursor, want).await.with_context(
                    || format!("query Solana slots [{cursor}, {want})"),
                )?;
            self.budget.observe(page.budget).await;

            // The documented below-history / no-progress condition. It is
            // a stop, never a reason to spin.
            if page.next_slot <= cursor {
                bail!(
                    "Solana HyperSync served slots [{cursor}, {want}) \
                     without advancing its cursor (next_slot \
                     {}). That is what the endpoint answers below its \
                     served history (from slot {FIRST_SERVED_SLOT}) or \
                     when it made no progress at the head.",
                    page.next_slot
                );
            }
            if page.next_slot > range.to {
                bail!(
                    "Solana HyperSync returned next_slot {} while \
                     streaming {range} at slot {cursor}",
                    page.next_slot
                );
            }

            let served = BlockRange::new(cursor, page.next_slot);

            // Continuity BEFORE anything reaches the writer.
            for batch in &page.batches {
                if let Some(previous) = previous {
                    if let Err(detail) = check_continuity(previous, batch)
                    {
                        self.metrics.stream_error();
                        return Err(Tripwire {
                            chain: self.settings.chain,
                            slot: batch.slot,
                            detail,
                        }
                        .into());
                    }
                }
                previous = Some(Continuity {
                    slot: batch.slot,
                    blockhash: batch.blockhash,
                    block_height: batch.block_height,
                });
            }

            let chain = self.settings.chain;
            let batches = page.batches;

            // CPU bound: keep the decode off the async workers.
            let rows = tokio::task::spawn_blocking(move || {
                svm::decode(chain, &batches)
            })
            .await
            .context("Solana decode task panicked")?;

            self.swaps_stored
                .fetch_add(rows.swaps.len() as u64, Ordering::Relaxed);
            self.skipped_slots.fetch_add(
                served.len().saturating_sub(rows.slots.len() as u64),
                Ordering::Relaxed,
            );

            self.writer
                .send(SvmBatch { rows, windows: vec![served] })
                .await?;

            cursor = page.next_slot;
            *reached = cursor;
        }

        Ok(())
    }

    /// The stored slot the first block of `from` must build on, or `None`
    /// when nothing below `from` is known to be adjacent.
    async fn anchor_for(&self, from: u64) -> Result<Option<Continuity>> {
        // Adjacent means: a live checkpoint ends exactly here. Then every
        // slot between the stored predecessor and `from` was asked for and
        // skipped, so the height chain must close over the boundary.
        let tiling = self
            .store
            .checkpoint_tiling(
                self.settings.chain,
                from.saturating_sub(1),
                MAX_CHECKPOINTS,
            )
            .await?;

        if !tiling.iter().any(|(_, to)| *to == from) {
            return Ok(None);
        }

        Ok(self
            .store
            .anchor_below(self.settings.chain, from)
            .await?
            .map(Continuity::from))
    }

    fn forget_committed(&mut self, from: u64, to: Option<u64>) {
        let purged = BlockRange::new(from, to.unwrap_or(u64::MAX));
        self.committed =
            crate::db::ranges::subtract_ranges(&self.committed, &[purged]);
    }

    /// Windows flushed under an epoch a purge superseded meanwhile.
    async fn purge_stale_flushes(&mut self) -> Result<Option<u64>> {
        let stale: Vec<BlockRange> =
            std::mem::take(&mut *self.stale.lock().unwrap());

        let mut lowest = None;

        for range in stale {
            self.purger
                .purge_range(
                    self.settings.chain,
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

    /// The Solana-only metric series. `lag_blocks` and `lag_seconds` come
    /// out of the shared head / indexed gauges, which is the point: a slot
    /// is 0.27 s and an Ethereum block is 12 s, so only the SECONDS series
    /// is comparable across families.
    async fn publish_stats(&self) {
        if !self.metrics.is_enabled() {
            return;
        }

        if let Ok(Some((slot, timestamp))) =
            self.store.newest_slot_timestamp(self.settings.chain).await
        {
            self.metrics.set_indexed_height(slot);
            self.metrics.set_indexed_timestamp(u64::from(timestamp));
        }

        self.metrics.set_solana_stats(SolanaStats {
            queries_last_minute: self.budget.queries_last_minute(),
            queries_total: self.budget.total_queries(),
            ratelimit_requests_left: self.budget.remaining_requests(),
            swaps_stored: self.swaps_stored.load(Ordering::Relaxed),
            skipped_slots: self.skipped_slots.load(Ordering::Relaxed),
        });
    }
}

// --------------------------------------------------------- pure helpers

/// Exclusive slot the indexer wants to reach right now. There are no
/// confirmations on Solana: `check_flags` refuses any value but 0.
pub fn target_slot(head: u64, end_slot: u64) -> u64 {
    if end_slot > 0 {
        head.min(end_slot)
    } else {
        head
    }
}

/// The holes of a checkpoint tiling inside `range`, ascending.
///
/// `tiling` may overlap and need not be sorted. At most `limit` holes are
/// returned; the rest are picked up by the next pass.
pub fn holes(
    range: BlockRange,
    tiling: &[(u64, u64)],
    limit: usize,
) -> Vec<BlockRange> {
    if range.is_empty() {
        return Vec::new();
    }

    let mut covered: Vec<(u64, u64)> = tiling
        .iter()
        .copied()
        .filter(|(from, to)| *to > range.from && *from < range.to)
        .map(|(from, to)| (from.max(range.from), to.min(range.to)))
        .filter(|(from, to)| from < to)
        .collect();
    covered.sort_unstable();

    let mut holes = Vec::new();
    let mut cursor = range.from;

    for (from, to) in covered {
        if from > cursor {
            holes.push(BlockRange::new(cursor, from));
            if holes.len() >= limit {
                return holes;
            }
        }
        cursor = cursor.max(to);
    }

    if cursor < range.to {
        holes.push(BlockRange::new(cursor, range.to));
    }

    holes
}

fn pass_backoff(failures: u32) -> Duration {
    Duration::from_secs(1)
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(MAX_PASS_BACKOFF)
}

/// Errors no retry can fix.
fn is_fatal(error: &anyhow::Error) -> bool {
    Tripwire::is_cause_of(error)
        || SvmWriterStopped::is_cause_of(error)
        || error
            .chain()
            .filter_map(|cause| {
                cause.downcast_ref::<crate::reorg::ReorgError>()
            })
            .any(crate::reorg::ReorgError::is_fatal)
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

/// `indexer verify --chain solana`, so `bin/indexer.rs` has one door per
/// family and nothing else to know.
pub use solana_verify::verify;
