//! Tests of the reorg logic against the in-memory model (`model.rs`).
//!
//! The proof obligation everywhere: after things settle, what a reader
//! sees (`FINAL` on base tables, the validity rule on aggregates) equals a
//! CLEAN INDEX of the canonical chain - whatever was interrupted, and
//! wherever.

use super::{
    find_fork_point,
    model::{
        assert_checkpoints_honest, assert_clean, check_clean, Agg,
        FakeChain, FakeStore, FlushFault, Node, NodeOptions, PassOutcome,
        Rng,
    },
    BlockHeader, CanonicalChain, PurgeReason, PurgeStep, ReorgConfig,
    ReorgError, ReorgStore, StreamGuard, Verdict,
};
use alloy::primitives::B256;
use futures::{future::BoxFuture, FutureExt};
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

const CHAIN: u64 = 1;

/// A node that indexed `blocks` blocks of a fresh chain.
async fn indexed(blocks: u64, options: NodeOptions) -> Node {
    let chain = FakeChain::new(options.chain_id + 7, 20_000);
    chain.extend(blocks - 1);
    let mut node = Node::new(options, chain, FakeStore::new());
    node.settle(false).await.unwrap();
    assert_clean(&node, "initial index");
    node
}

fn config(node: &Node) -> ReorgConfig {
    ReorgConfig {
        canonical_retry_delay: Duration::ZERO,
        ..*node.guard.config()
    }
}

/// Runs the search the way the guard would after the chain moved on:
/// block `mismatch_at` of the NEW chain does not build on what is stored.
async fn search(
    node: &Node,
    config: &ReorgConfig,
    mismatch_at: u64,
) -> Result<super::ForkPoint, ReorgError> {
    let parent = node.chain.block(mismatch_at).unwrap().header.parent_hash;
    find_fork_point(
        node.chain.as_ref(),
        node.store.as_ref(),
        config,
        mismatch_at,
        parent,
    )
    .await
}

// ------------------------------------------------------ fork-point search

#[tokio::test]
async fn search_finds_shallow_and_deep_forks() {
    for (depth, windows) in
        [(1, 1), (3, 1), (8, 2), (9, 2), (24, 3), (60, 4)]
    {
        let node = indexed(100, NodeOptions::new(CHAIN)).await;
        node.chain.reorg(depth, depth + 1);

        let fork = search(&node, &config(&node), 100).await.unwrap();

        assert_eq!(fork.fork_point, 100 - depth, "depth {depth}");
        assert_eq!(fork.depth, depth);
        // 8, then 16, then 32 ... blocks per window.
        assert_eq!(fork.windows, windows, "depth {depth}");
        assert_eq!(
            fork.new_hash,
            node.chain.block(99).unwrap().header.hash
        );
        assert_ne!(fork.old_hash, fork.new_hash);
    }
}

#[tokio::test]
async fn search_is_bounded_by_max_reorg_depth() {
    let mut options = NodeOptions::new(CHAIN);
    options.max_reorg_depth = 20;

    // Exactly the maximum is still repaired ...
    let node = indexed(100, options).await;
    node.chain.reorg(20, 21);
    let fork = search(&node, &config(&node), 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (80, 20));

    // ... one more is a typed, fatal error that tells the operator what
    // to do, and nothing was written.
    let node = indexed(100, options).await;
    let before = node.data();
    node.chain.reorg(21, 22);
    let error = search(&node, &config(&node), 100).await.unwrap_err();

    assert!(error.is_fatal());
    assert!(matches!(
        error,
        ReorgError::ReorgTooDeep {
            depth: 21,
            max: 20,
            mismatch_at: 100,
            ..
        }
    ));
    let message = error.to_string();
    assert!(message.contains("--max-reorg-depth 20"), "{message}");
    assert!(message.contains("Nothing was changed"), "{message}");
    assert_eq!(node.data(), before);
}

