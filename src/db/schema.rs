//! What the code needs to know about the SQL schema in `migrations/`:
//! generic helpers that read the embedded DDL, for any table of any
//! dataset. WHICH tables a dataset owns is the dataset's own business
//! (`core::BASE_TABLES`, `dex::BASE_TABLES`, ...), and each of them has
//! the unit test asserting that no table of its migrations is forgotten.
//!
//! Rows are never deleted (docs/design.md, section 2): a rollback INSERTS
//! tombstones, see [`tombstone_sql`].

use anyhow::{bail, Context, Result};
use std::{collections::HashMap, sync::OnceLock};

/// Name of the column holding the block number in a block scoped table:
/// `number` in `blocks`, `block_number` everywhere else.
pub fn block_number_column(table: &str) -> &'static str {
    if table == "blocks" {
        "number"
    } else {
        "block_number"
    }
}

/// Columns of every table created by the embedded migrations, in DDL
/// order. The migrations are the single source of truth: nothing in Rust
/// repeats a column list.
fn table_columns() -> &'static HashMap<String, Vec<String>> {
    static COLUMNS: OnceLock<HashMap<String, Vec<String>>> =
        OnceLock::new();

    COLUMNS.get_or_init(|| {
        super::migrate::embedded()
            .map(|migrations| {
                migrations
                    .iter()
                    .flat_map(|migration| {
                        tables_with_columns(&migration.sql)
                    })
                    .collect()
            })
            // A binary with a broken migration set does not get this far:
            // the build script and the migrator both refuse it.
            .unwrap_or_default()
    })
}

/// The statement that removes the rows of `[from_block, to_block)` (open
/// ended when `to_block` is `None`) of `chain` from `table`, WITHOUT a
/// delete: it inserts, server side, a copy of every live row in range with
/// `_version = version` and `is_deleted = 1`. `FINAL` hides a row whose
/// latest version is a tombstone; a canonical row streamed afterwards has
/// the same key and a newer `_version`, so it wins; keys the canonical
/// block no longer has stay dead. Materialized views see the insert and
/// tombstone the side tables.
///
/// - `version` must be greater than the `_version` of the rows
///   (`db::next_version()`).
/// - Idempotent: `FINAL` does not return rows that are already dead, so a
///   re-run adds nothing for them.
/// - Works for any table of the migrations that has `chain`, a block
///   number column, `_version` and `is_deleted` (the DEX tables too). Only
///   ever call it for base tables, never for a view fed side table.
///
/// Every other column is copied as is, by an explicit column list taken
/// from the migration DDL, so the copy has the same sorting key AND the
/// same partition (its `timestamp`) as the row it kills.
pub fn tombstone_sql(
    table: &str,
    chain: u64,
    from_block: u64,
    to_block: Option<u64>,
    version: u64,
) -> Result<String> {
    let block_column = block_number_column(table);

    if !has_column(table, block_column) {
        bail!("table '{table}' has no '{block_column}' column");
    }

    let upper = to_block
        .map(|to| format!(" AND `{block_column}` < {to}"))
        .unwrap_or_default();

    tombstone_sql_where(
        table,
        &format!(
            "chain = {chain} AND `{block_column}` >= {from_block}{upper}"
        ),
        version,
    )
}

/// [`tombstone_sql`] over an arbitrary `predicate` (already rendered,
/// without the `WHERE`). The rows a purge has to remove from a side table
/// are addressed by the side table's OWN block column plus the extra filter
/// of the base table it is fed from, which is not what the default
/// predicate builds - see `pipeline::store`.
///
/// Works for any table of the migrations with `chain`, `_version` and
/// `is_deleted`; the predicate is the caller's responsibility.
pub fn tombstone_sql_where(
    table: &str,
    predicate: &str,
    version: u64,
) -> Result<String> {
    let columns = table_columns().get(table).with_context(|| {
        format!("no table '{table}' in the migrations")
    })?;

    for required in ["chain", "_version", "is_deleted"] {
        if !columns.iter().any(|column| column == required) {
            bail!("table '{table}' has no '{required}' column");
        }
    }

    let quoted: Vec<String> =
        columns.iter().map(|column| format!("`{column}`")).collect();

    // Positional, without aliases: an alias named like a column would
    // shadow that column in the WHERE clause.
    let values: Vec<String> = columns
        .iter()
        .map(|column| match column.as_str() {
            "_version" => format!("toUInt64({version})"),
            "is_deleted" => "toUInt8(1)".to_string(),
            _ => format!("`{column}`"),
        })
        .collect();

    Ok(format!(
        "INSERT INTO `{table}` ({}) SELECT {} FROM `{table}` FINAL \
         WHERE {predicate}",
        quoted.join(", "),
        values.join(", "),
    ))
}

