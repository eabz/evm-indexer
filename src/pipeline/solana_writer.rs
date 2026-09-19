//! Writer task and commit protocol of the Solana pipeline.
//!
//! Identical in spirit to `pipeline::writer` + `Database::store`, with the
//! four things that make a commit a commit:
//!
//! 1. **One `_version` and one `epoch` per flush**, stamped on every row
//!    (`SvmRows::set_version` / `set_epoch`, which phase 1 already wrote).
//! 2. **Children first, `sol_slots` LAST.** A slot row exists only once all
//!    of its data is durable, which is what makes the commit marker a
//!    commit marker and what the gap heal relies on. `checkpoints` follows
//!    the marker: it is an index, the marker decides.
//! 3. **Synchronous inserts with the deterministic dedup token** of
//!    `db::FlushKey` (table, chain, slot span, `_version`), so a retried
//!    insert that WAS applied is dropped by the server - in the table AND
//!    in everything its materialized views feed.
//! 4. **Fenced.** The lease is asked before anything is written: a process
//!    that lost the chain, or whose own heartbeats lapsed, must not write.
//!
//! The one Solana-specific part is the checkpoint: `to_block` is the
//! server's `next_slot`, NEVER `max(slot) + 1`. Skipped slots are normal,
//! so "the slots we asked for and were served" is a property of the CURSOR
//! and the only thing that can tell a skipped slot from an unasked one
//! (docs/solana-research.md §11.4.3, witness 1).

use crate::{
    db::{next_version, ranges::BlockRange, Database, FlushKey},
    metrics::Metrics,
    pipeline::{
        lease::Fence,
        solana_store::{COMMIT_MARKER, SOL_TOKEN_BALANCES},
    },
    svm::SvmRows,
};
use anyhow::{Context, Result};
use log::{error, info, warn};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

/// Batches in flight between the decoder and the writer.
pub const CHANNEL_CAPACITY: usize = 4;

/// Decoded rows plus the windows they were served in.
#[derive(Debug, Default)]
pub struct SvmBatch {
    pub rows: SvmRows,
    /// Half open `[from_slot, next_slot)` windows, as the SERVER cursor
    /// reported them. This is what a checkpoint records.
    pub windows: Vec<BlockRange>,
}

impl SvmBatch {
    pub fn rows(&self) -> usize {
        self.rows.rows()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.windows.is_empty()
    }

    pub fn append(&mut self, other: &mut SvmBatch) {
        self.rows.append(&mut other.rows);
        self.windows.append(&mut other.windows);
    }

    /// Lowest and highest SLOT WITH A ROW in the batch. `None` when every
    /// served slot was skipped or matched nothing, which is normal and is
    /// exactly why the checkpoint uses [`Self::covered`] instead.
    pub fn slot_span(&self) -> Option<(u64, u64)> {
        let min = self.rows.slots.iter().map(|s| s.block_number).min()?;
        let max = self.rows.slots.iter().map(|s| s.block_number).max()?;
        Some((min, max))
    }

    /// The windows merged into contiguous ranges, ascending: the
    /// checkpoint rows this flush writes.
    pub fn covered(&self) -> Vec<BlockRange> {
        merge_ranges(&self.windows)
    }

    /// Slot span for the dedup token. A window is always present (the
    /// writer never queues an empty batch), so this never has to invent
    /// one.
    fn token_span(&self) -> Option<(u64, u64)> {
        let covered = self.covered();
        let first = covered.first()?;
        let last = covered.last()?;
        Some((first.from, last.to))
    }

    pub fn version(&self) -> u64 {
        self.rows.slots.first().map(|s| s._version).unwrap_or(0)
    }

    pub fn epoch(&self) -> u32 {
        self.rows.slots.first().map(|s| s.epoch).unwrap_or(0)
    }
}

/// Merges half open ranges into the smallest ascending, non touching set.
pub fn merge_ranges(ranges: &[BlockRange]) -> Vec<BlockRange> {
    let mut sorted: Vec<BlockRange> =
        ranges.iter().copied().filter(|r| !r.is_empty()).collect();
    sorted.sort_unstable_by_key(|r| (r.from, r.to));

    let mut merged: Vec<BlockRange> = Vec::with_capacity(sorted.len());
    for range in sorted {
        match merged.last_mut() {
            Some(last) if range.from <= last.to => {
                last.to = last.to.max(range.to)
            }
            _ => merged.push(range),
        }
    }
    merged
}

