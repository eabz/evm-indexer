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
    db::{ranges::BlockRange, Database},
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
        status::{ChainState, StatusSink},
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

/// Being at most this far behind means "caught up", and only then is a
/// cadence worth waiting out.
///
/// Solana produces 3.76 slots/s (docs/solana-research.md §11.2), so one
/// [`TIP_CADENCE`] is ~15 new slots; 32 is that with room for a slow tick.
/// Anything beyond it is real lag, and waiting only makes it worse.
const TIP_PACE_SLOTS: u64 = 32;

/// Slots asked for in one metered query while backfilling. The server
/// truncates at its own ~5 s execution budget (30-66 slots measured); this
/// only has to be comfortably larger than that, because the CURSOR decides.
const BACKFILL_WINDOW_SLOTS: u64 = 10_000;

/// Cap of the backoff between failed passes.
const MAX_PASS_BACKOFF: Duration = Duration::from_secs(60);

/// `/readyz`: not ready when nothing happened for this long.
const READY_STALENESS: Duration = Duration::from_secs(120);

/// Gap ranges a single pass streams. A listing cut off here does NOT let
/// the cursor past the holes above it - see [`MissingSlots::covered_until`].
const MAX_GAPS_PER_PASS: usize = 1_000;

/// How often the contiguous runs of `checkpoints` are collapsed into one
/// covering row each. Same value and same reason as the EVM loop: without
/// it the table grows by one row per flush for ever (~25k rows a day at the
/// Solana head), and every resume reads all of them.
const COMPACT_CHECKPOINTS_EVERY: Duration = Duration::from_secs(300);

/// How often the operator's `sol_dex_programs` overlay is read again, so a
/// registry change does not need a restart. It only ever ADDS knowledge, so
/// a failed reload keeps the previous answer and is never fatal.
const RELOAD_PROGRAM_NAMES_EVERY: Duration = Duration::from_secs(300);

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
    /// Resolves when this chain should stop. `indexer run` passes the
    /// process signal, `indexer fleet` a per-chain cancellation handle.
    pub shutdown: BoxFuture<'static, ()>,
    /// Metered queries a minute this process allows itself. Ignored when
    /// [`Self::budget`] is set.
    pub max_queries_per_minute: u32,
    /// A budget SHARED with other chains. The Envio rate limit is per
    /// token, not per chain, so a fleet with two Solana endpoints under one
    /// token must not hand each of them the whole allowance
    /// (docs/design.md section 15). `None` = a private budget of
    /// `max_queries_per_minute`, which is what `indexer run` uses.
    pub budget: Option<Arc<Budget>>,
    /// The metrics handle to record into; `None` = build one from
    /// `--metrics-addr` and serve it here (`indexer run`).
    pub metrics: Option<Metrics>,
    /// Where the chain reports what it is doing. Off for `indexer run`.
    pub status: StatusSink,
}

impl<S: SlotSource> SolanaRuntime<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            lease: LeaseOptions::default(),
            shutdown: Box::pin(shutdown_signal()),
            max_queries_per_minute: DEFAULT_MAX_QUERIES_PER_MINUTE,
            budget: None,
            metrics: None,
            status: StatusSink::off(),
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

/// The operator's `sol_dex_programs` overlay.
///
/// A missing or empty table is NOT an error: the registry is a judgement
/// an operator makes over time (`src/svm/registry.rs`), and an unlisted
/// program simply keeps its built-in venue name. What would be an error is
/// a read that fails for another reason, so this does not swallow one.
pub(crate) async fn load_program_names(
    db: &Database,
) -> Result<svm::registry::ProgramNames> {
    let rows: Vec<svm::registry::SolDexProgram> = db
        .db
        .query(svm::registry::LOAD_SQL)
        .fetch_all()
        .await
        .context("read the sol_dex_programs registry")?;

    Ok(svm::registry::ProgramNames::new(rows))
}

