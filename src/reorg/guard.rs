//! The state machine the pipeline talks to.
//!
//! ```text
//! startup()                      once per process: adopt the stored epoch
//! per pass:
//!   begin_pass(missing, target)  heals gaps (first inspection of a range
//!                                only), re-reads the epoch after a failure
//!   per missing range [a, b):
//!     per response:
//!       observe(headers, guard)  BEFORE the rows go to the writer
//!         Continue            -> send the rows to the writer
//!         Rollback(rollback)  -> drop the response, repair(&rollback),
//!                                cursor = min(cursor, fork_point),
//!                                abandon the pass and start a new one
//!     check_join(b)              is the stored block b still canonical?
//! nothing to stream:
//!   check_tip()                  optional: is the stored head still canonical?
//! ```
//!
//! How the layers interact:
//!
//! * `--confirmations N` keeps the newest N blocks out of the database. A
//!   reorg no deeper than N never touches a stored block, so no mismatch is
//!   ever seen and NOTHING is purged; the guard only matters for reorgs
//!   deeper than N (or N = 0). The guard does not need to know N.
//! * Gap healing runs before a range is streamed, so the blocks streamed
//!   into it are never counted twice by the aggregates. It can bump the
//!   epoch on its own; a rollback later in the same pass simply bumps it
//!   again.
//! * A rollback resets the guard: the next block is compared with what is
//!   STORED, not with what was streamed before.

use super::{
    find_fork_point, BlockHeader, CanonicalChain, PurgeReason,
    PurgeReport, PurgeStep, Purger, ReorgConfig, ReorgError, StreamGuard,
};
use alloy::primitives::B256;
use log::warn;
use std::sync::Arc;

/// Reads before "the block below is not stored" is believed.
const SEED_READS: u32 = 3;

/// Answer to [`ReorgGuard::observe`] / [`ReorgGuard::check_join`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The blocks build on what is known. Store them.
    Continue,
    /// Stored blocks are orphaned. Drop the response, call
    /// [`ReorgGuard::repair`], resume from `fork_point`.
    Rollback(Rollback),
}

/// A decided rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rollback {
    /// Lowest orphaned block: purge from here, resume streaming from here.
    pub fork_point: u64,
    /// Exclusive end of the purge. `None` (open ended) for a reorg at the
    /// tip. `Some` when the mismatch was found while filling a gap BELOW
    /// other stored blocks: only the segment proven orphaned is purged,
    /// the blocks above are judged when streaming reaches them.
    pub purge_to: Option<u64>,
    /// Block heights rolled back.
    pub depth: u64,
    /// Height at which stored and canonical hash were first seen to differ.
    pub mismatch_height: u64,
    /// Highest stored block at detection time.
    pub old_head: u64,
    /// Stored / canonical hash at `mismatch_height`.
    pub old_hash: B256,
    pub new_hash: B256,
}

pub struct ReorgGuard {
    config: ReorgConfig,
    canonical: Arc<dyn CanonicalChain>,
    purger: Purger,
    /// Number and hash of the last block that passed `observe`.
    last: Option<(u64, B256)>,
    /// Gap healing: ranges below this block were inspected since startup.
    healed_until: u64,
    /// Gap healing: everything from this block on was healed by the first
    /// pass (`None` until then).
    tail_healed_from: Option<u64>,
    /// A purge failed half way: the epoch of the writer may be behind the
    /// `reorgs` table. Re-read it before anything else is written.
    epoch_unknown: bool,
}

impl ReorgGuard {
    pub fn new(
        config: ReorgConfig,
        canonical: Arc<dyn CanonicalChain>,
        purger: Purger,
    ) -> Self {
        Self {
            config,
            canonical,
            purger,
            last: None,
            healed_until: config.start_block,
            tail_healed_from: None,
            epoch_unknown: true,
        }
    }

    pub fn config(&self) -> &ReorgConfig {
        &self.config
    }

    /// Last block that passed [`observe`](Self::observe).
    pub fn last(&self) -> Option<(u64, B256)> {
        self.last
    }

