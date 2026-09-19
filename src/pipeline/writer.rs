//! Writer task: accumulates rows and flushes them in big batches.
//!
//! A flush happens when the buffer holds `flush_rows` rows, when the oldest
//! buffered row is `flush_interval` old, or when a barrier is requested
//! (end of a streamed pass / shutdown). The channel feeding the writer is
//! bounded, so a slow database slows the HyperSync stream down instead of
//! growing memory.

use crate::{core::RowBatch, db::next_version, metrics::Metrics};
use anyhow::{Context, Result};
use log::{error, info};
use std::{future::Future, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::Instant,
};

/// Batches in flight between the transformer and the writer.
pub const CHANNEL_CAPACITY: usize = 4;

/// Destination of the flushed batches. `store` must only return `Ok` once
/// the whole batch is durable (block rows last), and must do its own
/// retries: an error is final and stops the indexer.
pub trait Sink: Send + Sync + 'static {
    /// The chain's purge generation (docs/design.md, section 2) to stamp
    /// on the rows of the flush that is about to happen. Called once per
    /// flush, right before [`Sink::store`].
    fn epoch(&self) -> impl Future<Output = Result<u32>> + Send {
        async { Ok(0) }
    }

    fn store(
        &self,
        batch: &RowBatch,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// The writer task is gone because a flush failed for good. Fatal: the
/// root cause is returned by [`Writer::shutdown`].
///
/// A distinct type (instead of probing the channel) because the barrier
/// acknowledgement is dropped slightly BEFORE the channel closes: checking
/// `is_closed()` right after a failed barrier can still say "open".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterStopped;

impl std::fmt::Display for WriterStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("writer stopped after a failed flush")
    }
}

impl std::error::Error for WriterStopped {}

impl WriterStopped {
    /// True when `error` (or anything in its chain) is a `WriterStopped`.
    pub fn is_cause_of(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| cause.is::<WriterStopped>())
    }
}

enum Message {
    Rows(Box<RowBatch>),
    Barrier(oneshot::Sender<()>),
    /// Final flush, then the task ends (handles may still exist).
    Stop,
}

pub struct Writer {
    handle: WriterHandle,
    task: JoinHandle<Result<()>>,
}

/// Sending side of the writer. Cheap to clone; the reorg logic holds one
/// to quiesce the writer before a purge.
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<Message>,
    metrics: Metrics,
}

impl Writer {
    pub fn spawn<S: Sink>(
        sink: S,
        flush_rows: usize,
        flush_interval: Duration,
    ) -> Self {
        Self::spawn_with_metrics(
            sink,
            flush_rows,
            flush_interval,
            Metrics::disabled(),
        )
    }

    pub fn spawn_with_metrics<S: Sink>(
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
            metrics.clone(),
        ));
        Self { handle: WriterHandle { tx, metrics }, task }
    }

    pub fn handle(&self) -> WriterHandle {
        self.handle.clone()
    }

    /// See [`WriterHandle::send`].
    pub async fn send(&self, rows: RowBatch) -> Result<()> {
        self.handle.send(rows).await
    }

    /// See [`WriterHandle::barrier`].
    pub async fn barrier(&self) -> Result<()> {
        self.handle.barrier().await
    }

    /// Flushes what is left and returns the writer's final result (the
    /// flush error if it stopped early).
    pub async fn shutdown(self) -> Result<()> {
        // Ignored on purpose: a closed channel means the task already
        // ended, and its result is what is returned below.
        let _ = self.handle.tx.send(Message::Stop).await;
        drop(self.handle);
        self.task.await.context("writer task panicked")?
    }
}

impl WriterHandle {
    /// Queues rows (whole blocks). Waits while the writer is busy. Fails
    /// when the writer stopped because a flush failed.
    pub async fn send(&self, rows: RowBatch) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let sent = self
            .tx
            .send(Message::Rows(Box::new(rows)))
            .await
            .map_err(|_| WriterStopped.into());

        self.metrics.channel_fill(
            CHANNEL_CAPACITY - self.tx.capacity(),
            CHANNEL_CAPACITY,
        );

        sent
    }

    /// Returns once everything queued so far is durably stored.
    pub async fn barrier(&self) -> Result<()> {
        let (ack, done) = oneshot::channel();

        self.tx
            .send(Message::Barrier(ack))
            .await
            .map_err(|_| WriterStopped)?;

        // The acknowledgement is only ever dropped unanswered when the
        // flush failed and the writer task is returning its error.
        done.await.map_err(|_| WriterStopped.into())
    }
}

async fn run<S: Sink>(
    sink: S,
    mut rx: mpsc::Receiver<Message>,
    flush_rows: usize,
    flush_interval: Duration,
    metrics: Metrics,
) -> Result<()> {
    let mut buffer = RowBatch::default();
    // When the oldest buffered row has to be flushed.
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
            // Interval elapsed.
            None => {
                flush(&sink, &mut buffer, &mut deadline, &metrics).await?
            }
            // Shutdown, or every sender is gone: final flush.
            Some(None) | Some(Some(Message::Stop)) => {
                return flush(&sink, &mut buffer, &mut deadline, &metrics)
                    .await;
            }
            Some(Some(Message::Rows(mut rows))) => {
                buffer.append(&mut rows);

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
                // The requester may have gone away; nothing to do then.
                let _ = ack.send(());
            }
        }
    }
}