/// Polls until EVERYTHING the last flush wrote is readable: its
/// `sol_slots` row and its `checkpoints` row.
///
/// Both, and not just the marker. A flush writes the marker and the
/// checkpoints as two separate inserts, and a window whose slots were all
/// skipped writes no marker row at all - so a gate that only proves the
/// marker lets a purge start while the newest checkpoint is still
/// invisible. The purge then tombstones the checkpoints it can see, the
/// invisible one survives, the tiling shows no hole where the slots were
/// removed, and those slots are never streamed again.
pub(crate) async fn wait_until_visible(
    db: Database,
    last: LastFlush,
) -> Result<()> {
    let Some(mark) = *last.lock().unwrap() else {
        return Ok(());
    };

    let (from, to) = mark.checkpoint;
    let version = mark.version;

    // No FINAL anywhere: the exact row versions this process wrote.
    let mut sql = vec![format!(
        "SELECT toUInt64(count()) FROM checkpoints \
         WHERE chain = {} AND from_block = {from} AND to_block = {to} \
         AND _version = {version} AND is_deleted = 0",
        db.chain_id
    )];

    if let Some(slot) = mark.slot {
        sql.push(format!(
            "SELECT toUInt64(count()) FROM `{COMMIT_MARKER}` \
             WHERE chain = {} AND block_number = {slot} \
             AND _version = {version}",
            db.chain_id
        ));
    }

    for statement in sql {
        let mut readable = false;

        for _ in 0..VISIBILITY_ATTEMPTS {
            let rows: u64 = db
                .db
                .query(&statement)
                .fetch_one()
                .await
                .context("read back the last Solana flush")?;

            if rows > 0 {
                readable = true;
                break;
            }

            tokio::time::sleep(VISIBILITY_DELAY).await;
        }

        if !readable {
            bail!(
                "the last Solana flush (version {version}, slots \
                 [{from}, {to})) can not be read back after {:?}: {statement}",
                VISIBILITY_DELAY * VISIBILITY_ATTEMPTS
            );
        }
    }

    Ok(())
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
    /// Decode and store the launchpad rows (on unless `--no-launchpads`).
    launchpads: bool,
}

struct SolanaIndexer<S: SlotSource> {
    settings: SolanaSettings,
    source: S,
    db: Database,
    store: SolanaReorgStore,
    purger: Purger,
    writer: SvmWriter,
    budget: Arc<Budget>,
    metrics: Metrics,
    /// The chain's lease. Housekeeping that rewrites rows another process
    /// could be splitting (the checkpoint compaction) asks it first.
    fence: Fence,
    /// Windows this process committed (and did not purge since). A
    /// checkpoint listing that misses one is a stale read, not a gap.
    committed: Vec<BlockRange>,
    stale: Arc<Mutex<Vec<BlockRange>>>,
    /// Gap heals are only looked for on the first inspection of a range.
    healed_until: u64,
    /// Where the last `stream_range` stopped, with the continuity of the
    /// last block it served: `(next slot to serve, its predecessor)`.
    ///
    /// This is the anchor of the NEXT pass whenever that pass starts
    /// exactly here, and it is exact - unlike re-reading it from
    /// `checkpoints FINAL`, which at the head means reading back the row
    /// the previous pass wrote a moment ago and losing the only fork check
    /// this chain has whenever that read is stale.
    last_continuity: Option<(u64, Continuity)>,
    /// The operator's `sol_dex_programs` overlay. It can only ADD
    /// knowledge: an unlisted program keeps its built-in venue name, so an
    /// empty table decodes exactly as before. Re-read every
    /// [`RELOAD_PROGRAM_NAMES_EVERY`], so a registry change does not need a
    /// restart.
    program_names: Arc<svm::registry::ProgramNames>,
    reloaded_names: Option<tokio::time::Instant>,
    /// When the checkpoints were last compacted.
    compacted: Option<tokio::time::Instant>,
    /// Counters for the Solana-only metric series.
    swaps_stored: Arc<AtomicU64>,
    launchpad_rows: Arc<AtomicU64>,
    skipped_slots: Arc<AtomicU64>,
    /// Where the chain says what it is doing. Off outside `indexer fleet`.
    status: StatusSink,
    /// The last state sent to `status`, so only CHANGES are reported.
    reported: Option<ChainState>,
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