/// Does `table` of the embedded migrations have `column`?
pub fn has_column(table: &str, column: &str) -> bool {
    table_columns()
        .get(table)
        .is_some_and(|columns| columns.iter().any(|name| name == column))
}

/// `(target, source)` of every INCREMENTAL materialized view in `sql`
/// (`CREATE MATERIALIZED VIEW .. TO <target> AS SELECT .. FROM <source>`).
/// Refreshable views recompute their target instead of being fed by an
/// insert, so they are left out.
///
/// This is how the purge learns which side tables a base table feeds: the
/// migrations are the single source of truth, exactly like the column
/// lists above.
pub fn view_sources(sql: &str) -> Vec<(String, String)> {
    let unquote = |name: &str| name.trim_matches('`').to_string();

    split_sql_statements(sql)
        .into_iter()
        .map(|statement| {
            statement.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|statement| {
            statement.starts_with("CREATE MATERIALIZED VIEW")
                && !statement.contains(" REFRESH ")
        })
        .filter_map(|statement| {
            let (_, rest) = statement.split_once(" TO ")?;
            let target = unquote(rest.split(' ').next()?);

            // The first `FROM` that names a TABLE. Several views select
            // from a subquery (`FROM ( SELECT *, arrayJoin(..) FROM t )`),
            // so `FROM (` is skipped and the search continues inside.
            let source = statement
                .match_indices(" FROM ")
                .filter_map(|(at, marker)| {
                    let word = statement[at + marker.len()..]
                        .split_whitespace()
                        .next()?;
                    (word != "(").then(|| unquote(word))
                })
                .next()?;

            (!target.is_empty() && !source.is_empty())
                .then_some((target, source))
        })
        .collect()
}

/// [`view_sources`] over every embedded migration, as `source -> targets`
/// (a base table can feed several side tables; a side table has exactly
/// one source).
pub fn view_targets() -> &'static HashMap<String, Vec<String>> {
    static TARGETS: OnceLock<HashMap<String, Vec<String>>> =
        OnceLock::new();

    TARGETS.get_or_init(|| {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();

        let migrations = super::migrate::embedded().unwrap_or_default();
        for migration in &migrations {
            for (target, source) in view_sources(&migration.sql) {
                let targets = map.entry(source).or_default();
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }

        map
    })
}

/// Number of LIVE rows of `[from_block, to_block)` in `table`. A purge
/// uses it to verify its tombstones: ClickHouse gives no read-your-writes
/// guarantee right after an INSERT returns (seen on 25.12: a part can stay
/// invisible to the next query for a few ms), so a tombstone statement
/// issued right after a flush can miss rows. Re-issue [`tombstone_sql`]
/// until this is 0: it is idempotent and needs no lock.
pub fn live_rows_sql(
    table: &str,
    chain: u64,
    from_block: u64,
    to_block: Option<u64>,
) -> String {
    let block_column = block_number_column(table);

    let upper = to_block
        .map(|to| format!(" AND `{block_column}` < {to}"))
        .unwrap_or_default();

    format!(
        "SELECT count() FROM `{table}` FINAL \
         WHERE chain = {chain} AND `{block_column}` >= {from_block}{upper}"
    )
}

/// Earliest `timestamp` of the rows of `[from_block, to_block)` in
/// `table`, as unix seconds (0 when there is none). Deliberately WITHOUT
/// `FINAL`: tombstoned rows count too, so a purge that crashed half way
/// and is re-run computes the same `from_ts` for its aggregates repair as
/// the first attempt did.
pub fn min_timestamp_sql(
    table: &str,
    chain: u64,
    from_block: u64,
    to_block: Option<u64>,
) -> String {
    let block_column = block_number_column(table);

    let upper = to_block
        .map(|to| format!(" AND `{block_column}` < {to}"))
        .unwrap_or_default();

    format!(
        "SELECT toUInt32(min(timestamp)) FROM `{table}` \
         WHERE chain = {chain} AND `{block_column}` >= {from_block}{upper}"
    )
}

/// Removes `-- line` and `/* block */` comments, leaving string literals
/// and quoted identifiers untouched.
pub fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' | '`' | '"' => {
                out.push(c);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            out.push(escaped);
                        }
                    } else if inner == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                for skipped in chars.by_ref() {
                    if previous == '*' && skipped == '/' {
                        break;
                    }
                    previous = skipped;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }

    out
}

/// Statements of a migration file WITHOUT any comment (the migrator's own
/// splitter keeps the comments inside a statement, which is right for
/// running it and wrong for picking it apart). Empty when the file does
/// not even split, which the migrator reports properly at build time.
pub fn split_sql_statements(sql: &str) -> Vec<String> {
    super::migrate::split_statements(&strip_sql_comments(sql))
        .unwrap_or_default()
}

/// Splits `body` at commas that are not nested in parentheses.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote: Option<char> = None;

    for (index, c) in body.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '`' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                parts.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&body[start..]);

    parts
}