/// Destination of a flush. `store` returns `Ok` only once the whole batch
/// is durable, marker last; an error is final and stops the indexer.
pub trait SvmSink: Send + Sync + 'static {
    /// The chain's purge generation to stamp on the rows about to be
    /// written. Read once per flush, right before [`SvmSink::store`].
    fn epoch(&self) -> impl Future<Output = Result<u32>> + Send {
        async { Ok(0) }
    }

    fn store(
        &self,
        batch: &SvmBatch,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// The writer task is gone because a flush failed for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SvmWriterStopped;

impl std::fmt::Display for SvmWriterStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Solana writer stopped after a failed flush")
    }
}

impl std::error::Error for SvmWriterStopped {}

impl SvmWriterStopped {
    pub fn is_cause_of(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| cause.is::<SvmWriterStopped>())
    }
}

enum Message {
    Rows(Box<SvmBatch>),
    Barrier(oneshot::Sender<()>),
    Stop,
}

pub struct SvmWriter {
    handle: SvmWriterHandle,
    task: JoinHandle<Result<()>>,
}

#[derive(Clone)]
pub struct SvmWriterHandle {
    tx: mpsc::Sender<Message>,
}

impl SvmWriter {
    pub fn spawn<S: SvmSink>(
        sink: S,
        flush_rows: usize,
        flush_interval: Duration,
        metrics: Metrics,
    ) -> Self {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let task = tokio::spawn(run(
            sink,
            rx,
            flush_rows,
            flush_interval,
            metrics,
        ));
        Self { handle: SvmWriterHandle { tx }, task }
    }

    pub fn handle(&self) -> SvmWriterHandle {
        self.handle.clone()
    }

    pub async fn send(&self, batch: SvmBatch) -> Result<()> {
        self.handle.send(batch).await
    }

    pub async fn barrier(&self) -> Result<()> {
        self.handle.barrier().await
    }

    /// Flushes what is left and returns the writer's final result.
    pub async fn shutdown(self) -> Result<()> {
        let _ = self.handle.tx.send(Message::Stop).await;
        drop(self.handle);
        self.task.await.context("Solana writer task panicked")?
    }

    /// Stops WITHOUT the final flush: whatever is buffered is dropped.
    ///
    /// Used on one path only, the continuity tripwire. There the whole
    /// point is that nothing further reaches the database - the buffered
    /// slots are below the break and would be perfectly valid, but an
    /// operator arriving at a stopped indexer should find the data exactly
    /// as the last COMPLETED commit left it, not one more flush along. The
    /// dropped slots are simply streamed again after the cause is fixed.
    pub async fn abort(self) {
        drop(self.handle);
        self.task.abort();
        let _ = self.task.await;
    }
}

impl SvmWriterHandle {
    pub async fn send(&self, batch: SvmBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        self.tx
            .send(Message::Rows(Box::new(batch)))
            .await
            .map_err(|_| SvmWriterStopped.into())
    }

    /// Returns once everything queued so far is durably stored.
    pub async fn barrier(&self) -> Result<()> {
        let (ack, done) = oneshot::channel();

        self.tx
            .send(Message::Barrier(ack))
            .await
            .map_err(|_| SvmWriterStopped)?;

        done.await.map_err(|_| SvmWriterStopped.into())
    }
}

async fn run<S: SvmSink>(
    sink: S,
    mut rx: mpsc::Receiver<Message>,
    flush_rows: usize,
    flush_interval: Duration,
    metrics: Metrics,
) -> Result<()> {
    let mut buffer = SvmBatch::default();
    let mut deadline: Option<Instant> = None;

    loop {
        let message = match deadline {
            Some(deadline) => tokio::select! {
                message = rx.recv() => Some(message),
                _ = tokio::time::sleep_until(deadline) => None,
            },
            None => Some(rx.recv().await),
        };

        match message {
            None => {
                flush(&sink, &mut buffer, &mut deadline, &metrics).await?
            }
            Some(None) | Some(Some(Message::Stop)) => {
                return flush(&sink, &mut buffer, &mut deadline, &metrics)
                    .await;
            }
            Some(Some(Message::Rows(mut batch))) => {
                buffer.append(&mut batch);

                if deadline.is_none() && !buffer.is_empty() {
                    deadline = Some(Instant::now() + flush_interval);
                }

                if buffer.rows() >= flush_rows {
                    flush(&sink, &mut buffer, &mut deadline, &metrics)
                        .await?;
                }
            }
            Some(Some(Message::Barrier(ack))) => {
                flush(&sink, &mut buffer, &mut deadline, &metrics).await?;
                let _ = ack.send(());
            }
        }
    }
}

