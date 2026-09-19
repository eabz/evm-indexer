//! `purge_range`: the one primitive that removes data.
//!
//! Insert only (docs/design.md, section 2): rows die by tombstone,
//! aggregates are repaired under a new epoch, nothing is ever deleted. The
//! order of the steps is the whole point, so it lives here, in one
//! function, and the test model can stop it after any step:
//!
//! ```text
//!  0 quiesce the writer          nothing of the old epoch is in flight, and
//!                                what was flushed can be read back
//!  1 epoch   = current + 1
//!  2 tombstone checkpoints       a checkpoint may under-claim, never over-claim
//!  3 tombstone children          transactions, logs, transfers, dex rows ...
//!    from_ts = start of day (UTC) of the earliest row in the range,
//!    to_ts   = start of the day AFTER the newest row in the range,
//!              both over ALL row versions and ALL block-scoped tables
//!  4 insert the `reorgs` row     arms the validity rule: readers briefly
//!                                UNDER-count the repaired buckets;
//!                                the writer adopts the epoch right away
//!  5 rebuild every aggregate     over [from_ts, to_ts) - exactly the
//!                                buckets the `reorgs` row hides - under
//!                                the new epoch
//!  6 tombstone `blocks`          LAST durable write = the commit marker
//!  7 repair the side tables       normally a no-op: their views already
//!                                 tombstoned them. A view push that was
//!                                 LOST (the base part landed, the push
//!                                 did not) is the one case nothing else
//!                                 can ever fix, so it is verified and
//!                                 repaired directly.
//!  8 mark the `reorgs` row completed
//!                                everything is durable; the next start
//!                                must not mistake the debris of THIS
//!                                purge for an unfinished one
//!  9 evict caches, metrics
//! ```
//!
//! Why a crash anywhere is harmless:
//!
//! * Rollback: until step 6 the orphaned `blocks` rows are alive, so the
//!   next start finds the same parent-hash mismatch, searches the same fork
//!   point and runs everything again under a newer epoch. The validity rule
//!   makes the abandoned epoch invisible.
//! * Step 6 itself may die half way (one INSERT per partition). Tombstoned
//!   blocks are then gaps; they still hold (tombstoned) children, so the
//!   first pass after the start heals them with this same function, and
//!   the blocks that survived are caught by the parent-hash checks.
//! * Gap heal: there is no `blocks` row to keep alive. The marker is the
//!   children themselves, dead or alive
//!   ([`ReorgStore::has_orphan_children`] counts tombstoned rows too), and
//!   they stay until the range is streamed again.
//! * `from_ts` / `to_ts` are computed over all row versions, so a second
//!   run sees the same (or a wider) window even though the first run
//!   already tombstoned part of the range. It may never see a NARROWER
//!   one: a bucket the validity rule hides and no repair covers would read
//!   as empty for ever.
//!
//! ClickHouse gives NO read-your-writes: a query issued right after an
//! INSERT returned can miss the new rows for a few milliseconds. So every
//! read that decides what to write is either repeated until it is proven
//! complete (steps 2, 3 and 6 re-issue their idempotent tombstone statement
//! until a count of the live rows says 0 twice in a row, a bounded number
//! of times), made independent of fresh writes (the rebuild leaves the
//! purged range out
//! instead of relying on tombstones; the epoch is also remembered in
//! memory), or taken twice (the `[from_ts, to_ts)` window: before anything
//! is written and again after the tombstones converged; the wider window
//! wins).

use super::{
    end_of_day, start_of_day, DiscoveryCache, PurgeReason, ReorgError,
    ReorgMetrics, ReorgRecord, ReorgStore, WriterControl,
};
use crate::db::ranges::BlockRange;
use alloy::primitives::B256;
use log::{info, warn};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// `reorgs.from_ts` is aligned to the widest aggregate bucket: one day.
pub const REPAIR_ALIGNMENT_SECONDS: u32 = 86_400;

