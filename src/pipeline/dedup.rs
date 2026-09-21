//! Guard of "retried inserts must not double count" (docs/design.md,
//! section 2): the server side insert deduplication only protects the
//! aggregates when EVERY table a flush writes AND every target of a
//! materialized view keeps a deduplication log
//! (`non_replicated_deduplication_window`). Verified by hand on ClickHouse
//! 25.12 and by `acceptance::a_retried_insert_does_not_double_count`.

#[cfg(test)]
mod tests {
    use crate::{
        db::{self, schema::split_sql_statements},
        pipeline::modules::ALL_MODULES,
    };
    use std::collections::BTreeSet;

    const SETTING: &str = "non_replicated_deduplication_window";

    fn statements() -> Vec<String> {
        db::migrate::embedded()
            .unwrap()
            .iter()
            .flat_map(|migration| split_sql_statements(&migration.sql))
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    }

    /// Tables that carry the window, through CREATE or ALTER.
    fn with_window(statements: &[String]) -> BTreeSet<String> {
        statements
            .iter()
            .filter(|s| s.contains(SETTING))
            .filter_map(|s| {
                let mut words = s.split(' ');
                match (words.next()?, words.next()?) {
                    ("ALTER", "TABLE") => words.next(),
                    ("CREATE", "TABLE") => s
                        .split(" (")
                        .next()
                        .and_then(|head| head.split(' ').next_back()),
                    _ => None,
                }
                .map(|name| name.trim_matches('`').to_string())
            })
            .collect()
    }

    /// Targets of the incremental materialized views (`... TO <table> AS`).
    /// Refreshable views recompute their target and are not fed by inserts.
    fn view_targets(statements: &[String]) -> BTreeSet<String> {
        statements
            .iter()
            .filter(|s| s.starts_with("CREATE MATERIALIZED VIEW"))
            .filter(|s| !s.contains(" REFRESH "))
            .filter_map(|s| {
                let (_, rest) = s.split_once(" TO ")?;
                Some(rest.split(' ').next()?.trim_matches('`').to_string())
            })
            .collect()
    }

    #[test]
    fn every_flush_table_and_view_target_keeps_a_deduplication_log() {
        let statements = statements();
        let protected = with_window(&statements);

        let mut required: BTreeSet<String> = view_targets(&statements);
        required.extend(
            crate::core::BASE_TABLES.iter().map(|t| t.to_string()),
        );
        required.insert("checkpoints".to_string());
        // Every table a flush writes, of every module: its block scoped
        // tables AND its insert order, which may hold more (a module can
        // write rows that are not block scoped, like
        // `prediction_outcome_tokens`).
        for spec in ALL_MODULES {
            required
                .extend(spec.base_tables.iter().map(|t| t.to_string()));
            required
                .extend(spec.insert_order.iter().map(|t| t.to_string()));
        }

        let missing: Vec<&String> =
            required.difference(&protected).collect();

        assert!(
            missing.is_empty(),
            "tables written by a flush (or fed by a materialized view of \
             one) without `{SETTING}`: {missing:?}. A retried insert would \
             double count there. Set it in the CREATE TABLE or add an \
             ALTER TABLE .. MODIFY SETTING migration."
        );

        assert!(required.len() > 40, "{}", required.len());
    }
}
