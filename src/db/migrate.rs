//! Embedded, versioned schema migrations.
//!
//! Every `migrations/NNNN_name.sql` is compiled into the binary by
//! `build.rs`. At startup (and through `indexer migrate`) the runner:
//!
//! 1. creates the database named in the url when it is missing,
//! 2. creates `schema_migrations (version, name, checksum, applied_at)`,
//! 3. refuses to go on when an applied migration's checksum differs from
//!    the embedded one, or when the database holds a version this binary
//!    does not know (binary older than the schema),
//! 4. applies what is pending in version order, one statement at a time,
//!    and records a migration only after its last statement succeeded.
//!
//! DDL in migration files carries no database prefix: statements run
//! against the database of the connection.
//!
//! # Failure model
//!
//! ClickHouse DDL is not transactional. A migration that fails midway is
//! NOT recorded and leaves its earlier statements applied. Migrations are
//! written idempotently (`IF NOT EXISTS` / `IF EXISTS`), so re-running after
//! fixing the cause is safe.
//!
//! # Concurrent startup
//!
//! One indexer process per chain may start at the same time against the
//! same database (`docker compose up` with several chains does exactly
//! that on the first boot). Two layers:
//!
//! **Correctness comes from idempotency, never from a lock**, because
//! ClickHouse has no advisory locks and nothing survives a `kill -9`:
//!
//! - `CREATE DATABASE` / `CREATE TABLE schema_migrations` use
//!   `IF NOT EXISTS`; ClickHouse serializes DDL on the same object.
//! - `schema_migrations` is re-read before every migration AND before every
//!   statement: a migration another process finished in the meantime is
//!   skipped (after its checksum was verified), which keeps a lagging
//!   runner from replaying old statements on top of a newer schema.
//! - Two runners inside the same migration both execute idempotent DDL:
//!   the second one is a no-op. A statement that still reports "already
//!   exists" (DDL written without `IF NOT EXISTS`) is tolerated with a
//!   warning.
//! - Recording is idempotent: the table is a `ReplacingMergeTree` ordered by
//!   `(version, checksum)` and always read with `FINAL`. Two runners
//!   recording the same migration collapse into one row, while two
//!   DIFFERENT binaries recording different content for one version leave
//!   two rows, which every later start reports as a checksum conflict.
//!
//! **A best-effort lock keeps the common case tidy.** Only when something
//! is pending, a runner takes `schema_migrations_lock` by creating that
//! table WITHOUT `IF NOT EXISTS`: table creation is atomic, exactly one
//! creator wins. The others poll until nothing is pending (then go on
//! without ever holding the lock) or until the lock is free. The holder
//! touches the lock before every statement and drops it when done, also on
//! failure. A lock left behind by a killed process is taken over once it
//! was not touched for [`LOCK_STALE`]; a wrong takeover (one statement
//! slower than that) only degrades to the lock-free behaviour above. So
//! with the lock each statement normally runs once (seed `INSERT`s are not
//! duplicated); without it nothing breaks.

mod filename;

pub use filename::parse_filename;

use super::DatabaseParams;
use anyhow::{anyhow, bail, Context, Result};
use clickhouse::{error::Error as ClickhouseError, Client, Row};
use log::{info, warn};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fmt, time::Duration};

include!(concat!(env!("OUT_DIR"), "/migrations.rs"));

/// Bookkeeping table, created in the database of the connection.
pub const MIGRATIONS_TABLE: &str = "schema_migrations";

/// Best-effort migration lock, see the module docs.
pub const LOCK_TABLE: &str = "schema_migrations_lock";

/// A lock not touched for this long belongs to a dead process.
pub const LOCK_STALE: Duration = Duration::from_secs(60);

const LOCK_POLL: Duration = Duration::from_millis(250);

const CONNECT_ATTEMPTS: u32 = 10;

/// ClickHouse error codes the runner reacts to.
const CODE_TABLE_ALREADY_EXISTS: u32 = 57;
const CODE_UNKNOWN_TABLE: u32 = 60;
const CODE_UNKNOWN_DATABASE: u32 = 81;
const CODE_DATABASE_ALREADY_EXISTS: u32 = 82;

/// Appended to every failure that can leave a migration half applied.
const RERUN_HINT: &str = "ClickHouse DDL is not transactional: the \
    migration was NOT recorded in schema_migrations and its earlier \
    statements stay applied. Migrations are idempotent (IF NOT EXISTS), so \
    fix the cause and run `indexer migrate` (or restart) again; it is safe.";

/// One versioned migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: u32,
    pub name: String,
    pub sql: String,
}

impl Migration {
    pub fn new(
        version: u32,
        name: impl Into<String>,
        sql: impl Into<String>,
    ) -> Self {
        Self { version, name: name.into(), sql: sql.into() }
    }

    /// `0004_checkpoints`.
    pub fn label(&self) -> String {
        label(self.version, &self.name)
    }

    /// Hex SHA-256 of the content, see [`checksum`].
    pub fn checksum(&self) -> String {
        checksum(&self.sql)
    }
}

fn label(version: u32, name: &str) -> String {
    format!("{version:0width$}_{name}", width = filename::VERSION_DIGITS)
}

/// Hex SHA-256 of a migration's content. Line endings are normalized to
/// `\n` first, so a checkout with `core.autocrlf` does not look tampered.
pub fn checksum(sql: &str) -> String {
    hex::encode(Sha256::digest(sql.replace("\r\n", "\n").as_bytes()))
}

/// The migrations compiled into this binary, sorted by version.
pub fn embedded() -> Result<Vec<Migration>> {
    let migrations = EMBEDDED
        .iter()
        .map(|(file_name, sql)| {
            let (version, name) =
                parse_filename(file_name).map_err(|e| anyhow!(e))?;
            Ok(Migration::new(version, name, *sql))
        })
        .collect::<Result<Vec<_>>>()?;

    validate(&migrations)?;

    Ok(migrations)
}

/// Checks a migration set before anything touches the database: versions
/// strictly ascending (hence unique) and non-zero, every file splits into
/// at least one statement.
pub fn validate(migrations: &[Migration]) -> Result<()> {
    for pair in migrations.windows(2) {
        if pair[0].version >= pair[1].version {
            bail!(
                "migrations are not in strictly ascending version order: \
                 {} comes before {}",
                pair[0].label(),
                pair[1].label()
            );
        }
    }

    for migration in migrations {
        if migration.version == 0 {
            bail!("migration {}: versions start at 1", migration.label());
        }

        let statements = split_statements(&migration.sql)
            .with_context(|| format!("migration {}", migration.label()))?;

        if statements.is_empty() {
            bail!("migration {} holds no statement", migration.label());
        }
    }

    Ok(())
}

// ---- statement splitter ----------------------------------------------------

/// A construct that was opened and never closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitError {
    pub what: &'static str,
    /// 1-based line where the construct starts.
    pub line: usize,
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unterminated {} starting at line {}",
            self.what, self.line
        )
    }
}

impl std::error::Error for SplitError {}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Splits a SQL script into statements on `;`, following the ClickHouse
/// lexer closely enough that a `;` is only a separator where ClickHouse
/// would see one. Not separators:
///
/// - `'strings'` with `\'` and `''` escapes,
/// - `` `quoted` `` and `"quoted"` identifiers (same escapes),
/// - `-- line comments` (also `#!` and `# `),
/// - `/* block /* comments */ */` (nested, like ClickHouse),
/// - `$tag$ heredoc strings $tag$`.
///
/// Statements are returned trimmed, without the `;` and without leading
/// comments; pieces holding only whitespace / comments are dropped.
pub fn split_statements(sql: &str) -> Result<Vec<String>, SplitError> {
    let mut statements = Vec::new();
    // Offset of the first code byte of the statement being scanned.
    let mut start: Option<usize> = None;

    for (piece, range) in lex(sql)? {
        match piece {
            Piece::Separator => {
                if let Some(from) = start.take() {
                    let statement = &sql[from..range.start];
                    statements.push(statement.trim_end().to_string());
                }
            }
            Piece::Code | Piece::Quoted => {
                start.get_or_insert(range.start);
            }
            Piece::Comment | Piece::Space => {}
        }
    }

    if let Some(from) = start {
        statements.push(sql[from..].trim_end().to_string());
    }

    Ok(statements)
}

/// What a stretch of a SQL script is, for [`lex`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Piece {
    /// Keywords, names, numbers, operators.
    Code,
    /// String literal, quoted identifier or heredoc, quotes included.
    Quoted,
    Comment,
    /// A `;` outside of everything else.
    Separator,
    Space,
}

/// Cuts a script into [`Piece`]s covering it entirely. Every delimiter is
/// ASCII, so the ranges always fall on UTF-8 boundaries.
fn lex(
    sql: &str,
) -> Result<Vec<(Piece, std::ops::Range<usize>)>, SplitError> {
    let bytes = sql.as_bytes();
    let unterminated = |what: &'static str, at: usize| SplitError {
        what,
        line: bytes[..at].iter().filter(|&&b| b == b'\n').count() + 1,
    };

    let mut pieces: Vec<(Piece, std::ops::Range<usize>)> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let byte = bytes[i];
        let next = bytes.get(i + 1).copied();

        let (piece, end) = match byte {
            b';' => (Piece::Separator, i + 1),
            b'\'' | b'"' | b'`' => {
                let what = match byte {
                    b'\'' => "string literal",
                    _ => "quoted identifier",
                };
                let end = skip_quoted(bytes, i)
                    .ok_or_else(|| unterminated(what, i))?;
                (Piece::Quoted, end)
            }
            b'-' if next == Some(b'-') => {
                (Piece::Comment, skip_line(bytes, i))
            }
            b'#' if matches!(next, Some(b' ' | b'!')) => {
                (Piece::Comment, skip_line(bytes, i))
            }
            b'/' if next == Some(b'*') => {
                let end = skip_block_comment(bytes, i)
                    .ok_or_else(|| unterminated("block comment", i))?;
                (Piece::Comment, end)
            }
            b'$' if i == 0
                || !(is_word(bytes[i - 1]) || bytes[i - 1] == b'$') =>
            {
                match heredoc_tag(bytes, i) {
                    Some(tag) => {
                        let end = find(bytes, i + tag.len(), tag)
                            .ok_or_else(|| {
                                unterminated("heredoc string", i)
                            })?;
                        (Piece::Quoted, end)
                    }
                    None => (Piece::Code, i + 1),
                }
            }
            _ if byte.is_ascii_whitespace() => (Piece::Space, i + 1),
            _ => (Piece::Code, i + 1),
        };

        match pieces.last_mut() {
            // Runs of code / space are one piece.
            Some((last, range))
                if *last == piece
                    && matches!(piece, Piece::Code | Piece::Space) =>
            {
                range.end = end;
            }
            _ => pieces.push((piece, i..end)),
        }

        i = end;
    }

    Ok(pieces)
}