/// The text between the parenthesis opening at `open` and its match.
fn parenthesized(sql: &str, open: usize) -> Option<&str> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;

    for (index, c) in sql[open..].char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '`' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(&sql[open + 1..open + index]);
                }
            }
            _ => {}
        }
    }

    None
}

/// `(table, columns)` of every `CREATE TABLE` statement in `sql`.
/// Materialized views, views, indexes, projections and constraints are not
/// columns / tables.
pub fn tables_with_columns(sql: &str) -> Vec<(String, Vec<String>)> {
    let mut tables = Vec::new();

    for statement in split_sql_statements(sql) {
        let mut words = statement.split_whitespace();

        if !words.next().is_some_and(|w| w.eq_ignore_ascii_case("create"))
            || !words
                .next()
                .is_some_and(|w| w.eq_ignore_ascii_case("table"))
        {
            continue;
        }

        let Some(open) = statement.find('(') else { continue };

        // `CREATE TABLE [IF NOT EXISTS] [db.]name (`
        let Some(name) = statement[..open]
            .split_whitespace()
            .last()
            .map(|name| name.rsplit('.').next().unwrap_or(name))
            .map(|name| name.trim_matches('`').to_string())
        else {
            continue;
        };

        let Some(body) = parenthesized(&statement, open) else { continue };

        let columns = split_top_level(body)
            .into_iter()
            .filter_map(|definition| {
                let first = definition.split_whitespace().next()?;
                let is_column = !["index", "projection", "constraint"]
                    .iter()
                    .any(|keyword| first.eq_ignore_ascii_case(keyword));
                is_column.then(|| first.trim_matches('`').to_string())
            })
            .collect();

        tables.push((name, columns));
    }

    tables
}

/// Names of the tables created in `sql` that have a block number column
/// (`block_number`, or `number` for `blocks`), in file order. Every one of
/// them must be listed in its module's `BASE_TABLES` or `SIDE_TABLES`,
/// which each module's own unit test asserts.
pub fn tables_with_block_number(sql: &str) -> Vec<String> {
    tables_with_columns(sql)
        .into_iter()
        .filter(|(table, columns)| {
            let column = block_number_column(table);
            columns.iter().any(|name| name == column)
        })
        .map(|(table, _)| table)
        .collect()
}

#[cfg(test)]
pub(crate) mod test_support {
    /// The core migrations, in order.
    pub const CORE_MIGRATIONS: [(&str, &str); 4] = [
        (
            "0001_core_tables.sql",
            include_str!("../../migrations/0001_core_tables.sql"),
        ),
        (
            "0002_read_path.sql",
            include_str!("../../migrations/0002_read_path.sql"),
        ),
        (
            "0003_core_aggregates.sql",
            include_str!("../../migrations/0003_core_aggregates.sql"),
        ),
        (
            "0004_reorgs_checkpoints.sql",
            include_str!("../../migrations/0004_reorgs_checkpoints.sql"),
        ),
    ];

