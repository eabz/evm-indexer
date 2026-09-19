//! A date in, a block number out (docs/design.md section 16).
//!
//! The coverage floor is chosen as a DATE - "one year before now" by
//! default, or whatever `--start-date` says - and a chain is indexed by
//! block number, so something has to translate. There is no endpoint that
//! answers "which block was mined on this day": the only thing the source
//! offers is "give me the header of block N", which carries its timestamp.
//!
//! So it is a binary search, and the cost is what a binary search costs:
//! about `log2(height)` header requests, twenty-five or so on a chain with
//! thirty million blocks, ONCE in a chain's life. The search runs through
//! [`CanonicalChain`], the same seam the fork-point search uses
//! (`src/source/evm.rs`), so the tests drive it against a fake chain and
//! nothing here ever opens a socket.
//!
//! **What "the block for a date" means here.** The FIRST block whose
//! timestamp is at or after midnight UTC of that date. Never a block
//! before it: the floor is a promise that everything from that moment on is
//! stored, and starting one block late would break the promise while
//! starting early only costs a little extra history.
//!
//! Two answers are not the obvious one, and both are deliberate:
//!
//! * **The chain is younger than the date asked for** - every block is
//!   older than the target, which on a live chain means the source is
//!   behind or the date is in the future. The answer is the head: start at
//!   the tip rather than silently index nothing.
//! * **The chain is younger than a year** (or than whatever date was
//!   given): genesis is already at or after the target, so the answer is
//!   block 0 and the floor is the chain's own beginning. This is what
//!   "one year before the chain's first launch" comes to in practice.

use crate::reorg::{BlockHeader, CanonicalChain};
use anyhow::{bail, Context, Result};

/// How far a probe may widen when the exact height comes back empty.
///
/// `include_all_blocks` means a height that exists always answers, so this
/// is for the edge of the archive and for a source that skips heights
/// (Solana's skipped slots). Small: a header this far from the probe is
/// close enough for a floor, and giving up is better than sliding.
const WIDEN_BY: u64 = 64;

/// A safety net, not a limit anyone should reach: a clean binary search
/// over a 64-bit height needs 64 steps, and every real chain needs half
/// that. Passing it means the timestamps are not ordered and the search
/// would not terminate.
const MAX_PROBES: usize = 128;

/// Where a date landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    /// First block at or after the requested moment.
    pub block: u64,
    /// That block's own timestamp, which is at or after the request (and
    /// usually a few seconds after it).
    pub timestamp: u32,
    /// Header requests the search made. Reported so the log can say what
    /// the one-off cost was, and so a test can hold it to `log2`.
    pub probes: usize,
}

/// The first block of `chain` whose timestamp is at or after `at_or_after`
/// (unix seconds).
///
/// `head` is the EXCLUSIVE upper bound the source will serve, which is what
/// `BlockSource::head` returns, so the highest block that exists is
/// `head - 1`.
pub async fn block_at_or_after(
    chain: &dyn CanonicalChain,
    head: u64,
    at_or_after: i64,
) -> Result<Resolved> {
    let mut probes = 0;

    // An empty chain has one possible answer.
    if head == 0 {
        return Ok(Resolved { block: 0, timestamp: 0, probes });
    }

    let genesis = lowest(chain, head, &mut probes).await?;
    if i64::from(genesis.timestamp) >= at_or_after {
        return Ok(Resolved {
            block: genesis.number,
            timestamp: genesis.timestamp,
            probes,
        });
    }

    let last = highest(chain, head, &mut probes).await?;
    if i64::from(last.timestamp) < at_or_after {
        // Every block this source has is older than the moment asked for:
        // a date in the future, or an archive a long way behind. Starting
        // at the tip is the only useful answer; indexing nothing is not.
        return Ok(Resolved {
            block: last.number,
            timestamp: last.timestamp,
            probes,
        });
    }

    // The invariant, held by every branch below: `low` is a block BEFORE
    // the moment, `high` is a block AT OR AFTER it, and the answer is the
    // first existing block in `(low, high]`.
    let mut low = genesis.number;
    let mut high = last.number;
    let mut high_timestamp = last.timestamp;

    while high - low > 1 {
        if probes > MAX_PROBES {
            bail!(
                "resolving a date to a block on this chain did not settle \
                 after {probes} header requests, which means the block \
                 timestamps are not in order. Give --start-block instead."
            );
        }

        let middle = low + (high - low) / 2;

        // Look in `[middle, high)` only. Every header that can come back
        // is therefore strictly inside the bracket, so whichever side it
        // narrows, the bracket really shrinks and the loop ends.
        let Some(header) =
            first_in(chain, middle, high, &mut probes).await?
        else {
            // No block exists anywhere in `[middle, high)`, so `high` is
            // already the first existing block at or after the moment.
            break;
        };

        if i64::from(header.timestamp) < at_or_after {
            low = header.number;
        } else {
            high = header.number;
            high_timestamp = header.timestamp;
        }
    }

    Ok(Resolved { block: high, timestamp: high_timestamp, probes })
}

