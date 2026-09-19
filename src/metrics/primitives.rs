//! Lock-free metric primitives. Everything is a `u64` in an atomic; the
//! only lock is the read-mostly map of a [`LabeledCounter`], taken for
//! writing once per new label value.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        RwLock,
    },
    time::Duration,
};

/// Monotonic counter.
#[derive(Default)]
pub(super) struct Counter(AtomicU64);

impl Counter {
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }

    /// For counters mirrored from a monotonic source owned elsewhere.
    pub fn set(&self, value: u64) {
        self.0.store(value, Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

/// Gauge that is absent from the exposition until it is set once.
pub(super) struct Gauge(AtomicU64);

/// No block number, timestamp or count ever reaches this value.
const UNSET: u64 = u64::MAX;

impl Default for Gauge {
    fn default() -> Self {
        Self(AtomicU64::new(UNSET))
    }
}

impl Gauge {
    pub fn set(&self, value: u64) {
        self.0.store(value.min(UNSET - 1), Relaxed);
    }

    pub fn get(&self) -> Option<u64> {
        match self.0.load(Relaxed) {
            UNSET => None,
            value => Some(value),
        }
    }
}

/// Counter with one label whose values are compile-time strings (table
/// names), so the cardinality is bounded by the code. Sorted, so the
/// exposition order is stable.
#[derive(Default)]
pub(super) struct LabeledCounter(
    RwLock<BTreeMap<&'static str, AtomicU64>>,
);

impl LabeledCounter {
    pub fn add(&self, label: &'static str, n: u64) {
        {
            // A poisoned lock only means another thread panicked between
            // two atomic operations: the map itself is always valid.
            let map = self.0.read().unwrap_or_else(|e| e.into_inner());
            if let Some(counter) = map.get(label) {
                counter.fetch_add(n, Relaxed);
                return;
            }
        }

        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        map.entry(label).or_default().fetch_add(n, Relaxed);
    }

    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        let map = self.0.read().unwrap_or_else(|e| e.into_inner());
        map.iter().map(|(label, c)| (*label, c.load(Relaxed))).collect()
    }
}

/// Histogram of durations with fixed upper bounds in seconds.
pub(super) struct Histogram {
    bounds: &'static [f64],
    /// One slot per bound plus the overflow (`+Inf`) slot. NOT cumulative:
    /// an observation touches exactly one slot.
    buckets: Box<[AtomicU64]>,
    sum_nanos: AtomicU64,
}

pub(super) struct HistogramSnapshot {
    /// `(upper bound, cumulative count)`, without the `+Inf` bucket.
    pub buckets: Vec<(f64, u64)>,
    pub count: u64,
    pub sum_seconds: f64,
}

impl Histogram {
    /// `bounds` must be sorted ascending.
    pub fn new(bounds: &'static [f64]) -> Self {
        debug_assert!(bounds.windows(2).all(|w| w[0] < w[1]));

        Self {
            bounds,
            buckets: (0..=bounds.len())
                .map(|_| AtomicU64::new(0))
                .collect(),
            sum_nanos: AtomicU64::new(0),
        }
    }

    pub fn observe(&self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        // `le` is inclusive.
        let slot = self
            .bounds
            .iter()
            .position(|bound| seconds <= *bound)
            .unwrap_or(self.bounds.len());

        self.buckets[slot].fetch_add(1, Relaxed);
        self.sum_nanos.fetch_add(
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
            Relaxed,
        );
    }

    /// `count` is derived from the buckets, so `_count` always equals the
    /// `+Inf` bucket even while observations race with the scrape.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut cumulative = 0;
        let mut buckets = Vec::with_capacity(self.bounds.len());

        for (bound, slot) in self.bounds.iter().zip(self.buckets.iter()) {
            cumulative += slot.load(Relaxed);
            buckets.push((*bound, cumulative));
        }

        let count =
            cumulative + self.buckets[self.bounds.len()].load(Relaxed);

        HistogramSnapshot {
            buckets,
            count,
            sum_seconds: self.sum_nanos.load(Relaxed) as f64 / 1e9,
        }
    }
}
