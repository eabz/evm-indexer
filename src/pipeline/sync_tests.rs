//! Sync loop tests with an in-memory block source and store: resume, gap
//! healing, live tail, `--end-block` exit and failure handling, without
//! HyperSync or ClickHouse.

use super::*;
use crate::{
    db::ranges::{assemble_missing_ranges, GapRow, RangeStats},
    reorg::{NoHooks, ReorgRecord, ReorgStore},
};
use alloy::primitives::B256;
use hypersync_client::{format::Hash, simple_types::Block};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::mpsc;

fn hash_of(number: u64) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&number.to_be_bytes());
    hash[31] = 1;
    hash
}

/// Stored blocks (number -> hash). Doubles as the writer's sink.
#[derive(Clone, Default)]
struct MemoryStore {
    blocks: Arc<Mutex<BTreeMap<u64, B256>>>,
    flushes: Arc<Mutex<Vec<usize>>>,
    fail: Arc<AtomicBool>,
    /// A purge that fails half way: the transient database error of
    /// `purge_stale_flushes`.
    fail_purge: Arc<AtomicBool>,
    purged: Arc<Mutex<Vec<(u64, Option<u64>)>>>,
}

impl MemoryStore {
    fn with_blocks(numbers: impl IntoIterator<Item = u64>) -> Self {
        let store = Self::default();
        for number in numbers {
            store
                .blocks
                .lock()
                .unwrap()
                .insert(number, B256::new(hash_of(number)));
        }
        store
    }

    fn numbers(&self) -> Vec<u64> {
        self.blocks.lock().unwrap().keys().copied().collect()
    }
}

impl Sink for MemoryStore {
    async fn store(&self, batch: &RowBatch) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            bail!("database is down");
        }
        let mut blocks = self.blocks.lock().unwrap();
        for block in &batch.blocks {
            blocks.insert(block.number, block.hash);
        }
        self.flushes.lock().unwrap().push(batch.blocks.len());
        Ok(())
    }
}

impl Progress for MemoryStore {
    /// Same contract as the SQL: stats + "gap before each stored block".
    async fn missing_ranges(
        &self,
        range: BlockRange,
    ) -> Result<MissingRanges> {
        let blocks = self.blocks.lock().unwrap();
        let stored: Vec<u64> =
            blocks.range(range.from..range.to).map(|(n, _)| *n).collect();

        let stats = RangeStats {
            indexed: stored.len() as u64,
            max_number: stored.last().copied().unwrap_or(0),
        };

        let mut gaps = Vec::new();
        let mut previous = range.from as i64 - 1;
        for number in stored {
            if number as i64 > previous + 1 {
                gaps.push(GapRow {
                    gap_start: previous + 1,
                    gap_end: number as i64,
                });
            }
            previous = number as i64;
        }

        Ok(assemble_missing_ranges(range, stats, &gaps, 10_000))
    }
}

