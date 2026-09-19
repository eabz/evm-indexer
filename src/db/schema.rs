//! What the code needs to know about the SQL schema in `migrations/`.
//!
//! The lists of block scoped tables are code, not convention: a rollback
//! has to reach every one of them, and a unit test asserts that no table of
//! the migrations is forgotten.
//!
//! Rows are never deleted (docs/design.md, section 2): a rollback INSERTS
//! tombstones, see [`tombstone_sql`].

use anyhow::{bail, Context, Result};
use std::{collections::HashMap, sync::OnceLock};

/// Core tables whose rows belong to a block and are written by the indexer,
/// in the order a purge must tombstone them: children first, `blocks`
/// LAST. While the old `blocks` row is alive a crashed purge is detected
/// and re-run (which is harmless), so the commit marker goes last,
/// mirroring the insert order.
///
/// Not listed on purpose: `tokens` (not block scoped), the aggregates of
/// `0003` (repaired per epoch, see `db::derived`), `checkpoints` (ranges,
/// handled by the pipeline) and the `contracts` view. The DEX tables are in
/// `dex::BLOCK_SCOPED_TABLES`.
pub const BASE_TABLES: &[&str] = &[
    "erc20_transfers",
    "erc721_transfers",
    "erc1155_transfers",
    "logs",
    "withdrawals",
    "transactions",
    "blocks",
];

