//! One indexer process per chain.
//!
//! The epoch of a chain lives in the memory of its writer (docs/design.md,
//! section 2): two processes indexing the SAME chain would each keep their
//! own and hide each other's aggregates. There is no lock in ClickHouse and
//! the design forbids one, so liveness is announced instead, insert only:
//!
//! * every process inserts a heartbeat row into `indexer_instances` every
//!   [`LeaseOptions::heartbeat`], stamped with the SERVER clock (no clock
//!   skew between hosts);
//! * at startup, another instance of the chain whose heartbeat is younger
//!   than [`LeaseOptions::ttl`] may be alive (or may have been killed a
//!   moment ago). The new process waits one `ttl`: if that heartbeat moved,
//!   the other process IS alive and this one refuses to start; if not, it
//!   is dead and this one takes over;
//! * while running, every heartbeat also looks for a live OLDER instance
//!   (two processes started in the same instant, or a read that lagged at
//!   startup): the younger process stops. A process whose own heartbeats
//!   stalled for longer than the ttl stops for ANY live instance: it may
//!   have been taken over;
//! * a clean shutdown writes `released = 1`, so a restart does not wait.
//!
//! Cost: one tiny insert and one tiny query per heartbeat.

use crate::db::Database;
use anyhow::{bail, Context, Result};
use clickhouse::Row;
use log::{info, warn};
use serde::Deserialize;
use std::time::Duration;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Debug, Clone, Copy)]
pub struct LeaseOptions {
    pub heartbeat: Duration,
    /// A heartbeat older than this belongs to a dead process.
    pub ttl: Duration,
}

impl Default for LeaseOptions {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(10),
            ttl: Duration::from_secs(35),
        }
    }
}

#[derive(Debug, Clone, Row, Deserialize, PartialEq, Eq)]
struct Other {
    instance: String,
    host: String,
    /// Unix ms, server clock.
    started_ms: i64,
    heartbeat_ms: i64,
}

pub struct Lease {
    db: Database,
    instance: String,
    host: String,
    started_ms: i64,
    task: JoinHandle<()>,
}

/// Randomly keyed std hasher: unpredictable enough for an instance id,
/// without a new dependency.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish()
}

fn sql_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

async fn beat(
    db: &Database,
    instance: &str,
    host: &str,
    started_ms: i64,
    released: bool,
) -> Result<()> {
    db.db
        .query(&format!(
            "INSERT INTO indexer_instances \
             (chain, instance, host, started_at, heartbeat, released) \
             SELECT {}, {}, {}, fromUnixTimestamp64Milli(toInt64({})), \
             now64(3), {}",
            db.chain_id,
            sql_string(instance),
            sql_string(host),
            started_ms,
            u8::from(released)
        ))
        .execute()
        .await
        .context("write the instance heartbeat")
}

/// Other instances of the chain with a heartbeat younger than `ttl` that
/// did not release.
async fn others_alive(
    db: &Database,
    instance: &str,
    ttl: Duration,
) -> Result<Vec<Other>> {
    db.db
        .query(&format!(
            "SELECT instance, any(host) AS host, \
             toUnixTimestamp64Milli(min(started_at)) AS started_ms, \
             toUnixTimestamp64Milli(max(heartbeat)) AS heartbeat_ms \
             FROM indexer_instances \
             WHERE chain = {} AND instance != {} \
             GROUP BY instance \
             HAVING argMax(released, heartbeat) = 0 \
             AND max(heartbeat) > now64(3) - toIntervalMillisecond({})",
            db.chain_id,
            sql_string(instance),
            ttl.as_millis()
        ))
        .fetch_all::<Other>()
        .await
        .context("query the live indexer instances")
}