/// The store as the reorg guard sees it: blocks only, never any orphan,
/// nothing to purge (the purge logic has its own in-memory model and
/// tests in `crate::reorg`).
impl ReorgStore for MemoryStore {
    fn current_epoch(&self, _: u64) -> BoxFuture<'_, Result<u32>> {
        Box::pin(async { Ok(0) })
    }

    fn stored_head(&self, _: u64) -> BoxFuture<'_, Result<Option<u64>>> {
        Box::pin(async {
            Ok(self.blocks.lock().unwrap().keys().next_back().copied())
        })
    }

    fn stored_hashes(
        &self,
        _: u64,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, Result<Vec<(u64, B256)>>> {
        Box::pin(async move {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .range(from..to)
                .map(|(number, hash)| (*number, *hash))
                .collect())
        })
    }

    fn has_orphan_children(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    fn live_side_rows(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn tombstone_side_rows(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
        _: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn timestamp_span(
        &self,
        _: u64,
        from: u64,
        to: Option<u64>,
    ) -> BoxFuture<'_, Result<Option<(u32, u32)>>> {
        Box::pin(async move {
            if self.fail_purge.load(Ordering::SeqCst) {
                bail!("database is down");
            }
            self.purged.lock().unwrap().push((from, to));
            Ok(None)
        })
    }

    fn live_children(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn live_blocks(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn live_checkpoints(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn tombstone_checkpoints(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
        _: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn tombstone_children(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
        _: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }

    fn insert_reorg<'a>(
        &'a self,
        _: &'a ReorgRecord,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn rebuild_derived(
        &self,
        _: u64,
        _: u32,
        _: u32,
        _: u32,
        _: u64,
        _: Option<u64>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn tombstone_blocks(
        &self,
        _: u64,
        _: u64,
        _: Option<u64>,
        _: u64,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async { Ok(0) })
    }
}

impl CanonicalChain for MockSource {
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, Result<Vec<BlockHeader>>> {
        Box::pin(async move {
            Ok((from..to)
                .map(|number| BlockHeader {
                    number,
                    hash: B256::new(hash_of(number)),
                    parent_hash: B256::new(hash_of(
                        number.wrapping_sub(1),
                    )),
                    timestamp: 0,
                })
                .collect())
        })
    }
}

/// Serves a linear chain in fixed size responses.
#[derive(Clone)]
struct MockSource {
    /// Successive answers of `head()`; the last one repeats forever.
    heads: Arc<Mutex<VecDeque<u64>>>,
    blocks_per_response: u64,
    requested: Arc<Mutex<Vec<BlockRange>>>,
    /// Fail the stream (once) when it reaches this block.
    fail_once_at: Arc<Mutex<Option<u64>>>,
    /// Leave this block out of its response (protocol violation).
    omit_block: Option<u64>,
}

impl MockSource {
    fn new(heads: &[u64]) -> Self {
        Self {
            heads: Arc::new(Mutex::new(heads.iter().copied().collect())),
            blocks_per_response: 7,
            requested: Default::default(),
            fail_once_at: Default::default(),
            omit_block: None,
        }
    }

    fn requested(&self) -> Vec<BlockRange> {
        self.requested.lock().unwrap().clone()
    }
}

impl BlockSource for MockSource {
    async fn head(&self) -> Result<u64> {
        let mut heads = self.heads.lock().unwrap();
        if heads.len() > 1 {
            Ok(heads.pop_front().unwrap())
        } else {
            Ok(*heads.front().unwrap())
        }
    }

    async fn stream(
        &self,
        range: BlockRange,
    ) -> Result<Receiver<Result<SourceResponse>>> {
        self.requested.lock().unwrap().push(range);

        let (tx, rx) = mpsc::channel(2);
        let source = self.clone();

        tokio::spawn(async move {
            let mut from = range.from;

            while from < range.to {
                let next_block =
                    (from + source.blocks_per_response).min(range.to);

                let fail = {
                    let mut fail_at = source.fail_once_at.lock().unwrap();
                    match *fail_at {
                        Some(at) if (from..next_block).contains(&at) => {
                            *fail_at = None;
                            true
                        }
                        _ => false,
                    }
                };

                if fail {
                    let _ = tx
                        .send(Err(anyhow::anyhow!("connection reset")))
                        .await;
                    return;
                }

                let blocks = (from..next_block)
                    .filter(|number| Some(*number) != source.omit_block)
                    .map(|number| Block {
                        number: Some(number),
                        hash: Some(Hash::from(hash_of(number))),
                        parent_hash: Some(Hash::from(hash_of(
                            number.wrapping_sub(1),
                        ))),
                        ..Default::default()
                    })
                    .collect();

                let response = SourceResponse {
                    next_block,
                    data: ResponseRows {
                        blocks: vec![blocks],
                        ..Default::default()
                    },
                    rollback_guard: None,
                };

                if tx.send(Ok(response)).await.is_err() {
                    return;
                }

                from = next_block;
            }
        });

        Ok(rx)
    }
}

async fn indexer(
    source: MockSource,
    store: MemoryStore,
    settings: SyncSettings,
) -> Indexer<MockSource, MemoryStore> {
    let writer = Writer::spawn(
        store.clone(),
        1_000_000,
        Duration::from_secs(3_600),
    );

    let gate = WriterGate {
        fence: crate::pipeline::lease::Fence::open(),
        writer: writer.handle(),
        visible: Box::new(|| Box::pin(async { Ok(()) })),
        adopt: Box::new(|_| {}),
    };

    let purger = Purger::new(
        Arc::new(store.clone()),
        Arc::new(gate),
        Arc::new(NoHooks),
        Arc::new(NoHooks),
    );

    Indexer {
        settings,
        progress: store,
        guard: ReorgGuard::new(
            ReorgConfig::new(settings.chain_id, settings.start_block),
            Arc::new(source.clone()),
            purger.clone(),
        ),
        purger,
        source,
        writer,
        modules: EnabledModules::default(),
        decode_state: DecodeState::default(),
        discovery: None,
        metrics: Metrics::disabled(),
        committed: Vec::new(),
        stale: Default::default(),
        fence: crate::pipeline::lease::Fence::open(),
        compacted: None,
    }
}

fn settings(start_block: u64, end_block: u64) -> SyncSettings {
    SyncSettings {
        chain_id: 1,
        start_block,
        end_block,
        confirmations: 0,
        new_blocks_only: false,
        tip_interval: Duration::ZERO,
    }
}

#[tokio::test(start_paused = true)]
async fn syncs_a_fresh_range_and_exits_at_end_block() {
    let source = MockSource::new(&[1_000]);
    let store = MemoryStore::default();

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(10, 60)).await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (10..60).collect::<Vec<_>>());
    assert_eq!(source.requested(), vec![BlockRange::new(10, 60)]);
}

#[tokio::test(start_paused = true)]
async fn heals_gaps_without_refetching_stored_blocks() {
    let source = MockSource::new(&[1_000]);
    let store = MemoryStore::with_blocks((10..20).chain(30..40));

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (0..50).collect::<Vec<_>>());
    assert_eq!(
        source.requested(),
        vec![
            BlockRange::new(0, 10),
            BlockRange::new(20, 30),
            BlockRange::new(40, 50),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_to_do_exits_immediately() {
    let source = MockSource::new(&[1_000]);
    let store = MemoryStore::with_blocks(0..50);

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    indexer.sync().await.unwrap();

    assert!(source.requested().is_empty());
}

#[tokio::test(start_paused = true)]
async fn follows_the_head_as_it_grows() {
    // Head moves 20 -> 23 -> 23 -> 31 -> 40.
    let source = MockSource::new(&[20, 23, 23, 31, 40]);
    let store = MemoryStore::default();

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 40)).await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (0..40).collect::<Vec<_>>());
    // Each pass only asks for the new blocks; nothing twice.
    assert_eq!(
        source.requested(),
        vec![
            BlockRange::new(0, 20),
            BlockRange::new(20, 23),
            BlockRange::new(23, 31),
            BlockRange::new(31, 40),
        ]
    );
    // One commit point (barrier flush) per pass.
    assert_eq!(*store.flushes.lock().unwrap(), vec![20, 3, 8, 9]);
}

#[tokio::test(start_paused = true)]
async fn new_blocks_only_starts_at_the_current_head() {
    let source = MockSource::new(&[100, 105, 110]);
    let store = MemoryStore::default();

    let mut indexer = indexer(
        source.clone(),
        store.clone(),
        SyncSettings {
            chain_id: 1,
            start_block: 0,
            end_block: 110,
            confirmations: 0,
            new_blocks_only: true,
            tip_interval: Duration::ZERO,
        },
    )
    .await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (100..110).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn a_failed_stream_is_retried_from_what_is_missing() {
    let source = MockSource::new(&[1_000]);
    *source.fail_once_at.lock().unwrap() = Some(25);
    let store = MemoryStore::default();

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (0..50).collect::<Vec<_>>());

    let requested = source.requested();
    assert_eq!(requested[0], BlockRange::new(0, 50));
    // The blocks delivered before the failure were kept: the retry
    // resumes after them instead of starting over.
    assert_eq!(requested[1], BlockRange::new(21, 50));
}

#[tokio::test(start_paused = true)]
async fn a_failed_flush_is_fatal() {
    let source = MockSource::new(&[1_000]);
    let store = MemoryStore::default();
    store.fail.store(true, Ordering::SeqCst);

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    assert!(indexer.sync().await.is_err());

    let error = indexer.writer.shutdown().await.unwrap_err();
    assert!(format!("{error:#}").contains("database is down"));
    assert!(store.numbers().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_response_with_a_missing_block_never_advances() {
    let mut source = MockSource::new(&[1_000]);
    source.omit_block = Some(12);
    let store = MemoryStore::default();

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 30)).await;

    // The loop retries forever (with backoff); observe it for a while.
    let outcome =
        tokio::time::timeout(Duration::from_secs(600), indexer.sync())
            .await;
    assert!(outcome.is_err(), "sync must not finish");

    indexer.writer.shutdown().await.unwrap();

    // Whole responses before the bad one are stored, the bad one is not.
    assert_eq!(store.numbers(), (0..7).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn confirmations_keep_the_tip_unindexed() {
    // Head stays at 100: with 10 confirmations only [0, 90) is final.
    let source = MockSource::new(&[100]);
    let store = MemoryStore::default();

    let mut indexer = indexer(
        source.clone(),
        store.clone(),
        SyncSettings { confirmations: 10, ..settings(0, 0) },
    )
    .await;

    let outcome =
        tokio::time::timeout(Duration::from_secs(120), indexer.sync())
            .await;
    assert!(outcome.is_err(), "following the head never finishes");

    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (0..90).collect::<Vec<_>>());
    assert_eq!(source.requested(), vec![BlockRange::new(0, 90)]);
}

#[tokio::test(start_paused = true)]
async fn confirmed_blocks_are_indexed_as_the_head_moves() {
    let source = MockSource::new(&[100, 104, 130]);
    let store = MemoryStore::default();

    let mut indexer = indexer(
        source.clone(),
        store.clone(),
        SyncSettings { confirmations: 10, ..settings(80, 120) },
    )
    .await;

    // --end-block 120 is reached once the head is at 130.
    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (80..120).collect::<Vec<_>>());
    assert_eq!(
        source.requested(),
        vec![
            BlockRange::new(80, 90),
            BlockRange::new(90, 94),
            BlockRange::new(94, 120),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn new_blocks_only_with_confirmations_starts_at_the_confirmed_head()
{
    let source = MockSource::new(&[100, 120]);
    let store = MemoryStore::default();

    let mut indexer = indexer(
        source.clone(),
        store.clone(),
        SyncSettings {
            confirmations: 10,
            new_blocks_only: true,
            ..settings(0, 105)
        },
    )
    .await;

    indexer.sync().await.unwrap();
    indexer.writer.shutdown().await.unwrap();

    assert_eq!(store.numbers(), (90..105).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn a_dead_writer_is_fatal_even_when_the_stream_failed_too() {
    let source = MockSource::new(&[1_000]);
    *source.fail_once_at.lock().unwrap() = Some(25);
    let store = MemoryStore::default();
    store.fail.store(true, Ordering::SeqCst);

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    // Must return on the first pass instead of backing off and retrying.
    let error = indexer.sync().await.unwrap_err();
    assert!(WriterStopped::is_cause_of(&error));
    assert_eq!(source.requested().len(), 1);
}

/// The queue of flushes that raced another process's purge is the ONLY
/// record that those blocks have to be indexed again - their rows are
/// stored, so no gap query ever asks for them. A purge that fails must
/// therefore leave the queue alone (docs/review-round-4.md, MAJOR 3): the
/// old code drained it into a local `Vec` and lost the failed span AND
/// every span after it on the first transient error.
#[tokio::test(start_paused = true)]
async fn a_failed_purge_keeps_the_stale_flush_spans() {
    let source = MockSource::new(&[1_000]);
    let store = MemoryStore::with_blocks(0..50);

    let mut indexer =
        indexer(source.clone(), store.clone(), settings(0, 50)).await;

    let queued =
        vec![BlockRange::new(10, 20), BlockRange::new(30, 40)];
    *indexer.stale.lock().unwrap() = queued.clone();

    // The first purge fails: nothing may be forgotten.
    store.fail_purge.store(true, Ordering::SeqCst);
    assert!(indexer.purge_stale_flushes().await.is_err());
    assert_eq!(*indexer.stale.lock().unwrap(), queued);

    // The database comes back: both spans are purged, and only then are
    // they taken out of the queue.
    store.fail_purge.store(false, Ordering::SeqCst);
    assert_eq!(indexer.purge_stale_flushes().await.unwrap(), Some(10));
    assert!(indexer.stale.lock().unwrap().is_empty());

    let mut purged: Vec<(u64, Option<u64>)> =
        store.purged.lock().unwrap().clone();
    purged.dedup();
    assert_eq!(purged, vec![(10, Some(20)), (30, Some(40))]);
}