    // The fleet hands its own handle in and serves one endpoint for every
    // chain; `indexer run` builds one here and serves it itself.
    let fleet_metrics = runtime.metrics.is_some();
    let metrics = match (runtime.metrics, config.metrics_addr) {
        (Some(metrics), _) => metrics,
        (None, Some(_)) => Metrics::new(chain, READY_STALENESS),
        (None, None) => Metrics::disabled(),
    };

    let status = runtime.status;
    status.state(ChainState::Starting);

    let (stop_metrics, metrics_stopped) = watch::channel(false);

    if let Some(addr) = config.metrics_addr.filter(|_| !fleet_metrics) {
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

    // The operator's curated program registry, read once: it is a few
    // dozen rows and it only ADDS knowledge, so an empty table (a fresh
    // database) decodes exactly as the built-in venue list does.
    let program_names = load_program_names(&db).await?;

    info!(
        "Modules on Solana: DEX on, launchpads {}. Program registry: {} \
         operator row(s).",
        if config.launchpads { "on" } else { "off (--no-launchpads)" },
        program_names.len(),
    );

    let store = SolanaReorgStore::new(db.clone());
    let last_flush = LastFlush::default();

    // The queue of flushes that raced another process's purge lives in
    // memory, so a restart would lose whatever was still in it. Nothing
    // else would ever ask for those slots again (their rows ARE stored,
    // so no gap query reports them), so the same question is asked of the
    // database once per start - the EVM rule, over Solana's commit marker
    // (docs/review-round-4.md, MAJOR 3).
    let stale = Arc::new(Mutex::new(
        db.stale_flush_ranges_in(COMMIT_MARKER, "block_number")
            .await
            .context("look for flushes that raced another purge")?,
    ));

    {
        let queued = stale.lock().unwrap();
        if !queued.is_empty() {
            warn!(
                "Chain {chain}: {} slot range(s) were flushed under an \
                 epoch another process had already superseded ({}). Their \
                 aggregate contributions are hidden, so they are purged \
                 and indexed again before anything else.",
                queued.len(),
                queued
                    .iter()
                    .take(8)
                    .map(|range| range.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

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

    // One HyperSync token serves every chain, and Envio meters the TOKEN:
    // a fleet passes one budget in and every Solana chain draws from it.
    let budget = runtime.budget.unwrap_or_else(|| {
        Arc::new(Budget::new(runtime.max_queries_per_minute))
    });

    let mut indexer = SolanaIndexer {
        settings: SolanaSettings {
            chain,
            start_slot,
            end_slot: config.end_block,
            new_slots_only: config.new_blocks_only,
            launchpads: config.launchpads,
        },
        source: runtime.source,
        db: db.clone(),
        store,
        purger,
        writer,
        budget: budget.clone(),
        metrics: metrics.clone(),
        fence: lease.fence(),
        committed: Vec::new(),
        stale,
        healed_until: 0,
        last_continuity: None,
        program_names: Arc::new(program_names),
        reloaded_names: None,
        compacted: None,
        swaps_stored: Arc::new(AtomicU64::new(0)),
        launchpad_rows: Arc::new(AtomicU64::new(0)),
        skipped_slots: Arc::new(AtomicU64::new(0)),
        status: status.clone(),
        reported: None,
    };

    // 1. Stop the stream ...
    let result = tokio::select! {
        result = indexer.sync() => result,
        _ = runtime.shutdown => {
            info!("Shutdown requested, flushing buffered slots.");
            Ok(())
        }
        reason = async {
            // The borrow of the watch cell is dropped before the `pending`
            // await below, so this future stays `Send` and the whole loop
            // can be spawned (the acceptance test for "a second process is
            // refused" does exactly that).
            let verdict = match
                fatal.wait_for(|reason| reason.is_some()).await
            {
                Ok(reason) => Some(reason.clone().unwrap_or_default()),
                // The lease task ended without a verdict: never fatal.
                Err(_) => None,
            };

            match verdict {
                Some(reason) => reason,
                None => std::future::pending().await,
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

        // `--new-blocks-only` starts at the head and asks the checkpoints
        // nothing: saying "the checkpoints tile [start, head)" there would
        // be a plain lie on an empty database, which is exactly what the
        // first live run printed.
        let mut cursor = if self.settings.new_slots_only {
            info!(
                "--new-blocks-only: starting at the head, slot {head}. \
                 Nothing below it will be indexed by this process."
            );
            head
        } else {
            let resume = self.resume_point().await?;
            if resume > self.settings.start_slot {
                info!(
                    "Checkpoints tile slots [{}, {resume}) without a hole.",
                    self.settings.start_slot
                );
            }
            resume
        };

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

            // BEFORE the loop decides there is nothing to do. The stale
            // queue is drained inside `pass()`, and `pass()` is skipped
            // when `target <= cursor` - so a bounded run whose range is
            // already tiled exited without ever draining it, although
            // startup announced in capitals that those spans would be
            // "purged and indexed again before anything else" (review F,
            // NEW-5). Nothing else ever asks for them: their rows ARE
            // stored, so no hole appears in the tiling.
            if target <= cursor {
                match self.purge_stale_flushes().await {
                    Ok(Some(from)) => cursor = cursor.min(from),
                    Ok(None) => {}
                    Err(e) if is_fatal(&e) => return Err(e),
                    // Transient: the next turn of the loop tries again,
                    // and the queue still holds every span.
                    Err(e) => warn!(
                        "Chain {chain}: could not purge the flushes that \
                         raced another process: {e:#}"
                    ),
                }
            }

            let behind = target.saturating_sub(cursor);

            // Wait a cadence only when we are ACTUALLY CAUGHT UP, i.e. the
            // slots above the cursor are just the handful the chain made
            // since the last commit. Pacing while the head is genuinely
            // ahead adds the wait straight to the lag - measured on the
            // first live run, where a cadence applied at any distance
            // under 256 slots let the lag drift from 0 to 76 slots while
            // the process was spending 4 of its 25 queries a minute.
            //
            // The budget governor, not this timer, is what keeps the query
            // rate polite; this timer only exists to stop us spending a
            // whole query on three new slots.
            let paced = end_slot == 0
                && behind <= TIP_PACE_SLOTS
                && last_tip_commit
                    .is_some_and(|at| at.elapsed() < TIP_CADENCE);

            // One comparison per turn, a call only when it changed.
            self.report(if behind <= TIP_PACE_SLOTS {
                ChainState::Following
            } else {
                ChainState::Backfilling
            });

            if target > cursor && !paced {
                match self.pass(BlockRange::new(cursor, target)).await {
                    Ok(PassOutcome::Covered(covered)) => {
                        failures = 0;
                        cursor = covered;
                        self.metrics.set_ready(true);
                        last_tip_commit =
                            Some(tokio::time::Instant::now());
                        self.compact_checkpoints().await;
                        self.reload_program_names().await;
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
                        self.status.failed(
                            &crate::tokens::redact::redact_urls(&format!(
                                "{e:#}"
                            )),
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

    /// Tells the status sink about a state CHANGE and nothing else.
    fn report(&mut self, state: ChainState) {
        if self.reported != Some(state) {
            self.reported = Some(state);
            self.status.state(state);
        }
    }

    /// Witness 1: the slot up to which the live checkpoints tile
    /// `[start_slot, ..)` with no hole.
    async fn resume_point(&self) -> Result<u64> {
        self.store
            .resume_point(self.settings.chain, self.settings.start_slot)
            .await
    }

    /// The holes of the checkpoint tiling inside `range`.
    ///
    /// This REPLACES the EVM "every integer has a `blocks` row" query, and
    /// it has to: on Solana a slot with no row is usually a skipped slot,
    /// and that query would report every one of them as a gap for ever.
    async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<MissingSlots> {
        let tiling = self
            .store
            .checkpoint_tiling(
                self.settings.chain,
                range.from,
                Some(range.to),
            )
            .await?;

        Ok(holes(range, &tiling, MAX_GAPS_PER_PASS))
    }

    /// Housekeeping after a committed pass: collapse the runs of
    /// contiguous `checkpoints` this process keeps adding to (one row per
    /// flush, ~25k a day at the Solana head) into one covering row each.
    ///
    /// Never fatal: it changes no answer, only how many rows hold it. And
    /// never without the lease - a purge of another process may be
    /// splitting the very rows this would merge. Exactly the EVM loop's
    /// `Indexer::compact_checkpoints`.
    async fn compact_checkpoints(&mut self) {
        let now = tokio::time::Instant::now();

        if self
            .compacted
            .is_some_and(|last| now - last < COMPACT_CHECKPOINTS_EVERY)
        {
            return;
        }

        if let Err(e) = self.fence.check() {
            debug!("Not compacting the checkpoints: {e:#}");
            return;
        }

        self.compacted = Some(now);

        if let Err(e) = self.store.compact_checkpoints().await {
            warn!(
                "Chain {}: could not compact the checkpoints: {e:#}. \
                 Nothing is wrong with what is stored; the next pass \
                 tries again.",
                self.settings.chain
            );
        }
    }

    /// Re-reads the operator's `sol_dex_programs` overlay, so adding a
    /// program to the registry takes effect without a restart.
    ///
    /// Never fatal: the registry only ADDS names, so keeping the previous
    /// answer for another interval loses nothing.
    async fn reload_program_names(&mut self) {
        let now = tokio::time::Instant::now();

        if self
            .reloaded_names
            .is_some_and(|last| now - last < RELOAD_PROGRAM_NAMES_EVERY)
        {
            return;
        }

        self.reloaded_names = Some(now);

        match load_program_names(&self.db).await {
            Ok(names) => {
                if names.len() != self.program_names.len() {
                    info!(
                        "Chain {}: the `sol_dex_programs` registry now has \
                         {} operator row(s) (was {}).",
                        self.settings.chain,
                        names.len(),
                        self.program_names.len()
                    );
                }
                self.program_names = Arc::new(names);
            }
            Err(e) => warn!(
                "Chain {}: could not re-read the `sol_dex_programs` \
                 registry: {e:#}. Keeping the {} row(s) read before.",
                self.settings.chain,
                self.program_names.len()
            ),
        }
    }

    fn saw_head(&self, head: u64) {
        self.metrics.set_head(head.saturating_sub(1));
    }

    async fn pass(&mut self, range: BlockRange) -> Result<PassOutcome> {
        if let Some(from) = self.purge_stale_flushes().await? {
            return Ok(PassOutcome::Restart(from));
        }

        let MissingSlots { mut ranges, covered_until } =
            self.missing_ranges(range).await?;

        // No read-your-writes: what this process committed is not a gap,
        // whatever a lagging read says.
        ranges =
            crate::db::ranges::subtract_ranges(&ranges, &self.committed);

        // Gap healing, on the FIRST inspection of a range only: a flush
        // that died between its children and its `sol_slots` insert left
        // orphans there, and streaming on top of them would make the
        // candles count both.
        //
        // `covered_until`, NOT `range.to`: when the hole listing hit
        // `MAX_GAPS_PER_PASS` it only accounts for the slots below the last
        // hole it returned, and marking the rest inspected would leave
        // those holes unexamined for the life of the process.
        self.heal_gaps(&ranges, covered_until).await?;

        let missing = ranges;

        if missing.is_empty() {
            return Ok(PassOutcome::Covered(covered_until));
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

        Ok(PassOutcome::Covered(covered_until))
    }

    /// Purges the gap ranges that still hold rows, once per range.
    ///
    /// A hole in the checkpoint tiling is about to be streamed again, so
    /// EVERY row already stored inside it would be counted twice by the
    /// materialized views (the base tables still read right under `FINAL`;
    /// the candles and the launchpad aggregates are
    /// `SimpleAggregateFunction(sum, ..)` and can not take a contribution
    /// back). There are two ways a hole can hold rows, and both have to be
    /// purged:
    ///
    /// * **orphan children** - a flush that died between its children and
    ///   its `sol_slots` insert. This is the EVM invariant, checked through
    ///   the same trait.
    /// * **live commit markers** - a flush whose marker landed and whose
    ///   CHECKPOINT did not (the checkpoint is a separate insert after the
    ///   marker: its retries can be exhausted, or the writer aborted
    ///   between the two awaits). `has_orphan_children` is false for those
    ///   slots, precisely because their marker is alive, so without this
    ///   second test the range would be re-streamed on top of live data.
    ///   Design §2 says so in one line: `checkpoints` is an index, not the
    ///   resume oracle - the DATA decides.
    ///
    /// A purge of a range that holds nothing is not free, so the cheap
    /// count comes first and only a hole that really holds something is
    /// purged. The purge itself is idempotent.
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

            let stored =
                self.store.stored_slots(self.settings.chain, *gap).await?;

            if stored == 0
                && !self
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

            if stored > 0 {
                warn!(
                    "Chain {}: slots {gap} are not claimed by any \
                     checkpoint but still hold {stored} live \
                     `{COMMIT_MARKER}` row(s) - the checkpoint of that \
                     flush never landed. Purging them before the range is \
                     streamed again, so the candles do not count it twice.",
                    self.settings.chain
                );
            } else {
                warn!(
                    "Chain {}: slots {gap} hold rows of a flush that died \
                     before its `{COMMIT_MARKER}` insert. Purging them \
                     before the range is streamed again.",
                    self.settings.chain
                );
            }

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
            let names = self.program_names.clone();
            let launchpads = self.settings.launchpads;

            // CPU bound: keep the decode off the async workers.
            let mut rows = tokio::task::spawn_blocking(move || {
                svm::decode_with(chain, &batches, &names)
            })
            .await
            .context("Solana decode task panicked")?;

            // `--no-launchpads`. The decode itself still ran: it is a pure
            // function over rows that were fetched anyway, so skipping it
            // would save nothing on the wire and only add a branch to the
            // decoder. What the flag means is that none of it is STORED.
            if !launchpads {
                rows.launchpads = Default::default();
            }

            self.swaps_stored
                .fetch_add(rows.swaps.len() as u64, Ordering::Relaxed);
            self.launchpad_rows.fetch_add(
                rows.launchpads.rows() as u64,
                Ordering::Relaxed,
            );
            self.skipped_slots.fetch_add(
                served.len().saturating_sub(rows.slots.len() as u64),
                Ordering::Relaxed,
            );

            self.writer.send(SvmBatch::new(rows, vec![served])).await?;

            cursor = page.next_slot;
            *reached = cursor;

            // Carry the anchor to the next pass in memory. At the head the
            // next pass starts exactly here, and this is the value it
            // needs: re-reading it from `checkpoints FINAL` would mean
            // reading back the row this pass is about to write, which is
            // the one read ClickHouse is allowed to answer stale.
            if let Some(previous) = previous {
                self.last_continuity = Some((cursor, previous));
            }
        }

        Ok(())
    }

    /// The stored slot the first block of `from` must build on, or `None`
    /// when nothing below `from` is known to be adjacent.
    ///
    /// Three sources, in order of trust:
    ///
    /// 1. **The anchor this process carried over from the last pass.** At
    ///    the head this is the normal case and it is exact.
    /// 2. **The database**, when a live checkpoint ends exactly at `from`:
    ///    then every slot between the stored predecessor and `from` was
    ///    asked for and skipped, so the height chain must close over the
    ///    boundary.
    /// 3. **Nothing** - and then the first served block of the range is
    ///    not checked, which is why case 2 failing where an anchor was
    ///    expected is a `warn!` and not silence. The measured ~3%
    ///    no-read-your-writes rate used to switch the only fork check this
    ///    chain has off at about one head boundary in 33, with no log line.
    async fn anchor_for(&self, from: u64) -> Result<Option<Continuity>> {
        if let Some(carried) = carried_anchor(self.last_continuity, from) {
            return Ok(Some(carried));
        }

        if self.store.checkpoint_ends_at(self.settings.chain, from).await?
        {
            return Ok(self
                .store
                .anchor_below(self.settings.chain, from)
                .await?
                .map(Continuity::from));
        }

        // Not adjacent. Nothing stored below means this is simply the
        // bottom of the index; a stored predecessor means the checkpoint
        // that should claim it is missing or was not readable.
        if let Some(below) =
            self.store.anchor_below(self.settings.chain, from).await?
        {
            warn!(
                "Chain {}: no live checkpoint ends at slot {from}, though \
                 slot {} is stored below it. The continuity of the first \
                 block served for this range can not be checked. This is \
                 expected right after a purge of the range below; if it \
                 repeats at the head, the `checkpoints` read is lagging \
                 behind the writes.",
                self.settings.chain, below.block_number
            );
        }

        Ok(None)
    }

    fn forget_committed(&mut self, from: u64, to: Option<u64>) {
        let purged = BlockRange::new(from, to.unwrap_or(u64::MAX));
        self.committed =
            crate::db::ranges::subtract_ranges(&self.committed, &[purged]);

        // The carried anchor described a block that may no longer be
        // stored: a purge is exactly when it must be read from the
        // database again rather than believed.
        if self.last_continuity.is_some_and(|(reached, _)| reached > from)
        {
            self.last_continuity = None;
        }
    }

    /// Windows flushed under an epoch a purge superseded meanwhile.
    ///
    /// The queue is the ONLY record that those slots have to be indexed
    /// again - their rows are stored, so no gap query reports them - so a
    /// span leaves it only after its purge succeeded. That rule, and the
    /// loop that applies it, are [`Purger::purge_queued`], shared with the
    /// EVM pipeline (docs/review-round-4.md, MAJOR 3); this used to take
    /// the whole `Vec` and lose the failed span and every span after it
    /// on the first transient error.
    async fn purge_stale_flushes(&mut self) -> Result<Option<u64>> {
        let purger = self.purger.clone();
        let stale = self.stale.clone();
        let mut forgotten: Vec<BlockRange> = Vec::new();

        let lowest = purger
            .purge_queued(
                self.settings.chain,
                &stale,
                PurgeReason::GapHeal,
                |range| forgotten.push(range),
            )
            .await;

        // Also after a failure part way through: what WAS purged must not
        // stay in `committed`, or the next pass would take those slots for
        // stored and never stream them again.
        for range in forgotten {
            self.forget_committed(range.from, Some(range.to));
        }

        Ok(lowest?)
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

/// The holes of a checkpoint tiling inside a range, and how far the
/// listing accounts for.
///
/// The second field is what stops a truncated listing from advancing the
/// cursor past holes nobody looked at: the EVM path calls it
/// `MissingRanges::covered_until` and this is the same idea.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSlots {
    /// Ascending, non overlapping, at most `limit` of them.
    pub ranges: Vec<BlockRange>,
    /// Every slot of the inspected range below this one is either claimed
    /// by the tiling or listed in `ranges`. Equals the end of the
    /// inspected range unless the listing was truncated at `limit`.
    pub covered_until: u64,
}

/// The holes of a checkpoint tiling inside `range`, ascending.
///
/// `tiling` may overlap and need not be sorted. At most `limit` holes are
/// returned; the rest are picked up by a later pass, which is what
/// [`MissingSlots::covered_until`] keeps honest.
pub fn holes(
    range: BlockRange,
    tiling: &[(u64, u64)],
    limit: usize,
) -> MissingSlots {
    if range.is_empty() {
        return MissingSlots {
            ranges: Vec::new(),
            covered_until: range.to,
        };
    }

    let mut covered: Vec<(u64, u64)> = tiling
        .iter()
        .copied()
        .filter(|(from, to)| *to > range.from && *from < range.to)
        .map(|(from, to)| (from.max(range.from), to.min(range.to)))
        .filter(|(from, to)| from < to)
        .collect();
    covered.sort_unstable();

    let mut holes: Vec<BlockRange> = Vec::new();
    let mut cursor = range.from;

    for (from, to) in covered {
        if from > cursor {
            holes.push(BlockRange::new(cursor, from));
            if holes.len() >= limit {
                // Cut off: nothing above the last hole was looked at, so
                // the caller must not treat it as covered.
                let covered_until =
                    holes.last().map(|hole| hole.to).unwrap_or(range.from);
                return MissingSlots { ranges: holes, covered_until };
            }
        }
        cursor = cursor.max(to);
    }

    if cursor < range.to {
        holes.push(BlockRange::new(cursor, range.to));
    }

    MissingSlots { ranges: holes, covered_until: range.to }
}

/// The anchor carried over from the last pass, when it applies to a range
/// starting at `from`.
///
/// It applies only when the last pass stopped EXACTLY there. One slot of
/// distance means slots in between were never served by this process, so
/// nothing says the next produced block is the stored one's successor and
/// the anchor has to come from the database (or from nowhere).
pub fn carried_anchor(
    last: Option<(u64, Continuity)>,
    from: u64,
) -> Option<Continuity> {
    last.filter(|(reached, _)| *reached == from)
        .map(|(_, continuity)| continuity)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn range(from: u64, to: u64) -> BlockRange {
        BlockRange::new(from, to)
    }

    #[test]
    fn a_complete_tiling_has_no_hole_and_covers_the_whole_range() {
        let missing = holes(range(100, 200), &[(90, 150), (150, 260)], 10);

        assert!(missing.ranges.is_empty());
        assert_eq!(missing.covered_until, 200);
    }

    #[test]
    fn the_holes_of_a_tiling_are_ascending_and_the_range_is_covered() {
        let missing =
            holes(range(100, 200), &[(100, 120), (140, 160)], 10);

        assert_eq!(missing.ranges, vec![range(120, 140), range(160, 200)]);
        assert_eq!(missing.covered_until, 200);
    }

    /// A hole listing cut off at `limit` must NOT report the range as
    /// covered up to its end.
    ///
    /// `pass()` returns `covered_until` as the new cursor and `heal_gaps`
    /// marks everything below it inspected. With `range.to` there, the
    /// holes above the cut fall below both for the life of the process:
    /// they are never streamed, never healed, and `/readyz` still says
    /// ready. Only a restart would find them.
    #[test]
    fn a_truncated_hole_listing_does_not_claim_the_slots_above_the_cut() {
        // Ten one-slot holes between eleven one-slot checkpoints.
        let tiling: Vec<(u64, u64)> =
            (0..11).map(|i| (100 + i * 2, 101 + i * 2)).collect();

        let missing = holes(range(100, 200), &tiling, 3);

        assert_eq!(
            missing.ranges,
            vec![range(101, 102), range(103, 104), range(105, 106)]
        );
        // The end of the LAST listed hole, not 200: nothing above it was
        // looked at.
        assert_eq!(missing.covered_until, 106);

        // ... and the untruncated listing of the same tiling really does
        // hold more, so the test is not passing by accident.
        let all = holes(range(100, 200), &tiling, 1_000);
        assert!(all.ranges.len() > 3);
        assert_eq!(all.covered_until, 200);
    }

    #[test]
    fn an_empty_range_has_no_holes() {
        let missing = holes(range(100, 100), &[], 10);
        assert!(missing.ranges.is_empty());
        assert_eq!(missing.covered_until, 100);
    }

    // ------------------------------------------------- the carried anchor

    fn continuity(slot: u64) -> Continuity {
        Continuity { slot, blockhash: [7u8; 32], block_height: 900_000 }
    }

    /// The anchor the last pass left behind is the next pass's predecessor
    /// when - and only when - the two are adjacent.
    ///
    /// At the head every pass starts exactly where the previous one
    /// stopped, and re-reading that boundary from `checkpoints FINAL`
    /// means reading back the row the previous pass has just written: the
    /// one read ClickHouse is allowed to answer stale (~3% measured). When
    /// it does, the only fork check this chain has is skipped for the
    /// first served block, with no log line.
    #[test]
    fn the_carried_anchor_is_used_only_when_it_is_adjacent() {
        let last = Some((500, continuity(498)));

        assert_eq!(carried_anchor(last, 500), Some(continuity(498)));

        // One slot away is not adjacent: slots in between were never
        // served by this process, so nothing says the next produced block
        // is the stored one's successor.
        assert_eq!(carried_anchor(last, 501), None);
        assert_eq!(carried_anchor(last, 499), None);
        assert_eq!(carried_anchor(None, 500), None);
    }

    #[test]
    fn committed_ranges_merge_and_empty_ones_are_dropped() {
        let mut committed = Vec::new();
        remember(&mut committed, range(100, 110));
        remember(&mut committed, range(120, 130));
        remember(&mut committed, range(110, 120));
        remember(&mut committed, range(200, 200));

        assert_eq!(committed, vec![range(100, 130)]);
    }
}