    /// Call once before the first pass: the writer adopts the epoch the
    /// chain is at (a purge of an earlier run may have bumped it).
    pub async fn startup(&mut self) -> Result<u32, ReorgError> {
        let epoch = self.purger.current_epoch(self.config.chain).await?;

        self.purger.writer().adopt_epoch(epoch);
        self.epoch_unknown = false;

        Ok(epoch)
    }

    /// Checks the blocks of one response (ascending) and its rollback
    /// guard, BEFORE their rows are handed to the writer.
    ///
    /// The parent hash of a block is compared with the block streamed
    /// right before it or, when that is unknown (start of a range, after a
    /// rollback), with the STORED block below it. A failing lookup is an
    /// error, never "no evidence": skipping the comparison once is how an
    /// orphaned segment would stay in the database for ever. Block 0 and
    /// `start_block` have no predecessor as far as this process is
    /// concerned.
    ///
    /// On a mismatch the writer is quiesced and the fork point searched;
    /// nothing is purged yet.
    pub async fn observe(
        &mut self,
        headers: &[BlockHeader],
        guard: Option<&StreamGuard>,
    ) -> Result<Verdict, ReorgError> {
        if let Some(guard) = guard {
            let verdict = self
                .check_link(guard.first_block, guard.first_parent_hash)
                .await?;

            if verdict != Verdict::Continue {
                return Ok(verdict);
            }
        }

        for header in headers {
            let verdict =
                self.check_link(header.number, header.parent_hash).await?;

            if verdict != Verdict::Continue {
                return Ok(verdict);
            }

            self.last = Some((header.number, header.hash));
        }

        Ok(Verdict::Continue)
    }

    /// Call when a streamed range ended at block `next` (exclusive end) and
    /// block `next` is already stored: a gap was filled. Compares the
    /// stored block with the canonical one, so an orphaned segment ABOVE a
    /// gap is found right away instead of when the chain head moves.
    pub async fn check_join(
        &mut self,
        next: u64,
    ) -> Result<Verdict, ReorgError> {
        let chain = self.config.chain;

        let Some((last_number, last_hash)) = self.last else {
            return Ok(Verdict::Continue);
        };

        if last_number.checked_add(1) != Some(next) {
            return Ok(Verdict::Continue);
        }

        let store = self.purger.store();

        let stored = store
            .stored_hashes(chain, next, next + 1)
            .await
            .map_err(step(PurgeStep::FindForkPoint))?;

        let Some((_, stored_hash)) =
            stored.into_iter().find(|(number, _)| *number == next)
        else {
            return Ok(Verdict::Continue);
        };

        let headers = self
            .canonical
            .headers(next, next + 1)
            .await
            .map_err(|source| ReorgError::Canonical { source })?;

        let Some(header) = headers.first().filter(|h| h.number == next)
        else {
            // The canonical chain ends below `next` right now. The check
            // at the tip catches it once the chain has grown.
            return Ok(Verdict::Continue);
        };

        if header.parent_hash != last_hash {
            return Err(ReorgError::CanonicalUnstable {
                chain,
                at: next,
                attempts: 1,
                detail: format!(
                    "header {next} does not build on the block streamed \
                     before it"
                ),
            });
        }

        if header.hash == stored_hash {
            self.last = Some((next, stored_hash));
            return Ok(Verdict::Continue);
        }

        // Blocks below `next` were just streamed: the fork point is
        // `next` itself and everything stored above goes.
        self.purger
            .writer()
            .quiesce()
            .await
            .map_err(step(PurgeStep::Quiesce))?;

        let old_head = self.stored_head().await?.unwrap_or(next).max(next);
        let depth = old_head - next + 1;

        if depth > self.config.max_reorg_depth {
            return Err(ReorgError::ReorgTooDeep {
                chain,
                mismatch_at: next,
                depth,
                max: self.config.max_reorg_depth,
            });
        }

        Ok(self.rollback(Rollback {
            fork_point: next,
            purge_to: None,
            depth,
            mismatch_height: next,
            old_head,
            old_hash: stored_hash,
            new_hash: header.hash,
        }))
    }