/// `at` is on `/*`; returns the offset after the matching `*/`. Block
/// comments NEST in ClickHouse (verified on 25.12: `SELECT /* a /* b */ c
/// */ 1` is valid, `SELECT /* a /* b */ 1` is "comment is not closed").
fn skip_block_comment(bytes: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0_usize;
    let mut i = at;

    while i + 1 < bytes.len() {
        match (bytes[i], bytes[i + 1]) {
            (b'/', b'*') => {
                depth += 1;
                i += 2;
            }
            (b'*', b'/') => {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }

    None
}

/// `at` is on the opening quote; returns the offset after the closing one.
/// A doubled quote (`''`) needs no special case: it closes one literal and
/// immediately opens the next, which splits identically.
fn skip_quoted(bytes: &[u8], at: usize) -> Option<usize> {
    let quote = bytes[at];
    let mut i = at + 1;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return Some(i + 1),
            _ => i += 1,
        }
    }

    None
}

/// Offset of the newline ending the line comment at `at` (or the end).
fn skip_line(bytes: &[u8], at: usize) -> usize {
    bytes[at..]
        .iter()
        .position(|&b| b == b'\n')
        .map_or(bytes.len(), |offset| at + offset)
}

/// Offset just after the first `needle` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset + needle.len())
}

/// `$tag$` (tag = word characters, possibly empty) starting at `at`.
fn heredoc_tag(bytes: &[u8], at: usize) -> Option<&[u8]> {
    let len = bytes[at + 1..].iter().take_while(|&&b| is_word(b)).count();
    let end = at + 1 + len;

    (bytes.get(end) == Some(&b'$')).then(|| &bytes[at..=end])
}

/// The clickhouse crate binds `?` placeholders in query text, `??` being a
/// literal `?`. Migration statements are sent verbatim.
fn escape_placeholders(statement: &str) -> String {
    statement.replace('?', "??")
}

// ---- idempotency lint ------------------------------------------------------

/// A statement reduced to what the lint looks at: comments removed, every
/// quoted section replaced by `_`, upper case, single spaces.
pub fn skeleton(statement: &str) -> Result<String, SplitError> {
    let mut code = String::new();

    for (piece, range) in lex(statement)? {
        match piece {
            Piece::Code => code.push_str(&statement[range]),
            Piece::Quoted => code.push_str(" _ "),
            Piece::Comment | Piece::Space | Piece::Separator => {
                code.push(' ')
            }
        }
    }

    // Parentheses and commas are word boundaries for the lint.
    let code = code.replace(['(', ')', ','], " ").to_ascii_uppercase();

    Ok(code.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `ALTER TABLE` clauses that need `IF NOT EXISTS` right after them.
const ALTER_NEEDS_IF_NOT_EXISTS: [&str; 5] = [
    "ADD COLUMN",
    "ADD INDEX",
    "ADD PROJECTION",
    "ADD CONSTRAINT",
    "ADD STATISTICS",
];

/// `ALTER TABLE` clauses that need `IF EXISTS` right after them.
const ALTER_NEEDS_IF_EXISTS: [&str; 8] = [
    "DROP COLUMN",
    "DROP INDEX",
    "DROP PROJECTION",
    "DROP CONSTRAINT",
    "DROP STATISTICS",
    "CLEAR COLUMN",
    "CLEAR INDEX",
    "RENAME COLUMN",
];

/// `ALTER TABLE` clauses that are never safe to replay blindly.
const ALTER_NOT_IDEMPOTENT: [&str; 7] = [
    "UPDATE",
    "ATTACH PARTITION",
    "ATTACH PART",
    "DETACH PARTITION",
    "DETACH PART",
    "MOVE PARTITION",
    "REPLACE PARTITION",
];

/// Why replaying `statement` could fail or change the outcome, `None`
/// when it is idempotent. The runner's failure model (re-run after a
/// partial failure, several processes racing) REQUIRES idempotent
/// statements; a unit test holds every embedded migration to it.
///
/// Accepted: `CREATE ... IF NOT EXISTS` / `CREATE OR REPLACE`, `DROP` /
/// `TRUNCATE ... IF EXISTS`, `ALTER TABLE` with `ADD ... IF NOT EXISTS`,
/// `DROP|CLEAR|RENAME COLUMN ... IF EXISTS`, and every `MODIFY ...`
/// (`MODIFY SETTING`, `MODIFY COLUMN`, `MODIFY TTL`, ...), `RESET
/// SETTING`, `COMMENT COLUMN`, `MATERIALIZE ...`, `DELETE WHERE`; plus
/// statements without lasting effect on the schema (`SELECT`, `SYSTEM`,
/// `OPTIMIZE`, `GRANT`, `REVOKE`, lightweight `DELETE`).
///
/// Refused: plain `INSERT` (seed rows are duplicated by a replay),
/// `RENAME` / `EXCHANGE`, `ATTACH` / `DETACH`, `ALTER ... UPDATE`,
/// partition moves, and anything the lint does not know.
pub fn idempotency_violation(statement: &str) -> Option<String> {
    let code = match skeleton(statement) {
        Ok(code) => code,
        Err(e) => return Some(e.to_string()),
    };
    let padded = format!(" {code} ");
    let first = code.split(' ').next().unwrap_or_default();

    // Every `clause` occurrence must be followed by `guard`.
    let unguarded = |clause: &str, guard: &str| {
        padded.match_indices(&format!(" {clause} ")).any(|(at, found)| {
            !padded[at + found.len()..].starts_with(&format!("{guard} "))
        })
    };

    match first {
        "CREATE" => {
            let guarded = code.starts_with("CREATE OR REPLACE ")
                || padded.contains(" IF NOT EXISTS ");
            (!guarded).then(|| {
                "CREATE without IF NOT EXISTS (or OR REPLACE)".to_string()
            })
        }
        "DROP" | "TRUNCATE" => (!padded.contains(" IF EXISTS "))
            .then(|| format!("{first} without IF EXISTS")),
        "ALTER" => {
            for clause in ALTER_NEEDS_IF_NOT_EXISTS {
                if unguarded(clause, "IF NOT EXISTS") {
                    return Some(format!(
                        "ALTER ... {clause} without IF NOT EXISTS"
                    ));
                }
            }
            for clause in ALTER_NEEDS_IF_EXISTS {
                if unguarded(clause, "IF EXISTS") {
                    return Some(format!(
                        "ALTER ... {clause} without IF EXISTS"
                    ));
                }
            }
            ALTER_NOT_IDEMPOTENT
                .iter()
                .find(|clause| padded.contains(&format!(" {clause} ")))
                .map(|clause| {
                    format!("ALTER ... {clause} is not idempotent")
                })
        }
        "SELECT" | "WITH" | "SYSTEM" | "OPTIMIZE" | "GRANT" | "REVOKE"
        | "DELETE" => None,
        "INSERT" => Some(
            "INSERT is duplicated by a replay; seed data does not belong \
             in a migration"
                .to_string(),
        ),
        "RENAME" | "EXCHANGE" | "ATTACH" | "DETACH" => {
            Some(format!("{first} is not idempotent"))
        }
        other => Some(format!(
            "'{other}' statements are not known to be idempotent"
        )),
    }
}

/// A reviewed exception to [`idempotency_violation`] for one embedded
/// statement. Adding one is a code review decision: say in `reason` why
/// replaying the statement (after a partial failure, or by two racing
/// processes) is harmless.
#[derive(Debug, Clone, Copy)]
pub struct IdempotencyException {
    /// `0004_reorgs_checkpoints`.
    pub migration: &'static str,
    /// Start of the statement's [`skeleton`].
    pub skeleton_starts_with: &'static str,
    pub reason: &'static str,
}

/// See [`IdempotencyException`]. Entries that match nothing fail the tests.
pub const IDEMPOTENCY_EXCEPTIONS: &[IdempotencyException] = &[];

// ---- plan ------------------------------------------------------------------

/// A row of `schema_migrations`.
#[derive(Debug, Clone, PartialEq, Eq, Row, Deserialize)]
pub struct AppliedMigration {
    pub version: u32,
    pub name: String,
    pub checksum: String,
}

/// Reasons to refuse to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// An applied migration no longer matches the embedded file.
    ChecksumMismatch {
        version: u32,
        name: String,
        recorded: String,
        embedded: String,
    },
    /// The database holds a migration this binary does not know.
    UnknownVersion { version: u32, name: String, newest_known: u32 },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChecksumMismatch {
                version,
                name,
                recorded,
                embedded,
            } => {
                write!(
                    f,
                    "migration {} was modified after it was applied: \
                     schema_migrations records checksum {recorded}, this \
                     binary embeds {embedded}. Applied migrations are \
                     immutable: restore the original file and put the \
                     change in a new migration.",
                    label(*version, name)
                )
            }
            Self::UnknownVersion { version, name, newest_known } => {
                write!(
                f,
                "the database has migration {} applied, which this binary \
                 does not know (its newest is {newest_known:04}). The \
                 binary is older than the schema: deploy a newer indexer.",
                label(*version, name)
            )
            }
        }
    }
}

