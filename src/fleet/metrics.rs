//! ONE `/metrics` for the whole fleet, every series labelled with `chain`
//! (docs/design.md section 15).
//!
//! Nothing new is measured. Each chain already has its own
//! [`crate::metrics::Metrics`] handle, and that handle already stamps every
//! sample it writes with its own `chain` label - `indexer run` has done
//! that since the metrics endpoint existed. So the fleet's exposition is
//! the chains' expositions, merged.
//!
//! Merged, and not concatenated: the text format allows one `# HELP` and
//! one `# TYPE` line per metric family, and a scraper rejects a second
//! one. [`merge`] keeps the first of each and groups every chain's samples
//! under it, which is also what makes the output readable.
//!
//! `indexer run`'s own output is untouched: it serves one chain's
//! `Metrics::render` exactly as before.

use super::supervisor::Supervisor;
use crate::metrics::Exposition;
use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

/// The fleet's `/metrics` and `/readyz`.
pub struct FleetExposition {
    /// Weak, so the endpoint never keeps the supervisor (and with it every
    /// chain task) alive after shutdown.
    supervisor: Weak<Supervisor>,
}

impl FleetExposition {
    pub fn new(supervisor: &Arc<Supervisor>) -> Arc<Self> {
        Arc::new(Self { supervisor: Arc::downgrade(supervisor) })
    }
}

impl Exposition for FleetExposition {
    fn render(&self) -> String {
        let Some(supervisor) = self.supervisor.upgrade() else {
            return String::new();
        };

        merge(
            supervisor
                .metrics_handles()
                .into_iter()
                .map(|(_, metrics)| metrics.render()),
        )
    }

    /// Ready when every chain that is SUPPOSED to be running is ready. A
    /// chain the owner stopped on purpose must not make the whole process
    /// look broken.
    fn readiness(&self) -> Result<(), String> {
        let Some(supervisor) = self.supervisor.upgrade() else {
            return Err(
                "not ready: the fleet is shutting down".to_string()
            );
        };

        let mut not_ready = Vec::new();

        for (chain, metrics) in supervisor.running_metrics_handles() {
            if let Err(reason) = metrics.readiness() {
                not_ready.push(format!("chain {chain}: {reason}"));
            }
        }

        if not_ready.is_empty() {
            Ok(())
        } else {
            Err(not_ready.join("; "))
        }
    }
}

/// One metric family while it is being merged.
#[derive(Default)]
struct Family {
    help: String,
    kind: String,
    samples: Vec<String>,
}

/// Merges several Prometheus text expositions into one, keeping a single
/// `# HELP` / `# TYPE` pair per family and grouping the samples under it.
pub fn merge(expositions: impl IntoIterator<Item = String>) -> String {
    let mut families: BTreeMap<String, Family> = BTreeMap::new();

    for text in expositions {
        let mut current: Option<String> = None;

        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest
                    .split_once(' ')
                    .map_or(rest, |(name, _)| name)
                    .to_string();
                let family = families.entry(name.clone()).or_default();
                if family.help.is_empty() {
                    family.help = line.to_string();
                }
                current = Some(name);
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest
                    .split_once(' ')
                    .map_or(rest, |(name, _)| name)
                    .to_string();
                let family = families.entry(name.clone()).or_default();
                if family.kind.is_empty() {
                    family.kind = line.to_string();
                }
                current = Some(name);
            } else if !line.trim().is_empty() {
                // A sample belongs to the family whose header it follows;
                // histogram samples carry _bucket / _sum / _count suffixes
                // that are not family names of their own.
                if let Some(family) = current
                    .as_ref()
                    .and_then(|name| families.get_mut(name))
                {
                    family.samples.push(line.to_string());
                }
            }
        }
    }

    let mut out = String::with_capacity(8 * 1024);

    for family in families.values() {
        if family.samples.is_empty() {
            continue;
        }
        if !family.help.is_empty() {
            out.push_str(&family.help);
            out.push('\n');
        }
        if !family.kind.is_empty() {
            out.push_str(&family.kind);
            out.push('\n');
        }
        for sample in &family.samples {
            out.push_str(sample);
            out.push('\n');
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::Metrics;
    use std::time::Duration;

    fn chain_metrics(chain: u64, head: u64) -> String {
        let metrics = Metrics::new(chain, Duration::from_secs(120));
        metrics.set_head(head);
        metrics.set_indexed_height(head - 5);
        metrics.rows_inserted("blocks", 10);
        metrics.render()
    }

    #[test]
    fn every_series_of_every_chain_carries_its_own_chain_label() {
        let merged =
            merge([chain_metrics(1, 100), chain_metrics(8453, 50)]);

        assert!(merged.contains("evm_indexer_head_block{chain=\"1\"} 100"));
        assert!(
            merged.contains("evm_indexer_head_block{chain=\"8453\"} 50")
        );

        // Nothing without a chain label got through.
        for line in merged.lines().filter(|l| !l.starts_with('#')) {
            assert!(line.contains("chain=\""), "{line}");
        }
    }

    #[test]
    fn a_family_is_declared_exactly_once() {
        let merged = merge([
            chain_metrics(1, 100),
            chain_metrics(10, 100),
            chain_metrics(8453, 100),
        ]);

        for name in [
            "evm_indexer_head_block",
            "evm_indexer_flushes_total",
            "evm_indexer_flush_duration_seconds",
            "evm_indexer_rows_inserted_total",
        ] {
            let help = merged
                .lines()
                .filter(|line| {
                    line.starts_with(&format!("# HELP {name} "))
                })
                .count();
            let kind = merged
                .lines()
                .filter(|line| {
                    line.starts_with(&format!("# TYPE {name} "))
                })
                .count();
            assert_eq!(help, 1, "{name}: {help} HELP lines");
            assert_eq!(kind, 1, "{name}: {kind} TYPE lines");
        }
    }

    #[test]
    fn a_histogram_keeps_its_buckets_under_its_own_family() {
        let metrics = Metrics::new(1, Duration::from_secs(120));
        metrics.flush_observed(Duration::from_millis(30), 5, true);
        let merged = merge([metrics.render()]);

        let header = merged
            .find("# TYPE evm_indexer_flush_duration_seconds histogram")
            .expect("the histogram family");
        let bucket = merged
            .find("evm_indexer_flush_duration_seconds_bucket")
            .expect("a bucket sample");
        let count = merged
            .find("evm_indexer_flush_duration_seconds_count")
            .expect("the count sample");

        assert!(header < bucket && bucket < count);
    }

    #[test]
    fn a_fleet_with_no_chains_renders_nothing_rather_than_garbage() {
        assert!(merge(Vec::<String>::new()).is_empty());
        assert!(merge([String::new()]).is_empty());
    }
}