/// Read-path side tables, fed by materialized views. NEVER tombstoned (or
/// written) directly: their views pass `_version` and `is_deleted` through,
/// so a tombstone inserted into the base table tombstones exactly the side
/// rows of that base row.
pub const SIDE_TABLES: &[&str] = &[
    "tx_lookup",
    "block_lookup",
    "transactions_by_address",
    "logs_by_address",
    "erc20_transfers_by_account",
    "nft_transfers_by_account",
];

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
/// them must be listed in a `BLOCK_SCOPED_TABLES`.
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
    use super::{
        test_support::{view_selects, CORE_MIGRATIONS},
        *,
    };
    use std::collections::HashSet;

    fn all_sql() -> String {
        CORE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_table_with_a_block_number_is_classified() {
        let listed: HashSet<&str> =
            BASE_TABLES.iter().chain(SIDE_TABLES).copied().collect();
        assert_eq!(
            listed.len(),
            BASE_TABLES.len() + SIDE_TABLES.len(),
            "a table is listed twice"
        );

        let mut found = HashSet::new();
        for (file, sql) in CORE_MIGRATIONS {
            for table in tables_with_block_number(sql) {
                assert!(
                    listed.contains(table.as_str()),
                    "{table} ({file}) has a block number column but is \
                     neither in BASE_TABLES nor in SIDE_TABLES: a rollback \
                     would leave its rows behind"
                );
                found.insert(table);
            }
        }

        // And nothing is listed that does not exist.
        for table in listed {
            assert!(found.contains(table), "{table} is not in migrations");
        }
    }

    #[test]
    fn base_tables_are_written_directly_and_side_tables_only_by_views() {
        assert_eq!(BASE_TABLES.last(), Some(&"blocks"));
        assert_eq!(block_number_column("blocks"), "number");
        assert_eq!(block_number_column("logs"), "block_number");

        for table in BASE_TABLES {
            assert!(
                view_selects(table).is_empty(),
                "{table} is fed by a materialized view: it is a side table"
            );
        }

        for table in SIDE_TABLES {
            let selects = view_selects(table);
            assert!(
                !selects.is_empty(),
                "{table} has no materialized view"
            );

            for select in selects {
                // The pass-through that makes tombstones propagate.
                for column in ["epoch", "_version", "is_deleted"] {
                    assert!(
                        select.contains(&format!(" {column}")),
                        "the view of {table} does not pass {column} through"
                    );
                }
                // A filter would stop tombstones from reaching the table.
                assert!(
                    !select.contains("is_deleted ="),
                    "the view of {table} filters on is_deleted"
                );
                // Fed from a base table, never from another side table.
                assert!(
                    BASE_TABLES
                        .iter()
                        .any(|base| select
                            .contains(&format!("FROM {base}"))),
                    "{table}: {select}"
                );
            }
        }
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (file, sql) in CORE_MIGRATIONS {
            let lowered = strip_sql_comments(sql).to_lowercase();

            // The database comes from the connection.
            assert!(!lowered.contains("indexer."), "{file}: db prefix");
            assert!(!lowered.contains("create database"), "{file}");
            // No projections, no bloom filter zoo.
            assert!(!lowered.contains("projection"), "{file}");
            assert!(!lowered.contains("bloom_filter"), "{file}");
            // Distinct counts are states, never uniqExact in a sum.
            assert!(!lowered.contains("uniqexact"), "{file}");
            // Traces are out of scope (docs/design.md, section 9).
            assert!(!lowered.contains("trace"), "{file}");

            for statement in split_sql_statements(sql) {
                let lowered = statement.to_lowercase();
                assert!(
                    lowered.starts_with("create table if not exists ")
                        || lowered.starts_with(
                            "create materialized view if not exists "
                        )
                        || lowered
                            .starts_with("create view if not exists "),
                    "{file}: not idempotent: {statement}"
                );
            }
        }

        let all = all_sql();
        let statements = split_sql_statements(&all);

        for (table, columns) in tables_with_columns(&all) {
            let is_base = BASE_TABLES.contains(&table.as_str());
            let is_side = SIDE_TABLES.contains(&table.as_str());
            if !is_base && !is_side {
                continue;
            }

            for required in ["chain", "epoch", "_version", "is_deleted"] {
                assert!(
                    columns.iter().any(|c| c == required),
                    "{table} lacks {required}"
                );
            }

            let ddl = statements
                .iter()
                .find(|s| {
                    s.starts_with(&format!(
                        "CREATE TABLE IF NOT EXISTS {table} "
                    ))
                })
                .unwrap();
            assert!(
                ddl.contains(
                    "ENGINE = ReplacingMergeTree(_version, is_deleted)"
                ),
                "{table}"
            );
            assert!(
                ddl.contains(
                    "do_not_merge_across_partitions_select_final = 1"
                ),
                "{table}"
            );
            assert!(ddl.contains("is_deleted UInt8 DEFAULT 0"), "{table}");
            assert!(ddl.contains("epoch UInt32 DEFAULT 0"), "{table}");

            // 50+ chains in one database: base tables by month ONLY, side
            // tables by chain (lookups do not know the month).
            let partition = if is_base {
                "PARTITION BY toYYYYMM(timestamp) "
            } else {
                "PARTITION BY chain "
            };
            let normalized =
                ddl.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                normalized.contains(partition),
                "{table}: {partition}"
            );
        }
    }

    #[test]
    fn contracts_is_a_view_over_successful_deployments() {
        let all = all_sql();

        assert!(tables_with_columns(&all)
            .iter()
            .all(|(table, _)| table != "contracts"));

        let view = split_sql_statements(&all)
            .into_iter()
            .find(|s| {
                s.starts_with("CREATE VIEW IF NOT EXISTS contracts ")
            })
            .expect("contracts view");

        assert!(view.contains("FROM transactions FINAL"));
        // NULL (pre-Byzantium receipts have no status) counts as success.
        assert!(view.contains("ifNull(status, 'success') = 'success'"));
        assert!(view.contains("contract_created != toFixedString('', 20)"));
    }

    #[test]
    fn tombstones_copy_every_column_of_the_migration_ddl() {
        let all = all_sql();
        let ddl: HashMap<String, Vec<String>> =
            tables_with_columns(&all).into_iter().collect();

        for table in BASE_TABLES {
            let sql = tombstone_sql(table, 56, 100, None, 1_700).unwrap();
            let columns = &ddl[*table];

            // INSERT list == DDL columns, in order, all quoted.
            let list = sql
                .split_once(" (")
                .and_then(|(_, rest)| rest.split_once(") SELECT "))
                .map(|(list, _)| list)
                .unwrap();
            let expected: Vec<String> =
                columns.iter().map(|c| format!("`{c}`")).collect();
            assert_eq!(list, expected.join(", "), "{table}");

            // SELECT list: the same columns, except the two that make it
            // a tombstone.
            let select = sql
                .split_once(") SELECT ")
                .and_then(|(_, rest)| rest.split_once(" FROM "))
                .map(|(select, _)| select)
                .unwrap();
            let expected: Vec<String> = columns
                .iter()
                .map(|c| match c.as_str() {
                    "_version" => "toUInt64(1700)".to_string(),
                    "is_deleted" => "toUInt8(1)".to_string(),
                    _ => format!("`{c}`"),
                })
                .collect();
            assert_eq!(select, expected.join(", "), "{table}");

            let block_column = block_number_column(table);
            assert!(
                sql.ends_with(&format!(
                    " FROM `{table}` FINAL WHERE chain = 56 AND \
                     `{block_column}` >= 100"
                )),
                "{sql}"
            );
            assert!(!sql.to_uppercase().contains("DELETE "), "{sql}");
        }
    }

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
}