/// The first header in `[from, to)`, or `None` when the source has no
/// block there at all.
///
/// A timestamp of 0 is a MISSING block time, never a block mined in 1970
/// (docs/design.md section 2), so a header carrying one is not an answer.
async fn first_in(
    chain: &dyn CanonicalChain,
    from: u64,
    to: u64,
    probes: &mut usize,
) -> Result<Option<BlockHeader>> {
    // One height first: that is the whole request on a chain with no
    // holes, which is every EVM chain.
    let one = (from + 1).min(to);
    *probes += 1;
    let headers = chain
        .headers(from, one)
        .await
        .with_context(|| format!("read the header of block {from}"))?;
    if let Some(header) = headers.iter().find(|h| h.timestamp > 0) {
        return Ok(Some(*header));
    }

    if one >= to {
        return Ok(None);
    }

    // A hole at that exact height. Widen upwards - never downwards, or the
    // answer could end up BEFORE the moment asked for - and stay inside
    // the bracket the caller gave.
    let widened = (from + WIDEN_BY).min(to);
    *probes += 1;
    let headers =
        chain.headers(from, widened).await.with_context(|| {
            format!("read the headers of [{from}, {widened})")
        })?;

    Ok(headers.into_iter().find(|header| header.timestamp > 0))
}

/// The lowest block the source actually has.
async fn lowest(
    chain: &dyn CanonicalChain,
    head: u64,
    probes: &mut usize,
) -> Result<BlockHeader> {
    first_in(chain, 0, head, probes).await?.with_context(|| {
        format!(
            "the source served no block header at all below {}, so a date \
             cannot be resolved to a block here. Give --start-block \
             instead.",
            WIDEN_BY.min(head)
        )
    })
}

