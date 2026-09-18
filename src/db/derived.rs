// STUB - replaced by schema engineer at merge
//
//! Incremental aggregates that `purge_range` repairs bucket by bucket
//! (docs/design.md §1 "Aggregates" and §2).

/// An `AggregatingMergeTree` table fed by a materialized view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedTable {
    /// Target table.
    pub name: &'static str,
    /// 60, 3600, 86400.
    pub bucket_seconds: u32,
    /// DateTime column holding the bucket start.
    pub bucket_column: &'static str,
    /// `INSERT INTO <name> SELECT ... FROM <base> FINAL WHERE chain = {chain}
    ///  AND timestamp >= {from_ts} GROUP BY ...` - must produce exactly what
    /// the MV produces.
    pub rebuild_sql: &'static str,
}
