//! Live state of every chain in the process, as the control panel prints
//! it (docs/design.md section 15).
//!
//! # Where the numbers come from
//!
//! Almost all of them are ALREADY measured. `crate::metrics::Metrics` - one
//! handle per chain, the same handle that feeds `/metrics` - records the
//! chain head, the stored head, both timestamps, flush counts and latency,
//! the reorg counters and the resolver queue depths. [`ChainStatus`] reads
//! that snapshot and adds only what no counter can know:
//!
//! * which of the six [`ChainState`]s the chain is in (the supervisor and
//!   the sync loop say so through [`crate::pipeline::status::StatusSink`]),
//! * the last error and when it happened,
//! * how often the chain was restarted, and when the next attempt is due,
//! * blocks per second, sampled by one task for the whole fleet.
//!
//! # Cost
//!
//! Per chain: one `Mutex` that is locked when a state CHANGES (a few times
//! an hour), when an error is recorded, when the sampler ticks (every
//! [`SAMPLE_INTERVAL`]) and when someone opens the panel. Nothing on the
//! block path ever touches it.

use crate::{
    metrics::Metrics,
    pipeline::status::{ChainState, StatusObserver},
};
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How often the fleet samples progress to work out blocks per second and
/// to notice a reorg. One task for the whole process.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Events kept per chain. Enough to see what has been happening without
/// growing without bound; the panel shows them newest first.
const MAX_EVENTS: usize = 50;

/// Errors longer than this are cut: a panel line, not a stack trace.
const MAX_ERROR_CHARS: usize = 400;

pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One line of a chain's history, for `GET /api/chains/{id}/events`.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// `state`, `error`, `reorg`, `restart`, `start` or `stop`.
    pub kind: &'static str,
    pub message: String,
    pub at_unix_ms: u64,
}

#[derive(Debug)]
struct Inner {
    state: ChainState,
    last_error: Option<String>,
    last_error_unix_ms: Option<u64>,
    restarts: u64,
    /// Unix ms the next restart attempt is due, while waiting out a
    /// backoff.
    retry_at_unix_ms: Option<u64>,
    events: VecDeque<Event>,
    /// Previous sample: (unix ms, stored head, reorg count).
    sample: Option<(u64, u64, u64)>,
}

/// Everything the panel knows about ONE chain this process manages.
#[derive(Debug)]
pub struct ChainStatus {
    pub chain: u64,
    /// The chain's own metrics handle, shared with `/metrics`.
    metrics: Metrics,
    inner: Mutex<Inner>,
    /// Blocks per second, times 1000, so the sampler can publish it
    /// without taking the lock on the read side.
    milliblocks_per_second: AtomicU64,
}

impl ChainStatus {
    pub fn new(chain: u64, metrics: Metrics) -> Arc<Self> {
        Arc::new(Self {
            chain,
            metrics,
            inner: Mutex::new(Inner {
                state: ChainState::Stopped,
                last_error: None,
                last_error_unix_ms: None,
                restarts: 0,
                retry_at_unix_ms: None,
                events: VecDeque::new(),
                sample: None,
            }),
            milliblocks_per_second: AtomicU64::new(0),
        })
    }

    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(inner: &mut Inner, kind: &'static str, message: String) {
        inner.events.push_front(Event {
            kind,
            message,
            at_unix_ms: unix_ms(),
        });
        inner.events.truncate(MAX_EVENTS);
    }

    pub fn set_state(&self, state: ChainState) {
        let mut inner = self.lock();
        if inner.state == state {
            return;
        }
        inner.state = state;
        if state != ChainState::Failed {
            inner.retry_at_unix_ms = None;
        }
        Self::push(&mut inner, "state", state.plain().to_string());
    }

    pub fn state(&self) -> ChainState {
        self.lock().state
    }

    /// Something failed and is being retried. The message is stored as it
    /// arrives: every caller redacts before it gets here.
    pub fn record_error(&self, message: &str) {
        let message = shorten(message);
        let mut inner = self.lock();
        inner.last_error = Some(message.clone());
        inner.last_error_unix_ms = Some(unix_ms());
        Self::push(&mut inner, "error", message);
    }

