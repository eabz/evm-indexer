//! Prometheus text exposition format, version 0.0.4.
//!
//! <https://prometheus.io/docs/instrumenting/exposition_formats/>

use super::primitives::Histogram;
use std::fmt::{Display, Write};

/// Prefix of every metric name.
pub(super) const PREFIX: &str = "evm_indexer_";

pub(super) const CONTENT_TYPE: &str =
    "text/plain; version=0.0.4; charset=utf-8";

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Counter,
    Gauge,
    Histogram,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
            Kind::Histogram => "histogram",
        }
    }
}

/// Label values: backslash, double quote and line feed are escaped.
pub(super) fn escape_label_value(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// HELP text: backslash and line feed are escaped (quotes are not).
pub(super) fn escape_help(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Writes metric families. Every sample gets the constant `chain` label
/// first, then the labels of the call.
pub(super) struct Encoder {
    out: String,
    /// `chain="<escaped>"`, ready to be pasted into a label set.
    chain_label: String,
}

impl Encoder {
    pub fn new(chain: &str) -> Self {
        let mut chain_label = String::from("chain=\"");
        escape_label_value(chain, &mut chain_label);
        chain_label.push('"');

        Self { out: String::with_capacity(4096), chain_label }
    }

    pub fn finish(self) -> String {
        self.out
    }

    /// `# HELP` and `# TYPE` lines; call once per family, before samples.
    pub fn family(&mut self, name: &str, help: &str, kind: Kind) {
        let _ = write!(self.out, "# HELP {PREFIX}{name} ");
        escape_help(help, &mut self.out);
        let _ = writeln!(
            self.out,
            "\n# TYPE {PREFIX}{name} {}",
            kind.as_str()
        );
    }

    pub fn sample(
        &mut self,
        name: &str,
        labels: &[(&str, &str)],
        value: impl Display,
    ) {
        self.sample_with_suffix(name, "", labels, value);
    }

    fn sample_with_suffix(
        &mut self,
        name: &str,
        suffix: &str,
        labels: &[(&str, &str)],
        value: impl Display,
    ) {
        let _ = write!(self.out, "{PREFIX}{name}{suffix}{{");
        self.out.push_str(&self.chain_label);

        for (key, label_value) in labels {
            let _ = write!(self.out, ",{key}=\"");
            escape_label_value(label_value, &mut self.out);
            self.out.push('"');
        }

        let _ = writeln!(self.out, "}} {value}");
    }

    /// Single-sample family.
    pub fn scalar(
        &mut self,
        name: &str,
        help: &str,
        kind: Kind,
        value: impl Display,
    ) {
        self.family(name, help, kind);
        self.sample(name, &[], value);
    }

    /// Cumulative `_bucket{le=...}` series ending in `+Inf`, then `_sum`
    /// and `_count`.
    pub fn histogram(
        &mut self,
        name: &str,
        help: &str,
        histogram: &Histogram,
    ) {
        let snapshot = histogram.snapshot();

        self.family(name, help, Kind::Histogram);

        for (bound, cumulative) in &snapshot.buckets {
            let le = bound.to_string();
            self.sample_with_suffix(
                name,
                "_bucket",
                &[("le", &le)],
                cumulative,
            );
        }

        self.sample_with_suffix(
            name,
            "_bucket",
            &[("le", "+Inf")],
            snapshot.count,
        );
        self.sample_with_suffix(name, "_sum", &[], snapshot.sum_seconds);
        self.sample_with_suffix(name, "_count", &[], snapshot.count);
    }
}
