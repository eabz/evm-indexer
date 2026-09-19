//! Test helpers: the launchpad migrations as text.

pub const TABLES_SQL: &str =
    include_str!("../../migrations/0030_launchpad_tables.sql");
pub const AGGREGATES_SQL: &str =
    include_str!("../../migrations/0031_launchpad_aggregates.sql");
pub const VIEWS_SQL: &str =
    include_str!("../../migrations/0032_launchpad_views.sql");

/// (file name, contents) in application order.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("0030_launchpad_tables.sql", TABLES_SQL),
    ("0031_launchpad_aggregates.sql", AGGREGATES_SQL),
    ("0032_launchpad_views.sql", VIEWS_SQL),
];

/// Statements of a migration without `--` comments. The launchpad
/// migrations keep `;` out of comments and strings (asserted by a test),
/// so a plain split is exact.
pub fn statements(sql: &str) -> Vec<String> {
    let without_comments: String = sql
        .lines()
        .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
        .collect::<Vec<_>>()
        .join("\n");

    without_comments
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Collapses every run of whitespace into one space.
pub fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}
