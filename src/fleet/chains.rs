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
use std::collections::BTreeMap;

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
        Ok(mut settings) => {
            // A row written by an older build can name a setting this one
            // no longer lets a web page change - the HyperSync endpoint and
            // the RPC endpoints were editable before the security review
            // (MAJOR 4). Dropping them is the safe reading of "an endpoint
            // never comes from the panel": the chain starts with the values
            // the fleet process itself was given instead of refusing to
            // start at all, and the owner is told.
            settings.retain(|key, _| {
                let known = crate::configs::CHAIN_SETTINGS
                    .iter()
                    .any(|setting| setting.name == key);

                if !known {
                    warn!(
                        "fleet_chains: chain {} has a stored setting \
                         '{key}' that the control panel is no longer \
                         allowed to set. It is ignored; the value the \
                         fleet process itself was given is used instead.",
                        row.chain
                    );
                }

                known
            });

            Some(DesiredChain {
                chain: row.chain,
                desired: Desired::parse(&row.desired),
                settings,
            })
        }
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

/// What every chain in this database promises, in the one sentence
/// `indexer verify` and the panel both use (docs/design.md section 16).
///
/// One query for the whole fleet: `coverage_v` is one row per chain, and
/// the panel asks for every chain at once. A chain with no floor stored
/// yet is simply absent from the map, and its card shows no coverage line
/// rather than an invented one.
///
/// The head is named by its BLOCK and not by its date here. Dating it would
/// be one point read per chain on every poll of the panel, for a number
/// that is moving anyway; `indexer verify` is the place that spends that
/// read.
pub async fn coverage(db: &Database) -> Result<BTreeMap<u64, String>> {
    let rows = db
        .db
        .query(
            "SELECT chain, coverage_from_block, coverage_from_ts, reason, \
             covered_to_block FROM coverage_v ORDER BY chain",
        )
        .fetch_all::<(u64, u64, u32, String, u64)>()
        .await
        .context("read what each chain promises (coverage_v)")?;

    Ok(rows
        .into_iter()
        .map(|(chain, block, timestamp, reason, covered_to_block)| {
            let coverage = crate::coverage::store::Coverage {
                floor: crate::coverage::store::Floor {
                    block,
                    timestamp,
                    reason: crate::coverage::store::Reason::parse(&reason)
                        .unwrap_or(
                            crate::coverage::store::Reason::StartBlock,
                        ),
                },
                covered_to_block,
            };
            (
                chain,
                crate::coverage::store::sentence(
                    &coverage,
                    crate::coverage::store::unit_of(chain),
                    None,
                    None,
                ),
            )
        })
        .collect())
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
            settings: "{\"confirmations\":\"10\"}".to_string(),
        })
        .unwrap();
        assert_eq!(good.desired, Desired::Running);
        assert_eq!(good.settings["confirmations"], "10");

        // Not an object of strings.
        assert!(parse_row(StoredRow {
            chain: 2,
            desired: "running".to_string(),
            settings: "{\"confirmations\":10}".to_string(),
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

    /// Review MAJOR 4: a row written before the endpoints stopped being
    /// panel-editable must not stop the chain from starting, and must not
    /// resurrect the setting either.
    #[test]
    fn a_stored_setting_the_panel_may_no_longer_set_is_dropped() {
        let row = parse_row(StoredRow {
            chain: 1,
            desired: "running".to_string(),
            settings: "{\"rpc\":\"x\",\"hypersync-url\":\"y\",\"confirmations\":\"12\"}"
                .to_string(),
        })
        .unwrap();

        assert_eq!(row.settings.len(), 1, "{:?}", row.settings);
        assert_eq!(row.settings["confirmations"], "12");
        assert!(!row.settings.contains_key("rpc"));
        assert!(!row.settings.contains_key("hypersync-url"));
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