/// The steps of the reorg machinery. Used in error messages and by the
/// test model to inject a failure at a given step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PurgeStep {
    Quiesce,
    /// Reads of the fork-point search and of the detector.
    FindForkPoint,
    /// Reads of the first-pass gap healing.
    FindOrphans,
    ReadEpoch,
    MinTimestamp,
    TombstoneCheckpoints,
    TombstoneChildren,
    InsertReorg,
    RebuildDerived,
    TombstoneBlocks,
    /// Repair of the read-path side tables whose view push was lost.
    TombstoneSideTables,
    /// Reads that check a tombstone statement caught everything.
    Verify,
}

impl PurgeStep {
    pub fn as_str(&self) -> &'static str {
        match self {
            PurgeStep::Quiesce => "quiesce writer",
            PurgeStep::FindForkPoint => "find fork point",
            PurgeStep::FindOrphans => "find orphan children",
            PurgeStep::ReadEpoch => "read epoch",
            PurgeStep::MinTimestamp => "min timestamp",
            PurgeStep::TombstoneCheckpoints => "tombstone checkpoints",
            PurgeStep::TombstoneChildren => "tombstone children",
            PurgeStep::InsertReorg => "insert reorgs row",
            PurgeStep::RebuildDerived => "rebuild aggregates",
            PurgeStep::TombstoneBlocks => "tombstone blocks",
            PurgeStep::TombstoneSideTables => "repair side tables",
            PurgeStep::Verify => "verify tombstones",
        }
    }
}

impl fmt::Display for PurgeStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a finished purge did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeReport {
    /// Epoch rows are written with from now on.
    pub epoch: u32,
    /// `None`: the range held no row of any version, nothing was written
    /// and the epoch did not change.
    pub from_ts: Option<u32>,
    /// Exclusive end of the repaired bucket range (start of the day after
    /// the newest purged row), `None` together with `from_ts`.
    pub to_ts: Option<u32>,
    pub checkpoints_tombstoned: u64,
    pub children_tombstoned: u64,
    pub blocks_tombstoned: u64,
    /// Rows a lost materialized-view push left alive in a side table and
    /// this purge had to tombstone itself. Normally 0.
    pub side_rows_tombstoned: u64,
}

/// Knobs of a purge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurgeOptions {
    /// How often a tombstone statement is issued before giving up on
    /// seeing zero live rows.
    pub tombstone_attempts: u32,
    /// Pause before re-issuing it (doubles every time).
    pub retry_delay: Duration,
}

impl Default for PurgeOptions {
    fn default() -> Self {
        Self {
            tombstone_attempts: 8,
            retry_delay: Duration::from_millis(25),
        }
    }
}

/// Runs purges against a store. Cheap to clone; clones share the epoch
/// memory.
#[derive(Clone)]
pub struct Purger {
    store: Arc<dyn ReorgStore>,
    writer: Arc<dyn WriterControl>,
    cache: Arc<dyn DiscoveryCache>,
    metrics: Arc<dyn ReorgMetrics>,
    options: PurgeOptions,
    /// `_version` of the tombstones of one purge.
    next_version: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Highest epoch this process wrote per chain: the `reorgs` row of the
    /// previous purge may not be readable yet.
    epochs: Arc<Mutex<HashMap<u64, u32>>>,
}

/// How many counts in a row have to say "no live row left" before
/// [`Purger::tombstone_until_gone`] believes them.
///
/// Two, because a lagging read heals on the next try (docs/design.md §2:
/// the misses are transient, 44-137 per 3,200 in the measured repro). Two
/// reads separated by the retry delay therefore cannot both be the answer
/// from before this loop's insert - and every extra read costs one cheap
/// `count()` per purge step, which is the right price for the only
/// verification a purge has.
const ZERO_READS_REQUIRED: u32 = 2;

/// Which tombstone statement [`Purger::tombstone_until_gone`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Checkpoints,
    Children,
    Blocks,
    SideTables,
}

impl Purger {
    pub fn new(
        store: Arc<dyn ReorgStore>,
        writer: Arc<dyn WriterControl>,
        cache: Arc<dyn DiscoveryCache>,
        metrics: Arc<dyn ReorgMetrics>,
    ) -> Self {
        Self {
            store,
            writer,
            cache,
            metrics,
            options: PurgeOptions::default(),
            next_version: Arc::new(crate::db::next_version),
            epochs: Arc::default(),
        }
    }

