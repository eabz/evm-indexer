//! The Solana candle aggregates of `migrations/0042` and nothing else:
//! the [`DerivedTable`] type, the placeholder rendering and the monthly
//! slicing are infrastructure and live in `db::derived`, the dataset owns
//! only its instances - the same split `core::derived`, `dex::derived`,
//! `predictions::derived` and `launchpads::derived` make.
//!
//! WHICH of these a Solana purge has to repair (these plus the SHARED
//! `launchpad_*` aggregates) is the pipeline's question, and is answered
//! by `pipeline::solana_store::repaired_derived`.

use crate::db::derived::DerivedTable;

/// **The candle predicate, written down once.**
///
/// Four places have to apply exactly the same restriction to
/// `sol_dex_swaps` or a repaired index stops equalling a clean one:
///
/// 1. the three materialized views of `migrations/0042`,
/// 2. the `rebuild_sql` a purge re-runs (the macro below),
/// 3. the base side of the candle cross-check of `indexer verify`
///    (`pipeline::solana_verify::check_candles`, which reads
///    [`SOL_CANDLE_FILTER`]),
/// 4. anything else that recounts what a candle counted.
///
/// A migration cannot read a Rust constant, so the SQL files hold the
/// text; everything in Rust reads it from HERE, and
/// `the_candle_predicate_is_the_same_in_the_views_and_the_rebuild`
/// compares the migration's own `WITH` clause and `WHERE` conjuncts with
/// these, conjunct by conjunct. Drift in any one of the four fails that
/// test. (Review F, NEW-1: the pool guard reached the views and not the
/// rebuild, so every purge re-created the merged price series B3 was
/// about.)
///
/// `pool_id != toFixedString('', 32)`: the decoder leaves `pool_id` empty
/// when it cannot name the pool (review round 4, B3). Those rows are real
/// trades and are stored, but they share one key per venue, so grouping
/// them would merge unrelated tokens into one price series.
macro_rules! sol_candle_filter {
    () => {
        "is_deleted = 0 AND pool_id != toFixedString('', 32)"
    };
}

/// The rows a Solana candle counts: see [`sol_candle_filter`].
///
/// Ready to paste after a `WHERE` or an `AND`; it carries no range and no
/// epoch, which is what lets the views, the rebuild and the verifier -
/// three statements with three different ranges - share it.
pub const SOL_CANDLE_FILTER: &str = sol_candle_filter!();

/// The `WITH` clause of every Solana candle: the dust floor
/// (`dex::derived::DUST_FLOOR_RAW`) and the two price expressions.
macro_rules! sol_candle_with {
    () => {
        "(amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, reserve0 != 0 AND reserve1 != 0 AS pool_ok, toFloat64(reserve1) / toFloat64(reserve0) AS pool_price"
    };
}

/// The dust guard and the price expressions: see [`sol_candle_with`].
pub const SOL_CANDLE_WITH: &str = sol_candle_with!();

/// The three Solana candle aggregates of `migrations/0042`.
///
/// Every `rebuild_sql` is the `SELECT` of the table's materialized view
/// with `FINAL`, the rebuild range and the NEW epoch instead of the rows'
/// own; `solana_tests` compares the two texts so a change to one breaks
/// without the other.
///
/// Like the DEX candles (`dex::derived`) they carry the purged block range
/// `{purge_from}` / `{purge_to}` and leave it out themselves. Relying on
/// the tombstones instead is what docs/design.md §2 says not to do:
/// ClickHouse gives no read-your-writes guarantee, so a `sol_dex_swaps`
/// part tombstoned a moment earlier can still be visible to the rebuild,
/// which would file the purged swaps under the NEW epoch and count them a
/// second time when the range is streamed again. `block_number` on
/// `sol_dex_swaps` is the SLOT, which is exactly what a Solana purge ranges
/// over.
macro_rules! sol_candles {
    ($name:literal, $seconds:literal) => {
        DerivedTable {
            name: $name,
            bucket_seconds: $seconds,
            bucket_column: "bucket",
            rebuild_sql: concat!(
                "INSERT INTO ",
                $name,
                " WITH ",
                sol_candle_with!(),
                " SELECT chain, pool_id, venue_program,",
                " toDateTime(intDiv(toUInt32(timestamp), ",
                $seconds,
                ") * ",
                $seconds,
                ", 'UTC') AS bucket,",
                " toUInt32({epoch}) AS epoch,",
                " argMinStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS open,",
                " argMaxStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS close,",
                " max(if(trade_ok, trade_price, NULL)) AS high,",
                " min(if(trade_ok, trade_price, NULL)) AS low,",
                " countIf(trade_ok) AS trades,",
                " argMinStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_open,",
                " argMaxStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_close,",
                " max(if(pool_ok, pool_price, NULL)) AS pool_high,",
                " min(if(pool_ok, pool_price, NULL)) AS pool_low,",
                " countIf(pool_ok) AS pool_prices,",
                " sum(abs(toFloat64(amount0))) AS volume0,",
                " sum(abs(toFloat64(amount1))) AS volume1,",
                " count() AS swaps,",
                " uniqState(trader) AS traders",
                " FROM sol_dex_swaps FINAL",
                " WHERE chain = {chain} AND timestamp >= toDateTime({from_ts}) AND timestamp < toDateTime({to_ts})",
                " AND ",
                sol_candle_filter!(),
                " AND NOT (block_number >= {purge_from} AND block_number < {purge_to})",
                " GROUP BY chain, pool_id, venue_program, bucket, epoch"
            ),
        }
    };
}

