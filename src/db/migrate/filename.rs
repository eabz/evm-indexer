//! Migration file naming rules.
//!
//! This file is compiled twice: as a module of `db::migrate` and, through
//! `#[path]`, by `build.rs`, so the build script and the runtime can never
//! disagree about what a valid migration file is. It must stay free of any
//! dependency other than `std`.

/// Number of digits of the version prefix (`NNNN_name.sql`).
pub const VERSION_DIGITS: usize = 4;

/// Parses `NNNN_name.sql` into `(version, name)`.
///
/// - `NNNN`: exactly four ASCII digits, not `0000`.
/// - `name`: `[a-z0-9_]+`, starting with a letter or digit.
/// - extension: exactly `.sql`.
pub fn parse_filename(file_name: &str) -> Result<(u32, String), String> {
    let malformed = |why: &str| {
        Err(format!(
            "malformed migration file name '{file_name}': {why} \
             (expected NNNN_name.sql, e.g. 0004_checkpoints.sql)"
        ))
    };

    let Some(stem) = file_name.strip_suffix(".sql") else {
        return malformed("the extension must be .sql");
    };

    let Some((digits, name)) = stem.split_once('_') else {
        return malformed("no '_' between the version and the name");
    };

    if digits.len() != VERSION_DIGITS
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return malformed("the version must be exactly four digits");
    }

    let version: u32 = match digits.parse() {
        Ok(version) => version,
        Err(_) => return malformed("the version is not a number"),
    };

    if version == 0 {
        return malformed("versions start at 0001");
    }

    if name.is_empty() {
        return malformed("the name is empty");
    }

    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return malformed(
            "the name may only contain lowercase letters, digits and '_'",
        );
    }

    if name.starts_with('_') {
        return malformed("the name must start with a letter or a digit");
    }

    Ok((version, name.to_string()))
}

/// Checks that `(version, file name)` pairs, sorted by version, hold no
/// version twice. Used by `build.rs` (the runtime gets an already checked
/// set).
#[allow(dead_code)]
pub fn check_unique(sorted: &[(u32, String)]) -> Result<(), String> {
    for pair in sorted.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "duplicate migration version {:0width$}: '{}' and '{}'",
                pair[0].0,
                pair[0].1,
                pair[1].1,
                width = VERSION_DIGITS
            ));
        }
    }

    Ok(())
}