    /// Optional, for when there is nothing to stream: is the newest stored
    /// block still canonical? A reorg that does not make the chain LONGER
    /// than what is stored is otherwise only noticed when the next block
    /// arrives. One header lookup; call it as often as that is worth.
    pub async fn check_tip(&mut self) -> Result<Verdict, ReorgError> {
        let chain = self.config.chain;

        let Some(head) = self.stored_head().await? else {
            return Ok(Verdict::Continue);
        };

        if head < self.config.start_block {
            return Ok(Verdict::Continue);
        }

        let headers = self
            .canonical
            .headers(head, head + 1)
            .await
            .map_err(|source| ReorgError::Canonical { source })?;

        let Some(header) = headers.first().filter(|h| h.number == head)
        else {
            // The source does not have that height (yet, or any more):
            // it may simply lag behind. The next block settles it.
            return Ok(Verdict::Continue);
        };

        let stored = self
            .purger
            .store()
            .stored_hashes(chain, head, head + 1)
            .await
            .map_err(step(PurgeStep::FindForkPoint))?;

        if stored
            .iter()
            .any(|(n, hash)| *n == head && *hash == header.hash)
        {
            return Ok(Verdict::Continue);
        }

        // Same as "block head + 1 names another parent".
        self.last = None;
        self.mismatch(head + 1, header.hash).await
    }

    /// Purges what a [`Verdict::Rollback`] named. Afterwards the writer
    /// stamps the new epoch and the caller resumes from `fork_point`.
    ///
    /// If this fails (or the process dies) nothing needs to be remembered:
    /// the orphaned `blocks` rows are still alive, so the next pass detects
    /// the same reorg and repairs it from the start.
    pub async fn repair(
        &mut self,
        rollback: &Rollback,
    ) -> Result<PurgeReport, ReorgError> {
        self.last = None;

        let report = self
            .purge(
                rollback.fork_point,
                rollback.purge_to,
                PurgeReason::Reorg {
                    old_head: rollback.old_head,
                    old_hash: rollback.old_hash,
                    new_hash: rollback.new_hash,
                    depth: rollback.depth,
                },
            )
            .await?;

        self.purger.metrics().reorg(rollback.depth);

        Ok(report)
    }

    async fn purge(
        &mut self,
        from: u64,
        to: Option<u64>,
        reason: PurgeReason,
    ) -> Result<PurgeReport, ReorgError> {
        // The guard only purges ranges in which it SAW rows.
        let result = self
            .purger
            .purge(self.config.chain, from, to, reason, true)
            .await;

        if result.is_err() {
            self.epoch_unknown = true;
        }

        result
    }

    /// Start of a pass: gap healing. Call with the missing
    /// ranges `[from, to)` the pass is about to stream (ascending) and the
    /// exclusive end of the range the pass inspected.
    ///
    /// A gap can hold children of a flush that died before its `blocks`
    /// rows were written, or of a purge that died half way. Streaming into
    /// it would count those rows twice in the aggregates, so such a gap is
    /// purged first (reason `gap_heal`).
    ///
    /// Only the first inspection of a range since startup does anything:
    /// afterwards this process cannot leave orphans behind (a failed flush
    /// is fatal, a rollback purges its whole range). The first call also
    /// heals everything above the stored head, open ended, because the
    /// chain may be shorter now than when the orphans were written.
    ///
    /// Returns the number of purges run.
    pub async fn begin_pass(
        &mut self,
        missing: &[(u64, u64)],
        inspected_to: u64,
    ) -> Result<u32, ReorgError> {
        let chain = self.config.chain;
        let mut purges = 0;

        if self.epoch_unknown {
            self.startup().await?;
        }

        let tail_from = match self.tail_healed_from {
            Some(tail_from) => tail_from,
            None => {
                let above_head =
                    self.stored_head().await?.map_or(0, |head| head + 1);
                let tail_from = inspected_to
                    .max(above_head)
                    .max(self.config.start_block);

                if self.heal(chain, tail_from, None).await? {
                    purges += 1;
                }

                self.tail_healed_from = Some(tail_from);
                tail_from
            }
        };

        for (from, to) in missing {
            let from = (*from).max(self.healed_until);
            let to = (*to).min(tail_from);

            if from < to && self.heal(chain, from, Some(to)).await? {
                purges += 1;
            }
        }

        self.healed_until = self.healed_until.max(inspected_to);

        Ok(purges)
    }

