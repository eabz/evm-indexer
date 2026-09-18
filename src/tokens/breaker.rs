//! Circuit breaker with a doubling cool-down, shared by the metadata
//! fetcher (one for the RPC as a whole) and the multi-endpoint caller (one
//! per endpoint).
//!
//! Uses the tokio clock so it can be driven by `tokio::time::pause` in
//! tests; outside of tests that is the monotonic system clock.

use std::{
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use tokio::time::Instant;

struct State {
    open_until: Option<Instant>,
    next_cooldown: Duration,
}

/// Once tripped, [`is_open`](Self::is_open) stays `true` for the cool-down;
/// every consecutive trip doubles the cool-down up to a maximum and a
/// [`reset`](Self::reset) brings it back to the initial value.
pub struct CircuitBreaker {
    cooldown: Duration,
    max_cooldown: Duration,
    state: Mutex<State>,
}

impl CircuitBreaker {
    pub fn new(cooldown: Duration, max_cooldown: Duration) -> Self {
        Self {
            cooldown,
            max_cooldown,
            state: Mutex::new(State {
                open_until: None,
                next_cooldown: cooldown,
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_open(&self) -> bool {
        self.state().open_until.is_some_and(|until| Instant::now() < until)
    }

    /// When the current cool-down ends (`None` when closed).
    pub fn open_until(&self) -> Option<Instant> {
        let now = Instant::now();
        self.state().open_until.filter(|until| now < *until)
    }

    /// Opens the breaker and returns the cool-down that was applied, or
    /// `None` when it was already open (a concurrent request tripped it
    /// for the same outage).
    pub fn trip(&self) -> Option<Duration> {
        let mut state = self.state();
        let now = Instant::now();

        if state.open_until.is_some_and(|until| now < until) {
            return None;
        }

        let cooldown = state.next_cooldown;
        state.open_until = Some(now + cooldown);
        state.next_cooldown = cooldown
            .saturating_mul(2)
            .min(self.max_cooldown)
            .max(self.cooldown);

        Some(cooldown)
    }

    /// Closes the breaker and forgets the doubled cool-down.
    pub fn reset(&self) {
        let mut state = self.state();
        state.open_until = None;
        state.next_cooldown = self.cooldown;
    }

    /// Cool-down the next [`trip`](Self::trip) will apply.
    pub fn next_cooldown(&self) -> Duration {
        self.state().next_cooldown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn doubles_up_to_the_maximum_and_resets() {
        let breaker = CircuitBreaker::new(
            Duration::from_secs(10),
            Duration::from_secs(25),
        );
        assert!(!breaker.is_open());
        assert_eq!(breaker.open_until(), None);

        assert_eq!(breaker.trip(), Some(Duration::from_secs(10)));
        assert!(breaker.is_open());
        // Already open: the same outage is not counted twice.
        assert_eq!(breaker.trip(), None);
        assert_eq!(breaker.next_cooldown(), Duration::from_secs(20));

        tokio::time::advance(Duration::from_secs(11)).await;
        assert!(!breaker.is_open());
        assert_eq!(breaker.trip(), Some(Duration::from_secs(20)));
        tokio::time::advance(Duration::from_secs(21)).await;
        assert_eq!(breaker.trip(), Some(Duration::from_secs(25)));
        tokio::time::advance(Duration::from_secs(26)).await;
        assert_eq!(breaker.trip(), Some(Duration::from_secs(25)));

        breaker.reset();
        assert!(!breaker.is_open());
        assert_eq!(breaker.next_cooldown(), Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn zero_cooldown_never_stays_open() {
        let breaker = CircuitBreaker::new(Duration::ZERO, Duration::ZERO);
        assert_eq!(breaker.trip(), Some(Duration::ZERO));
        assert!(!breaker.is_open());
    }
}