async fn flush<S: SvmSink>(
    sink: &S,
    buffer: &mut SvmBatch,
    deadline: &mut Option<Instant>,
    metrics: &Metrics,
) -> Result<()> {
    *deadline = None;

    if buffer.is_empty() {
        return Ok(());
    }

    let mut batch = std::mem::take(buffer);
    let rows = batch.rows() as u64;
    let started = Instant::now();
    metrics.flush_started();

    let stored = async {
        batch.rows.set_version(next_version());
        batch
            .rows
            .set_epoch(sink.epoch().await.context("read the epoch")?);
        sink.store(&batch).await
    }
    .await;

    metrics.flush_observed(started.elapsed(), rows, stored.is_ok());

    if let Err(e) = stored {
        error!("Solana flush failed, stopping: {e:#}");
        return Err(e);
    }

    if let Some(last) =
        batch.rows.slots.iter().max_by_key(|s| s.block_number)
    {
        metrics.set_indexed_height(last.block_number);
        metrics.set_indexed_timestamp(u64::from(last.timestamp));
    }

    let covered = batch.covered();
    let span = covered
        .first()
        .zip(covered.last())
        .map(|(first, last)| format!("[{}, {})", first.from, last.to))
        .unwrap_or_else(|| "-".to_string());

    let requested: u64 = covered.iter().map(BlockRange::len).sum();
    let present = batch.rows.slots.len() as u64;

    let pads = &batch.rows.launchpads;

    info!(
        "Stored slots {span}: {present} slot(s) of {requested} \
         ({} skipped), transactions ({}) swaps ({}) mints ({}) \
         launches ({}) curve trades ({}) graduations ({}) \
         balances ({}) in {:?}.",
        requested.saturating_sub(present),
        batch.rows.transactions.len(),
        batch.rows.swaps.len(),
        batch.rows.tokens.len(),
        pads.tokens.len(),
        pads.trades.len(),
        pads.graduations.len(),
        pads.balances.len(),
        started.elapsed(),
    );

    Ok(())
}

// --------------------------------------------------- the ClickHouse sink

/// The last flush: highest stored slot and the flush `_version`.
pub type LastFlush = Arc<Mutex<Option<(u64, u64)>>>;

pub struct ClickhouseSvmSink {
    pub db: Database,
    /// Asked before every flush: this process must not write once it lost
    /// the chain's lease or its own heartbeats stalled past the ttl.
    pub fence: Fence,
    pub last_flush: LastFlush,
    /// Slot windows flushed with an epoch that was superseded WHILE the
    /// flush ran: the validity rule may hide their candle contributions,
    /// so the sync loop purges and streams them again.
    pub stale: Arc<Mutex<Vec<BlockRange>>>,
}

impl SvmSink for ClickhouseSvmSink {
    async fn epoch(&self) -> Result<u32> {
        self.db.refresh_epoch().await
    }

    async fn store(&self, batch: &SvmBatch) -> Result<()> {
        self.fence.check()?;

        store_batch(&self.db, batch).await?;

        if let Some(last) =
            batch.rows.slots.iter().max_by_key(|s| s.block_number)
        {
            *self.last_flush.lock().unwrap() =
                Some((last.block_number, batch.version()));
        }

        match self.db.refresh_epoch().await {
            Ok(epoch) if epoch > batch.epoch() => {
                for range in batch.covered() {
                    warn!(
                        "Chain {}: the epoch moved from {} to {epoch} \
                         while slots {range} were flushed; they will be \
                         purged and indexed again.",
                        self.db.chain_id,
                        batch.epoch()
                    );
                    self.stale.lock().unwrap().push(range);
                }
            }
            Ok(_) => {}
            Err(e) => {
                log::debug!("Epoch re-read after the flush: {e:#}")
            }
        }

        Ok(())
    }
}

/// The commit protocol, in order. Public so the acceptance tests can stop
/// it half way and prove the heal.
pub async fn store_batch(db: &Database, batch: &SvmBatch) -> Result<()> {
    store_children(db, batch).await?;
    store_marker_and_checkpoints(db, batch).await
}

fn flush_key(db: &Database, batch: &SvmBatch) -> Option<FlushKey> {
    batch.token_span().map(|span| FlushKey {
        chain: db.chain_id,
        span,
        version: batch.version(),
    })
}

