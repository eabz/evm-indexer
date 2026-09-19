//! Reorg handling: the decisions, without ClickHouse and without HyperSync.
//!
//! Everything that touches the outside world goes through four small
//! traits, so the whole rollback logic runs (and is tested) in memory:
//!
//! | Trait | Implemented by | What it is |
//! |---|---|---|
//! | [`CanonicalChain`] | the HyperSync source | block headers of the chain as it is NOW |
//! | [`ReorgStore`] | the ClickHouse database | what is stored, and the insert-only purge steps |
//! | [`WriterControl`] | the pipeline writer | flush what is buffered, adopt a new epoch |
//! | [`DiscoveryCache`], [`ReorgMetrics`] | token / pool workers, metrics | optional hooks |
//!
//! The pieces, bottom up:
//!
//! * [`find_fork_point`] - given "block B does not build on what we stored",
//!   walks back in growing windows until stored and canonical agree.
//! * [`Purger::purge_range`] - the ONE primitive that removes data
//!   (docs/design.md, section 2): insert-only, idempotent, crash safe. The
//!   order of its steps lives here and nowhere else.
//! * [`ReorgGuard`] - the state machine the pipeline feeds every streamed
//!   header to. It answers [`Verdict::Continue`] or [`Verdict::Rollback`],
//!   repairs a rollback, and heals gaps on the first pass after a start.
//!
//! `README.md` next to this file explains all of it in plain language.
//!
//! One indexer process per chain is assumed (any number of chains share the
//! database). Two processes writing the SAME chain would race on the epoch.

mod fork;
mod guard;
mod purge;

#[cfg(test)]
mod model;
#[cfg(test)]
mod tests;

pub use fork::{find_fork_point, ForkPoint};
pub use guard::{ReorgGuard, Rollback, Verdict};
pub use purge::{
    PurgeOptions, PurgeReport, PurgeStep, Purger, REPAIR_ALIGNMENT_SECONDS,
};

use alloy::primitives::B256;
use futures::future::BoxFuture;
use std::{fmt, time::Duration};

/// Default of `--max-reorg-depth`.
pub const DEFAULT_MAX_REORG_DEPTH: u64 = 512;

/// First window of the fork-point search; every next one is twice as big.
pub const FIRST_SEARCH_WINDOW: u64 = 8;

/// The part of a block the reorg logic needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    /// Unix seconds.
    pub timestamp: u32,
}

/// What a HyperSync `rollback_guard` says about the first block of a
/// response: "block `first_block` builds on `first_parent_hash`". The
/// pipeline converts the HyperSync type into this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamGuard {
    pub first_block: u64,
    pub first_parent_hash: B256,
}

/// Settings of the reorg logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorgConfig {
    pub chain: u64,
    /// `--start-block`: blocks below it do not exist for this process. A
    /// fork point is never lower, and the first block is never compared
    /// with a stored predecessor.
    pub start_block: u64,
    /// `--max-reorg-depth`: a rollback of more blocks than this is a
    /// fatal error instead of a purge.
    pub max_reorg_depth: u64,
    /// How often the fork-point search starts over when the canonical
    /// source contradicts itself (it is reorganizing right now).
    pub canonical_attempts: u32,
    /// Pause between those attempts.
    pub canonical_retry_delay: Duration,
}

impl ReorgConfig {
    pub fn new(chain: u64, start_block: u64) -> Self {
        Self {
            chain,
            start_block,
            max_reorg_depth: DEFAULT_MAX_REORG_DEPTH,
            canonical_attempts: 5,
            canonical_retry_delay: Duration::from_millis(500),
        }
    }
}

/// The chain as the source sees it right now (HyperSync in production).
pub trait CanonicalChain: Send + Sync {
    /// Headers of `[from, to)`, ascending. A bounded range, at most a few
    /// hundred blocks. Heights the source does not have (the chain got
    /// shorter) are simply absent; the caller checks completeness and that
    /// the headers chain, so no validation is needed here.
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<BlockHeader>>>;
}