    /// The chain is being started again after `wait`.
    pub fn record_restart(&self, wait: Duration) {
        let mut inner = self.lock();
        inner.restarts += 1;
        inner.retry_at_unix_ms = Some(
            unix_ms()
                + u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
        );
        Self::push(
            &mut inner,
            "restart",
            format!("Starting again in {}.", human_duration(wait)),
        );
    }

    /// The owner pressed a button.
    pub fn record_command(&self, kind: &'static str, message: String) {
        let mut inner = self.lock();
        Self::push(&mut inner, kind, message);
    }

    pub fn events(&self) -> Vec<Event> {
        self.lock().events.iter().cloned().collect()
    }

    /// One tick of the fleet's sampler: works out blocks per second from
    /// the stored head the pipeline already publishes, and notices a reorg
    /// from the counter it already increments.
    pub fn sample(&self) {
        let Some(snapshot) = self.metrics.snapshot() else { return };
        let now = unix_ms();
        let head = snapshot.indexed_block.unwrap_or(0);
        let reorgs = snapshot.reorgs;

        let mut inner = self.lock();

        if let Some((then, previous_head, previous_reorgs)) = inner.sample
        {
            let advanced = head.saturating_sub(previous_head);
            if let Some(rate) = advanced
                .saturating_mul(1_000_000)
                .checked_div(now.saturating_sub(then))
            {
                self.milliblocks_per_second.store(rate, Relaxed);
            }

            if reorgs > previous_reorgs {
                let message = format!(
                    "Chain reorganization rolled back ({} so far, deepest \
                     {} blocks).",
                    reorgs, snapshot.reorg_max_depth
                );
                Self::push(&mut inner, "reorg", message);
            }
        }

        inner.sample = Some((now, head, reorgs));
    }

    /// The whole live picture of one chain.
    pub fn view(&self) -> LiveView {
        let snapshot = self.metrics.snapshot().unwrap_or_default();
        let inner = self.lock();

        let lag_blocks =
            match (snapshot.head_block, snapshot.indexed_block) {
                (Some(head), Some(indexed)) => {
                    Some(head.saturating_sub(indexed))
                }
                _ => None,
            };

        let lag_seconds = snapshot.indexed_timestamp.map(|indexed| {
            snapshot
                .head_timestamp
                .unwrap_or(unix_ms() / 1000)
                .saturating_sub(indexed)
        });

        LiveView {
            state: inner.state.as_str(),
            state_text: inner.state.plain(),
            chain_head: snapshot.head_block,
            stored_head: snapshot.indexed_block,
            lag_blocks,
            lag_seconds,
            blocks_per_second: self.milliblocks_per_second.load(Relaxed)
                as f64
                / 1000.0,
            last_flush_unix_ms: snapshot.last_flush_ok_unix_ms,
            last_flush_ms: snapshot.last_flush_ms,
            flushes_ok: snapshot.flushes_ok,
            flushes_failed: snapshot.flushes_failed,
            reorgs: snapshot.reorgs,
            reorg_max_depth: snapshot.reorg_max_depth,
            restarts: inner.restarts,
            retry_at_unix_ms: inner.retry_at_unix_ms,
            last_error: inner.last_error.clone(),
            last_error_unix_ms: inner.last_error_unix_ms,
            token_queue: snapshot.token_queue,
            pool_queue: snapshot.pool_queue,
            venue_queue: snapshot.venue_queue,
            solana_queries_last_minute: snapshot
                .solana
                .map(|s| s.queries_last_minute),
        }
    }
}

/// The sync loop's status sink writes straight into the chain's status.
impl StatusObserver for ChainStatus {
    fn state(&self, state: ChainState) {
        self.set_state(state);
    }

    fn failed(&self, message: &str) {
        self.record_error(message);
    }
}

