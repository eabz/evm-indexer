//! What the code needs to know about the SQL schema in `migrations/`.
//!
//! The list of block scoped tables is code, not convention: a rollback
//! (`purge_range`) has to delete from every one of them, and a unit test
//! asserts that no table of the migrations is forgotten.

/// Every core table (base + read-path side tables) whose rows belong to a
/// block, in the order a purge must delete from them: children and side
/// tables first, `blocks` LAST. While the old `blocks` row exists a crashed
/// purge is detected and re-run, so the commit marker goes last, mirroring
/// the insert order.
///
/// Not listed on purpose: `tokens` (not block scoped) and the aggregates of
/// `0003` (repaired per bucket, see `db::derived`). The DEX tables are in
/// `dex::BLOCK_SCOPED_TABLES`.
pub const BLOCK_SCOPED_TABLES: &[&str] = &[
    // Read-path side tables (0002).
    "tx_lookup",
    "transactions_by_address",
    "logs_by_address",
    "erc20_transfers_by_account",
    "nft_transfers_by_account",
    "traces_by_tx",
    // Children of a block (0001).
    "erc20_transfers",
    "erc721_transfers",
    "erc1155_transfers",
    "logs",
    "traces",
    "contracts",
    "withdrawals",
    "transactions",
    // The block itself: lookup first, commit marker last.
    "block_lookup",
    "blocks",
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
    pub const CORE_MIGRATIONS: [(&str, &str); 3] = [
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
    ];
}

#[cfg(test)]
mod tests {
    use super::{test_support::CORE_MIGRATIONS, *};
    use std::collections::HashSet;

    #[test]
    fn every_table_with_a_block_number_is_block_scoped() {
        let listed: HashSet<&str> =
            BLOCK_SCOPED_TABLES.iter().copied().collect();
        assert_eq!(
            listed.len(),
            BLOCK_SCOPED_TABLES.len(),
            "duplicate entry"
        );

        let mut found = HashSet::new();
        for (file, sql) in CORE_MIGRATIONS {
            for table in tables_with_block_number(sql) {
                assert!(
                    listed.contains(table.as_str()),
                    "{table} ({file}) has a block number column but is \
                     not in BLOCK_SCOPED_TABLES: a rollback would leave \
                     its rows behind"
                );
                found.insert(table);
            }
        }

        // And nothing is listed that does not exist.
        for table in BLOCK_SCOPED_TABLES {
            assert!(
                found.contains(*table),
                "{table} is not in migrations"
            );
        }
    }

    #[test]
    fn blocks_is_purged_last_and_side_tables_before_their_base() {
        assert_eq!(BLOCK_SCOPED_TABLES.last(), Some(&"blocks"));

        let position = |table: &str| {
            BLOCK_SCOPED_TABLES.iter().position(|t| *t == table).unwrap()
        };
        for (side, base) in [
            ("tx_lookup", "transactions"),
            ("transactions_by_address", "transactions"),
            ("logs_by_address", "logs"),
            ("erc20_transfers_by_account", "erc20_transfers"),
            ("nft_transfers_by_account", "erc721_transfers"),
            ("nft_transfers_by_account", "erc1155_transfers"),
            ("traces_by_tx", "traces"),
            ("block_lookup", "blocks"),
        ] {
            assert!(position(side) < position(base), "{side} / {base}");
        }

        assert_eq!(block_number_column("blocks"), "number");
        assert_eq!(block_number_column("logs"), "block_number");
    }

    #[test]
    fn migrations_follow_the_schema_rules() {
        for (file, sql) in CORE_MIGRATIONS {
            let lowered = strip_sql_comments(sql).to_lowercase();

            // The database comes from the connection.
            assert!(!lowered.contains("indexer."), "{file}: db prefix");
            assert!(!lowered.contains("create database"), "{file}");
            // No projections (they break lightweight deletes), no bloom
            // filter zoo.
            assert!(!lowered.contains("projection"), "{file}");
            assert!(!lowered.contains("bloom_filter"), "{file}");
            // Distinct counts are states, never uniqExact in a sum.
            assert!(!lowered.contains("uniqexact"), "{file}");

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

        // Every block scoped table carries chain + _version and is a
        // ReplacingMergeTree(_version).
        let all: String = CORE_MIGRATIONS
            .iter()
            .map(|(_, sql)| *sql)
            .collect::<Vec<_>>()
            .join("\n");

        let statements = split_sql_statements(&all);

        for (table, columns) in tables_with_columns(&all) {
            if !BLOCK_SCOPED_TABLES.contains(&table.as_str()) {
                continue;
            }
            for required in ["chain", "_version"] {
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
                ddl.contains("ENGINE = ReplacingMergeTree(_version)"),
                "{table}"
            );
            assert!(
                ddl.contains(
                    "do_not_merge_across_partitions_select_final = 1"
                ),
                "{table}"
            );
        }
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