/// Why a range is purged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeReason {
    /// Rollback after a fork-point search.
    Reorg {
        /// Highest stored block when the reorg was detected.
        old_head: u64,
        /// Stored / canonical hash at the height where they disagreed.
        old_hash: B256,
        new_hash: B256,
        depth: u64,
    },
    /// A gap range holding children of a flush that died before its
    /// `blocks` rows were written.
    GapHeal,
    /// One module (DEX, predictions) decodes stored logs again and its
    /// rows of the range are replaced. Run it against a [`ReorgStore`]
    /// scoped to that module:
    ///
    /// * `tombstone_children` / `live_children` / `min_timestamp` cover
    ///   the module's tables only; the `blocks` and checkpoint methods do
    ///   nothing and return 0 (the blocks stay).
    /// * `rebuild_derived` must STILL rebuild every aggregate of the
    ///   chain, not only the module's: the epoch and the validity rule are
    ///   per chain, so the new `reorgs` row hides every older contribution
    ///   from `from_ts` on. And it must leave the purged block range out
    ///   ONLY for the module's aggregates (their rows are written again);
    ///   the rows of every other table in that range stay alive, nobody
    ///   streams them again, so their rebuild has to count them.
    Redecode,
}

impl PurgeReason {
    /// Value of `reorgs.reason`.
    pub fn as_str(&self) -> &'static str {
        match self {
            PurgeReason::Reorg { .. } => "reorg",
            PurgeReason::GapHeal => "gap_heal",
            PurgeReason::Redecode => "redecode",
        }
    }
}

/// One row of the `reorgs` table. `detected_at` is filled by ClickHouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorgRecord {
    pub chain: u64,
    /// The purge generation this purge starts.
    pub epoch: u32,
    /// Start of day (UTC, unix seconds) of the earliest purged row: from
    /// this bucket on, only contributions of `epoch` or newer count.
    pub from_ts: u32,
    /// First purged block.
    pub fork_block: u64,
    /// Exclusive end of the purged range, `None` = open ended. Not a
    /// column today; kept for logs.
    pub to_block: Option<u64>,
    pub old_head: u64,
    pub old_hash: B256,
    pub new_hash: B256,
    pub depth: u64,
    /// CHILD rows tombstoned (the `blocks` rows die after this row is
    /// written, so they are not counted).
    pub rows_tombstoned: u64,
    /// `reorg` | `gap_heal` | `redecode`
    pub reason: &'static str,
}