impl std::error::Error for PlanError {}

/// What to do, given the embedded set and what the database recorded.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan<'a> {
    /// Known migrations already applied.
    pub applied: usize,
    /// To apply, in version order.
    pub pending: Vec<&'a Migration>,
    /// Oddities worth a log line that do not block startup.
    pub warnings: Vec<String>,
}

/// Pure planning step. `migrations` must be sorted ([`validate`]).
///
/// A pending migration OLDER than an applied one is allowed (with a
/// warning): version ranges are reserved per area (`0010`+ for DEX), so
/// `0005` legitimately ships after `0010` was applied.
pub fn plan<'a>(
    migrations: &'a [Migration],
    applied: &[AppliedMigration],
) -> Result<Plan<'a>, PlanError> {
    let newest_known = migrations.last().map_or(0, |m| m.version);
    let mut warnings = Vec::new();

    // `applied` may hold several rows per version (conflicting checksums
    // recorded by different binaries): every one of them is checked.
    for row in applied {
        let Some(migration) =
            migrations.iter().find(|m| m.version == row.version)
        else {
            return Err(PlanError::UnknownVersion {
                version: row.version,
                name: row.name.clone(),
                newest_known,
            });
        };

        let embedded = migration.checksum();
        // Compared trimmed of NULs so a FixedString column works too.
        if row.checksum.trim_end_matches('\0') != embedded {
            return Err(PlanError::ChecksumMismatch {
                version: row.version,
                name: migration.name.clone(),
                recorded: row.checksum.clone(),
                embedded,
            });
        }

        if row.name != migration.name {
            warnings.push(format!(
                "Migration {:04} is recorded as '{}' but embedded as '{}' \
                 (same content, file renamed).",
                row.version, row.name, migration.name
            ));
        }
    }

    let is_applied =
        |version: u32| applied.iter().any(|row| row.version == version);
    let newest_applied = applied.iter().map(|row| row.version).max();

    let pending: Vec<&Migration> =
        migrations.iter().filter(|m| !is_applied(m.version)).collect();

    for migration in &pending {
        if newest_applied.is_some_and(|newest| migration.version < newest)
        {
            warnings.push(format!(
                "Migration {} is older than the newest applied migration \
                 ({:04}); applying it out of order.",
                migration.label(),
                newest_applied.unwrap_or_default()
            ));
        }
    }

    Ok(Plan {
        applied: migrations.len() - pending.len(),
        pending,
        warnings,
    })
}

// ---- runner ----------------------------------------------------------------

/// Result of [`Migrator::status`] (`indexer migrate --dry-run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub database_exists: bool,
    pub applied: usize,
    /// Labels (`0004_checkpoints`) of what would be applied, in order.
    pub pending: Vec<String>,
}

/// Result of [`Migrator::apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Already applied when this run started.
    pub already_applied: usize,
    /// Labels applied by this run, in order.
    pub applied: Vec<String>,
    /// Labels another process applied while this run was going.
    pub applied_elsewhere: Vec<String>,
}

/// `Code: 57. DB::Exception: ...` -> `57`.
fn parse_error_code(message: &str) -> Option<u32> {
    let rest = &message[message.find("Code: ")? + "Code: ".len()..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..digits].parse().ok()
}

fn error_code(error: &ClickhouseError) -> Option<u32> {
    match error {
        ClickhouseError::BadResponse(message) => parse_error_code(message),
        _ => None,
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("`{}`", identifier.replace('\\', "\\\\").replace('`', "\\`"))
}

/// First line(s) of a statement, for error messages.
fn excerpt(statement: &str) -> String {
    const MAX: usize = 160;

    let flat = statement.split_whitespace().collect::<Vec<_>>().join(" ");

    match flat.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}...", &flat[..cut]),
        None => flat,
    }
}

/// Applies migrations to the database named in a database url.
#[derive(Clone)]
pub struct Migrator {
    /// Bound to the target database.
    db: Client,
    /// Not bound to any database: used to create the target.
    server: Client,
    database: String,
    lock_stale: Duration,
}