    /// Another source of `_version`s (the in-memory model drives its own
    /// clock to simulate a wall clock that stepped back).
    pub fn with_version_source(
        mut self,
        next_version: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Self {
        self.next_version = next_version;
        self
    }

    pub fn with_options(mut self, options: PurgeOptions) -> Self {
        self.options = options;
        self
    }

    pub(super) fn store(&self) -> &dyn ReorgStore {
        self.store.as_ref()
    }

    pub(super) fn writer(&self) -> &dyn WriterControl {
        self.writer.as_ref()
    }

    pub(super) fn metrics(&self) -> &dyn ReorgMetrics {
        self.metrics.as_ref()
    }

    /// The epoch the chain is at: what `reorgs` says, or what this process
    /// wrote there, whichever is newer.
    pub async fn current_epoch(
        &self,
        chain: u64,
    ) -> Result<u32, ReorgError> {
        let stored =
            self.store.current_epoch(chain).await.map_err(|source| {
                ReorgError::Step { step: PurgeStep::ReadEpoch, source }
            })?;

        let remembered =
            self.epochs.lock().unwrap().get(&chain).copied().unwrap_or(0);

        Ok(stored.max(remembered))
    }

    /// Removes every block-scoped row of `chain` in `[from, to)` (`None` =
    /// open ended) and repairs the aggregates. Idempotent: running it
    /// again, after a success or after a failure at any step, converges to
    /// the same visible state. A range without any row is left alone.
    pub async fn purge_range(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        reason: PurgeReason,
    ) -> Result<PurgeReport, ReorgError> {
        self.purge(chain, from, to, reason, false).await
    }

    /// Purges every span a pipeline queued, NON-DESTRUCTIVELY, and returns
    /// the lowest block purged (`None`: the queue was empty). `purged` is
    /// called with each span as it is taken out of the queue.
    ///
    /// Both families queue the spans of flushes that raced another
    /// process's purge, and for those spans the queue is the ONLY record
    /// that they have to be indexed again: their rows are stored, so no
    /// gap query ever asks for them. A span therefore leaves the queue
    /// only after ITS purge succeeded - a transient ClickHouse error, a
    /// lost lease or `TombstonesNotConverging` leaves it, and everything
    /// after it, for the next pass to retry. Draining the whole `Vec` into
    /// a local one and returning `Err` half way through it dropped the
    /// rest for ever (docs/review-round-4.md, MAJOR 3).
    ///
    /// The entry is looked up again instead of being popped by index: the
    /// writer task pushes into the same queue while this runs.
    pub async fn purge_queued(
        &self,
        chain: u64,
        queue: &Mutex<Vec<BlockRange>>,
        reason: PurgeReason,
        mut purged: impl FnMut(BlockRange),
    ) -> Result<Option<u64>, ReorgError> {
        let mut lowest: Option<u64> = None;

        loop {
            let Some(range) = queue.lock().unwrap().first().copied()
            else {
                return Ok(lowest);
            };

            self.purge_range(chain, range.from, Some(range.to), reason)
                .await?;

            // Only now is the span repaired.
            queue.lock().unwrap().retain(|queued| *queued != range);
            purged(range);

            lowest = Some(
                lowest.map_or(range.from, |low: u64| low.min(range.from)),
            );
        }
    }

    /// `rows_expected`: the caller SAW rows in the range (orphaned blocks,
    /// orphan children). Not finding any is then a stale read, not an
    /// empty range, and must not be reported as a finished purge.
    pub(super) async fn purge(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        reason: PurgeReason,
        rows_expected: bool,
    ) -> Result<PurgeReport, ReorgError> {
        let started = Instant::now();
        let at = |step: PurgeStep| {
            move |source: anyhow::Error| ReorgError::Step { step, source }
        };

        // 0. Nothing stamped with the old epoch may land after the rebuild
        //    took its snapshot: it would be invisible for ever.
        self.writer.quiesce().await.map_err(at(PurgeStep::Quiesce))?;

        // 1. The epoch only becomes real with the `reorgs` row (step 4): a
        //    run that died before it re-uses the number, which nothing
        //    carries yet.
        let current = self.current_epoch(chain).await?;

        let first_look = self
            .store
            .timestamp_span(chain, from, to)
            .await
            .map_err(at(PurgeStep::MinTimestamp))?;

        if first_look.is_none() {
            if rows_expected {
                return Err(ReorgError::Step {
                    step: PurgeStep::MinTimestamp,
                    source: anyhow::anyhow!(
                        "rows were seen from block {from} on a moment ago \
                         but none can be read now (stale read)"
                    ),
                });
            }

            // Not a single row version in the range: nothing to tombstone,
            // no bucket to repair, no reason to burn an epoch.
            self.writer.adopt_epoch(current);

            return Ok(PurgeReport {
                epoch: current,
                from_ts: None,
                to_ts: None,
                checkpoints_tombstoned: 0,
                children_tombstoned: 0,
                blocks_tombstoned: 0,
                side_rows_tombstoned: 0,
            });
        }

        let epoch = current
            .checked_add(1)
            .ok_or(ReorgError::EpochOverflow { chain })?;
        let version = (self.next_version)();

        // 2. Checkpoints first: between here and the end a checkpoint may
        //    claim LESS than what is stored (harmless, the blocks are
        //    there), never more.
        let checkpoints_tombstoned = self
            .tombstone_until_gone(
                Target::Checkpoints,
                chain,
                from,
                to,
                version,
            )
            .await?;

        // 3. Children and side-less bases; side tables follow through
        //    their views.
        let children_tombstoned = self
            .tombstone_until_gone(
                Target::Children,
                chain,
                from,
                to,
                version,
            )
            .await?;

        // Second look, several round trips later: a row the first one
        // could not see yet must not move the repair to a later day, and
        // must not leave the last day it touched out of it either. The
        // window only ever GROWS between the two looks - a repair that
        // covers more than the validity rule hides is merely extra work.
        let second_look = self
            .store
            .timestamp_span(chain, from, to)
            .await
            .map_err(at(PurgeStep::MinTimestamp))?;

        let looks = || first_look.into_iter().chain(second_look);

        let mut from_ts = looks()
            .map(|(min, _)| min)
            .min()
            .map(start_of_day)
            .unwrap_or_default();

        // Exclusive, and always at least one whole day: the repair covers
        // every bucket the validity rule is about to hide.
        let to_ts = looks()
            .map(|(_, max)| max)
            .max()
            .map(end_of_day)
            .unwrap_or_default()
            .max(from_ts.saturating_add(super::REPAIR_ALIGNMENT_SECONDS));

        // Zero is not a block time, it is a MISSING one: an EVM genesis
        // block often has `timestamp` 0, and a Solana slot whose
        // `blockTime` the node omitted is stored as 0 too. Starting the
        // repair window there armed the validity rule from 1970 on -
        // `epoch_floor_v` expands one row per day since the epoch and
        // raises the floor on every one of them, so the WHOLE chain
        // reads as zero until a rebuild that slices fifty years into
        // monthly INSERTs per aggregate finishes (measured: ~20,700 day
        // rows, ~678 months x ~20 aggregates). One bogus row blanked a
        // chain's entire history (docs/review-round-4.md, MAJOR 6).
        //
        // [`ReorgStore::timestamp_span`] is asked for the smallest
        // NON-ZERO timestamp for that reason. A store that still reports
        // 0 next to a real newest timestamp is clamped here, loudly: the
        // repair covers the last day of the range instead of every day
        // since 1970. What clamping costs is that the days below keep
        // the contributions of the rows being tombstoned - wrong numbers
        // in old buckets, incomparably better than hiding every bucket
        // of the chain behind a rebuild that takes hours.
        if from_ts == 0 && to_ts > super::REPAIR_ALIGNMENT_SECONDS {
            from_ts =
                to_ts.saturating_sub(super::REPAIR_ALIGNMENT_SECONDS);
            warn!(
                "Chain {chain}: a row of blocks [{from}, {}) has \
                 `timestamp` 0, which is not a block time. Repairing \
                 only unix time [{from_ts}, {to_ts}) instead of \
                 everything since 1970 - the buckets below it keep what \
                 the purged rows contributed. Fix the source: a missing \
                 block time is being stored as 0.",
                to.map_or("head".to_string(), |to| to.to_string()),
            );
        }

        // 4. Before the rebuild: under-count for a moment, never double
        //    count.
        let (old_head, old_hash, new_hash, depth) = match reason {
            PurgeReason::Reorg { old_head, old_hash, new_hash, depth } => {
                (old_head, old_hash, new_hash, depth)
            }
            PurgeReason::GapHeal | PurgeReason::Redecode => {
                (0, B256::ZERO, B256::ZERO, 0)
            }
        };

        let mut record = ReorgRecord {
            chain,
            epoch,
            from_ts,
            to_ts,
            fork_block: from,
            to_block: to,
            old_head,
            old_hash,
            new_hash,
            depth,
            rows_tombstoned: children_tombstoned,
            reason: reason.as_str(),
            version,
            completed: false,
        };

        // Remembered BEFORE the insert: if its acknowledgement is lost the
        // row may exist, and the number must not be used twice.
        self.epochs.lock().unwrap().insert(chain, epoch);

        self.store
            .insert_reorg(&record)
            .await
            .map_err(at(PurgeStep::InsertReorg))?;

        // The epoch is real now. Adopted here, not at the end: if a later
        // step fails and the process lives on, its writer must not keep
        // stamping an epoch the validity rule already hides.
        self.writer.adopt_epoch(epoch);

        // 5. Bucket repair of exactly the buckets the `reorgs` row hides,
        //    `[from_ts, to_ts)` - not "from from_ts to now". It leaves the
        //    purged block range out by itself, so it neither counts the
        //    still-alive orphaned `blocks` rows nor depends on seeing the
        //    tombstones of step 3.
        self.store
            .rebuild_derived(chain, from_ts, to_ts, epoch, from, to)
            .await
            .map_err(at(PurgeStep::RebuildDerived))?;

        // 6. The commit marker goes last, mirroring the insert order.
        let blocks_tombstoned = self
            .tombstone_until_gone(Target::Blocks, chain, from, to, version)
            .await?;

        // 7. The side tables should be empty now: their views saw every
        //    tombstone of steps 3 and 6. Whatever is still alive is the
        //    debris of a push that was lost, and nothing but this repairs
        //    it. Last, so `blocks` (and its `block_lookup`) are covered.
        let side_rows_tombstoned = self
            .tombstone_until_gone(
                Target::SideTables,
                chain,
                from,
                to,
                version,
            )
            .await?;

        if side_rows_tombstoned > 0 {
            warn!(
                "Chain {chain}: {side_rows_tombstoned} row(s) survived in \
                 the read-path side tables of blocks [{from}, {to:?}) \
                 although their base rows are gone (a materialized view \
                 push was lost); tombstoned directly."
            );
        }

        // 8. Every write of this purge is durable: mark it finished, so
        //    the next start can tell its debris (tombstoned rows at block
        //    numbers a chain that got SHORTER does not have any more) from
        //    the leftovers of a purge that died half way. Same
        //    `(chain, epoch)` row, replaced.
        record.completed = true;
        self.store
            .insert_reorg(&record)
            .await
            .map_err(at(PurgeStep::InsertReorg))?;

        // 9. In memory only: a restart starts with empty caches.
        self.cache.evict_range(from, to);

        let elapsed = started.elapsed();
        self.metrics.purge_observed(elapsed, blocks_tombstoned);

        let range = match to {
            Some(to) => format!("[{from}, {to})"),
            None => format!("[{from}, head]"),
        };

        match reason {
            PurgeReason::Reorg { .. } => warn!(
                "Chain {chain}: rolled back blocks {range}: \
                 {blocks_tombstoned} blocks, {children_tombstoned} rows, \
                 {checkpoints_tombstoned} checkpoints tombstoned, \
                 aggregates rebuilt over unix time \
                 [{from_ts}, {to_ts}); epoch is now {epoch} \
                 ({elapsed:?})."
            ),
            PurgeReason::Redecode => info!(
                "Chain {chain}: cleared {range} for re-decoding: \
                 {children_tombstoned} rows tombstoned, aggregates rebuilt \
                 over unix time [{from_ts}, {to_ts}); epoch is now \
                 {epoch} ({elapsed:?})."
            ),
            PurgeReason::GapHeal => info!(
                "Chain {chain}: healed gap {range} left by an interrupted \
                 write: {children_tombstoned} rows tombstoned, aggregates \
                 rebuilt over unix time [{from_ts}, {to_ts}); epoch is \
                 now {epoch} ({elapsed:?})."
            ),
        }

        Ok(PurgeReport {
            epoch,
            from_ts: Some(from_ts),
            to_ts: Some(to_ts),
            checkpoints_tombstoned,
            children_tombstoned,
            blocks_tombstoned,
            side_rows_tombstoned,
        })
    }

    /// Issues a tombstone statement until a count of the live rows says 0
    /// [`ZERO_READS_REQUIRED`] times in a row. The statement is an
    /// `INSERT .. SELECT .. FINAL`: it can miss rows that were written a
    /// moment ago, and repeating it is free (rows that are already dead
    /// are not selected again).
    ///
    /// The count can miss them too, which is the whole point of the
    /// repetition (docs/design.md §2, "No read-your-writes"). One zero is
    /// therefore not proof that the range is empty: it is either the
    /// truth, or a read served from just before this loop's own insert -
    /// and that is exactly the case where stopping is wrong
    /// (docs/review-round-4.md, MAJOR 7). A zero has to be confirmed by a
    /// second read, taken after the retry delay, so that the two cannot
    /// be the same lagging answer.
    async fn tombstone_until_gone(
        &self,
        target: Target,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> Result<u64, ReorgError> {
        let store = self.store.as_ref();
        let attempts = self.options.tombstone_attempts.max(1);
        let mut delay = self.options.retry_delay;
        let mut tombstoned = 0;
        let mut live = 0;

        let step = match target {
            Target::Checkpoints => PurgeStep::TombstoneCheckpoints,
            Target::Children => PurgeStep::TombstoneChildren,
            Target::Blocks => PurgeStep::TombstoneBlocks,
            Target::SideTables => PurgeStep::TombstoneSideTables,
        };

        let count = || async {
            match target {
                Target::Checkpoints => {
                    store.live_checkpoints(chain, from, to)
                }
                Target::Children => store.live_children(chain, from, to),
                Target::Blocks => store.live_blocks(chain, from, to),
                Target::SideTables => {
                    store.live_side_rows(chain, from, to)
                }
            }
            .await
            .map_err(|source| ReorgError::Step {
                step: PurgeStep::Verify,
                source,
            })
        };

        for attempt in 1..=attempts {
            tombstoned += match target {
                Target::Checkpoints => {
                    store.tombstone_checkpoints(chain, from, to, version)
                }
                Target::Children => {
                    store.tombstone_children(chain, from, to, version)
                }
                Target::Blocks => {
                    store.tombstone_blocks(chain, from, to, version)
                }
                Target::SideTables => {
                    store.tombstone_side_rows(chain, from, to, version)
                }
            }
            .await
            .map_err(|source| ReorgError::Step { step, source })?;

            live = count().await?;

            // A zero, confirmed. Every confirming read is taken after the
            // delay, so it cannot be the same lagging answer; the first
            // one that is not zero ends the confirmation and the loop
            // issues the statement again.
            let mut zeros = u32::from(live == 0);
            while zeros > 0 && zeros < ZERO_READS_REQUIRED {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                live = count().await?;
                zeros = if live == 0 { zeros + 1 } else { 0 };
            }

            if zeros >= ZERO_READS_REQUIRED {
                return Ok(tombstoned);
            }

            if attempt < attempts && !delay.is_zero() {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
        }

        Err(ReorgError::TombstonesNotConverging {
            chain,
            step,
            live,
            attempts,
        })
    }
}