/// What is stored, plus the insert-only steps of a purge. One method = one
/// ClickHouse round trip (or one statement per table); the ORDER in which
/// they run is decided by [`Purger::purge_range`], never by the
/// implementation. `to: None` means "open ended" everywhere.
///
/// Every method must be safe to repeat: a purge that died half way is
/// simply run again.
pub trait ReorgStore: Send + Sync {
    /// `max(epoch)` of the chain in `reorgs`, 0 when it has none. The
    /// writer stamps this on every row.
    fn current_epoch(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<u32>>;

    /// Highest LIVE block (`blocks FINAL`), `None` when nothing is stored.
    fn stored_head(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<Option<u64>>>;

    /// `(number, hash)` of the LIVE blocks (`blocks FINAL`) in `[from,
    /// to)`, any order. Missing heights are simply absent.
    fn stored_hashes(
        &self,
        chain: u64,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<(u64, B256)>>>;

    /// True when any child table holds a row, LIVE OR TOMBSTONED (no
    /// `FINAL`), at a block number in the range that has no live `blocks`
    /// row. Tombstoned rows count on purpose: they are the only trace a
    /// gap-heal purge that died half way leaves behind (see README,
    /// "What is guaranteed after a crash").
    fn has_orphan_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<bool>>;

    /// Smallest `timestamp` in the range over EVERY block-scoped table,
    /// `blocks` included, over ALL row versions (no `FINAL`, tombstoned
    /// rows included). `None` when the range holds no row at all.
    fn min_timestamp(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<Option<u32>>>;

    /// LIVE rows (`FINAL`) of every block-scoped table EXCEPT `blocks` in
    /// the range: what [`tombstone_children`](Self::tombstone_children)
    /// still has to catch.
    fn live_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// LIVE `blocks` rows in the range.
    fn live_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// LIVE rows (`FINAL`) of the READ-PATH SIDE TABLES of the scope in
    /// the range, after the base tables were tombstoned: what a lost
    /// materialized-view push left behind.
    ///
    /// A side row is written only by the view of its base row, so once the
    /// base row is dead nothing ever rewrites the side row: without this
    /// check an orphan there is permanent (a `tx_lookup` entry pointing at
    /// a block that was rolled back, a `dex_swaps_by_pool` row of a swap
    /// that never happened).
    fn live_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// Tombstones those rows DIRECTLY, with the same statement shape the
    /// base tables use. Only ever a REPAIR: in the normal case the views
    /// already did it and this writes nothing. Returns the rows
    /// tombstoned.
    fn tombstone_side_rows(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// LIVE checkpoints overlapping the range.
    fn live_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// Tombstones every live checkpoint overlapping the range and
    /// re-inserts the part of it that lies outside the range. Returns the
    /// checkpoints tombstoned.
    fn tombstone_checkpoints(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// Tombstones the live rows of every block-scoped table EXCEPT
    /// `blocks` in the range (`INSERT .. SELECT .., version, 1 FROM t FINAL
    /// WHERE ..`; side tables follow through their views). Returns the
    /// rows tombstoned.
    fn tombstone_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;

    /// Appends the audit row that also arms the validity rule.
    fn insert_reorg<'a>(
        &'a self,
        record: &'a ReorgRecord,
    ) -> BoxFuture<'a, anyhow::Result<()>>;

    /// Bucket repair: runs the rebuild of EVERY `DerivedTable` (core, DEX,
    /// predictions) for the chain from `from_ts` on, filed under `epoch`.
    /// Every rebuild must leave out the purged block range by itself: its
    /// `blocks` rows are still alive at this point, and the tombstones of
    /// its children may not be readable yet.
    fn rebuild_derived(
        &self,
        chain: u64,
        from_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Tombstones the live `blocks` rows in the range. Always the LAST
    /// write of a purge. Returns the rows tombstoned.
    fn tombstone_blocks(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>>;
}

/// The two things a purge needs from the pipeline writer.
pub trait WriterControl: Send + Sync {
    /// Flushes (or drops) every buffered row of the chain. When it
    /// returns, nothing is in flight - no row stamped with the old epoch
    /// may reach the database afterwards - AND what was flushed can be
    /// read back (ClickHouse has no read-your-writes: poll for the
    /// `blocks` rows of the last flush). Cheap when nothing is buffered.
    fn quiesce(&self) -> BoxFuture<'_, anyhow::Result<()>>;

    /// Rows written from now on carry `epoch`.
    fn adopt_epoch(&self, epoch: u32);
}

/// Caches of background discoveries (tokens, pools) keyed by block.
pub trait DiscoveryCache: Send + Sync {
    /// Forget what was discovered in `[from, to)` (`None` = open ended),
    /// so it is discovered again when the range is streamed again.
    fn evict_range(&self, from: u64, to: Option<u64>);
}

pub trait ReorgMetrics: Send + Sync {
    /// A rollback of `depth` blocks was decided.
    fn reorg(&self, depth: u64);
    /// A purge (rollback or gap heal) finished.
    fn purge_observed(&self, duration: Duration, blocks: u64);
}

/// For callers without caches or metrics.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHooks;

impl DiscoveryCache for NoHooks {
    fn evict_range(&self, _from: u64, _to: Option<u64>) {}
}

impl ReorgMetrics for NoHooks {
    fn reorg(&self, _depth: u64) {}
    fn purge_observed(&self, _duration: Duration, _blocks: u64) {}
}

impl ReorgMetrics for crate::metrics::Metrics {
    fn reorg(&self, depth: u64) {
        crate::metrics::Metrics::reorg(self, depth);
    }

    fn purge_observed(&self, duration: Duration, blocks: u64) {
        crate::metrics::Metrics::purge_observed(self, duration, blocks);
    }
}

/// Everything that can go wrong. [`ReorgError::is_fatal`] tells the sync
/// loop whether retrying the pass can help.
#[derive(Debug)]
pub enum ReorgError {
    /// The rollback would be deeper than `--max-reorg-depth`. Fatal.
    ReorgTooDeep { chain: u64, mismatch_at: u64, depth: u64, max: u64 },
    /// The stored block 0 is not the canonical block 0. Fatal.
    GenesisMismatch { chain: u64, stored: B256, canonical: B256 },
    /// The canonical source kept contradicting itself. Retry the pass.
    CanonicalUnstable {
        chain: u64,
        at: u64,
        attempts: u32,
        detail: String,
    },
    /// The streamed blocks contradict each other but nothing STORED is
    /// wrong. Retry the pass.
    SourceInconsistent { chain: u64, at: u64 },
    /// A tombstone statement kept missing rows. Fatal: the restart finds
    /// the same orphans and purges again.
    TombstonesNotConverging {
        chain: u64,
        step: PurgeStep,
        live: u64,
        attempts: u32,
    },
    /// A chain has used all 2^32 epochs. Fatal.
    EpochOverflow { chain: u64 },
    /// The canonical source failed. Retry the pass.
    Canonical { source: anyhow::Error },
    /// The store or the writer failed at `step`. Retry the pass: the purge
    /// starts over.
    Step { step: PurgeStep, source: anyhow::Error },
}

impl ReorgError {
    /// Fatal errors need an operator; everything else heals by running
    /// the pass again.
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            ReorgError::ReorgTooDeep { .. }
                | ReorgError::GenesisMismatch { .. }
                | ReorgError::EpochOverflow { .. }
                | ReorgError::TombstonesNotConverging { .. }
        )
    }
}

impl fmt::Display for ReorgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReorgError::ReorgTooDeep {
                chain,
                mismatch_at,
                depth,
                max,
            } => {
                write!(
                    f,
                    "chain {chain}: the reorg detected at block \
                     {mismatch_at} is at least {depth} blocks deep, more \
                     than --max-reorg-depth {max}. Nothing was changed. \
                     Either the HyperSync endpoint serves a different \
                     chain than the one stored (check --hypersync-url and \
                     --chain-id), or the chain really reorganized that \
                     deep: then restart with a larger --max-reorg-depth \
                     (the purge and re-index are automatic) and consider \
                     a larger --confirmations"
                )
            }
            ReorgError::GenesisMismatch { chain, stored, canonical } => {
                write!(
                    f,
                    "chain {chain}: the stored block 0 ({stored:?}) is not \
                     the block 0 of the source ({canonical:?}). The \
                     database holds a different network under this chain \
                     id (or a devnet was reset). Nothing was changed: \
                     point the indexer at the right endpoint or at an \
                     empty database"
                )
            }
            ReorgError::CanonicalUnstable {
                chain,
                at,
                attempts,
                detail,
            } => write!(
                f,
                "chain {chain}: the source kept changing while looking \
                 for the fork point below block {at} ({attempts} \
                 attempts, last problem: {detail}). Nothing was changed, \
                 the pass is retried"
            ),
            ReorgError::SourceInconsistent { chain, at } => write!(
                f,
                "chain {chain}: block {at} does not build on the block \
                 streamed before it, but nothing stored is affected. The \
                 pass is retried"
            ),
            ReorgError::TombstonesNotConverging {
                chain,
                step,
                live,
                attempts,
            } => write!(
                f,
                "chain {chain}: step `{step}` still sees {live} live \
                 row(s) after {attempts} attempts. Something else is \
                 writing this chain (only one indexer per chain is \
                 supported), or ClickHouse is badly behind. Restarting is \
                 safe: the purge is detected and run again"
            ),
            ReorgError::EpochOverflow { chain } => {
                write!(f, "chain {chain}: ran out of purge epochs (2^32)")
            }
            ReorgError::Canonical { source } => {
                write!(f, "canonical header lookup failed: {source:#}")
            }
            ReorgError::Step { step, source } => write!(
                f,
                "purge stopped at step `{step}`: {source:#}. It is run \
                 again from the start by the next pass"
            ),
        }
    }
}

impl std::error::Error for ReorgError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReorgError::Canonical { source }
            | ReorgError::Step { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Start of day (UTC) of `timestamp`: the alignment of `reorgs.from_ts`.
pub fn start_of_day(timestamp: u32) -> u32 {
    timestamp - timestamp % REPAIR_ALIGNMENT_SECONDS
}