/// Unique enough to tell lock holders apart in logs and in the lock's
/// comment.
fn lock_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());

    format!(
        "pid{}-{nanos:x}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl Migrator {
    /// Parses the url. Does not connect yet.
    pub fn new(database_url: &str) -> Result<Self> {
        let params = DatabaseParams::parse(database_url)?;

        for warning in &params.warnings {
            warn!("{warning}");
        }

        let server = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);

        Ok(Self {
            db: server.clone().with_database(&params.database),
            server,
            database: params.database,
            lock_stale: LOCK_STALE,
        })
    }

    /// Overrides [`LOCK_STALE`].
    pub fn with_lock_stale(mut self, lock_stale: Duration) -> Self {
        self.lock_stale = lock_stale;
        self
    }

    /// Waits for the server and reports whether the database exists.
    async fn database_exists(&self) -> Result<bool> {
        let mut attempt = 0;

        loop {
            attempt += 1;

            let error =
                match self.db.query("SELECT 1").fetch_one::<u8>().await {
                    Ok(_) => return Ok(true),
                    Err(e)
                        if error_code(&e)
                            == Some(CODE_UNKNOWN_DATABASE) =>
                    {
                        return Ok(false)
                    }
                    Err(e) => e,
                };

            if attempt >= CONNECT_ATTEMPTS {
                return Err(anyhow!(error).context(format!(
                    "could not connect to ClickHouse after \
                     {CONNECT_ATTEMPTS} attempts"
                )));
            }

            let wait = Duration::from_secs(2_u64.pow(attempt.min(5)));
            warn!(
                "ClickHouse connection attempt {attempt}/{CONNECT_ATTEMPTS} \
                 failed: {error}. Retrying in {wait:?}."
            );
            tokio::time::sleep(wait).await;
        }
    }

    async fn create_database(&self) -> Result<()> {
        info!("Creating database '{}'.", self.database);

        let statement = format!(
            "CREATE DATABASE IF NOT EXISTS {}",
            quote_identifier(&self.database)
        );

        match self
            .server
            .query(&escape_placeholders(&statement))
            .execute()
            .await
        {
            Ok(()) => Ok(()),
            // Lost a race against another indexer.
            Err(e)
                if error_code(&e)
                    == Some(CODE_DATABASE_ALREADY_EXISTS) =>
            {
                Ok(())
            }
            Err(e) => Err(anyhow!(e)
                .context(format!("create database '{}'", self.database))),
        }
    }

    async fn create_migrations_table(&self) -> Result<()> {
        // No version column: identical records collapse, conflicting
        // checksums for one version both survive and are reported.
        let statement = format!(
            "CREATE TABLE IF NOT EXISTS {MIGRATIONS_TABLE} (
                version UInt32,
                name String,
                checksum String,
                applied_at DateTime DEFAULT now()
            )
            ENGINE = ReplacingMergeTree
            ORDER BY (version, checksum)"
        );

        match self.db.query(&statement).execute().await {
            Ok(()) => Ok(()),
            Err(e)
                if error_code(&e) == Some(CODE_TABLE_ALREADY_EXISTS) =>
            {
                Ok(())
            }
            Err(e) => Err(anyhow!(e)
                .context(format!("create table {MIGRATIONS_TABLE}"))),
        }
    }

    /// `None` when the bookkeeping table does not exist.
    async fn read_applied(&self) -> Result<Option<Vec<AppliedMigration>>> {
        let query = format!(
            "SELECT version, name, checksum FROM {MIGRATIONS_TABLE} FINAL \
             ORDER BY version, checksum"
        );

        match self.db.query(&query).fetch_all::<AppliedMigration>().await {
            Ok(rows) => Ok(Some(rows)),
            Err(e) if error_code(&e) == Some(CODE_UNKNOWN_TABLE) => {
                Ok(None)
            }
            Err(e) => {
                Err(anyhow!(e).context(format!("read {MIGRATIONS_TABLE}")))
            }
        }
    }

    async fn is_recorded(&self, version: u32) -> Result<bool> {
        let query = format!(
            "SELECT count() FROM {MIGRATIONS_TABLE} FINAL WHERE version = ?"
        );

        let count = self
            .db
            .query(&query)
            .bind(version)
            .fetch_one::<u64>()
            .await
            .with_context(|| format!("read {MIGRATIONS_TABLE}"))?;

        Ok(count > 0)
    }

    async fn record(&self, migration: &Migration) -> Result<()> {
        let query = format!(
            "INSERT INTO {MIGRATIONS_TABLE} (version, name, checksum, \
             applied_at) VALUES (?, ?, ?, now())"
        );

        self.db
            .query(&query)
            .bind(migration.version)
            .bind(migration.name.as_str())
            .bind(migration.checksum())
            .execute()
            .await
            .with_context(|| {
                format!(
                    "migration {} was applied but could not be recorded in \
                     {MIGRATIONS_TABLE}; it will be applied again on the \
                     next run, which is safe (idempotent DDL)",
                    migration.label()
                )
            })
    }

    /// What [`apply`](Self::apply) would do. Creates nothing.
    pub async fn status(
        &self,
        migrations: &[Migration],
    ) -> Result<Status> {
        validate(migrations)?;

        let database_exists = self.database_exists().await?;

        let applied = if database_exists {
            self.read_applied().await?.unwrap_or_default()
        } else {
            Vec::new()
        };

        let plan = plan(migrations, &applied)?;

        Ok(Status {
            database_exists,
            applied: plan.applied,
            pending: plan.pending.iter().map(|m| m.label()).collect(),
        })
    }

    /// Brings the database up to date with `migrations`.
    pub async fn apply(&self, migrations: &[Migration]) -> Result<Report> {
        validate(migrations)?;

        if !self.database_exists().await? {
            self.create_database().await?;
        }

        self.create_migrations_table().await?;

        let applied = self.read_applied().await?.unwrap_or_default();
        let first = plan(migrations, &applied)?;

        for warning in &first.warnings {
            warn!("{warning}");
        }

        let mut report =
            Report { already_applied: first.applied, ..Report::default() };

        if first.pending.is_empty() {
            info!(
                "Database schema is up to date ({} migrations).",
                first.applied
            );
            return Ok(report);
        }

        info!(
            "{} pending migration(s) for database '{}'.",
            first.pending.len(),
            self.database
        );

        let lock = self.acquire_lock(migrations).await?;

        let result = self
            .apply_pending(
                migrations,
                &first.pending,
                lock.as_deref(),
                &mut report,
            )
            .await;

        if let Some(token) = &lock {
            self.release_lock(token).await;
        }

        result.map(|()| report)
    }

    async fn apply_pending(
        &self,
        migrations: &[Migration],
        pending: &[&Migration],
        lock: Option<&str>,
        report: &mut Report,
    ) -> Result<()> {
        for &migration in pending {
            // Whoever held the lock before us (or, without a lock, whoever
            // runs next to us) may have applied it: look again, and
            // re-check everything that was recorded in the meantime.
            let applied = self.read_applied().await?.unwrap_or_default();
            plan(migrations, &applied)?;

            let done =
                applied.iter().any(|r| r.version == migration.version)
                    || !self.run_statements(migration, lock).await?;

            if done {
                info!(
                    "Migration {} was applied by another process.",
                    migration.label()
                );
                report.applied_elsewhere.push(migration.label());
                continue;
            }

            self.record(migration).await?;
            info!("Applied migration {}.", migration.label());
            report.applied.push(migration.label());
        }

        Ok(())
    }

    /// Takes the best-effort lock. `None`: nothing is pending any more
    /// (another process finished while this one waited), no lock held.
    async fn acquire_lock(
        &self,
        migrations: &[Migration],
    ) -> Result<Option<String>> {
        let token = lock_token();
        // Memory engine: no data directory, the table is only a name.
        let create = format!(
            "CREATE TABLE {LOCK_TABLE} (holder String) ENGINE = Memory \
             COMMENT '{token}'"
        );
        let age = format!(
            "SELECT toUInt64(greatest(now() - metadata_modification_time, \
             0)) FROM system.tables WHERE database = currentDatabase() \
             AND name = '{LOCK_TABLE}'"
        );

        let mut announced = false;

        loop {
            match self.db.query(&create).execute().await {
                Ok(()) => return Ok(Some(token)),
                Err(e)
                    if error_code(&e)
                        == Some(CODE_TABLE_ALREADY_EXISTS) => {}
                Err(e) => {
                    return Err(
                        anyhow!(e).context("take the migration lock")
                    )
                }
            }

            // Somebody else is migrating. Maybe they are done already.
            let applied = self.read_applied().await?.unwrap_or_default();
            if plan(migrations, &applied)?.pending.is_empty() {
                return Ok(None);
            }

            if !announced {
                info!(
                    "Another process is applying migrations; waiting for \
                     it (lock table {LOCK_TABLE})."
                );
                announced = true;
            }

            let age = self
                .db
                .query(&age)
                .fetch_optional::<u64>()
                .await
                .context("read the migration lock")?;

            match age {
                // Released in the meantime: try again right away.
                None => continue,
                Some(age) if age >= self.lock_stale.as_secs() => {
                    warn!(
                        "The migration lock was not touched for {age}s: \
                         its holder is gone. Taking over."
                    );
                    self.drop_lock().await;
                }
                Some(_) => tokio::time::sleep(LOCK_POLL).await,
            }
        }
    }

    /// Keeps a held lock from looking stale. Best effort.
    async fn touch_lock(&self, token: &str, beat: usize) {
        // The comment has to change for the metadata time to move.
        let touch = format!(
            "ALTER TABLE {LOCK_TABLE} MODIFY COMMENT '{token} {beat}'"
        );

        // Failing means the lock was taken over; the takeover is safe
        // (idempotent DDL), so there is nothing to do about it.
        let _ = self.db.query(&touch).execute().await;
    }

    async fn drop_lock(&self) {
        let drop = format!("DROP TABLE IF EXISTS {LOCK_TABLE} SYNC");

        if let Err(e) = self.db.query(&drop).execute().await {
            warn!(
                "Could not drop the migration lock table {LOCK_TABLE} \
                 ({e}); other processes take it over after {:?}.",
                self.lock_stale
            );
        }
    }

    /// Drops the lock if it is still ours (it is not after a takeover).
    async fn release_lock(&self, token: &str) {
        let holder = format!(
            "SELECT comment FROM system.tables WHERE database =              currentDatabase() AND name = '{LOCK_TABLE}'"
        );

        match self.db.query(&holder).fetch_optional::<String>().await {
            Ok(Some(comment)) if !comment.starts_with(token) => {}
            _ => self.drop_lock().await,
        }
    }

    /// Runs the statements of one migration. `Ok(false)` when another
    /// process recorded the migration in the meantime (the rest of the
    /// statements was skipped, nothing is left to record).
    async fn run_statements(
        &self,
        migration: &Migration,
        lock: Option<&str>,
    ) -> Result<bool> {
        let statements = split_statements(&migration.sql)
            .with_context(|| format!("migration {}", migration.label()))?;
        let total = statements.len();

        for (index, statement) in statements.iter().enumerate() {
            if index > 0 && self.is_recorded(migration.version).await? {
                return Ok(false);
            }

            if let Some(token) = lock {
                self.touch_lock(token, index).await;
            }

            let result = self
                .db
                .query(&escape_placeholders(statement))
                .execute()
                .await;

            match result {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        error_code(&e),
                        Some(
                            CODE_TABLE_ALREADY_EXISTS
                                | CODE_DATABASE_ALREADY_EXISTS
                        )
                    ) =>
                {
                    warn!(
                        "Migration {} statement {}/{total}: the object \
                         already exists (previous partial run, or another \
                         indexer migrating concurrently); continuing. Write \
                         DDL with IF NOT EXISTS. Statement: {}",
                        migration.label(),
                        index + 1,
                        excerpt(statement)
                    );
                }
                Err(e) => {
                    return Err(anyhow!(e).context(format!(
                        "migration {} failed at statement {}/{total} \
                         ({}). {RERUN_HINT}",
                        migration.label(),
                        index + 1,
                        excerpt(statement)
                    )));
                }
            }
        }

        Ok(true)
    }
}

/// Applies the embedded migrations to the database of `database_url`,
/// creating the database when needed.
pub async fn run(database_url: &str) -> Result<Report> {
    let migrations = embedded()?;
    Migrator::new(database_url)?.apply(&migrations).await
}