    /// SELECT of the materialized view writing `TO <table>`, whitespace
    /// normalized. Panics when there is none or more than `nth + 1`.
    pub fn view_selects(table: &str) -> Vec<String> {
        let marker = format!(" TO {table} AS ");

        CORE_MIGRATIONS
            .iter()
            .flat_map(|(_, sql)| super::split_sql_statements(sql))
            .map(|statement| {
                statement.split_whitespace().collect::<Vec<_>>().join(" ")
            })
            .filter(|statement| {
                statement.starts_with("CREATE MATERIALIZED VIEW")
            })
            .filter_map(|statement| {
                statement
                    .split_once(&marker)
                    .map(|(_, select)| select.to_string())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tombstone_ranges_and_errors() {
        let sql = tombstone_sql("logs", 1, 10, Some(20), 5).unwrap();
        assert!(sql.ends_with(
            "WHERE chain = 1 AND `block_number` >= 10 AND `block_number` < 20"
        ));

        let sql = tombstone_sql("blocks", 1, 10, Some(20), 5).unwrap();
        assert!(sql.ends_with(
            "WHERE chain = 1 AND `number` >= 10 AND `number` < 20"
        ));

        // Unknown tables and tables that can not be tombstoned are errors,
        // never a malformed statement.
        assert!(tombstone_sql("nope", 1, 0, None, 1).is_err());
        assert!(tombstone_sql("tokens", 1, 0, None, 1).is_err());
        assert!(tombstone_sql("reorgs", 1, 0, None, 1).is_err());
        assert!(tombstone_sql("daily_block_stats", 1, 0, None, 1).is_err());

        assert_eq!(
            min_timestamp_sql("blocks", 7, 3, Some(9)),
            "SELECT toUInt32(min(timestamp)) FROM `blocks` WHERE chain = 7 \
             AND `number` >= 3 AND `number` < 9"
        );
        assert!(!min_timestamp_sql("logs", 7, 3, None).contains("FINAL"));

        assert_eq!(
            live_rows_sql("blocks", 7, 3, Some(9)),
            "SELECT count() FROM `blocks` FINAL WHERE chain = 7 AND \
             `number` >= 3 AND `number` < 9"
        );
        assert!(live_rows_sql("logs", 7, 3, None)
            .ends_with("AND `block_number` >= 3"));
    }

    #[test]
    fn statement_splitting_survives_semicolons_in_strings_and_comments() {
        let sql = "-- a comment; with a semicolon\n\
                   CREATE TABLE a (x String DEFAULT ';', `y;z` UInt8);\n\
                   /* block; comment */ CREATE TABLE b (n UInt8) -- tail;\n;\n";

        let statements = split_sql_statements(sql);

        assert_eq!(statements.len(), 2, "{statements:?}");
        assert!(statements[0].contains("DEFAULT ';'"));
        assert!(statements[0].contains("`y;z`"));
        assert!(statements[1].starts_with("CREATE TABLE b"));
    }

    #[test]
    fn finds_tables_and_columns() {
        let sql = "
            CREATE TABLE IF NOT EXISTS indexer.`things` (
              chain UInt64,
              `block_number` UInt64 CODEC(Delta, ZSTD), -- the block
              pair Tuple(FixedString(20), Array(FixedString(32))),
              INDEX idx block_number TYPE minmax GRANULARITY 1
            ) ENGINE = MergeTree ORDER BY (chain, block_number);
            CREATE TABLE no_blocks (chain UInt64, number_of_things UInt8)
              ENGINE = MergeTree ORDER BY chain;
            CREATE TABLE blocks (chain UInt64, number UInt64)
              ENGINE = MergeTree ORDER BY chain;
            CREATE MATERIALIZED VIEW mv TO things AS
              SELECT chain, block_number FROM other;
        ";

        assert_eq!(
            tables_with_columns(sql)[0],
            (
                "things".to_string(),
                vec![
                    "chain".to_string(),
                    "block_number".to_string(),
                    "pair".to_string()
                ]
            )
        );
        assert_eq!(
            tables_with_block_number(sql),
            vec!["things", "blocks"]
        );
    }

    /// A table partitioned by the MONTH of a column must declare that
    /// column `DateTime('UTC')`.
    ///
    /// `toYYYYMM` of a plain `DateTime` takes the month in the SERVER's
    /// timezone, while the writer splits a flush into whole UTC months so
    /// that no insert touches more monthly partitions than ClickHouse
    /// allows (`db::flush_windows`, `MAX_MONTHS_PER_FLUSH`). On a server
    /// that is not on UTC the two disagree at every month boundary, so a
    /// 90-UTC-month slice could land in 91 partitions
    /// (review round 4, MINOR 18).
    #[test]
    fn every_monthly_partition_key_is_in_utc() {
        let mut checked = 0;

        for migration in super::super::migrate::embedded().unwrap() {
            for statement in split_sql_statements(&migration.sql) {
                let normalized = statement
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");

                let Some(rest) =
                    normalized.split("PARTITION BY toYYYYMM(").nth(1)
                else {
                    continue;
                };
                let Some(column) = rest.split(')').next() else {
                    continue;
                };
                // Already explicit: `toYYYYMM(x, 'UTC')`.
                if column.contains(',') {
                    checked += 1;
                    continue;
                }

                let table = normalized
                    .split("CREATE TABLE IF NOT EXISTS ")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("?");

                assert!(
                    normalized
                        .contains(&format!("{column} DateTime('UTC')")),
                    "{table} is PARTITION BY toYYYYMM({column}) but \
                     '{column}' is not DateTime('UTC'): the month would \
                     be taken in the server's timezone, not in UTC"
                );
                checked += 1;
            }
        }

        assert!(checked > 30, "only {checked} monthly partition keys");
    }
}