/// Step 1: everything except the commit marker, concurrently.
///
/// Three families, and they all go BEFORE `sol_slots`:
///
/// * the `sol_*` DEX rows;
/// * the SHARED `launchpad_*` rows - the very same chain-neutral tables the
///   EVM launchpad decoder writes, with 32-byte Solana ids - plus the
///   Solana-only `sol_token_balances`;
/// * `sol_tokens` and `sol_launchpad_configs`, which are NOT block scoped.
///   They are in the flush anyway: a mint row without its swap is
///   harmless, while a swap whose mint has no decimals cannot be valued,
///   and a curve trade whose config is missing cannot be priced.
pub async fn store_children(
    db: &Database,
    batch: &SvmBatch,
) -> Result<()> {
    let Some(key) = flush_key(db, batch) else {
        return Ok(());
    };

    let pads = &batch.rows.launchpads;

    // One `tokio::join!` so a flush is one round of concurrent inserts,
    // exactly like the EVM `Database::store`.
    let results = tokio::join!(
        db.insert_flush("sol_dex_swaps", &batch.rows.swaps, &key),
        db.insert_flush(
            "sol_transactions",
            &batch.rows.transactions,
            &key
        ),
        db.insert_flush("sol_tokens", &batch.rows.tokens, &key),
        db.insert_flush("launchpad_tokens", &pads.tokens, &key),
        db.insert_flush("launchpad_trades", &pads.trades, &key),
        db.insert_flush("launchpad_graduations", &pads.graduations, &key),
        db.insert_flush(
            "launchpad_creator_fees",
            &pads.creator_fees,
            &key
        ),
        db.insert_flush("sol_launchpad_configs", &pads.configs, &key),
        db.insert_flush(SOL_TOKEN_BALANCES, &pads.balances, &key),
    );

    let (r0, r1, r2, r3, r4, r5, r6, r7, r8) = results;
    let failures: Vec<String> = [r0, r1, r2, r3, r4, r5, r6, r7, r8]
        .into_iter()
        .filter_map(|r| r.err())
        .map(|e| format!("{e:#}"))
        .collect();

    if !failures.is_empty() {
        anyhow::bail!(
            "failed to store the Solana batch: {}",
            failures.join("; ")
        );
    }

    Ok(())
}