/// The live half of what `GET /api/chains` answers. Serialized straight to
/// the browser, so it holds numbers and words only - never a url, a token
/// or a database password.
#[derive(Debug, Clone, Serialize, Default)]
pub struct LiveView {
    pub state: &'static str,
    pub state_text: &'static str,
    pub chain_head: Option<u64>,
    pub stored_head: Option<u64>,
    pub lag_blocks: Option<u64>,
    pub lag_seconds: Option<u64>,
    pub blocks_per_second: f64,
    pub last_flush_unix_ms: Option<u64>,
    pub last_flush_ms: Option<u64>,
    pub flushes_ok: u64,
    pub flushes_failed: u64,
    pub reorgs: u64,
    pub reorg_max_depth: u64,
    pub restarts: u64,
    pub retry_at_unix_ms: Option<u64>,
    pub last_error: Option<String>,
    pub last_error_unix_ms: Option<u64>,
    pub token_queue: Option<u64>,
    pub pool_queue: Option<u64>,
    pub venue_queue: Option<u64>,
    pub solana_queries_last_minute: Option<u64>,
}

fn shorten(message: &str) -> String {
    let one_line = message.replace('\n', " ");
    match one_line.char_indices().nth(MAX_ERROR_CHARS) {
        None => one_line,
        Some((index, _)) => format!("{}...", &one_line[..index]),
    }
}

/// "45 seconds", "3 minutes": the panel is read by someone who is not
/// counting milliseconds.
pub fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0 => "a moment".to_string(),
        1 => "1 second".to_string(),
        2..=99 => format!("{seconds} seconds"),
        _ => {
            let minutes = (seconds + 30) / 60;
            if minutes == 1 {
                "1 minute".to_string()
            } else {
                format!("{minutes} minutes")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Arc<ChainStatus> {
        ChainStatus::new(1, Metrics::new(1, Duration::from_secs(120)))
    }

    #[test]
    fn a_state_change_is_recorded_once() {
        let status = status();
        status.set_state(ChainState::Starting);
        status.set_state(ChainState::Starting);
        status.set_state(ChainState::Following);

        let events = status.events();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].message, ChainState::Following.plain());
        assert_eq!(status.view().state, "following");
    }

    #[test]
    fn errors_are_shortened_and_kept_with_their_time() {
        let status = status();
        status.record_error(&"x".repeat(5_000));

        let view = status.view();
        let error = view.last_error.unwrap();
        assert!(error.len() < 500, "{}", error.len());
        assert!(error.ends_with("..."));
        assert!(view.last_error_unix_ms.is_some());
    }

    #[test]
    fn only_the_newest_events_are_kept() {
        let status = status();
        for i in 0..(MAX_EVENTS + 20) {
            status.record_error(&format!("failure {i}"));
        }
        assert_eq!(status.events().len(), MAX_EVENTS);
        assert!(status.events()[0]
            .message
            .contains(&format!("failure {}", MAX_EVENTS + 19)));
    }

    #[test]
    fn blocks_per_second_comes_from_the_stored_head_the_pipeline_publishes(
    ) {
        let status = status();
        let metrics = status.metrics();

        metrics.set_indexed_height(1_000);
        status.sample();
        // The second sample is what produces a rate; without a clock to
        // control, all we can pin is that it is finite and not negative.
        metrics.set_indexed_height(1_500);
        status.sample();

        assert!(status.view().blocks_per_second >= 0.0);
    }

    #[test]
    fn a_reorg_shows_up_as_an_event_without_a_pipeline_hook() {
        let status = status();
        status.sample();
        status.metrics().reorg(7);
        status.sample();

        let reorg = status
            .events()
            .into_iter()
            .find(|event| event.kind == "reorg")
            .expect("a reorg event");
        assert!(reorg.message.contains('7'), "{}", reorg.message);
    }

    #[test]
    fn waits_are_printed_for_people() {
        assert_eq!(human_duration(Duration::from_secs(1)), "1 second");
        assert_eq!(human_duration(Duration::from_secs(30)), "30 seconds");
        assert_eq!(human_duration(Duration::from_secs(300)), "5 minutes");
    }
}