/// Lists what [`run`] would apply, without changing anything.
pub async fn status(database_url: &str) -> Result<Status> {
    let migrations = embedded()?;
    Migrator::new(database_url)?.status(&migrations).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(sql: &str) -> Vec<String> {
        split_statements(sql).unwrap()
    }

    // ---- splitter ----

    #[test]
    fn splits_on_semicolons_and_trims() {
        assert_eq!(
            split("CREATE TABLE a (x UInt8);\n\n  SELECT 1 ;\nSELECT 2"),
            ["CREATE TABLE a (x UInt8)", "SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn empty_and_blank_scripts_hold_no_statement() {
        assert!(split("").is_empty());
        assert!(split("  \n\t ").is_empty());
        assert!(split(";;; ;\n;").is_empty());
        assert!(split("-- only a comment").is_empty());
        assert!(split("/* a; b */ ; -- c;\n ;").is_empty());
    }

    #[test]
    fn last_statement_needs_no_semicolon() {
        assert_eq!(split("SELECT 1; SELECT 2"), ["SELECT 1", "SELECT 2"]);
        assert_eq!(split("SELECT 1"), ["SELECT 1"]);
    }

    #[test]
    fn semicolon_in_single_quoted_string() {
        assert_eq!(
            split("INSERT INTO t VALUES ('a;b'); SELECT ';'"),
            ["INSERT INTO t VALUES ('a;b')", "SELECT ';'"]
        );
    }

    #[test]
    fn backslash_escaped_quote_in_string() {
        assert_eq!(
            split(r"SELECT 'it\'s; fine'; SELECT 2"),
            [r"SELECT 'it\'s; fine'", "SELECT 2"]
        );
    }

    #[test]
    fn escaped_backslash_before_closing_quote() {
        // `'\\'` is a complete string holding one backslash.
        assert_eq!(
            split(r"SELECT '\\'; SELECT 'x;y'"),
            [r"SELECT '\\'", "SELECT 'x;y'"]
        );
    }

    #[test]
    fn doubled_quote_escape_in_string() {
        assert_eq!(
            split("SELECT 'it''s; fine'; SELECT 2"),
            ["SELECT 'it''s; fine'", "SELECT 2"]
        );
        assert_eq!(split("SELECT ''';'''; SELECT 2").len(), 2);
        assert_eq!(
            split("SELECT ''; SELECT 2"),
            ["SELECT ''", "SELECT 2"]
        );
    }

    #[test]
    fn semicolon_in_quoted_identifiers() {
        assert_eq!(
            split("SELECT `a;b`, \"c;d\" FROM t; SELECT 2"),
            ["SELECT `a;b`, \"c;d\" FROM t", "SELECT 2"]
        );
        assert_eq!(
            split(r#"SELECT `a\`;b`, "c\";d", "e"";f"; SELECT 2"#).len(),
            2
        );
    }

    #[test]
    fn other_quote_kinds_inside_a_quoted_section_are_inert() {
        assert_eq!(
            split(r#"SELECT 'a"b;`c'; SELECT "x'y;"; SELECT `p"q';`"#),
            [
                r#"SELECT 'a"b;`c'"#,
                r#"SELECT "x'y;""#,
                r#"SELECT `p"q';`"#
            ]
        );
    }

    #[test]
    fn comment_markers_inside_strings_are_inert() {
        assert_eq!(
            split("SELECT '-- not a comment'; SELECT '/* nor this'; SELECT 3"),
            ["SELECT '-- not a comment'", "SELECT '/* nor this'", "SELECT 3"]
        );
    }

    #[test]
    fn semicolon_in_line_comment() {
        assert_eq!(
            split("SELECT 1 -- one; still a comment\n, 2; SELECT 3"),
            ["SELECT 1 -- one; still a comment\n, 2", "SELECT 3"]
        );
        // Without a space, and at the very end without a newline.
        assert_eq!(split("SELECT 1;--x;y"), ["SELECT 1"]);
    }

    #[test]
    fn hash_line_comments() {
        assert_eq!(
            split("#! shebang; style\nSELECT 1; # trailing; comment\nSELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        // A lone '#' is not a comment for ClickHouse either.
        assert_eq!(split("SELECT #x; SELECT 2").len(), 2);
    }

    #[test]
    fn quotes_inside_comments_are_inert() {
        assert_eq!(
            split("-- don't stop\nSELECT 1; /* it's; \"fine` */ SELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn semicolon_in_block_comment() {
        assert_eq!(
            split("SELECT /* a; b\n c; */ 1; SELECT 2"),
            ["SELECT /* a; b\n c; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn block_comments_nest() {
        // Like ClickHouse 25.12: `SELECT /* a /* b */ c */ 1` returns 1 and
        // `SELECT /* a /* b */ 1` is "Multiline comment is not closed".
        assert_eq!(
            split("SELECT /* a; /* b; */ c; */ 1; SELECT 2"),
            ["SELECT /* a; /* b; */ c; */ 1", "SELECT 2"]
        );
        assert_eq!(
            split("/* x /* y /* z */ ; */ ; */ SELECT 1; SELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            split_statements("/* a /* b */ SELECT 1; SELECT 2"),
            Err(SplitError { what: "block comment", line: 1 })
        );
        assert_eq!(
            split_statements("SELECT 1;\n/* a /* b */ c"),
            Err(SplitError { what: "block comment", line: 2 })
        );
        // A quote inside a nested comment stays inert.
        assert_eq!(split("/* a /* ' */ ' */ SELECT 1;"), ["SELECT 1"]);
    }

    #[test]
    fn tricky_block_comment_edges() {
        assert_eq!(
            split("/**/SELECT 1;/***/SELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        // `/*/` does not close itself.
        assert_eq!(
            split_statements("/*/ SELECT 1;"),
            Err(SplitError { what: "block comment", line: 1 })
        );
    }

    #[test]
    fn division_and_minus_are_not_comments() {
        assert_eq!(
            split("SELECT 4 / 2 - 1, 3 -1, a/b; SELECT 2"),
            ["SELECT 4 / 2 - 1, 3 -1, a/b", "SELECT 2"]
        );
    }

    #[test]
    fn leading_comments_are_dropped_inner_ones_kept() {
        assert_eq!(
            split(
                "-- header\n/* license */\nCREATE TABLE t (\n  a UInt8 -- x\n);"
            ),
            ["CREATE TABLE t (\n  a UInt8 -- x\n)"]
        );
    }

    #[test]
    fn heredoc_strings() {
        assert_eq!(
            split("SELECT $tag$ a; 'b -- c $tag$; SELECT $$;$$; SELECT 3"),
            ["SELECT $tag$ a; 'b -- c $tag$", "SELECT $$;$$", "SELECT 3"]
        );
        // A different tag does not close it.
        assert_eq!(split("SELECT $a$ ; $b$ ; $a$; SELECT 2").len(), 2);
        // `$` inside an identifier or alone is not a heredoc.
        assert_eq!(split("SELECT a$b$ FROM t; SELECT 2").len(), 2);
        assert_eq!(split("SELECT $ ; SELECT 2").len(), 2);
    }

    #[test]
    fn multibyte_text_is_preserved() {
        assert_eq!(
            split(
                "SELECT 'héllo; wörld ✓'; SELECT `名前;` FROM t -- ✓;\n;"
            ),
            ["SELECT 'héllo; wörld ✓'", "SELECT `名前;` FROM t -- ✓;"]
        );
    }

    #[test]
    fn crlf_line_endings() {
        assert_eq!(
            split("SELECT 1; -- c;\r\nSELECT 2;\r\n"),
            ["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn unterminated_constructs_are_errors_with_the_line() {
        assert_eq!(
            split_statements("SELECT 1;\nSELECT 'oops; SELECT 2;"),
            Err(SplitError { what: "string literal", line: 2 })
        );
        assert_eq!(
            split_statements("SELECT 'ends with escape\\'"),
            Err(SplitError { what: "string literal", line: 1 })
        );
        assert_eq!(
            split_statements("\n\nSELECT `oops"),
            Err(SplitError { what: "quoted identifier", line: 3 })
        );
        assert_eq!(
            split_statements("SELECT 1; /* never closed"),
            Err(SplitError { what: "block comment", line: 1 })
        );
        assert_eq!(
            split_statements("SELECT $x$ never closed $y$"),
            Err(SplitError { what: "heredoc string", line: 1 })
        );
        assert!(split_statements("SELECT '")
            .unwrap_err()
            .to_string()
            .contains("unterminated string literal starting at line 1"));
    }

    #[test]
    fn realistic_migration() {
        let sql = "\
-- 0001: core tables; binary types
CREATE TABLE IF NOT EXISTS blocks (
  chain UInt64,
  number UInt64 CODEC(Delta, ZSTD), -- monotonic; compresses well
  extra_data String CODEC(ZSTD(3)) COMMENT 'raw bytes; not hex'
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, number)
SETTINGS do_not_merge_across_partitions_select_final = 1;

/* read path; fed by an MV */
CREATE MATERIALIZED VIEW IF NOT EXISTS tx_lookup_mv TO tx_lookup AS
SELECT chain, hash, concat('0x;', lower(hex(hash))) AS `pretty;hash`
FROM transactions;
";
        let statements = split(sql);

        assert_eq!(statements.len(), 2);
        assert!(
            statements[0].starts_with("CREATE TABLE IF NOT EXISTS blocks")
        );
        assert!(statements[0].ends_with("= 1"));
        assert!(statements[1].starts_with("CREATE MATERIALIZED VIEW"));
        assert!(statements[1].ends_with("FROM transactions"));
    }

    #[test]
    fn placeholders_are_escaped_for_the_client() {
        assert_eq!(
            escape_placeholders("SELECT a ? 'x?' : 'y'"),
            "SELECT a ?? 'x??' : 'y'"
        );
    }

    // ---- idempotency lint ----

    #[test]
    fn skeleton_drops_comments_and_quoted_text() {
        assert_eq!(
            skeleton(
                "create table /* c */ if not exists `my table` (\n  a \
                 String DEFAULT 'INSERT; x' -- DROP\n)"
            )
            .unwrap(),
            "CREATE TABLE IF NOT EXISTS _ A STRING DEFAULT _"
        );
        assert!(skeleton("SELECT 'oops").is_err());
    }

    #[test]
    fn lint_accepts_idempotent_statements() {
        for statement in [
            "CREATE TABLE IF NOT EXISTS t (a UInt8) ENGINE = Memory",
            "create table if not exists t (a UInt8) ENGINE = Memory",
            "CREATE MATERIALIZED VIEW IF NOT EXISTS mv TO t AS SELECT 1",
            "CREATE VIEW IF NOT EXISTS v AS SELECT 1",
            "CREATE OR REPLACE VIEW v AS SELECT 1",
            "CREATE OR REPLACE FUNCTION f AS (x) -> x",
            "CREATE DICTIONARY IF NOT EXISTS d (a UInt8) PRIMARY KEY a",
            "CREATE DATABASE IF NOT EXISTS x",
            "DROP TABLE IF EXISTS t",
            "DROP VIEW IF EXISTS v SYNC",
            "TRUNCATE TABLE IF EXISTS t",
            // The pipeline's 0090_dedup_windows.
            "ALTER TABLE blocks MODIFY SETTING \
             non_replicated_deduplication_window = 10000",
            "ALTER TABLE t RESET SETTING non_replicated_deduplication_window",
            "ALTER TABLE t MODIFY COLUMN a UInt64 CODEC(ZSTD(3))",
            "ALTER TABLE t MODIFY TTL ts + INTERVAL 1 DAY",
            "ALTER TABLE t MODIFY COMMENT 'x'",
            "ALTER TABLE t ADD COLUMN IF NOT EXISTS b UInt8, \
             ADD COLUMN IF NOT EXISTS c UInt8 AFTER b",
            "ALTER TABLE t ADD INDEX IF NOT EXISTS i a TYPE minmax",
            "ALTER TABLE t DROP COLUMN IF EXISTS b",
            "ALTER TABLE t RENAME COLUMN IF EXISTS b TO c",
            "ALTER TABLE t MATERIALIZE INDEX i",
            "ALTER TABLE t DELETE WHERE a = 1",
            "ALTER TABLE t COMMENT COLUMN IF EXISTS a 'ADD COLUMN x'",
            "SELECT 1",
            "SYSTEM RELOAD DICTIONARIES",
            "OPTIMIZE TABLE t FINAL",
            "GRANT SELECT ON t TO reader",
            "DELETE FROM t WHERE a = 1",
            // Scary words inside comments / strings / identifiers.
            "CREATE TABLE IF NOT EXISTS t (\n  a UInt8 COMMENT 'INSERT \
             INTO x', -- DROP TABLE y\n  `RENAME` UInt8\n) ENGINE = Memory",
        ] {
            assert_eq!(idempotency_violation(statement), None, "{statement}");
        }
    }

    #[test]
    fn lint_refuses_non_idempotent_statements() {
        for (statement, why) in [
            ("CREATE TABLE t (a UInt8) ENGINE = Memory", "IF NOT EXISTS"),
            ("CREATE VIEW v AS SELECT 1", "IF NOT EXISTS"),
            ("CREATE MATERIALIZED VIEW mv TO t AS SELECT 1", "IF NOT EXISTS"),
            // The guard must be code, not a comment or a string.
            (
                "CREATE TABLE /* IF NOT EXISTS */ t (a UInt8) ENGINE = Memory",
                "IF NOT EXISTS",
            ),
            ("DROP TABLE t", "IF EXISTS"),
            ("TRUNCATE TABLE t", "IF EXISTS"),
            ("INSERT INTO t VALUES (1)", "INSERT"),
            ("insert into t select 1", "INSERT"),
            ("ALTER TABLE t ADD COLUMN b UInt8", "ADD COLUMN"),
            (
                "ALTER TABLE t ADD COLUMN IF NOT EXISTS b UInt8, \
                 ADD COLUMN c UInt8",
                "ADD COLUMN",
            ),
            ("ALTER TABLE t ADD INDEX i a TYPE minmax", "ADD INDEX"),
            ("ALTER TABLE t DROP COLUMN b", "DROP COLUMN"),
            ("ALTER TABLE t RENAME COLUMN b TO c", "RENAME COLUMN"),
            ("ALTER TABLE t UPDATE a = a + 1 WHERE 1", "UPDATE"),
            ("ALTER TABLE t ATTACH PARTITION 1 FROM u", "ATTACH PARTITION"),
            ("RENAME TABLE a TO b", "RENAME"),
            ("EXCHANGE TABLES a AND b", "EXCHANGE"),
            ("DETACH TABLE a", "DETACH"),
            ("USE other", "USE"),
            ("SELECT 'oops", "unterminated"),
        ] {
            let violation = idempotency_violation(statement)
                .unwrap_or_else(|| panic!("accepted: {statement}"));
            assert!(violation.contains(why), "{statement}: {violation}");
        }
    }

    /// Correctness of the runner (re-run after a partial failure, racing
    /// processes, lock takeover) rests on this.
    #[test]
    fn embedded_migrations_are_idempotent() {
        let mut used = vec![false; IDEMPOTENCY_EXCEPTIONS.len()];
        let mut violations = Vec::new();

        for migration in embedded().unwrap() {
            let statements = split_statements(&migration.sql).unwrap();

            for (index, statement) in statements.iter().enumerate() {
                let Some(violation) = idempotency_violation(statement)
                else {
                    continue;
                };

                let code = skeleton(statement).unwrap();
                let exception =
                    IDEMPOTENCY_EXCEPTIONS.iter().position(|e| {
                        e.migration == migration.label()
                            && code.starts_with(e.skeleton_starts_with)
                    });

                match exception {
                    Some(at) => used[at] = true,
                    None => violations.push(format!(
                        "{} statement {}: {violation}: {}",
                        migration.label(),
                        index + 1,
                        excerpt(statement)
                    )),
                }
            }
        }

        assert!(
            violations.is_empty(),
            "non-idempotent statements in embedded migrations (fix them, \
             or add a reviewed IDEMPOTENCY_EXCEPTIONS entry):\n{}",
            violations.join("\n")
        );

        for (exception, used) in IDEMPOTENCY_EXCEPTIONS.iter().zip(used) {
            assert!(
                used,
                "stale idempotency exception (matches nothing): \
                 {exception:?}"
            );
            assert!(
                exception.reason.len() >= 20
                    && !exception.skeleton_starts_with.is_empty(),
                "exception without a real reason / pattern: {exception:?}"
            );
        }
    }

    // ---- file names (shared with build.rs) ----

    #[test]
    fn valid_file_names() {
        assert_eq!(
            parse_filename("0001_core_tables.sql"),
            Ok((1, "core_tables".to_string()))
        );
        assert_eq!(
            parse_filename("0010_dex.sql"),
            Ok((10, "dex".to_string()))
        );
        assert_eq!(
            parse_filename("9999_v2_erc20.sql"),
            Ok((9999, "v2_erc20".to_string()))
        );
    }

    #[test]
    fn malformed_file_names() {
        for name in [
            "create_tables.sql",
            "indexes.sql",
            "1_short.sql",
            "001_short.sql",
            "00001_long.sql",
            "0000_zero.sql",
            "0001.sql",
            "0001_.sql",
            "0001__x.sql",
            "0001-dash.sql",
            "0001_Upper.sql",
            "0001_sp ace.sql",
            "0001_dot.x.sql",
            "0001_x.SQL",
            "0001_x.sql.bak",
            "0001_x",
            "+001_x.sql",
            "٠٠٠١_x.sql",
            "",
        ] {
            let error = parse_filename(name).unwrap_err();
            assert!(
                error.contains("malformed migration file name")
                    && error.contains(name),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn duplicate_versions_are_reported_with_both_files() {
        let files = vec![
            (1, "0001_a.sql".to_string()),
            (2, "0002_b.sql".to_string()),
            (2, "0002_c.sql".to_string()),
        ];

        let error = filename::check_unique(&files).unwrap_err();
        assert!(error.contains("duplicate migration version 0002"));
        assert!(
            error.contains("0002_b.sql") && error.contains("0002_c.sql")
        );

        assert!(filename::check_unique(&files[..2]).is_ok());
        assert!(filename::check_unique(&[]).is_ok());
    }

    // ---- embedded set ----

    #[test]
    fn embedded_migrations_are_valid_and_sorted() {
        let migrations = embedded().unwrap();

        assert!(!migrations.is_empty());
        assert_eq!(migrations.len(), EMBEDDED.len());
        assert!(migrations
            .windows(2)
            .all(|p| p[0].version < p[1].version));
    }

    #[test]
    fn embedded_migrations_carry_no_database_prefix() {
        // The database comes from the url (design §6): no `indexer.x`
        // where an object name goes (prose in comments is fine).
        const BEFORE_OBJECT: [&str; 7] =
            ["TABLE", "VIEW", "EXISTS", "FROM", "TO", "INTO", "JOIN"];

        for migration in embedded().unwrap() {
            for statement in split_statements(&migration.sql).unwrap() {
                let tokens: Vec<&str> = statement
                    .split(|c: char| c.is_whitespace() || c == '(')
                    .filter(|token| !token.is_empty())
                    .collect();

                for pair in tokens.windows(2) {
                    let prefixed = pair[1].starts_with("indexer.")
                        || pair[1].starts_with("`indexer`.");
                    let object = BEFORE_OBJECT
                        .contains(&pair[0].to_ascii_uppercase().as_str());

                    assert!(
                        !(prefixed && object),
                        "{}: hard-coded database prefix '{}' in: {}",
                        migration.label(),
                        pair[1],
                        excerpt(&statement)
                    );
                }
            }
        }
    }

    // ---- checksum / validation ----

    #[test]
    fn checksum_is_sha256_hex_and_stable() {
        assert_eq!(
            checksum(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            checksum("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(checksum("SELECT 1").len(), 64);
    }

    #[test]
    fn checksum_ignores_line_ending_style_only() {
        assert_eq!(checksum("a;\r\nb;\r\n"), checksum("a;\nb;\n"));
        assert_ne!(checksum("a;\nb;\n"), checksum("a;\nb;"));
        assert_ne!(checksum("SELECT 1"), checksum("SELECT 2"));
        assert_ne!(checksum("SELECT 1"), checksum("SELECT  1"));
    }

    fn set(versions: &[u32]) -> Vec<Migration> {
        versions
            .iter()
            .map(|&v| {
                Migration::new(v, format!("m{v}"), format!("SELECT {v};"))
            })
            .collect()
    }

    fn applied_rows(migrations: &[Migration]) -> Vec<AppliedMigration> {
        migrations
            .iter()
            .map(|m| AppliedMigration {
                version: m.version,
                name: m.name.clone(),
                checksum: m.checksum(),
            })
            .collect()
    }

    #[test]
    fn validate_requires_strictly_ascending_versions() {
        assert!(validate(&set(&[1, 2, 10])).is_ok());
        assert!(validate(&[]).is_ok());

        let unordered = validate(&set(&[2, 1])).unwrap_err().to_string();
        assert!(unordered.contains("ascending"), "{unordered}");

        assert!(validate(&set(&[1, 1])).is_err());
        assert!(validate(&set(&[0, 1])).is_err());
    }

    #[test]
    fn validate_rejects_empty_and_unsplittable_migrations() {
        let empty = [Migration::new(1, "empty", "-- nothing here\n;")];
        assert!(validate(&empty)
            .unwrap_err()
            .to_string()
            .contains("0001_empty holds no statement"));

        let broken = [Migration::new(2, "broken", "SELECT 'oops;")];
        let error = format!("{:#}", validate(&broken).unwrap_err());
        assert!(error.contains("0002_broken"), "{error}");
        assert!(error.contains("unterminated string literal"), "{error}");
    }

    // ---- plan ----

    #[test]
    fn fresh_database_applies_everything_in_order() {
        let migrations = set(&[1, 2, 3, 10]);
        let plan = plan(&migrations, &[]).unwrap();

        assert_eq!(plan.applied, 0);
        assert_eq!(plan.pending, migrations.iter().collect::<Vec<_>>());
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn up_to_date_database_has_nothing_pending() {
        let migrations = set(&[1, 2, 3]);
        let plan = plan(&migrations, &applied_rows(&migrations)).unwrap();

        assert_eq!(plan.applied, 3);
        assert!(plan.pending.is_empty());
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn only_the_tail_is_pending() {
        let migrations = set(&[1, 2, 3, 4]);
        let plan =
            plan(&migrations, &applied_rows(&migrations[..2])).unwrap();

        assert_eq!(plan.applied, 2);
        assert_eq!(
            plan.pending.iter().map(|m| m.version).collect::<Vec<_>>(),
            [3, 4]
        );
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn applied_row_order_does_not_matter() {
        let migrations = set(&[1, 2, 3]);
        let mut rows = applied_rows(&migrations[..2]);
        rows.reverse();

        let plan = plan(&migrations, &rows).unwrap();
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].version, 3);
    }

    #[test]
    fn out_of_order_pending_is_applied_with_a_warning() {
        // 0010 (DEX range) applied before 0004 shipped.
        let migrations = set(&[1, 4, 10]);
        let rows =
            applied_rows(&[migrations[0].clone(), migrations[2].clone()]);

        let plan = plan(&migrations, &rows).unwrap();

        assert_eq!(plan.applied, 2);
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].version, 4);
        assert_eq!(plan.warnings.len(), 1);
        assert!(
            plan.warnings[0].contains("0004_m4"),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn changed_checksum_is_refused() {
        let migrations = set(&[1, 2]);
        let mut rows = applied_rows(&migrations);
        rows[1].checksum = checksum("SELECT 'the original';");

        let error = plan(&migrations, &rows).unwrap_err();

        assert_eq!(
            error,
            PlanError::ChecksumMismatch {
                version: 2,
                name: "m2".into(),
                recorded: rows[1].checksum.clone(),
                embedded: migrations[1].checksum(),
            }
        );
        let text = error.to_string();
        assert!(text.contains("0002_m2"), "{text}");
        assert!(text.contains("modified after it was applied"), "{text}");
    }

    #[test]
    fn conflicting_records_for_one_version_are_refused() {
        // Two different binaries raced: one row matches, one does not.
        let migrations = set(&[1]);
        let mut rows = applied_rows(&migrations);
        rows.push(AppliedMigration {
            version: 1,
            name: "m1".into(),
            checksum: checksum("something else"),
        });

        assert!(matches!(
            plan(&migrations, &rows),
            Err(PlanError::ChecksumMismatch { version: 1, .. })
        ));
    }

    #[test]
    fn unknown_applied_version_is_refused() {
        let migrations = set(&[1, 2]);
        let newer = set(&[1, 2, 3]);

        let error = plan(&migrations, &applied_rows(&newer)).unwrap_err();

        assert_eq!(
            error,
            PlanError::UnknownVersion {
                version: 3,
                name: "m3".into(),
                newest_known: 2
            }
        );
        let text = error.to_string();
        assert!(text.contains("0003_m3"), "{text}");
        assert!(text.contains("older than the schema"), "{text}");

        // Also when the unknown version sits in a hole of the known set.
        let sparse = set(&[1, 3]);
        assert!(matches!(
            plan(&sparse, &applied_rows(&set(&[1, 2]))),
            Err(PlanError::UnknownVersion { version: 2, .. })
        ));
    }

    #[test]
    fn renamed_migration_with_same_content_only_warns() {
        let migrations = set(&[1]);
        let mut rows = applied_rows(&migrations);
        rows[0].name = "old_name".into();

        let plan = plan(&migrations, &rows).unwrap();
        assert!(plan.pending.is_empty());
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("old_name"));
    }

    #[test]
    fn fixed_string_padding_in_recorded_checksum_is_ignored() {
        let migrations = set(&[1]);
        let mut rows = applied_rows(&migrations);
        rows[0].checksum.push_str("\0\0");

        assert!(plan(&migrations, &rows).is_ok());
    }

    // ---- helpers ----

    #[test]
    fn error_codes_are_extracted_from_server_messages() {
        assert_eq!(
            parse_error_code(
                "Code: 57. DB::Exception: Table x.y already exists. \
                 (TABLE_ALREADY_EXISTS) (version 25.12.1.1)"
            ),
            Some(57)
        );
        assert_eq!(
            parse_error_code("bad response: Code: 81. DB::Exception: no"),
            Some(81)
        );
        assert_eq!(parse_error_code("Code: x"), None);
        assert_eq!(parse_error_code("connection refused"), None);
    }

    #[test]
    fn identifiers_are_quoted() {
        assert_eq!(quote_identifier("indexer"), "`indexer`");
        assert_eq!(quote_identifier("a`b\\c"), "`a\\`b\\\\c`");
    }

    #[test]
    fn excerpts_are_flattened_and_bounded() {
        assert_eq!(
            excerpt("CREATE  TABLE\n  t (a UInt8)"),
            "CREATE TABLE t (a UInt8)"
        );

        let long = excerpt(&"é".repeat(500));
        assert_eq!(long.chars().count(), 163);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn labels_are_zero_padded() {
        assert_eq!(
            Migration::new(4, "checkpoints", "").label(),
            "0004_checkpoints"
        );
        assert_eq!(label(12345, "x"), "12345_x");
    }
}

/// Against a real ClickHouse. Ignored by default:
///
/// ```sh
/// TEST_DATABASE_URL=http://default@localhost:8123/indexer \
///   cargo test migrate::integration -- --ignored
/// ```
///
/// Only the server part of the url is used: every test works in its own
/// throwaway database (`migrate_test_*`), dropped at the end.
#[cfg(test)]
mod integration {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh database name and the url pointing at it.
    struct Scratch {
        name: String,
        url: String,
        server: Client,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let base = std::env::var("TEST_DATABASE_URL").expect(
                "TEST_DATABASE_URL must be set for the ignored tests",
            );

            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos();
            let name = format!(
                "migrate_test_{label}_{}_{nanos}_{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );

            let mut url = url::Url::parse(&base).unwrap();
            url.set_path(&name);

            let params = DatabaseParams::parse(url.as_str()).unwrap();
            let server = Client::default()
                .with_url(&params.endpoint)
                .with_user(&params.user)
                .with_password(&params.password);

            Self { name, url: url.to_string(), server }
        }

        fn migrator(&self) -> Migrator {
            Migrator::new(&self.url).unwrap()
        }

        async fn count(&self, query: &str) -> u64 {
            self.server
                .query(&query.replace("{db}", &self.name))
                .fetch_one::<u64>()
                .await
                .unwrap()
        }

        async fn database_exists(&self) -> bool {
            self.count(
                "SELECT count() FROM system.databases WHERE name = '{db}'",
            )
            .await
                == 1
        }

        async fn table_exists(&self, table: &str) -> bool {
            self.count(&format!(
                "SELECT count() FROM system.tables \
                 WHERE database = '{{db}}' AND name = '{table}'"
            ))
            .await
                == 1
        }

        async fn recorded(&self) -> Vec<u32> {
            self.server
                .query(&format!(
                    "SELECT version FROM {}.{MIGRATIONS_TABLE} FINAL \
                     ORDER BY version",
                    self.name
                ))
                .fetch_all::<u32>()
                .await
                .unwrap()
        }

        async fn drop(self) {
            self.server
                .query(&format!(
                    "DROP DATABASE IF EXISTS {} SYNC",
                    self.name
                ))
                .execute()
                .await
                .unwrap();
        }
    }

    fn sample() -> Vec<Migration> {
        vec![
            Migration::new(
                1,
                "core",
                "-- core; tables\n\
                 CREATE TABLE IF NOT EXISTS t1 (\n\
                   a UInt64, -- first; column\n\
                   b String DEFAULT 'x;y' /* a ; in a comment */\n\
                 ) ENGINE = MergeTree ORDER BY a;\n\
                 CREATE TABLE IF NOT EXISTS t2 (a UInt64, `odd;name` String)\n\
                 ENGINE = MergeTree ORDER BY a;\n",
            ),
            Migration::new(
                2,
                "read_path",
                "CREATE TABLE IF NOT EXISTS t1_by_b (b String, a UInt64)\n\
                 ENGINE = ReplacingMergeTree ORDER BY (b, a);\n\
                 CREATE MATERIALIZED VIEW IF NOT EXISTS t1_by_b_mv TO t1_by_b\n\
                 AS SELECT b, a FROM t1;",
            ),
            Migration::new(
                10,
                "seed",
                "INSERT INTO t1 (a, b) VALUES (1, 'what?; it''s \\'fine\\'');\n\
                 ALTER TABLE t2 ADD COLUMN IF NOT EXISTS c UInt8;",
            ),
        ]
    }

    fn labels(migrations: &[Migration]) -> Vec<String> {
        migrations.iter().map(Migration::label).collect()
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn fresh_database_then_second_run_is_a_noop() {
        let scratch = Scratch::new("fresh");
        let migrations = sample();

        assert!(!scratch.database_exists().await);

        // Dry run: reports everything pending, creates nothing.
        let status = scratch.migrator().status(&migrations).await.unwrap();
        assert!(!status.database_exists);
        assert_eq!(status.applied, 0);
        assert_eq!(status.pending, labels(&migrations));
        assert!(!scratch.database_exists().await);

        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, 0);
        assert_eq!(report.applied, labels(&migrations));
        assert!(report.applied_elsewhere.is_empty());

        assert!(scratch.database_exists().await);
        for table in
            ["t1", "t2", "t1_by_b", "t1_by_b_mv", MIGRATIONS_TABLE]
        {
            assert!(scratch.table_exists(table).await, "{table}");
        }
        assert_eq!(scratch.recorded().await, [1, 2, 10]);
        assert!(!scratch.table_exists(LOCK_TABLE).await);

        // Statements ran verbatim against the url's database: the string
        // with `?`, `;` and both quote escapes arrived intact, through the
        // materialized view too.
        let stored = scratch
            .server
            .query(&format!("SELECT b FROM {}.t1_by_b", scratch.name))
            .fetch_one::<String>()
            .await
            .unwrap();
        assert_eq!(stored, "what?; it's 'fine'");

        // Second run: nothing to do, nothing re-executed (the seed row is
        // not inserted twice).
        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, 3);
        assert!(report.applied.is_empty());
        assert_eq!(scratch.count("SELECT count() FROM {db}.t1").await, 1);
        assert_eq!(scratch.recorded().await, [1, 2, 10]);

        let status = scratch.migrator().status(&migrations).await.unwrap();
        assert!(status.database_exists);
        assert_eq!(status.applied, 3);
        assert!(status.pending.is_empty());

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn new_migrations_are_applied_incrementally() {
        let scratch = Scratch::new("incremental");
        let migrations = sample();

        let report =
            scratch.migrator().apply(&migrations[..1]).await.unwrap();
        assert_eq!(report.applied, ["0001_core"]);

        // 0010 ships before 0002 (reserved ranges), then 0002 arrives.
        let skipping = [migrations[0].clone(), migrations[2].clone()];
        let report = scratch.migrator().apply(&skipping).await.unwrap();
        assert_eq!(report.already_applied, 1);
        assert_eq!(report.applied, ["0010_seed"]);

        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, 2);
        assert_eq!(report.applied, ["0002_read_path"]);
        assert_eq!(scratch.recorded().await, [1, 2, 10]);

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn tampered_checksum_is_refused() {
        let scratch = Scratch::new("tamper");
        let mut migrations = sample();

        scratch.migrator().apply(&migrations).await.unwrap();

        migrations[1].sql.push_str("\n-- edited after the fact\n");

        let error =
            scratch.migrator().apply(&migrations).await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<PlanError>(),
                Some(PlanError::ChecksumMismatch { version: 2, .. })
            ),
            "{error:#}"
        );
        assert!(scratch.migrator().status(&migrations).await.is_err());

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn unknown_newer_version_is_refused() {
        let scratch = Scratch::new("newer");
        let migrations = sample();

        scratch.migrator().apply(&migrations).await.unwrap();

        // An older binary, which only knows 0001 and 0002.
        let error =
            scratch.migrator().apply(&migrations[..2]).await.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<PlanError>(),
                Some(PlanError::UnknownVersion {
                    version: 10,
                    newest_known: 2,
                    ..
                })
            ),
            "{error:#}"
        );

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn failed_migration_is_not_recorded_and_can_be_rerun() {
        let scratch = Scratch::new("failure");

        let broken = vec![
            Migration::new(
                1,
                "ok",
                "CREATE TABLE IF NOT EXISTS a (x UInt8) \
                 ENGINE = MergeTree ORDER BY x;",
            ),
            Migration::new(
                2,
                "half",
                "CREATE TABLE IF NOT EXISTS b (x UInt8) \
                 ENGINE = MergeTree ORDER BY x;\n\
                 CREATE TABLE IF NOT EXISTS c (x NoSuchType) \
                 ENGINE = MergeTree ORDER BY x;\n\
                 CREATE TABLE IF NOT EXISTS d (x UInt8) \
                 ENGINE = MergeTree ORDER BY x;",
            ),
        ];

        let error = scratch.migrator().apply(&broken).await.unwrap_err();
        let text = format!("{error:#}");
        assert!(
            text.contains("0002_half failed at statement 2/3"),
            "{text}"
        );
        assert!(text.contains("NOT recorded"), "{text}");
        assert!(text.contains("it is safe"), "{text}");

        // 0001 recorded, 0002 half applied and not recorded.
        assert_eq!(scratch.recorded().await, [1]);
        assert!(scratch.table_exists("b").await);
        assert!(!scratch.table_exists("c").await);
        assert!(!scratch.table_exists("d").await);
        // The lock does not outlive the failure.
        assert!(!scratch.table_exists(LOCK_TABLE).await);

        // Never recorded, so fixing the file is not "tampering"; the
        // re-run passes over the statement that already went through.
        let mut fixed = broken.clone();
        fixed[1].sql = fixed[1].sql.replace("NoSuchType", "UInt8");

        let report = scratch.migrator().apply(&fixed).await.unwrap();
        assert_eq!(report.already_applied, 1);
        assert_eq!(report.applied, ["0002_half"]);
        assert!(scratch.table_exists("d").await);
        assert_eq!(scratch.recorded().await, [1, 2]);

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn non_idempotent_ddl_already_present_is_tolerated() {
        let scratch = Scratch::new("exists");

        let first = vec![Migration::new(
            1,
            "a",
            "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;",
        )];
        scratch.migrator().apply(&first).await.unwrap();

        // Same object created again, without IF NOT EXISTS.
        let second = vec![
            first[0].clone(),
            Migration::new(
                2,
                "again",
                "CREATE TABLE a (x UInt8) ENGINE = MergeTree ORDER BY x;\n\
                 CREATE TABLE b (x UInt8) ENGINE = MergeTree ORDER BY x;",
            ),
        ];
        let report = scratch.migrator().apply(&second).await.unwrap();
        assert_eq!(report.applied, ["0002_again"]);
        assert!(scratch.table_exists("b").await);

        scratch.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn racing_runners_converge() {
        const RUNNERS: usize = 6;

        let scratch = Scratch::new("race");

        // Wide enough that the runners overlap inside migrations.
        let mut migrations = sample();
        for version in 11..=16 {
            let tables = (0..6)
                .map(|n| {
                    format!(
                        "CREATE TABLE IF NOT EXISTS r{version}_{n} \
                         (x UInt8) ENGINE = MergeTree ORDER BY x;"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            migrations.push(Migration::new(
                version,
                format!("race{version}"),
                tables,
            ));
        }

        // The database does not exist: creating it, the bookkeeping table
        // and every migration all race.
        let handles: Vec<_> = (0..RUNNERS)
            .map(|_| {
                let migrator = scratch.migrator();
                let migrations = migrations.clone();
                tokio::spawn(
                    async move { migrator.apply(&migrations).await },
                )
            })
            .collect();

        let mut applied_by_someone = Vec::new();

        for handle in handles {
            let report = handle.await.unwrap().unwrap();
            eprintln!("runner report: {report:?}");

            // Every runner accounts for every migration exactly once.
            assert_eq!(
                report.already_applied
                    + report.applied.len()
                    + report.applied_elsewhere.len(),
                migrations.len(),
                "{report:?}"
            );
            applied_by_someone.extend(report.applied);
        }

        // The lock made every migration run exactly once, in one runner
        // (nobody was slow enough for a takeover): the seed row of 0010
        // exists once.
        applied_by_someone.sort();
        assert_eq!(applied_by_someone, {
            let mut all = labels(&migrations);
            all.sort();
            all
        });
        assert_eq!(scratch.count("SELECT count() FROM {db}.t1").await, 1);
        assert!(!scratch.table_exists(LOCK_TABLE).await);

        // One visible record per migration, whoever wrote it.
        assert_eq!(
            scratch.recorded().await,
            migrations.iter().map(|m| m.version).collect::<Vec<_>>()
        );
        assert!(scratch.table_exists("r16_5").await);
        assert!(scratch.table_exists("t1_by_b_mv").await);

        // And the result is a clean no-op for whoever starts next.
        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, migrations.len());
        assert!(report.applied.is_empty());

        scratch.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn racing_runners_without_a_working_lock_still_converge() {
        const RUNNERS: usize = 6;

        // A stale threshold of zero makes every waiter steal the lock at
        // once (age >= 0): all runners execute every statement side by side, which is
        // the lock-free fallback the design relies on. No seed INSERT in
        // this set: only idempotent DDL is safe in that mode.
        let scratch = Scratch::new("lockfree");
        let migrations: Vec<Migration> =
            sample().into_iter().filter(|m| m.version != 10).collect();

        let handles: Vec<_> = (0..RUNNERS)
            .map(|_| {
                let migrator =
                    scratch.migrator().with_lock_stale(Duration::ZERO);
                let migrations = migrations.clone();
                tokio::spawn(
                    async move { migrator.apply(&migrations).await },
                )
            })
            .collect();

        for handle in handles {
            let report = handle.await.unwrap().unwrap();
            assert_eq!(
                report.already_applied
                    + report.applied.len()
                    + report.applied_elsewhere.len(),
                migrations.len(),
                "{report:?}"
            );
        }

        // Duplicate records collapse.
        assert_eq!(scratch.recorded().await, [1, 2]);
        assert!(scratch.table_exists("t1_by_b_mv").await);

        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, 2);
        assert!(report.applied.is_empty());

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn stale_lock_of_a_dead_process_is_taken_over() {
        let scratch = Scratch::new("stale");
        let migrations = sample();

        // A process that died while migrating: 0001 recorded, lock left.
        scratch.migrator().apply(&migrations[..1]).await.unwrap();
        scratch
            .server
            .query(&format!(
                "CREATE TABLE {}.{LOCK_TABLE} (holder String) \
                 ENGINE = Memory COMMENT 'dead'",
                scratch.name
            ))
            .execute()
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let report = scratch
            .migrator()
            .with_lock_stale(Duration::from_secs(2))
            .apply(&migrations)
            .await
            .unwrap();

        // It waited for the lock to become stale, then went through.
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert_eq!(report.already_applied, 1);
        assert_eq!(report.applied, ["0002_read_path", "0010_seed"]);
        assert!(!scratch.table_exists(LOCK_TABLE).await);

        // A leftover lock never blocks a start that has nothing to do.
        scratch
            .server
            .query(&format!(
                "CREATE TABLE {}.{LOCK_TABLE} (holder String) \
                 ENGINE = Memory COMMENT 'dead'",
                scratch.name
            ))
            .execute()
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let report = scratch.migrator().apply(&migrations).await.unwrap();
        assert_eq!(report.already_applied, 3);
        assert!(started.elapsed() < Duration::from_secs(2));

        scratch.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs ClickHouse (TEST_DATABASE_URL)"]
    async fn embedded_migrations_apply_to_a_fresh_database() {
        let scratch = Scratch::new("embedded");

        let report = run(&scratch.url).await.unwrap();
        assert_eq!(report.applied.len(), EMBEDDED.len());

        let report = run(&scratch.url).await.unwrap();
        assert_eq!(report.already_applied, EMBEDDED.len());
        assert!(report.applied.is_empty());

        let status = status(&scratch.url).await.unwrap();
        assert!(status.pending.is_empty());

        // Everything landed in the url's database.
        assert!(
            scratch
                .count(
                    "SELECT count() FROM system.tables \
                     WHERE database = '{db}'"
                )
                .await
                > 1
        );

        scratch.drop().await;
    }
}