#[tokio::test]
async fn search_stops_at_start_block_and_at_the_bottom_of_the_store() {
    // `--start-block 90`: everything stored is orphaned. The fork point is
    // the start block, not an error and not something below it.
    let mut options = NodeOptions::new(CHAIN);
    options.start_block = 90;
    let node = indexed(100, options).await;
    node.chain.reorg(30, 31);
    let fork = search(&node, &config(&node), 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (90, 10));

    // Same data, but the process was started with a LOWER start block
    // (the store simply begins at 90, e.g. `--new-blocks-only`): "nothing
    // stored" below 90 is a hole, neither a match nor a mismatch.
    let mut lower = config(&node);
    lower.start_block = 0;
    let fork = search(&node, &lower, 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (90, 10));

    // Orphaned blocks BELOW the start block are not ours to judge.
    let node = indexed(100, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(30, 31);
    let mut higher = config(&node);
    higher.start_block = 95;
    let fork = search(&node, &higher, 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (95, 5));

    // A mismatch at or below the start block has nothing below it.
    higher.start_block = 100;
    let fork = search(&node, &higher, 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (100, 0));
}

#[tokio::test]
async fn search_stops_at_a_hole() {
    let node = indexed(100, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(30, 31);
    // Blocks 92 and 93 were never stored.
    node.store.forget(CHAIN, 92, 94);

    let fork = search(&node, &config(&node), 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (94, 6));

    // Nothing stored right below the mismatch: the store contradicts
    // nothing.
    node.store.forget(CHAIN, 99, 100);
    let fork = search(&node, &config(&node), 100).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (100, 0));
    assert_eq!(fork.old_hash, B256::ZERO);
}

#[tokio::test]
async fn search_near_block_zero() {
    // Reorg of everything but block 0: the fork point is block 1.
    let node = indexed(6, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(5, 6);
    let fork = search(&node, &config(&node), 6).await.unwrap();
    assert_eq!((fork.fork_point, fork.depth), (1, 5));

    // A different block 0 is a different network: fatal, nothing purged.
    let node = indexed(6, NodeOptions::new(CHAIN)).await;
    let other = FakeChain::new(999, 20_000);
    other.extend(10);
    let parent = other.block(6).unwrap().header.parent_hash;
    let error = find_fork_point(
        other.as_ref(),
        node.store.as_ref(),
        &config(&node),
        6,
        parent,
    )
    .await
    .unwrap_err();

    assert!(error.is_fatal());
    assert!(matches!(error, ReorgError::GenesisMismatch { .. }));
    assert!(error.to_string().contains("different network"));
}

/// A canonical source that misbehaves for its first `bad` calls.
struct Flaky {
    chain: Arc<FakeChain>,
    bad: AtomicU32,
    mode: u8,
}

impl CanonicalChain for Flaky {
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<BlockHeader>>> {
        async move {
            let mut headers = self.chain.headers(from, to).await?;
            if self.bad.load(Ordering::Relaxed) > 0 {
                self.bad.fetch_sub(1, Ordering::Relaxed);
                match self.mode {
                    // A header from another fork in the middle.
                    0 => headers[1].hash = B256::repeat_byte(0xee),
                    // The top of the window moved on.
                    1 => headers.last_mut().unwrap().hash = B256::ZERO,
                    // The chain got shorter.
                    _ => {
                        headers.pop();
                    }
                }
            }
            Ok(headers)
        }
        .boxed()
    }
}

#[tokio::test]
async fn search_starts_over_while_the_source_reorganizes() {
    for mode in 0..3u8 {
        let node = indexed(100, NodeOptions::new(CHAIN)).await;
        node.chain.reorg(12, 13);
        let parent = node.chain.block(100).unwrap().header.parent_hash;

        let flaky = Flaky {
            chain: node.chain.clone(),
            bad: AtomicU32::new(2),
            mode,
        };
        let fork = find_fork_point(
            &flaky,
            node.store.as_ref(),
            &config(&node),
            100,
            parent,
        )
        .await
        .unwrap();
        assert_eq!((fork.fork_point, fork.depth), (88, 12), "mode {mode}");

        // It never settles: a retryable error, nothing purged.
        let flaky = Flaky {
            chain: node.chain.clone(),
            bad: AtomicU32::new(u32::MAX),
            mode,
        };
        let error = find_fork_point(
            &flaky,
            node.store.as_ref(),
            &config(&node),
            100,
            parent,
        )
        .await
        .unwrap_err();
        assert!(!error.is_fatal());
        assert!(
            matches!(
                error,
                ReorgError::CanonicalUnstable { attempts: 4, .. }
            ),
            "{error}"
        );
    }
}

#[tokio::test]
async fn search_reports_a_stale_trigger_and_source_errors_as_retryable() {
    let node = indexed(100, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(5, 6);
    let stale_parent = node.chain.block(100).unwrap().header.parent_hash;
    // The block that triggered the search is itself reorganized away.
    node.chain.reorg(3, 4);

    let error = find_fork_point(
        node.chain.as_ref(),
        node.store.as_ref(),
        &config(&node),
        100,
        stale_parent,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, ReorgError::CanonicalUnstable { .. }));
    assert!(!error.is_fatal());

    node.chain.fail_next_headers(1);
    let error = search(&node, &config(&node), 100).await.unwrap_err();
    assert!(matches!(error, ReorgError::Canonical { .. }));
    assert!(!error.is_fatal());
}

// ------------------------------------------------------------ the guard

#[tokio::test]
async fn a_consistent_chain_continues_and_never_purges() {
    let mut node = indexed(50, NodeOptions::new(CHAIN)).await;
    node.chain.extend(50);
    node.settle(false).await.unwrap();

    assert_clean(&node, "extended");
    assert!(node.rollbacks.is_empty());
    assert!(node.data().reorgs.is_empty());
    assert_eq!(node.writer.epoch(), 0);
}

#[tokio::test]
async fn rollback_at_the_tip_end_to_end() {
    let mut node = indexed(60, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(5, 8);

    let outcome = node.pass().await.unwrap();
    let PassOutcome::RolledBack(rollback) = outcome else {
        panic!("expected a rollback, got {outcome:?}");
    };

    assert_eq!(rollback.fork_point, 55);
    assert_eq!(rollback.depth, 5);
    assert_eq!(rollback.old_head, 59);
    assert_eq!(rollback.mismatch_height, 59);
    assert_eq!(rollback.purge_to, None);
    assert_eq!(
        rollback.new_hash,
        node.chain.block(59).unwrap().header.hash
    );

    // The audit row, the epoch, the hooks.
    let data = node.data();
    assert_eq!(data.reorgs.len(), 1);
    let record = &data.reorgs[0];
    assert_eq!(record.epoch, 1);
    assert_eq!(record.fork_block, 55);
    assert_eq!(record.depth, 5);
    assert_eq!(record.old_head, 59);
    assert_eq!(record.reason, "reorg");
    assert_eq!(record.from_ts % 86_400, 0);
    assert_eq!(node.writer.epoch(), 1);
    assert_eq!(*node.recorder.evictions.lock().unwrap(), vec![(55, None)]);
    assert_eq!(*node.recorder.reorg_depths.lock().unwrap(), vec![5]);
    assert_eq!(*node.recorder.purged_blocks.lock().unwrap(), vec![5]);

    node.settle(false).await.unwrap();
    assert_clean(&node, "after the rollback");
    assert_eq!(node.data().reorgs.len(), 1);
}

#[tokio::test]
async fn purge_steps_run_in_the_documented_order() {
    let mut node = indexed(60, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(3, 4);
    node.store.take_journal(CHAIN);

    node.pass().await.unwrap();

    let journal: Vec<PurgeStep> = node
        .store
        .take_journal(CHAIN)
        .into_iter()
        .filter(|step| *step != PurgeStep::FindForkPoint)
        .collect();

    assert_eq!(
        journal,
        vec![
            PurgeStep::ReadEpoch,
            PurgeStep::MinTimestamp,
            PurgeStep::TombstoneCheckpoints,
            PurgeStep::Verify,
            PurgeStep::TombstoneChildren,
            PurgeStep::Verify,
            // Second look at `from_ts`, after the tombstones converged.
            PurgeStep::MinTimestamp,
            PurgeStep::InsertReorg,
            PurgeStep::RebuildDerived,
            // The commit marker is the last write.
            PurgeStep::TombstoneBlocks,
            PurgeStep::Verify,
        ]
    );
    // The writer was quiesced before the search AND before the purge.
    assert!(node.writer.quiesces() >= 2);
}

#[tokio::test]
async fn the_first_block_after_a_restart_is_checked_against_the_store() {
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    // The reorg happens while the indexer is down.
    node.chain.reorg(4, 6);
    node.restart();

    let outcome = node.pass().await.unwrap();
    assert!(
        matches!(outcome, PassOutcome::RolledBack(r) if r.fork_point == 36)
    );
    node.settle(false).await.unwrap();
    assert_clean(&node, "reorg while down");
}

#[tokio::test]
async fn a_failing_hash_lookup_is_an_error_not_a_pass() {
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(4, 6);
    node.restart();
    node.guard.startup().await.unwrap();

    // Today's detector treats a failed seed as "no evidence" and would
    // store blocks 40.. on top of an orphaned 36..39 for ever.
    node.store.fail_at(CHAIN, PurgeStep::FindForkPoint, false);
    let header = node.chain.block(40).unwrap().header;
    let error = node.guard.observe(&[header], None).await.unwrap_err();
    assert!(matches!(
        error,
        ReorgError::Step { step: PurgeStep::FindForkPoint, .. }
    ));

    // The retry sees the reorg.
    let verdict = node.guard.observe(&[header], None).await.unwrap();
    assert!(matches!(verdict, Verdict::Rollback(r) if r.fork_point == 36));
}

#[tokio::test]
async fn the_rollback_guard_alone_is_enough_evidence() {
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(2, 3);
    node.guard.startup().await.unwrap();

    let header = node.chain.block(40).unwrap().header;
    let stream_guard = StreamGuard {
        first_block: 40,
        first_parent_hash: header.parent_hash,
    };

    // A response without any block row.
    let verdict =
        node.guard.observe(&[], Some(&stream_guard)).await.unwrap();
    assert!(matches!(verdict, Verdict::Rollback(r) if r.fork_point == 38));

    // A guard describing some other window proves nothing.
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.guard.startup().await.unwrap();
    let elsewhere = StreamGuard {
        first_block: 45,
        first_parent_hash: B256::repeat_byte(0xee),
    };
    let verdict = node.guard.observe(&[], Some(&elsewhere)).await.unwrap();
    assert_eq!(verdict, Verdict::Continue);
}

#[tokio::test]
async fn block_zero_and_the_start_block_have_no_predecessor() {
    // Blocks 0..19 of an abandoned fork are in the store, but this process
    // starts at 20: block 20 is not compared with the stored block 19.
    let mut node = indexed(20, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(10, 40);
    node.options.start_block = 20;
    node.restart();

    node.settle(false).await.unwrap();
    assert!(node.rollbacks.is_empty());
    assert!(node.data().reorgs.is_empty());

    // Block 0 of a fresh database.
    let chain = FakeChain::new(3, 600);
    let mut node =
        Node::new(NodeOptions::new(CHAIN), chain, FakeStore::new());
    node.guard.startup().await.unwrap();
    let genesis = node.chain.block(0).unwrap().header;
    assert_eq!(
        node.guard.observe(&[genesis], None).await.unwrap(),
        Verdict::Continue
    );
}

#[tokio::test]
async fn confirmations_at_least_as_deep_as_the_reorg_purge_nothing() {
    let mut options = NodeOptions::new(CHAIN);
    options.confirmations = 6;
    let mut node = indexed(80, options).await;
    let mut rng = Rng::new(42);

    for round in 0..40 {
        let depth = rng.between(1, 6);
        node.chain.reorg(depth, depth + rng.between(0, 3));
        node.settle(false).await.unwrap();
        assert_clean(&node, &format!("round {round}"));
    }

    // Not a single purge: no epoch, no audit row, no tombstone.
    let data = node.data();
    assert!(node.rollbacks.is_empty());
    assert!(data.reorgs.is_empty());
    assert_eq!(node.writer.epoch(), 0);
    assert!(data.blocks.values().all(|versions| versions.len() == 1));

    // One block deeper than the confirmations: exactly that block goes.
    node.chain.reorg(7, 7);
    node.chain.extend(1);
    node.settle(false).await.unwrap();
    assert_eq!(node.rollbacks.len(), 1);
    assert_eq!(node.rollbacks[0].depth, 1);
    assert_clean(&node, "deeper than the confirmations");
}

#[tokio::test]
async fn a_reorg_inside_the_stream_is_caught_before_anything_is_stored() {
    for discard in [false, true] {
        let mut options = NodeOptions::new(CHAIN);
        options.discard_on_quiesce = discard;
        options.flush_every = 50;
        options.response_size = 2;
        let mut node = indexed(30, options).await;

        node.chain.extend(20);
        // After three more responses the newest 8 blocks are replaced:
        // blocks already streamed (and only buffered) become orphans.
        node.chain.reorg_after_calls(3, 16);

        node.settle(false).await.unwrap();
        assert_clean(&node, &format!("discard {discard}"));
    }
}

#[tokio::test]
async fn a_mismatch_below_other_stored_blocks_purges_only_its_segment() {
    // Stored: 0..39 (its last 5 blocks orphaned) and, from another run,
    // 60..79 (canonical). Filling the gap finds the mismatch at block 40.
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(5, 45);

    let mut upper = NodeOptions::new(CHAIN);
    upper.start_block = 60;
    let mut other =
        Node::new(upper, node.chain.clone(), node.store.clone());
    other.settle(false).await.unwrap();

    node.restart();
    let outcome = node.pass().await.unwrap();
    let PassOutcome::RolledBack(rollback) = outcome else {
        panic!("expected a rollback");
    };
    assert_eq!(rollback.fork_point, 35);
    assert_eq!(rollback.purge_to, Some(40));
    assert_eq!(rollback.old_head, 79);

    // The canonical blocks 60..79 were not touched.
    let data = node.data();
    assert!((60..80).all(|n| data.blocks[&n].len() == 1));

    node.settle(false).await.unwrap();
    assert_clean(&node, "bounded purge");
}

#[tokio::test]
async fn an_orphaned_segment_above_a_gap_is_found_when_the_gap_closes() {
    // 0..29 stored; blocks 50..59 stored by another run from a fork that
    // was abandoned since. The chain does not grow, so the tip check can
    // not help: the join check has to.
    let mut node = indexed(30, NodeOptions::new(CHAIN)).await;
    node.chain.extend(30);

    let mut upper = NodeOptions::new(CHAIN);
    upper.start_block = 50;
    let mut other =
        Node::new(upper, node.chain.clone(), node.store.clone());
    other.settle(false).await.unwrap();

    node.chain.reorg(15, 15);
    node.restart();
    node.settle(false).await.unwrap();

    assert_eq!(node.rollbacks.len(), 1);
    assert_eq!(node.rollbacks[0].fork_point, 50);
    assert_eq!(node.rollbacks[0].mismatch_height, 50);
    assert_clean(&node, "join check");
}

#[tokio::test]
async fn a_join_rollback_is_bounded_too() {
    let mut options = NodeOptions::new(CHAIN);
    options.max_reorg_depth = 5;
    let mut node = indexed(30, options).await;
    node.chain.extend(30);

    let mut upper = options;
    upper.start_block = 50;
    let mut other =
        Node::new(upper, node.chain.clone(), node.store.clone());
    other.settle(false).await.unwrap();

    node.chain.reorg(15, 15);
    node.restart();

    let error = node.settle(false).await.unwrap_err();
    assert!(matches!(
        error,
        ReorgError::ReorgTooDeep { depth: 10, max: 5, .. }
    ));
}

#[tokio::test]
async fn the_tip_check_sees_a_reorg_that_did_not_grow_the_chain() {
    let mut options = NodeOptions::new(CHAIN);
    let mut node = indexed(50, options).await;

    // Same length, other blocks: no new block will name a parent for a
    // while. Without the tip check nothing is noticed ...
    node.chain.reorg(3, 3);
    node.settle(false).await.unwrap();
    assert!(node.rollbacks.is_empty());
    assert!(check_clean(&node).is_err());

    // ... with it the rollback happens right away.
    options.check_tip = true;
    node.options = options;
    node.settle(false).await.unwrap();
    assert_eq!(node.rollbacks.len(), 1);
    assert_eq!(node.rollbacks[0].fork_point, 47);
    assert_clean(&node, "tip check");

    // A source that is BEHIND what is stored proves nothing.
    node.chain.reorg(5, 0);
    node.settle(false).await.unwrap();
    assert_eq!(node.rollbacks.len(), 1);
}

// ------------------------------------------------ no read-your-writes

#[tokio::test]
async fn an_epoch_is_never_used_twice_even_if_reorgs_reads_lag() {
    let mut node = indexed(60, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(4, 6);
    node.settle(false).await.unwrap();
    assert_eq!(node.data().reorgs.len(), 1);

    // The next purge comes right away and its read of `reorgs` does not
    // see the row that was just written.
    node.store.stale_epoch_once();
    node.chain.reorg(2, 3);
    node.settle(false).await.unwrap();

    let epochs: Vec<u32> =
        node.data().reorgs.iter().map(|r| r.epoch).collect();
    assert_eq!(epochs, vec![1, 2]);
    assert_eq!(node.writer.epoch(), 2);
    assert_clean(&node, "stale epoch read");
}

#[tokio::test]
async fn tombstones_are_reissued_until_nothing_is_left() {
    // On a lagging store every purge still ends with zero live rows in
    // its range, whatever the individual statements could see.
    for seed in 1..=40u64 {
        let chain = FakeChain::new(seed, 20_000);
        chain.extend(40);
        let mut options = NodeOptions::new(CHAIN);
        options.sloppy_writer = true;
        let mut node =
            Node::new(options, chain, FakeStore::with_lag(seed));
        node.settle(false).await.unwrap();

        node.chain.reorg(6, 8);
        node.settle(false).await.unwrap();
        assert_clean(&node, &format!("lagging store, seed {seed}"));
    }
}

#[tokio::test]
async fn rows_a_tombstone_statement_could_not_see_yet_are_caught() {
    let mut node = indexed(60, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(12, 14);

    // The first children tombstone misses half of the range, silently.
    node.store.miss_children_once();
    node.store.take_journal(CHAIN);
    node.pass().await.unwrap();

    let journal = node.store.take_journal(CHAIN);
    let statements = journal
        .iter()
        .filter(|step| **step == PurgeStep::TombstoneChildren)
        .count();
    assert_eq!(statements, 2, "{journal:?}");

    // Not one orphaned child row survived the purge.
    let data = node.data();
    for table in 0..2 {
        assert!(data.live_children(table).keys().all(|(n, _)| *n < 48));
    }

    node.settle(false).await.unwrap();
    assert_clean(&node, "missed rows");
}

#[tokio::test]
async fn from_ts_is_looked_at_twice() {
    // About one block per day.
    let chain = FakeChain::new(5, 170_000);
    chain.extend(40);
    let mut node =
        Node::new(NodeOptions::new(CHAIN), chain, FakeStore::new());
    node.settle(false).await.unwrap();

    // A fork point whose block is alone on its day.
    let day = |n: u64| {
        super::start_of_day(node.chain.block(n).unwrap().header.timestamp)
    };
    let fork = (25..40).find(|n| day(*n) < day(n + 1)).unwrap();
    let first_day = day(fork);

    // The first `min_timestamp` can not read the lowest orphaned block
    // yet. Trusting it would leave that block's day stale for ever.
    node.chain.reorg(41 - fork, 50 - fork);
    node.store.miss_lowest_timestamp_once();
    node.settle(false).await.unwrap();

    assert_eq!(node.rollbacks[0].fork_point, fork);
    assert_eq!(node.data().reorgs[0].from_ts, first_day);
    assert_clean(&node, "late first look");
}

#[tokio::test]
async fn tombstones_that_never_converge_are_a_typed_fatal_error() {
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(3, 4);
    node.store.children_never_die();

    let error = node.settle(false).await.unwrap_err();
    assert!(error.is_fatal());
    assert!(matches!(
        error,
        ReorgError::TombstonesNotConverging {
            step: PurgeStep::TombstoneChildren,
            attempts: 6,
            ..
        }
    ));
    assert!(error.to_string().contains("one indexer per chain"));
    // The commit marker was not touched: the restart finds the reorg again.
    assert!((37..40).all(|n| node.data().live_blocks().contains_key(&n)));
}

#[tokio::test]
async fn a_guard_purge_that_reads_nothing_is_not_reported_as_done() {
    // `purge_range` on an empty range is a no-op, but the guard only
    // purges where it SAW rows: reading none is a stale read, an error.
    let mut node = indexed(30, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(2, 3);
    node.guard.startup().await.unwrap();
    let header = node.chain.block(30).unwrap().header;
    let Verdict::Rollback(mut rollback) =
        node.guard.observe(&[header], None).await.unwrap()
    else {
        panic!("expected a rollback");
    };

    rollback.fork_point = 500;
    let error = node.guard.repair(&rollback).await.unwrap_err();
    assert!(matches!(
        error,
        ReorgError::Step { step: PurgeStep::MinTimestamp, .. }
    ));
    assert!(!error.is_fatal());
    assert!(node.data().reorgs.is_empty());
}

// ---------------------------------------------------------- re-decoding

/// A [`ReorgStore`] scoped to one module (child table 1), the way the
/// pipeline builds one for `indexer backfill --module dex`.
struct ModuleStore {
    inner: Arc<FakeStore>,
    /// The mistake to avoid: rebuilding (and range-excluding) like a
    /// rollback does.
    rebuild_like_a_rollback: bool,
}

impl ReorgStore for ModuleStore {
    fn current_epoch(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<u32>> {
        self.inner.current_epoch(chain)
    }

    fn stored_head(
        &self,
        chain: u64,
    ) -> BoxFuture<'_, anyhow::Result<Option<u64>>> {
        self.inner.stored_head(chain)
    }

    fn stored_hashes(
        &self,
        chain: u64,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, anyhow::Result<Vec<(u64, B256)>>> {
        self.inner.stored_hashes(chain, from, to)
    }

    fn has_orphan_children(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<bool>> {
        async { Ok(false) }.boxed()
    }

    fn min_timestamp(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<Option<u32>>> {
        self.inner.min_timestamp(chain, from, to)
    }

    fn live_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            let data = self.inner.snapshot(chain);
            Ok(data
                .live_children(1)
                .keys()
                .filter(|(n, _)| *n >= from && to.is_none_or(|to| *n < to))
                .count() as u64)
        }
        .boxed()
    }

    fn live_blocks(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async { Ok(0) }.boxed()
    }

    fn live_checkpoints(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async { Ok(0) }.boxed()
    }

    fn tombstone_checkpoints(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
        _version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async { Ok(0) }.boxed()
    }

    fn tombstone_children(
        &self,
        chain: u64,
        from: u64,
        to: Option<u64>,
        version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async move {
            self.inner.tombstone_children_of(
                chain,
                from,
                to,
                version,
                &[1],
            )
        }
        .boxed()
    }

    fn insert_reorg<'a>(
        &'a self,
        record: &'a super::ReorgRecord,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        self.inner.insert_reorg(record)
    }

    fn rebuild_derived(
        &self,
        chain: u64,
        from_ts: u32,
        epoch: u32,
        purged_from: u64,
        purged_to: Option<u64>,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            // EVERY aggregate is rebuilt (the epoch is per chain), but
            // only the module's own leaves the range out: the other
            // tables keep their rows there and nobody writes them again.
            let keep: &[Agg] = if self.rebuild_like_a_rollback {
                &[]
            } else {
                &[Agg::BlocksDaily, Agg::Child0Daily]
            };
            self.inner.rebuild_keeping(
                chain,
                from_ts,
                epoch,
                purged_from,
                purged_to,
                keep,
            )
        }
        .boxed()
    }

    fn tombstone_blocks(
        &self,
        _chain: u64,
        _from: u64,
        _to: Option<u64>,
        _version: u64,
    ) -> BoxFuture<'_, anyhow::Result<u64>> {
        async { Ok(0) }.boxed()
    }
}

#[tokio::test]
async fn redecoding_one_module_keeps_every_aggregate_right() {
    for rebuild_like_a_rollback in [false, true] {
        let node = indexed(60, NodeOptions::new(CHAIN)).await;
        let module = Arc::new(ModuleStore {
            inner: node.store.clone(),
            rebuild_like_a_rollback,
        });
        let purger = super::Purger::new(
            module,
            node.writer.clone(),
            node.recorder.clone(),
            node.recorder.clone(),
        );

        // Blocks 20..40 are decoded again (same result here).
        let report = purger
            .purge_range(CHAIN, 20, Some(40), PurgeReason::Redecode)
            .await
            .unwrap();
        assert_eq!(report.blocks_tombstoned, 0);
        assert_eq!(node.data().reorgs[0].reason, "redecode");

        let blocks: Vec<_> =
            (20..40).map(|n| node.chain.block(n).unwrap()).collect();
        node.store.insert_child_table(
            CHAIN,
            &blocks,
            1,
            node.writer.epoch(),
            crate::db::next_version(),
        );

        if rebuild_like_a_rollback {
            // NEGATIVE CONTROL: the aggregates of the OTHER tables lose
            // the range for ever.
            let problem = check_clean(&node).unwrap_err();
            assert!(problem.contains("aggregates differ"), "{problem}");
        } else {
            assert_clean(&node, "redecode");
        }
    }
}

#[tokio::test]
async fn purging_an_empty_range_changes_nothing() {
    let node = indexed(30, NodeOptions::new(CHAIN)).await;
    let before = node.data();
    let purger = super::Purger::new(
        node.store.clone(),
        node.writer.clone(),
        node.recorder.clone(),
        node.recorder.clone(),
    );

    let report = purger
        .purge_range(CHAIN, 500, None, PurgeReason::GapHeal)
        .await
        .unwrap();

    assert_eq!(report.epoch, 0);
    assert_eq!(report.from_ts, None);
    assert_eq!(node.data(), before);
}

#[tokio::test]
async fn purge_range_is_idempotent() {
    let mut node = indexed(60, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(6, 9);
    node.pass().await.unwrap();

    // The same purge again (and again): more epochs, same visible state.
    let purger = super::Purger::new(
        node.store.clone(),
        node.writer.clone(),
        node.recorder.clone(),
        node.recorder.clone(),
    );
    for _ in 0..3 {
        purger
            .purge_range(CHAIN, 54, None, PurgeReason::GapHeal)
            .await
            .unwrap();
    }

    assert_eq!(node.data().reorgs.len(), 4);
    assert_eq!(node.writer.epoch(), 4);
    node.settle(false).await.unwrap();
    assert_clean(&node, "purged four times");
}

// ------------------------------------------------ crash at every step

const FAULT_STEPS: [PurgeStep; 10] = [
    PurgeStep::FindForkPoint,
    PurgeStep::Quiesce,
    PurgeStep::ReadEpoch,
    PurgeStep::MinTimestamp,
    PurgeStep::TombstoneCheckpoints,
    PurgeStep::TombstoneChildren,
    PurgeStep::InsertReorg,
    PurgeStep::RebuildDerived,
    PurgeStep::TombstoneBlocks,
    PurgeStep::Verify,
];

fn inject(node: &Node, step: PurgeStep, partial: bool) {
    if step == PurgeStep::Quiesce {
        node.writer.fail_next_quiesce();
    } else {
        node.store.fail_at(node.options.chain_id, step, partial);
    }
}

/// THE core proof: a rollback interrupted at step N, for every N, with
/// the step done not at all or half way, followed by a process restart or
/// by a retry in the same process, always converges to a clean index.
#[tokio::test]
async fn a_rollback_interrupted_at_any_step_converges() {
    let mut cases = 0;

    for step in FAULT_STEPS {
        for partial in [false, true] {
            for restart in [false, true] {
                for discard in [false, true] {
                    let context = format!(
                        "step `{step}` partial={partial} \
                         restart={restart} discard={discard}"
                    );
                    let mut options = NodeOptions::new(CHAIN);
                    options.discard_on_quiesce = discard;
                    let mut node = indexed(70, options).await;
                    node.chain.reorg(9, 12);

                    inject(&node, step, partial);
                    let failed = node.pass().await;
                    assert!(failed.is_err(), "{context}: no failure");
                    assert_checkpoints_honest(&node.data(), &context);

                    if restart {
                        node.restart();
                    }
                    node.settle(restart).await.unwrap();
                    assert_clean(&node, &context);

                    // And it stays clean while the chain moves on.
                    node.chain.extend(7);
                    node.settle(false).await.unwrap();
                    assert_clean(&node, &context);
                    cases += 1;
                }
            }
        }
    }

    assert_eq!(cases, 80);
}

/// Same proof for gap healing: a flush died after the children (orphans),
/// then the heal itself is interrupted at every step.
#[tokio::test]
async fn a_gap_heal_interrupted_at_any_step_converges() {
    let mut cases = 0;

    for flush_fault in
        [FlushFault::AfterChildren, FlushFault::AfterSomeBlocks]
    {
        for step in FAULT_STEPS.into_iter().chain([PurgeStep::FindOrphans])
        {
            if step == PurgeStep::FindForkPoint {
                continue;
            }
            for partial in [false, true] {
                for restart in [false, true] {
                    let context = format!(
                        "{flush_fault:?}, step `{step}` partial={partial} \
                         restart={restart}"
                    );
                    let mut options = NodeOptions::new(CHAIN);
                    options.flush_every = 100;
                    let mut node = indexed(30, options).await;

                    // 24 blocks (about three days) die between the
                    // children and the `blocks` rows.
                    node.chain.extend(24);
                    node.writer.fail_next_flush(flush_fault);
                    assert!(node.pass().await.is_err(), "{context}");
                    node.restart();

                    // The heal of the first pass is interrupted ...
                    inject(&node, step, partial);
                    assert!(node.pass().await.is_err(), "{context}");
                    assert_checkpoints_honest(&node.data(), &context);

                    // ... and whatever comes next finishes the job.
                    if restart {
                        node.restart();
                    }
                    node.settle(restart).await.unwrap();
                    assert_clean(&node, &context);
                    assert!(
                        node.data().reorgs.iter().all(|r| r.reason
                            == "gap_heal"
                            && r.depth == 0),
                        "{context}"
                    );
                    cases += 1;
                }
            }
        }
    }

    assert_eq!(cases, 80);
}

/// The scenario behind "from_ts over ALL row versions": a gap spanning
/// several days whose first days were already tombstoned by a run that
/// died. The second run must still repair from the FIRST day.
#[tokio::test]
async fn a_second_heal_still_repairs_from_the_first_day() {
    async fn run(store: Arc<FakeStore>) -> (Node, u32) {
        let chain = FakeChain::new(77, 60_000);
        chain.extend(29);
        let mut options = NodeOptions::new(CHAIN);
        options.flush_every = 100;
        let mut node = Node::new(options, chain, store);
        node.settle(false).await.unwrap();

        node.chain.extend(24);
        node.writer.fail_next_flush(FlushFault::AfterChildren);
        assert!(node.pass().await.is_err());
        node.restart();

        // First heal: dies with the lower half of the children dead.
        node.store.fail_at(CHAIN, PurgeStep::TombstoneChildren, true);
        assert!(node.pass().await.is_err());
        node.restart();
        node.settle(true).await.unwrap();

        let first_orphan = node.chain.block(30).unwrap().header.timestamp;
        (node, super::start_of_day(first_orphan))
    }

    let (node, first_day) = run(FakeStore::new()).await;
    let last = node.chain.block(53).unwrap().header.timestamp;
    assert!(
        super::start_of_day(last) > first_day,
        "the gap must span more than one day"
    );
    assert!(node.data().reorgs.iter().all(|r| r.from_ts <= first_day));
    assert_clean(&node, "two-day gap");

    // NEGATIVE CONTROL: with `from_ts` computed over live rows only, the
    // second run starts its repair a day late and the first day keeps
    // counting the orphans for ever.
    let broken = FakeStore::with_min_ts_live_only();
    let (node, first_day) = run(broken).await;
    assert!(node.data().reorgs.iter().any(|r| r.from_ts > first_day));
    let problem = check_clean(&node).unwrap_err();
    assert!(problem.contains("aggregates differ"), "{problem}");
}

/// Why `has_orphan_children` has to count TOMBSTONED rows: a heal that
/// dies right after tombstoning the children leaves no live orphan behind,
/// but the aggregates still count them.
#[tokio::test]
async fn tombstoned_orphans_still_mark_an_unfinished_heal() {
    async fn run(store: Arc<FakeStore>) -> Node {
        let chain = FakeChain::new(78, 20_000);
        chain.extend(29);
        let mut options = NodeOptions::new(CHAIN);
        options.flush_every = 100;
        let mut node = Node::new(options, chain, store);
        node.settle(false).await.unwrap();

        node.chain.extend(12);
        node.writer.fail_next_flush(FlushFault::AfterChildren);
        assert!(node.pass().await.is_err());
        node.restart();

        // Children fully tombstoned, then the process dies.
        node.store.fail_at(CHAIN, PurgeStep::InsertReorg, false);
        assert!(node.pass().await.is_err());
        node.restart();
        node.settle(true).await.unwrap();
        node
    }

    assert_clean(&run(FakeStore::new()).await, "dead orphans");

    // NEGATIVE CONTROL: looking for live orphans only (what the design
    // text suggests) never finishes the repair: double counting.
    let broken = FakeStore::with_orphans_live_only();
    let problem = check_clean(&run(broken).await).unwrap_err();
    assert!(problem.contains("aggregates differ"), "{problem}");
}

#[tokio::test]
async fn orphans_above_a_chain_that_got_shorter_are_healed() {
    let mut options = NodeOptions::new(CHAIN);
    options.flush_every = 100;
    let mut node = indexed(30, options).await;

    node.chain.extend(10);
    node.writer.fail_next_flush(FlushFault::AfterChildren);
    assert!(node.pass().await.is_err());

    // While the indexer is down the chain loses those blocks: the orphans
    // sit ABOVE everything a pass will look at.
    node.chain.reorg(10, 0);
    node.restart();
    node.settle(true).await.unwrap();

    let data = node.data();
    assert_eq!(data.reorgs.len(), 1);
    assert_eq!(data.reorgs[0].reason, "gap_heal");
    assert_eq!(data.reorgs[0].to_block, None);

    node.chain.extend(15);
    node.settle(false).await.unwrap();
    assert_clean(&node, "orphans above the head");
}

#[tokio::test]
async fn the_epoch_survives_restarts_and_failed_purges() {
    let mut node = indexed(40, NodeOptions::new(CHAIN)).await;
    node.chain.reorg(3, 4);
    node.settle(false).await.unwrap();
    assert_eq!(node.writer.epoch(), 1);

    // A restarted writer starts with the stored epoch, not with 0.
    node.restart();
    node.chain.extend(3);
    node.settle(false).await.unwrap();
    assert_eq!(node.writer.epoch(), 1);

    // The `reorgs` row lands but its acknowledgement is lost: the process
    // lives on and must not keep writing with the epoch that row hides.
    node.chain.reorg(2, 5);
    node.store.fail_at(CHAIN, PurgeStep::InsertReorg, true);
    assert!(node.pass().await.is_err());
    node.settle(false).await.unwrap();
    assert_eq!(node.writer.epoch(), 3);
    assert_clean(&node, "lost acknowledgement");
}

// ------------------------------------------------------- random scenarios

struct Tally {
    scenarios: u64,
    operations: u64,
    settles_verified: u64,
    rollbacks: u64,
    gap_heals: u64,
    faults_hit: u64,
    lagged_reads: u64,
}

fn random_options(rng: &mut Rng, chain_id: u64) -> NodeOptions {
    NodeOptions {
        chain_id,
        start_block: [0, 0, 5, 17][rng.below(4) as usize],
        confirmations: [0, 0, 0, 2, 5][rng.below(5) as usize],
        max_reorg_depth: 40,
        flush_every: rng.between(1, 8) as usize,
        response_size: rng.between(1, 5),
        discard_on_quiesce: rng.chance(25),
        check_joins: rng.chance(75),
        stream_guards: rng.chance(50),
        check_tip: rng.chance(50),
        sloppy_writer: rng.chance(50),
    }
}

fn random_chain(
    rng: &mut Rng,
    seed: u64,
    start_block: u64,
) -> Arc<FakeChain> {
    // From "hundreds of blocks per day" to "a few blocks per MONTH" (the
    // latter crosses table partitions all the time).
    let block_time = [600, 7_000, 30_000, 400_000][rng.below(4) as usize];
    let chain = FakeChain::new(seed, block_time);
    chain.extend(start_block + rng.between(10, 40));
    chain
}

/// One random operation on one node, then let it settle.
async fn random_operation(
    node: &mut Node,
    rng: &mut Rng,
    tally: &mut Tally,
    seed: u64,
) {
    let chain_id = node.options.chain_id;

    match rng.below(7) {
        0 | 1 => node.chain.extend(rng.between(1, 12)),
        2 => {
            let depth = rng.between(1, 16);
            let new_len = (depth + rng.between(0, 4)).saturating_sub(1);
            node.chain.reorg(depth, new_len.max(1));
        }
        3 => {
            let step =
                FAULT_STEPS[rng.below(FAULT_STEPS.len() as u64) as usize];
            inject(node, step, rng.chance(50));
            let depth = rng.between(1, 10);
            node.chain.reorg(depth, depth + 1);
        }
        4 => {
            node.chain.extend(rng.between(2, 10));
            node.writer.fail_next_flush(if rng.chance(50) {
                FlushFault::AfterChildren
            } else {
                FlushFault::AfterSomeBlocks
            });
        }
        5 => {
            node.chain.extend(rng.between(4, 12));
            node.chain
                .reorg_after_calls(rng.between(1, 6), rng.between(1, 4));
        }
        _ => node.restart(),
    }

    let faulty = node.store.fault_pending(chain_id);
    let restart = rng.chance(60);
    let head_before = node.data().live_blocks().into_keys().max();

    // A scripted reorg can fire on the very last request of a pass: keep
    // going until the chain stood still for a whole settle.
    loop {
        let head = node.chain.head();
        let calls = node.chain.calls();
        node.settle(restart).await.unwrap();
        if node.chain.head() == head || node.chain.calls() == calls {
            break;
        }
    }

    if faulty && !node.store.fault_pending(chain_id) {
        tally.faults_hit += 1;
    }
    tally.operations += 1;

    // Checkpoints are honest at every moment.
    assert_checkpoints_honest(&node.data(), "after an operation");

    // A reorg that did not make the chain longer than what was stored can
    // not be seen yet (no block names a parent we could compare): that is
    // only checked once the chain has grown. Everything else must be
    // clean NOW.
    if head_before.is_none_or(|head| head + 1 < node.target()) {
        assert_clean(
            node,
            &format!("seed {seed} chain {chain_id} after an operation"),
        );
        tally.settles_verified += 1;
    }
}

async fn final_check(node: &mut Node, tally: &mut Tally, context: &str) {
    node.store.clear_faults();
    node.chain.clear_script();
    node.chain.extend(node.options.confirmations + 30);
    node.settle(true).await.unwrap();
    assert_clean(node, context);

    tally.settles_verified += 1;
    tally.rollbacks += node.rollbacks.len() as u64;
    tally.gap_heals += node
        .data()
        .reorgs
        .iter()
        .filter(|r| r.reason == "gap_heal")
        .count() as u64;
}

#[tokio::test]
async fn random_histories_always_settle_to_a_clean_index() {
    let mut tally = Tally {
        scenarios: 0,
        operations: 0,
        settles_verified: 0,
        rollbacks: 0,
        gap_heals: 0,
        faults_hit: 0,
        lagged_reads: 0,
    };

    for seed in 1..=400u64 {
        let mut rng = Rng::new(seed);
        let options = random_options(&mut rng, CHAIN);
        let chain = random_chain(&mut rng, seed, options.start_block);
        // Half of the histories run on a store without read-your-writes.
        let store = if seed % 2 == 0 {
            FakeStore::with_lag(seed)
        } else {
            FakeStore::new()
        };
        let mut node = Node::new(options, chain, store);

        for _ in 0..rng.between(8, 16) {
            random_operation(&mut node, &mut rng, &mut tally, seed).await;
        }

        final_check(&mut node, &mut tally, &format!("seed {seed}")).await;
        tally.scenarios += 1;
        tally.lagged_reads += node.store.lagged_reads();
    }

    println!(
        "random histories: {} scenarios, {} operations, {} verified \
         settles, {} rollbacks, {} gap heals, {} injected purge faults \
         hit, {} stale reads served",
        tally.scenarios,
        tally.operations,
        tally.settles_verified,
        tally.rollbacks,
        tally.gap_heals,
        tally.faults_hit,
        tally.lagged_reads
    );

    // The generator must actually exercise what it claims to.
    assert!(tally.lagged_reads > 1_000);
    assert!(tally.rollbacks > 400);
    assert!(tally.gap_heals > 100);
    assert!(tally.faults_hit > 100);
}

#[tokio::test]
async fn chains_sharing_a_store_never_affect_each_other() {
    let mut tally = Tally {
        scenarios: 0,
        operations: 0,
        settles_verified: 0,
        rollbacks: 0,
        gap_heals: 0,
        faults_hit: 0,
        lagged_reads: 0,
    };

    for seed in 1..=60u64 {
        let mut rng = Rng::new(seed ^ 0xC0FFEE);
        let store = FakeStore::with_lag(seed);
        let mut nodes = Vec::new();

        for chain_id in [1u64, 10, 137, 8453] {
            let options = random_options(&mut rng, chain_id);
            let chain = random_chain(
                &mut rng,
                seed * 1_000 + chain_id,
                options.start_block,
            );
            nodes.push(Node::new(options, chain, store.clone()));
        }

        for _ in 0..40 {
            let active = rng.below(nodes.len() as u64) as usize;

            let before: Vec<_> = nodes.iter().map(Node::data).collect();
            random_operation(
                &mut nodes[active],
                &mut rng,
                &mut tally,
                seed,
            )
            .await;

            for (index, node) in nodes.iter().enumerate() {
                if index != active {
                    assert_eq!(
                        node.data(),
                        before[index],
                        "seed {seed}: chain {} changed while chain {} \
                         was working",
                        node.options.chain_id,
                        nodes[active].options.chain_id
                    );
                }
            }
        }

        for node in nodes.iter_mut() {
            let context =
                format!("seed {seed} chain {}", node.options.chain_id);
            final_check(node, &mut tally, &context).await;
        }
        tally.scenarios += 1;
    }

    println!(
        "shared store: {} scenarios x 4 chains, {} operations, {} verified \
         settles, {} rollbacks, {} gap heals",
        tally.scenarios,
        tally.operations,
        tally.settles_verified,
        tally.rollbacks,
        tally.gap_heals
    );
    assert!(tally.rollbacks > 100);
}

#[test]
fn the_store_trait_is_object_safe_and_errors_are_plain() {
    fn assert_object_safe(_: &dyn ReorgStore, _: &dyn CanonicalChain) {}
    let store = FakeStore::new();
    let chain = FakeChain::new(1, 10);
    assert_object_safe(store.as_ref(), chain.as_ref());

    fn assert_error<E: std::error::Error + Send + Sync + 'static>() {}
    assert_error::<ReorgError>();
}
