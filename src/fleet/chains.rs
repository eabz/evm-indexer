//! `fleet_chains`: the chains the owner asked this process to index, and
//! the settings each of them starts with (migration 0007).
//!
//! Insert only, like every other table in this schema. A chain leaves the
//! fleet by getting `desired = 'stopped'`, never by a DELETE - the panel
//! has no destructive endpoint at all (docs/design.md section 15).
//!
//! **Read once, at start.** ClickHouse has no read-your-writes: a row the
//! panel wrote a second ago may not come back yet. So the supervisor's
//! memory is the authority while the process runs and this table is only
//! how the next start remembers what the owner wanted. Every write below
//! happens AFTER the change was applied in memory, and a write that fails
//! is a warning, not a failed command.

use crate::{
    configs::{ChainSettings, Desired},
    db::Database,
};
use anyhow::{Context, Result};
use clickhouse::Row;
use log::warn;
use serde::Deserialize;

/// One row of `fleet_chains`, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredChain {
    pub chain: u64,
    pub desired: Desired,
    pub settings: ChainSettings,
}

#[derive(Debug, Row, Deserialize)]
struct StoredRow {
    chain: u64,
    desired: String,
    settings: String,
}

/// Escapes a string for a ClickHouse literal, exactly as `pipeline::lease`
/// does. The settings come from a web page, so this is the boundary that
/// decides whether a quote in a value can become SQL.
fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The desired state of every chain, newest row per chain.
///
/// A row whose `settings` is not a JSON object of strings is reported and
/// SKIPPED rather than failing the start: one bad row must not stop a fleet
/// of fifty chains from coming up.
pub async fn load(db: &Database) -> Result<Vec<DesiredChain>> {
    let rows = db
        .db
        .query(
            "SELECT chain, desired, settings FROM fleet_chains_v \
             ORDER BY chain",
        )
        .fetch_all::<StoredRow>()
        .await
        .context("read the fleet_chains table")?;

    Ok(rows.into_iter().filter_map(parse_row).collect())
}

fn parse_row(row: StoredRow) -> Option<DesiredChain> {
    match serde_json::from_str::<ChainSettings>(&row.settings) {
        Ok(settings) => Some(DesiredChain {
            chain: row.chain,
            desired: Desired::parse(&row.desired),
            settings,
        }),
        Err(e) => {
            warn!(
                "fleet_chains: the settings of chain {} are not a JSON \
                 object of text values ({e}). The chain is ignored; fix \
                 the row, or add the chain again through the panel.",
                row.chain
            );
            None
        }
    }
}

/// Writes the desired state of one chain. Called AFTER the change took
/// effect in memory, so a failure here costs the next restart's memory and
/// nothing else.
pub async fn save(db: &Database, chain: &DesiredChain) -> Result<()> {
    let settings = serde_json::to_string(&chain.settings)
        .context("serialize the chain settings")?;

    db.db
        .query(&format!(
            "INSERT INTO fleet_chains (chain, desired, settings) \
             SELECT {}, {}, {}",
            chain.chain,
            sql_string(chain.desired.as_str()),
            sql_string(&settings)
        ))
        .execute()
        .await
        .with_context(|| {
            format!("store the desired state of chain {}", chain.chain)
        })
}

/// A chain indexed by ANOTHER process against the same database, seen
/// through the heartbeats of `indexer_instances` (migration 0005).
///
/// The panel shows these read-only: this process does not own them, must
/// not offer a stop button for them, and two writers on one chain would
/// corrupt its aggregates.
#[derive(Debug, Clone, Row, Deserialize, PartialEq, Eq)]
pub struct ForeignChain {
    pub chain: u64,
    pub host: String,
    /// Unix milliseconds, server clock.
    pub heartbeat_ms: i64,
}

/// Chains with a live heartbeat from a process that is not this one.
///
/// `ttl_ms` should be the lease ttl: a heartbeat older than that belongs to
/// a process that is gone.
pub async fn foreign(
    db: &Database,
    ttl_ms: u64,
    mine: &[u64],
) -> Result<Vec<ForeignChain>> {
    let exclude = if mine.is_empty() {
        String::new()
    } else {
        format!(
            "AND chain NOT IN ({}) ",
            mine.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
        )
    };

    db.db
        .query(&format!(
            "SELECT chain, any(host) AS host, \
             toUnixTimestamp64Milli(max(heartbeat)) AS heartbeat_ms \
             FROM indexer_instances \
             WHERE startsWith(instance, 'run|') {exclude}\
             GROUP BY chain \
             HAVING argMax(released, heartbeat) = 0 \
             AND max(heartbeat) > now64(3) - toIntervalMillisecond({ttl_ms}) \
             ORDER BY chain"
        ))
        .fetch_all::<ForeignChain>()
        .await
        .context("look for chains indexed by another process")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_from_a_web_page_can_not_become_sql() {
        assert_eq!(sql_string("plain"), "'plain'");
        assert_eq!(
            sql_string("{\"rpc\":\"o'neil\\\\\"}"),
            "'{\"rpc\":\"o\\'neil\\\\\\\\\"}'"
        );
    }

    #[test]
    fn a_row_whose_settings_are_broken_is_skipped_not_fatal() {
        let good = parse_row(StoredRow {
            chain: 1,
            desired: "running".to_string(),
            settings: "{\"start-block\":\"10\"}".to_string(),
        })
        .unwrap();
        assert_eq!(good.desired, Desired::Running);
        assert_eq!(good.settings["start-block"], "10");

        // Not an object of strings.
        assert!(parse_row(StoredRow {
            chain: 2,
            desired: "running".to_string(),
            settings: "{\"start-block\":10}".to_string(),
        })
        .is_none());

        // Not JSON at all.
        assert!(parse_row(StoredRow {
            chain: 3,
            desired: "running".to_string(),
            settings: "oops".to_string(),
        })
        .is_none());
    }

    #[test]
    fn an_unknown_desired_word_never_starts_a_chain() {
        let row = parse_row(StoredRow {
            chain: 1,
            desired: "RUNNING_MAYBE".to_string(),
            settings: "{}".to_string(),
        })
        .unwrap();
        assert_eq!(row.desired, Desired::Stopped);
    }
}
