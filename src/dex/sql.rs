//! Test helpers: the DEX migrations as text.

/// The chain registry (docs/design.md §13). NOT a DEX table - it is shared
/// by every analytics module - so it is not part of [`MIGRATIONS`], but the
/// integration tests apply it, because the views format ids through it.
pub const CHAINS_SQL: &str =
    include_str!("../../migrations/0006_chains.sql");

/// Migration 0004, which creates `reorgs` and, next to it, the SHARED
/// `epoch_floor_v` that every analytics module joins for the validity rule
/// (docs/design.md §1, "Aggregates"). Like [`CHAINS_SQL`] it is not a DEX
/// object and not part of [`MIGRATIONS`], but 0011 and 0012 read the view,
/// so the integration tests have to create it the way the migrator does.
pub const REORGS_SQL: &str =
    include_str!("../../migrations/0004_reorgs_checkpoints.sql");

/// The `reorgs` table and the `epoch_floor_v` view of [`REORGS_SQL`], in
/// that order. Sliced out of the real migration instead of copied, so the
/// test schema can never drift from it - the hand-written copy this
/// replaced declared `from_ts DateTime` where 0004 says `DateTime('UTC')`.
/// The rest of 0004 (checkpoints, the core aggregate views) needs core
/// tables the DEX tests do not create.
pub fn reorg_prerequisites() -> Vec<String> {
    let wanted = [
        "CREATE TABLE IF NOT EXISTS reorgs ",
        "CREATE VIEW IF NOT EXISTS epoch_floor_v ",
    ];

    let found: Vec<String> = statements(REORGS_SQL)
        .into_iter()
        .filter(|statement| {
            let normalized = normalize(statement);
            wanted.iter().any(|head| normalized.starts_with(head))
        })
        .collect();

    assert_eq!(found.len(), wanted.len(), "0004 changed shape");
    found
}

pub const TABLES_SQL: &str =
    include_str!("../../migrations/0010_dex_tables.sql");
pub const AGGREGATES_SQL: &str =
    include_str!("../../migrations/0011_dex_aggregates.sql");
pub const VIEWS_SQL: &str =
    include_str!("../../migrations/0012_dex_views.sql");

/// (file name, contents) in application order.
pub const MIGRATIONS: &[(&str, &str)] = &[
    ("0010_dex_tables.sql", TABLES_SQL),
    ("0011_dex_aggregates.sql", AGGREGATES_SQL),
    ("0012_dex_views.sql", VIEWS_SQL),
];

/// Statements of a migration without `--` comments. The DEX migrations
/// keep `;` out of comments and strings (asserted by a test), so a plain
/// split is exact.
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