pub const SOL_CANDLES_1M: DerivedTable =
    sol_candles!("sol_dex_candles_1m", 60);
pub const SOL_CANDLES_1H: DerivedTable =
    sol_candles!("sol_dex_candles_1h", 3600);
pub const SOL_CANDLES_1D: DerivedTable =
    sol_candles!("sol_dex_candles_1d", 86400);

/// Every Solana-only aggregate, all fed from `sol_dex_swaps`.
pub const SOL_DERIVED: &[DerivedTable] =
    &[SOL_CANDLES_1M, SOL_CANDLES_1H, SOL_CANDLES_1D];

/// The reader views of [`SOL_DERIVED`]: the only correct way to read them
/// (they apply the validity rule of docs/design.md §2).
pub const SOL_CANDLE_VIEWS: &[&str] = &[
    "sol_dex_candles_1m_v",
    "sol_dex_candles_1h_v",
    "sol_dex_candles_1d_v",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    /// Whitespace collapsed, so SQL laid out over several lines and SQL
    /// written as one long literal compare equal.
    fn normalize(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// The `SELECT` of the materialized view feeding `table`, out of
    /// migration 0042 itself.
    fn view_of(sql: &str, table: &str) -> String {
        let marker = format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS \
                              {table}_mv"
        );
        normalize(
            sql.split(&normalize(&marker))
                .nth(1)
                .unwrap_or_else(|| {
                    panic!("{table}_mv is in the migration")
                })
                .split(';')
                .next()
                .unwrap(),
        )
    }

    /// The conjuncts of `clause`, split on the top level `AND` only: the
    /// `AND` inside `NOT (a AND b)` is part of one conjunct.
    fn conjuncts(clause: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut depth = 0usize;
        let mut current = String::new();
        let words = clause.split_whitespace();

        for word in words {
            if word.eq_ignore_ascii_case("AND") && depth == 0 {
                parts.push(current.trim().to_owned());
                current = String::new();
                continue;
            }
            depth += word.matches('(').count();
            depth = depth.saturating_sub(word.matches(')').count());
            current.push_str(word);
            current.push(' ');
        }
        parts.push(current.trim().to_owned());
        parts.retain(|part| !part.is_empty());
        parts
    }

    /// Everything between `WHERE` and `GROUP BY` of a one-`SELECT`
    /// statement.
    fn where_clause(sql: &str) -> String {
        let sql = normalize(sql);
        let (_, rest) = sql.split_once(" WHERE ").expect("a WHERE clause");
        rest.split(" GROUP BY ").next().unwrap().trim().to_owned()
    }

    /// **The parity that matters**: the views, the rebuild and the
    /// verifier must restrict `sol_dex_swaps` in exactly the same way.
    ///
    /// The old test compared a hand-written list of six fragments, and the
    /// `WHERE` clause was not one of them - which is why the missing pool
    /// guard of the rebuild passed review (review F, NEW-1). This compares
    /// the whole `WITH` clause and the whole set of `WHERE` conjuncts,
    /// minus the three the rebuild owns (its range, its epoch, the purged
    /// block range), so the next divergence of any kind fails here.
    #[test]
    fn the_candle_predicate_is_the_same_in_the_views_and_the_rebuild() {
        let sql = normalize(
            &db::migrate::embedded()
                .unwrap()
                .iter()
                .find(|migration| {
                    migration.sql.contains("sol_dex_candles_1m")
                })
                .expect("migration 0042 is embedded")
                .sql,
        );

        // What the rebuild adds and the view cannot have: its own range,
        // and the block range of the purge it belongs to.
        let owned = |conjunct: &str| {
            ["{from_ts}", "{to_ts}", "{chain}", "{purge_from}"]
                .iter()
                .any(|placeholder| conjunct.contains(placeholder))
        };

        for table in SOL_DERIVED {
            let view = view_of(&sql, table.name);
            let rebuild = normalize(table.rebuild_sql);

            // 1. the WITH clause - the dust floor and the two prices.
            let with_of = |text: &str| {
                let (_, rest) =
                    text.split_once(" WITH ").expect("a WITH clause");
                rest.split(" SELECT ").next().unwrap().trim().to_owned()
            };
            assert_eq!(
                with_of(&view),
                normalize(SOL_CANDLE_WITH),
                "{}: the view's WITH clause is not the shared one",
                table.name
            );
            assert_eq!(
                with_of(&rebuild),
                normalize(SOL_CANDLE_WITH),
                "{}: the rebuild's WITH clause is not the shared one",
                table.name
            );

            // 2. the row filter, conjunct by conjunct.
            let shared = conjuncts(&normalize(SOL_CANDLE_FILTER));
            assert_eq!(
                conjuncts(&where_clause(&view)),
                shared,
                "{}_mv: the view's WHERE is not the shared candle filter",
                table.name
            );

            let mut rest = conjuncts(&where_clause(&rebuild));
            rest.retain(|conjunct| !owned(conjunct));
            assert_eq!(
                rest, shared,
                "{}: the rebuild's WHERE differs from the view's. A \
                 repaired index would not equal a clean one.",
                table.name
            );
        }
    }

    /// The rebuild must be the materialized view's own SELECT, or a
    /// repaired index would silently differ from a clean one.
    #[test]
    fn every_candle_rebuild_matches_its_materialized_view() {
        let sql: String = db::migrate::embedded()
            .unwrap()
            .iter()
            .find(|migration| migration.sql.contains("sol_dex_candles_1m"))
            .expect("migration 0042 is embedded")
            .sql
            .clone();

        for table in SOL_DERIVED {
            let mv = format!("{}_mv", table.name);
            let view = sql
                .split(&format!(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS {mv}"
                ))
                .nth(1)
                .unwrap_or_else(|| panic!("{mv} is in the migration"));
            let view = view.split(';').next().unwrap();

            let normalize = |text: &str| {
                text.split_whitespace().collect::<Vec<_>>().join(" ")
            };

            let rebuild = normalize(table.rebuild_sql);
            let view = normalize(view);

            // The parts that must be identical: the WITH clause and every
            // aggregate expression.
            for fragment in [
                "argMinStateIf(trade_price, (block_number, tx_index, \
                 ordinal), trade_ok) AS open",
                "argMaxStateIf(trade_price, (block_number, tx_index, \
                 ordinal), trade_ok) AS close",
                "countIf(pool_ok) AS pool_prices",
                "sum(abs(toFloat64(amount0))) AS volume0",
                "uniqState(trader) AS traders",
                "GROUP BY chain, pool_id, venue_program, bucket, epoch",
            ] {
                let fragment = normalize(fragment);
                assert!(
                    rebuild.contains(&fragment),
                    "{}: rebuild_sql is missing `{fragment}`",
                    table.name
                );
                assert!(
                    view.contains(&fragment),
                    "{mv}: the view is missing `{fragment}`"
                );
            }

            // The rebuild reads FINAL and the new epoch; the view reads the
            // insert block and the rows' own epoch.
            assert!(rebuild.contains("FROM sol_dex_swaps FINAL"));
            assert!(rebuild.contains("toUInt32({epoch}) AS epoch"));
            assert!(view.contains("FROM sol_dex_swaps"));
        }
    }

    #[test]
    fn the_candle_buckets_are_a_minute_an_hour_and_a_day() {
        let widths: Vec<u32> =
            SOL_DERIVED.iter().map(|t| t.bucket_seconds).collect();
        assert_eq!(widths, vec![60, 3_600, 86_400]);
        assert_eq!(SOL_DERIVED.len(), SOL_CANDLE_VIEWS.len());
    }

    /// A rebuild must leave the purged slot range out BY ITSELF: the
    /// tombstones of that range may not be visible yet when it runs
    /// (docs/design.md §2), and a swap counted here under the new epoch is
    /// counted a second time when the range is streamed again.
    #[test]
    fn every_candle_rebuild_excludes_the_purged_slot_range() {
        for table in SOL_DERIVED {
            assert!(
                table.rebuild_sql.contains("{purge_from}")
                    && table.rebuild_sql.contains("{purge_to}"),
                "{} does not exclude the purged slot range",
                table.name
            );

            let sql = table.rebuild_slice(1, 0, 86_400, 3, 500, Some(600));
            assert!(
                sql.contains(
                    "NOT (block_number >= 500 AND block_number < 600)"
                ),
                "{sql}"
            );

            // An open ended purge reaches to the top of the chain.
            let open = table.rebuild_slice(1, 0, 86_400, 3, 500, None);
            assert!(
                open.contains(&format!("block_number < {}", u64::MAX)),
                "{open}"
            );
        }
    }

    #[test]
    fn a_rebuild_is_sliced_by_month() {
        // One UTC month apart: two statements, never one insert touching
        // two partitions more than ClickHouse allows.
        let statements = SOL_CANDLES_1D.rebuild_statements(
            1_399_811_149,
            1_767_225_600, // 2026-01-01
            1_772_496_000, // 2026-03-03
            7,
            u64::MAX,
            None,
        );
        assert!(statements.len() >= 2, "{}", statements.len());
        assert!(statements[0].contains("chain = 1399811149"));
        assert!(statements[0].contains("toUInt32(7) AS epoch"));
        assert!(!statements[0].contains('{'), "{}", statements[0]);
    }
}