    async fn heal(
        &mut self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> Result<bool, ReorgError> {
        let orphans = self
            .purger
            .store()
            .has_orphan_children(chain, from, to)
            .await
            .map_err(step(PurgeStep::FindOrphans))?;

        if !orphans {
            return Ok(false);
        }

        self.purge(from, to, PurgeReason::GapHeal).await?;

        Ok(true)
    }

    /// Does block `number` build on what is known below it?
    async fn check_link(
        &mut self,
        number: u64,
        parent_hash: B256,
    ) -> Result<Verdict, ReorgError> {
        if number == 0 || number <= self.config.start_block {
            return Ok(Verdict::Continue);
        }

        let contiguous = matches!(
            self.last,
            Some((last, _)) if last.checked_add(1) == Some(number)
        );

        if !contiguous {
            // Start of a range: seed from the store. Mandatory.
            self.last = self.stored_block(number - 1).await?;
        }

        let Some((_, expected)) = self.last else {
            // Nothing stored below: nothing to contradict.
            return Ok(Verdict::Continue);
        };

        if expected == parent_hash {
            return Ok(Verdict::Continue);
        }

        self.mismatch(number, parent_hash).await
    }

    /// Block `number` names `parent_hash` as its parent and that is not
    /// what we have: find out how far back the disagreement goes.
    async fn mismatch(
        &mut self,
        number: u64,
        parent_hash: B256,
    ) -> Result<Verdict, ReorgError> {
        let chain = self.config.chain;

        // The block below may only be buffered: make the store complete
        // before it is compared with the canonical chain.
        self.purger
            .writer()
            .quiesce()
            .await
            .map_err(step(PurgeStep::Quiesce))?;

        let fork = find_fork_point(
            self.canonical.as_ref(),
            self.purger.store(),
            &self.config,
            number,
            parent_hash,
        )
        .await;

        // Whatever happens next, the stream is abandoned.
        self.last = None;
        let fork = fork?;

        if fork.depth == 0 {
            return Err(ReorgError::SourceInconsistent {
                chain,
                at: number,
            });
        }

        let old_head = self
            .stored_head()
            .await?
            .unwrap_or(number - 1)
            .max(number - 1);

        Ok(self.rollback(Rollback {
            fork_point: fork.fork_point,
            // Blocks stored at or above the mismatch are another segment.
            purge_to: (old_head >= number).then_some(number),
            depth: fork.depth,
            mismatch_height: number - 1,
            old_head,
            old_hash: fork.old_hash,
            new_hash: fork.new_hash,
        }))
    }

    fn rollback(&mut self, rollback: Rollback) -> Verdict {
        self.last = None;

        warn!(
            "Chain {}: REORG detected. Stored block {} has hash {:?} but \
             the chain now has {:?} there. Rolling back {} block(s) from \
             block {} (stored head {}).",
            self.config.chain,
            rollback.mismatch_height,
            rollback.old_hash,
            rollback.new_hash,
            rollback.depth,
            rollback.fork_point,
            rollback.old_head,
        );

        Verdict::Rollback(rollback)
    }

    /// The stored block `number`. "Not stored" is only believed when
    /// several reads in a row say so: ClickHouse has no read-your-writes,
    /// and a block flushed a moment ago that goes unseen here would never
    /// be compared with the chain again.
    async fn stored_block(
        &self,
        number: u64,
    ) -> Result<Option<(u64, B256)>, ReorgError> {
        for attempt in 1..=SEED_READS {
            let stored = self
                .purger
                .store()
                .stored_hashes(self.config.chain, number, number + 1)
                .await
                .map_err(step(PurgeStep::FindForkPoint))?;

            if let Some(found) =
                stored.into_iter().find(|(n, _)| *n == number)
            {
                return Ok(Some(found));
            }

            let delay = self.config.canonical_retry_delay;
            if attempt < SEED_READS && !delay.is_zero() {
                tokio::time::sleep(delay / 10).await;
            }
        }

        Ok(None)
    }

    async fn stored_head(&self) -> Result<Option<u64>, ReorgError> {
        self.purger
            .store()
            .stored_head(self.config.chain)
            .await
            .map_err(step(PurgeStep::FindForkPoint))
    }
}

fn step(step: PurgeStep) -> impl Fn(anyhow::Error) -> ReorgError {
    move |source| ReorgError::Step { step, source }
}