/// Moves the buffer into the sink. On failure the error is returned and
/// the writer stops: nothing after a failed flush is ever written.
async fn flush<S: Sink>(
    sink: &S,
    buffer: &mut RowBatch,
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
        // One `_version` and one `epoch` per flush: a re-inserted block
        // replaces itself, and the aggregates file every contribution
        // under the purge generation it was written in.
        batch.set_version(next_version());
        batch.set_epoch(sink.epoch().await.context("read the epoch")?);
        sink.store(&batch).await
    }
    .await;

    metrics.flush_observed(started.elapsed(), rows, stored.is_ok());

    if let Err(e) = stored {
        error!("Flush failed, stopping: {e:#}");
        return Err(e);
    }

    if let Some(last) = batch.blocks.iter().max_by_key(|b| b.number) {
        metrics.set_indexed_height(last.number);
        metrics.set_indexed_timestamp(u64::from(last.timestamp));
    }

    let span = batch
        .block_span()
        .map(|(min, max)| format!("{min}..={max}"))
        .unwrap_or_else(|| "-".to_string());

    let modules: String = batch
        .modules
        .counts()
        .into_iter()
        .filter(|(_, rows)| *rows > 0)
        .map(|(table, rows)| format!(" {table} ({rows})"))
        .collect();

    info!(
        "Stored {} blocks ({span}): transactions ({}) logs ({}) \
         withdrawals ({}) erc20 ({}) erc721 ({}) erc1155 ({}){modules} \
         in {:?}.",
        batch.blocks.len(),
        batch.transactions.len(),
        batch.logs.len(),
        batch.withdrawals.len(),
        batch.erc20_transfers.len(),
        batch.erc721_transfers.len(),
        batch.erc1155_transfers.len(),
        started.elapsed(),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::log::test_support::log_with;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    /// Records the row count of every flush; can be switched to failing.
    #[derive(Clone, Default)]
    struct MockSink {
        flushes: Arc<Mutex<Vec<usize>>>,
        fail: Arc<AtomicBool>,
    }

    impl MockSink {
        fn flushes(&self) -> Vec<usize> {
            self.flushes.lock().unwrap().clone()
        }
    }

    impl Sink for MockSink {
        async fn store(&self, batch: &RowBatch) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                anyhow::bail!("database is down");
            }
            self.flushes.lock().unwrap().push(batch.rows());
            Ok(())
        }
    }

    fn rows(count: usize) -> RowBatch {
        let mut batch = RowBatch::default();
        for _ in 0..count {
            batch.logs.push(log_with(&[], vec![]));
        }
        batch
    }

    const LONG: Duration = Duration::from_secs(3_600);

    #[tokio::test]
    async fn flushes_when_the_row_threshold_is_reached() {
        let sink = MockSink::default();
        let writer = Writer::spawn(sink.clone(), 10, LONG);

        writer.send(rows(4)).await.unwrap();
        writer.send(rows(4)).await.unwrap();
        writer.send(rows(4)).await.unwrap(); // 12 >= 10 -> flush
        writer.send(rows(3)).await.unwrap();
        writer.barrier().await.unwrap();

        assert_eq!(sink.flushes(), vec![12, 3]);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_when_the_interval_elapses() {
        let sink = MockSink::default();
        let writer = Writer::spawn(
            sink.clone(),
            1_000_000,
            Duration::from_millis(500),
        );

        writer.send(rows(2)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(sink.flushes().is_empty());

        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(sink.flushes(), vec![2]);

        // Nothing buffered: the timer must not produce empty flushes.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(sink.flushes(), vec![2]);

        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn barrier_flushes_and_waits() {
        let sink = MockSink::default();
        let writer = Writer::spawn(sink.clone(), 1_000_000, LONG);

        writer.send(rows(5)).await.unwrap();
        writer.barrier().await.unwrap();
        assert_eq!(sink.flushes(), vec![5]);

        // A barrier with nothing buffered is a no-op.
        writer.barrier().await.unwrap();
        assert_eq!(sink.flushes(), vec![5]);

        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_flushes_the_remainder() {
        let sink = MockSink::default();
        let writer = Writer::spawn(sink.clone(), 1_000_000, LONG);

        writer.send(rows(7)).await.unwrap();
        writer.shutdown().await.unwrap();

        assert_eq!(sink.flushes(), vec![7]);
    }

    #[tokio::test]
    async fn a_failed_flush_stops_the_writer_for_good() {
        let sink = MockSink::default();
        let writer = Writer::spawn(sink.clone(), 1_000_000, LONG);

        writer.send(rows(1)).await.unwrap();
        writer.barrier().await.unwrap();

        sink.fail.store(true, Ordering::SeqCst);
        writer.send(rows(2)).await.unwrap();
        // Identified by type right away, without waiting for the channel
        // to close.
        let error = writer.barrier().await.unwrap_err();
        assert!(WriterStopped::is_cause_of(&error));

        // Even if the database recovers nothing is written past the
        // failed flush: the process is expected to exit and resume.
        sink.fail.store(false, Ordering::SeqCst);
        let error =
            writer.send(rows(3)).await.unwrap_err().context("pass");
        assert!(WriterStopped::is_cause_of(&error));
        assert!(!WriterStopped::is_cause_of(&anyhow::anyhow!("other")));

        let error = writer.shutdown().await.unwrap_err();
        assert!(format!("{error:#}").contains("database is down"));
        assert_eq!(sink.flushes(), vec![1]);
    }
}