/// The highest block the source actually has.
async fn highest(
    chain: &dyn CanonicalChain,
    head: u64,
    probes: &mut usize,
) -> Result<BlockHeader> {
    let from = head.saturating_sub(WIDEN_BY);
    *probes += 1;
    let headers = chain.headers(from, head).await.with_context(|| {
        format!("read the headers of [{from}, {head})")
    })?;

    headers.into_iter().rfind(|header| header.timestamp > 0).with_context(
        || {
            format!(
                "the source served no block header in [{from}, {head}), \
                 although it reports a height of {head}. Give \
                 --start-block instead."
            )
        },
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy::primitives::B256;
    use futures::future::BoxFuture;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    /// A chain whose block `n` was mined at `genesis + n * spacing`, which
    /// is every property the search relies on and nothing else.
    pub(crate) struct FakeChain {
        pub genesis: i64,
        pub spacing: i64,
        pub height: u64,
        /// Heights the source simply does not have, to stand in for
        /// Solana's skipped slots and for the edge of an archive.
        pub missing: Vec<u64>,
        pub requests: Arc<AtomicUsize>,
    }

    impl FakeChain {
        pub fn new(genesis: i64, spacing: i64, height: u64) -> Self {
            Self {
                genesis,
                spacing,
                height,
                missing: Vec::new(),
                requests: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn timestamp(&self, number: u64) -> u32 {
            (self.genesis + self.spacing * number as i64) as u32
        }
    }

    impl CanonicalChain for FakeChain {
        fn headers(
            &self,
            from: u64,
            to: u64,
        ) -> BoxFuture<'_, Result<Vec<BlockHeader>>> {
            self.requests.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok((from..to.min(self.height))
                    .filter(|number| !self.missing.contains(number))
                    .map(|number| BlockHeader {
                        number,
                        hash: B256::ZERO,
                        parent_hash: B256::ZERO,
                        timestamp: self.timestamp(number),
                    })
                    .collect())
            })
        }
    }

    /// A chain of 30 million 2-second blocks - roughly an L2 - and a date
    /// in the middle of it.
    #[tokio::test]
    async fn a_date_lands_on_the_first_block_at_or_after_it() {
        let genesis = 1_600_000_000;
        let chain = FakeChain::new(genesis, 2, 30_000_000);

        // Exactly on a block boundary.
        let wanted = genesis + 2 * 12_345_678;
        let found =
            block_at_or_after(&chain, 30_000_000, wanted).await.unwrap();
        assert_eq!(found.block, 12_345_678);
        assert_eq!(i64::from(found.timestamp), wanted);

        // One second later: the NEXT block, never the one before.
        let found = block_at_or_after(&chain, 30_000_000, wanted + 1)
            .await
            .unwrap();
        assert_eq!(found.block, 12_345_679);
        assert!(i64::from(found.timestamp) > wanted);
    }

    /// The whole reason this is a binary search: the owner pays for it
    /// once, and it must not be a scan.
    #[tokio::test]
    async fn the_search_costs_about_log2_of_the_height() {
        let height = 30_000_000u64;
        let chain = FakeChain::new(1_600_000_000, 2, height);
        let requests = chain.requests.clone();

        let found = block_at_or_after(
            &chain,
            height,
            1_600_000_000 + 2 * 7_777_777,
        )
        .await
        .unwrap();

        assert_eq!(found.block, 7_777_777);

        // log2(30M) is under 25; the two end probes make it 27.
        let budget = (height as f64).log2().ceil() as usize + 4;
        let made = requests.load(Ordering::Relaxed);
        assert!(
            made <= budget,
            "{made} header requests for a chain of {height} blocks; \
             a binary search may use at most {budget}"
        );
        assert_eq!(made, found.probes);
    }

    #[tokio::test]
    async fn a_chain_younger_than_the_date_starts_at_genesis() {
        let chain = FakeChain::new(1_700_000_000, 12, 1_000);

        let found =
            block_at_or_after(&chain, 1_000, 1_600_000_000).await.unwrap();

        assert_eq!(found.block, 0, "the floor is the chain's own start");
        assert_eq!(found.timestamp, 1_700_000_000);
    }

    /// A date in the future, or a source that is far behind. Indexing
    /// nothing at all would be the wrong answer.
    #[tokio::test]
    async fn a_date_after_the_head_resolves_to_the_head() {
        let chain = FakeChain::new(1_600_000_000, 12, 1_000);

        let found =
            block_at_or_after(&chain, 1_000, 2_000_000_000).await.unwrap();

        assert_eq!(found.block, 999);
    }

    #[tokio::test]
    async fn an_empty_chain_resolves_to_block_zero() {
        let chain = FakeChain::new(1_600_000_000, 12, 0);
        let found =
            block_at_or_after(&chain, 0, 1_600_000_000).await.unwrap();
        assert_eq!(found.block, 0);
    }

    /// Heights the source does not have (skipped slots, the ragged edge of
    /// an archive) must not stall the search or push the answer BEFORE the
    /// moment asked for.
    #[tokio::test]
    async fn heights_the_source_skips_do_not_move_the_answer_earlier() {
        let mut chain = FakeChain::new(1_600_000_000, 10, 100_000);
        // A run of skipped heights right where a search is likely to probe.
        chain.missing = (50_000..50_040).collect();

        let wanted = 1_600_000_000 + 10 * 50_010;
        let found =
            block_at_or_after(&chain, 100_000, wanted).await.unwrap();

        assert!(
            i64::from(found.timestamp) >= wanted,
            "the floor landed BEFORE the date it was asked for"
        );
        assert!(found.block >= 50_010, "block {}", found.block);
        // And not wildly past it either.
        assert!(found.block <= 50_040, "block {}", found.block);
    }

    #[tokio::test]
    async fn the_very_first_and_very_last_block_are_reachable() {
        let chain = FakeChain::new(1_000, 5, 10);

        let found = block_at_or_after(&chain, 10, 0).await.unwrap();
        assert_eq!(found.block, 0);

        let found = block_at_or_after(&chain, 10, 1_045).await.unwrap();
        assert_eq!(found.block, 9);
    }
}