/// Step 2: the commit marker, then the checkpoints.
pub async fn store_marker_and_checkpoints(
    db: &Database,
    batch: &SvmBatch,
) -> Result<()> {
    let Some(key) = flush_key(db, batch) else {
        return Ok(());
    };

    db.insert_flush(COMMIT_MARKER, &batch.rows.slots, &key).await?;

    // `to_block` is the SERVER's cursor, not `max(slot) + 1`: inside one
    // served window a missing integer is a skipped slot, so only the
    // cursor can say which slots were asked for at all.
    let checkpoints: Vec<crate::db::ranges::DatabaseCheckpoint> = batch
        .covered()
        .into_iter()
        .map(|range| crate::db::ranges::DatabaseCheckpoint {
            chain: db.chain_id,
            from_block: range.from,
            to_block: range.to,
            epoch: batch.epoch(),
            _version: key.version,
        })
        .collect();

    db.insert_flush("checkpoints", &checkpoints, &key).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svm::models::SolSlot;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn slot(number: u64) -> SolSlot {
        SolSlot {
            chain: 1,
            block_number: number,
            blockhash: [0u8; 32],
            parent_slot: number.saturating_sub(1),
            parent_blockhash: [0u8; 32],
            block_height: number,
            timestamp: 1_700_000_000,
            epoch: 0,
            _version: 0,
            is_deleted: 0,
        }
    }

    fn batch(from: u64, to: u64, slots: &[u64]) -> SvmBatch {
        let mut rows = SvmRows::default();
        for number in slots {
            rows.slots.push(slot(*number));
        }
        SvmBatch { rows, windows: vec![BlockRange::new(from, to)] }
    }

    /// One recorded flush: the row count and the checkpoint windows it
    /// would write.
    type Flush = (usize, Vec<(u64, u64)>);

    #[derive(Clone, Default)]
    struct MockSink {
        flushes: Arc<Mutex<Vec<Flush>>>,
        fail: Arc<AtomicBool>,
    }

    impl SvmSink for MockSink {
        async fn store(&self, batch: &SvmBatch) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("database is down");
            }
            self.flushes.lock().unwrap().push((
                batch.rows(),
                batch.covered().iter().map(|r| (r.from, r.to)).collect(),
            ));
            Ok(())
        }
    }

    const LONG: Duration = Duration::from_secs(3_600);

    #[test]
    fn a_window_with_only_skipped_slots_still_produces_a_checkpoint() {
        // The server served [100, 140) and every slot in it was skipped.
        // The checkpoint must still claim the window, or the range would
        // be re-asked for ever.
        let batch = batch(100, 140, &[]);
        assert_eq!(batch.slot_span(), None);
        assert_eq!(batch.covered(), vec![BlockRange::new(100, 140)]);
        assert_eq!(batch.token_span(), Some((100, 140)));
        assert!(!batch.is_empty(), "a served window is not nothing");
    }

    #[test]
    fn touching_windows_merge_into_one_checkpoint() {
        let mut batch = batch(100, 140, &[100, 101]);
        let mut next = batch2(140, 180, &[150]);
        batch.append(&mut next);

        assert_eq!(batch.covered(), vec![BlockRange::new(100, 180)]);
        // The dedup token spans the whole flush.
        assert_eq!(batch.token_span(), Some((100, 180)));
    }

    fn batch2(from: u64, to: u64, slots: &[u64]) -> SvmBatch {
        batch(from, to, slots)
    }

    #[test]
    fn a_hole_between_windows_stays_two_checkpoints() {
        // Two gap ranges healed in one pass: the checkpoint tiling must
        // NOT claim the slots between them.
        let mut batch = batch(100, 140, &[100]);
        let mut other = batch2(200, 240, &[200]);
        batch.append(&mut other);

        assert_eq!(
            batch.covered(),
            vec![BlockRange::new(100, 140), BlockRange::new(200, 240)]
        );
    }

    #[test]
    fn merging_is_ascending_and_drops_empty_ranges() {
        let merged = merge_ranges(&[
            BlockRange::new(30, 40),
            BlockRange::new(5, 5),
            BlockRange::new(10, 20),
            BlockRange::new(15, 32),
        ]);
        assert_eq!(merged, vec![BlockRange::new(10, 40)]);
        assert!(merge_ranges(&[]).is_empty());
    }

    #[tokio::test]
    async fn flushes_when_the_row_threshold_is_reached() {
        let sink = MockSink::default();
        let writer =
            SvmWriter::spawn(sink.clone(), 3, LONG, Metrics::disabled());

        writer.send(batch(0, 10, &[0, 1])).await.unwrap();
        writer.send(batch(10, 20, &[10, 11])).await.unwrap();
        writer.barrier().await.unwrap();

        let flushes = sink.flushes.lock().unwrap().clone();
        assert_eq!(flushes.len(), 1, "{flushes:?}");
        assert_eq!(flushes[0].1, vec![(0, 20)]);

        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_the_remainder() {
        let sink = MockSink::default();
        let writer = SvmWriter::spawn(
            sink.clone(),
            1_000_000,
            LONG,
            Metrics::disabled(),
        );

        writer.send(batch(0, 10, &[0])).await.unwrap();
        writer.shutdown().await.unwrap();

        assert_eq!(sink.flushes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_flush_stops_the_writer_for_good() {
        let sink = MockSink::default();
        let writer = SvmWriter::spawn(
            sink.clone(),
            1_000_000,
            LONG,
            Metrics::disabled(),
        );

        sink.fail.store(true, Ordering::SeqCst);
        writer.send(batch(0, 10, &[0])).await.unwrap();
        let error = writer.barrier().await.unwrap_err();
        assert!(SvmWriterStopped::is_cause_of(&error));

        // Even if the database recovers, nothing is written past the
        // failed flush.
        sink.fail.store(false, Ordering::SeqCst);
        let error = writer.send(batch(10, 20, &[10])).await.unwrap_err();
        assert!(SvmWriterStopped::is_cause_of(&error));

        let error = writer.shutdown().await.unwrap_err();
        assert!(format!("{error:#}").contains("database is down"));
        assert!(sink.flushes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn one_version_and_one_epoch_per_flush() {
        #[derive(Clone, Default)]
        struct Recorder(Arc<Mutex<Vec<(u64, u32)>>>);

        impl SvmSink for Recorder {
            async fn epoch(&self) -> Result<u32> {
                Ok(7)
            }

            async fn store(&self, batch: &SvmBatch) -> Result<()> {
                // Every row, of every table, carries the same stamps.
                for s in &batch.rows.slots {
                    self.0.lock().unwrap().push((s._version, s.epoch));
                }
                Ok(())
            }
        }

        let sink = Recorder::default();
        let writer = SvmWriter::spawn(
            sink.clone(),
            1_000_000,
            LONG,
            Metrics::disabled(),
        );

        writer.send(batch(0, 10, &[0, 1, 2])).await.unwrap();
        writer.shutdown().await.unwrap();

        let stamps = sink.0.lock().unwrap().clone();
        assert_eq!(stamps.len(), 3);
        assert!(stamps.iter().all(|stamp| *stamp == stamps[0]));
        assert_eq!(stamps[0].1, 7);
        assert!(stamps[0].0 > 0);
    }
}
