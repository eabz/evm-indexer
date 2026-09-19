//! Fork-point search.
//!
//! Input: "block `B` claims parent hash `P`, but what is stored at `B - 1`
//! is something else". Output: the lowest block that has to go.
//!
//! The search walks DOWN from `B - 1` in windows of 8, 16, 32 ... blocks.
//! For each window it fetches the canonical headers and the stored hashes
//! and compares them height by height, top down:
//!
//! * stored hash == canonical hash -> everything up to here is fine, the
//!   fork point is the block above.
//! * stored hash != canonical hash -> orphaned, keep walking.
//! * nothing stored at this height -> stop, the fork point is the block
//!   above. A hole ends the search instead of being skipped: the blocks
//!   below a hole are a separate stored segment, and they are compared with
//!   the canonical chain when the hole is filled (the first block of every
//!   streamed range is checked against its stored predecessor). This is
//!   also what makes a fresh database or a `--start-block` deployment work:
//!   "nothing stored below" is a hole, never a mismatch and never a match.
//!
//! Bounds: never below `start_block`, never more than `max_reorg_depth`
//! blocks (typed error, nothing is changed), and a stored block 0 that
//! differs is a different network, not a reorg.
//!
//! The canonical source may be reorganizing WHILE we ask. Every window must
//! be complete, chain internally and link to the window above it (the top
//! one links to `P`). If not, the whole search starts over after a short
//! pause, a bounded number of times.

use super::{
    BlockHeader, CanonicalChain, ReorgConfig, ReorgError, ReorgStore,
    FIRST_SEARCH_WINDOW,
};
use alloy::primitives::B256;
use log::debug;
use std::collections::HashMap;

/// Result of a fork-point search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForkPoint {
    /// Lowest block to purge: every stored block in `[fork_point,
    /// mismatch_at)` is orphaned, the stored block below it (if any) is
    /// canonical.
    pub fork_point: u64,
    /// `mismatch_at - fork_point`.
    pub depth: u64,
    /// Stored / canonical hash of `mismatch_at - 1`.
    pub old_hash: B256,
    pub new_hash: B256,
    /// Windows fetched by the successful attempt.
    pub windows: u32,
}

/// Why one attempt has to be thrown away.
enum Attempt {
    Found(ForkPoint),
    Unstable(String),
}

/// Finds the fork point below `mismatch_at`, whose canonical parent hash
/// is `canonical_parent`.
///
/// A result with `depth == 0` means the store does NOT contradict the
/// canonical chain (the block below is missing or matches): there is
/// nothing to purge.
pub async fn find_fork_point(
    canonical: &dyn CanonicalChain,
    store: &dyn ReorgStore,
    config: &ReorgConfig,
    mismatch_at: u64,
    canonical_parent: B256,
) -> Result<ForkPoint, ReorgError> {
    let attempts = config.canonical_attempts.max(1);
    let mut detail = String::new();

    for attempt in 1..=attempts {
        match search_once(
            canonical,
            store,
            config,
            mismatch_at,
            canonical_parent,
        )
        .await?
        {
            Attempt::Found(fork) => return Ok(fork),
            Attempt::Unstable(why) => {
                debug!(
                    "Chain {}: fork-point search below {mismatch_at}, \
                     attempt {attempt}/{attempts}: {why}",
                    config.chain
                );
                detail = why;
            }
        }

        if attempt < attempts && !config.canonical_retry_delay.is_zero() {
            tokio::time::sleep(config.canonical_retry_delay).await;
        }
    }

    Err(ReorgError::CanonicalUnstable {
        chain: config.chain,
        at: mismatch_at,
        attempts,
        detail,
    })
}

async fn search_once(
    canonical: &dyn CanonicalChain,
    store: &dyn ReorgStore,
    config: &ReorgConfig,
    mismatch_at: u64,
    canonical_parent: B256,
) -> Result<Attempt, ReorgError> {
    let chain = config.chain;
    let floor = config.start_block;
    let max = config.max_reorg_depth;

    // Lowest height worth looking at. One more than `max` heights: a match
    // at `mismatch_at - max - 1` is a rollback of exactly `max` blocks.
    // (`min`: a mismatch at or below `start_block` has nothing below it.)
    let lowest = mismatch_at
        .saturating_sub(max.saturating_add(1))
        .max(floor)
        .min(mismatch_at);

    let found = |fork_point: u64, old_hash: B256, windows: u32| {
        Attempt::Found(ForkPoint {
            fork_point,
            depth: mismatch_at - fork_point,
            old_hash,
            new_hash: canonical_parent,
            windows,
        })
    };

    let mut hi = mismatch_at;
    // Hash the top header of the next window must have.
    let mut link = canonical_parent;
    let mut window = FIRST_SEARCH_WINDOW.max(1);
    let mut windows = 0u32;
    // Stored hash right below the mismatch (for the audit row).
    let mut old_hash = B256::ZERO;

    while hi > lowest {
        let lo = hi.saturating_sub(window).max(lowest);

        let headers = canonical
            .headers(lo, hi)
            .await
            .map_err(|source| ReorgError::Canonical { source })?;

        if let Err(why) = check_window(&headers, lo, hi, link) {
            return Ok(Attempt::Unstable(why));
        }

        let stored: HashMap<u64, B256> = store
            .stored_hashes(chain, lo, hi)
            .await
            .map_err(|source| ReorgError::Step {
                step: super::PurgeStep::FindForkPoint,
                source,
            })?
            .into_iter()
            .collect();

        windows += 1;

        for header in headers.iter().rev() {
            let height = header.number;

            let Some(stored_hash) = stored.get(&height).copied() else {
                // A hole (or the bottom of what is stored).
                return Ok(found(height + 1, old_hash, windows));
            };

            if height + 1 == mismatch_at {
                old_hash = stored_hash;
            }

            if stored_hash == header.hash {
                return Ok(found(height + 1, old_hash, windows));
            }

            if height == 0 {
                return Err(ReorgError::GenesisMismatch {
                    chain,
                    stored: stored_hash,
                    canonical: header.hash,
                });
            }
        }

        link = headers[0].parent_hash;
        hi = lo;
        window = window.saturating_mul(2);
    }

    // Every height of `[lowest, mismatch_at)` is stored and orphaned.
    let depth = mismatch_at - lowest;

    if depth > max {
        return Err(ReorgError::ReorgTooDeep {
            chain,
            mismatch_at,
            depth,
            max,
        });
    }

    // Reached `start_block`: what is below is not ours to judge.
    Ok(found(lowest, old_hash, windows))
}

/// A usable window is complete, ascending, chains internally and its top
/// header has the hash the window above (or the mismatching block) names
/// as its parent.
fn check_window(
    headers: &[BlockHeader],
    lo: u64,
    hi: u64,
    link: B256,
) -> Result<(), String> {
    if headers.len() as u64 != hi - lo {
        return Err(format!(
            "asked for headers [{lo}, {hi}), got {}",
            headers.len()
        ));
    }

    for (offset, header) in headers.iter().enumerate() {
        if header.number != lo + offset as u64 {
            return Err(format!(
                "header {} where {} was expected",
                header.number,
                lo + offset as u64
            ));
        }
    }

    for pair in headers.windows(2) {
        if pair[1].parent_hash != pair[0].hash {
            return Err(format!(
                "header {} does not build on header {}",
                pair[1].number, pair[0].number
            ));
        }
    }

    let top = headers[headers.len() - 1];

    if top.hash != link {
        return Err(format!(
            "header {} is no longer the parent of block {}",
            top.number,
            top.number + 1
        ));
    }

    Ok(())
}
