//! The one thing a chain's pipeline tells the outside world about itself
//! while it runs (docs/design.md section 15).
//!
//! `indexer run` is one chain per process, so "how is it going" is answered
//! by the log and by `/metrics`. `indexer fleet` runs dozens of chains in
//! one process and has to answer it per chain, in a web page, for someone
//! who is not reading the log.
//!
//! **Almost nothing new is measured here.** Head, stored head, lag, rows,
//! flush latency, reorg counts and the worker queues are already recorded
//! by [`crate::metrics`], one handle per chain, and the fleet reads them
//! from there. What `metrics` cannot know is the handful of facts that only
//! exist between two runs of the sync loop: which of the six states the
//! chain is in, and what the last thing that went wrong was.
//!
//! So this is a two-method sink, called a few times a minute at most:
//!
//! * [`StatusSink::state`] on a state CHANGE (the pipeline compares first),
//! * [`StatusSink::failed`] when a pass failed and will be retried.
//!
//! A sink with no observer - what `indexer run` uses - costs one branch per
//! call and allocates nothing, exactly like [`crate::metrics::Metrics`]
//! when `--metrics-addr` is not given.

use std::sync::Arc;

/// What a chain is doing, in the words the control panel prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChainState {
    /// Connecting, migrating, taking the lease, reading the resume point.
    Starting,
    /// Far behind the chain head and catching up.
    Backfilling,
    /// At the head: every new block is picked up as it appears.
    Following,
    /// Reached `--end-block`, or the owner stopped it.
    Stopped,
    /// The chain stopped with an error and is being restarted.
    Failed,
    /// Another process holds this chain's lease. Not an error: this one
    /// waits and takes over if the other one goes away.
    RunningElsewhere,
}

impl ChainState {
    /// The machine-readable word in the JSON API and in the panel's CSS.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Backfilling => "backfilling",
            Self::Following => "following",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::RunningElsewhere => "running_elsewhere",
        }
    }

    /// One short sentence the owner can act on.
    pub fn plain(self) -> &'static str {
        match self {
            Self::Starting => "Starting up",
            Self::Backfilling => "Catching up",
            Self::Following => "Up to date",
            Self::Stopped => "Stopped",
            Self::Failed => "Stopped by an error, restarting",
            Self::RunningElsewhere => {
                "Indexed by another process on this database"
            }
        }
    }
}

/// What a [`StatusSink`] delivers to. The fleet supervisor implements it;
/// nothing else does.
pub trait StatusObserver: Send + Sync + 'static {
    fn state(&self, state: ChainState);

    /// Something went wrong and will be retried. `message` is already
    /// formatted for a human and carries no secret (the pipeline's error
    /// chain is redacted before it gets here).
    fn failed(&self, message: &str);
}

/// Cheap, clonable handle to a chain's status. [`StatusSink::default`] is
/// the no-op sink used by `indexer run`.
#[derive(Clone, Default)]
pub struct StatusSink {
    observer: Option<Arc<dyn StatusObserver>>,
}

impl std::fmt::Debug for StatusSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.observer {
            Some(_) => "StatusSink(fleet)",
            None => "StatusSink(off)",
        })
    }
}

impl StatusSink {
    pub fn new(observer: Arc<dyn StatusObserver>) -> Self {
        Self { observer: Some(observer) }
    }

    /// The sink `indexer run` uses: records nothing.
    pub fn off() -> Self {
        Self::default()
    }

    pub fn state(&self, state: ChainState) {
        if let Some(observer) = &self.observer {
            observer.state(state);
        }
    }

    pub fn failed(&self, message: &str) {
        if let Some(observer) = &self.observer {
            observer.failed(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        states: Mutex<Vec<ChainState>>,
        errors: Mutex<Vec<String>>,
    }

    impl StatusObserver for Recorder {
        fn state(&self, state: ChainState) {
            self.states.lock().unwrap().push(state);
        }
        fn failed(&self, message: &str) {
            self.errors.lock().unwrap().push(message.to_string());
        }
    }

    #[test]
    fn the_no_op_sink_of_indexer_run_does_nothing_at_all() {
        let sink = StatusSink::off();
        sink.state(ChainState::Following);
        sink.failed("boom");
        assert_eq!(format!("{sink:?}"), "StatusSink(off)");
    }

    #[test]
    fn an_observer_sees_every_call() {
        let recorder = Arc::new(Recorder::default());
        let sink = StatusSink::new(recorder.clone());

        sink.state(ChainState::Starting);
        sink.state(ChainState::Following);
        sink.failed("the source is down");

        assert_eq!(
            *recorder.states.lock().unwrap(),
            [ChainState::Starting, ChainState::Following]
        );
        assert_eq!(recorder.errors.lock().unwrap().len(), 1);
    }

    #[test]
    fn every_state_has_a_stable_word_and_a_sentence() {
        for state in [
            ChainState::Starting,
            ChainState::Backfilling,
            ChainState::Following,
            ChainState::Stopped,
            ChainState::Failed,
            ChainState::RunningElsewhere,
        ] {
            assert!(!state.as_str().is_empty());
            assert!(!state.plain().is_empty());
            assert!(!state.as_str().contains(' '));
        }
    }
}