impl Lease {
    /// Announces this process and makes sure it is alone on the chain.
    /// `fatal` receives the reason when a live older instance shows up
    /// later.
    pub async fn acquire(
        db: &Database,
        options: LeaseOptions,
        fatal: watch::Sender<Option<String>>,
    ) -> Result<Self> {
        let instance = format!(
            "{:016x}{:016x}",
            random_u64(),
            random_u64() ^ u64::from(std::process::id())
        );
        let host = std::env::var("HOSTNAME")
            .unwrap_or_else(|_| format!("pid-{}", std::process::id()));

        let started_ms: i64 = db
            .db
            .query("SELECT toUnixTimestamp64Milli(now64(3))")
            .fetch_one()
            .await
            .context("read the server clock")?;

        beat(db, &instance, &host, started_ms, false).await?;

        let seen = others_alive(db, &instance, options.ttl).await?;

        if !seen.is_empty() {
            warn!(
                "Chain {}: another indexer instance wrote a heartbeat less \
                 than {:?} ago ({}). Waiting {:?} to see whether it is \
                 alive or was killed.",
                db.chain_id,
                options.ttl,
                describe(&seen),
                options.ttl
            );

            let deadline = tokio::time::Instant::now() + options.ttl;
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(options.heartbeat.min(options.ttl))
                    .await;
                beat(db, &instance, &host, started_ms, false).await?;
            }

            let now = others_alive(db, &instance, options.ttl).await?;
            let alive: Vec<Other> = now
                .into_iter()
                .filter(|other| {
                    seen.iter().all(|before| {
                        before.instance != other.instance
                            || other.heartbeat_ms > before.heartbeat_ms
                    })
                })
                .collect();

            if !alive.is_empty() {
                // Do not leave a live-looking row behind.
                let _ = beat(db, &instance, &host, started_ms, true).await;
                bail!(
                    "another indexer process is already indexing chain {} \
                     into this database ({}). Two processes on the same \
                     chain corrupt its aggregates: stop the other one \
                     first.",
                    db.chain_id,
                    describe(&alive)
                );
            }

            info!(
                "Chain {}: the other instance is gone, taking over.",
                db.chain_id
            );
        }

        let task = {
            let db = db.clone();
            let instance = instance.clone();
            let host = host.clone();

            tokio::spawn(async move {
                let mut tick = tokio::time::interval(options.heartbeat);
                tick.tick().await;

                loop {
                    tick.tick().await;

                    if let Err(e) =
                        beat(&db, &instance, &host, started_ms, false)
                            .await
                    {
                        warn!("Instance heartbeat failed: {e:#}");
                        continue;
                    }

                    let older =
                        match others_alive(&db, &instance, options.ttl)
                            .await
                        {
                            Ok(others) => others
                                .into_iter()
                                .filter(|other| {
                                    (
                                        other.started_ms,
                                        other.instance.as_str(),
                                    ) < (started_ms, instance.as_str())
                                })
                                .collect::<Vec<_>>(),
                            Err(e) => {
                                warn!("Instance check failed: {e:#}");
                                continue;
                            }
                        };

                    if !older.is_empty() {
                        let _ = fatal.send(Some(format!(
                            "another indexer process is indexing chain {} \
                             into this database ({}) and has precedence. \
                             Stopping this one.",
                            db.chain_id,
                            describe(&older)
                        )));
                        return;
                    }
                }
            })
        };

        Ok(Self { db: db.clone(), instance, host, started_ms, task })
    }

    /// Clean shutdown: the next start does not have to wait.
    pub async fn release(self) {
        self.task.abort();
        if let Err(e) = beat(
            &self.db,
            &self.instance,
            &self.host,
            self.started_ms,
            true,
        )
        .await
        {
            warn!("Could not release the instance lease: {e:#}");
        }
    }
}

/// Dropped without [`Lease::release`] (a fatal error path): the heartbeat
/// stops, and the next start takes over after one `ttl`.
impl Drop for Lease {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn describe(others: &[Other]) -> String {
    others
        .iter()
        .map(|other| {
            format!("instance {} on {}", &other.instance[..8], other.host)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped() {
        assert_eq!(sql_string("plain"), "'plain'");
        assert_eq!(sql_string("o'neil\\"), "'o\\'neil\\\\'");
    }

    #[test]
    fn ttl_outlasts_several_heartbeats() {
        let options = LeaseOptions::default();
        assert!(options.ttl >= options.heartbeat * 3);
    }
}
