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
//!
//! # Fencing
//!
//! Announcing liveness is not enough by itself. A process that is frozen
//! (SIGSTOP, `docker pause`, a VM live-migration stun, the cgroup freezer
//! during a node drain) or cut off from ClickHouse for longer than the ttl
//! stops beating; another process then takes over legitimately - and the
//! first one can still have an insert in flight, or wake up and flush.
//!
//! So every writer holds a [`Fence`] and asks it before each flush and
//! before each purge ([`Fence::check`]): it refuses to write when the
//! lease was lost, and also when this process' OWN last successful
//! heartbeat is older than the ttl, because from that moment on it cannot
//! know whether it still holds the chain.
//!
//! **Residual window, honestly.** The check is not atomic with the insert:
//! between `check()` and the moment ClickHouse accepts the part, up to one
//! flush can still land from a process that is being taken over. The
//! window is bounded by the time a single flush takes, it requires the
//! takeover to happen inside exactly that window, and the takeover itself
//! waits one full ttl before it starts (`Lease::acquire`). Closing it
//! completely needs a fencing token the database enforces, which
//! ClickHouse does not offer (no conditional insert, no compare-and-set),
//! and the design forbids a lock. What the fence removes is the LONG
//! exposure - a frozen process that comes back minutes later and keeps
//! writing under an epoch nobody else knows about.

use crate::db::Database;
use anyhow::{bail, Context, Result};
use clickhouse::Row;
use log::{info, warn};
use serde::Deserialize;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle, time::Instant};

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
pub(crate) struct Other {
    instance: String,
    host: String,
    /// Unix ms, server clock.
    started_ms: i64,
    heartbeat_ms: i64,
}

/// What a writer asks before it writes. Cheap to clone; every clone sees
/// the same lease.
#[derive(Clone)]
pub struct Fence {
    /// The lease was given up: another instance has precedence, or took
    /// over while this one was not beating. Never goes back.
    lost: Arc<AtomicBool>,
    /// When this process last wrote a heartbeat successfully.
    last_ok: Arc<Mutex<Instant>>,
    ttl: Duration,
    chain: u64,
}

impl Fence {
    fn new(chain: u64, ttl: Duration) -> Self {
        Self {
            lost: Arc::new(AtomicBool::new(false)),
            last_ok: Arc::new(Mutex::new(Instant::now())),
            ttl,
            chain,
        }
    }

    /// A fence of a process that holds no lease at all (`indexer
    /// backfill`, tests): always open.
    pub fn open() -> Self {
        Self::new(0, Duration::MAX)
    }

    fn beat_ok(&self) {
        *self.last_ok.lock().unwrap() = Instant::now();
    }

    fn give_up(&self) {
        self.lost.store(true, Ordering::SeqCst);
    }

    /// How long ago this process last proved it is alive.
    pub fn stalled_for(&self) -> Duration {
        self.last_ok.lock().unwrap().elapsed()
    }

    /// `Err` when this process must not write: it lost the lease, or its
    /// own heartbeats have been stalled for longer than the ttl and
    /// another process may have taken the chain over. See the module
    /// documentation for the residual window.
    pub fn check(&self) -> Result<()> {
        if self.lost.load(Ordering::SeqCst) {
            bail!(
                "chain {}: this process lost its indexer lease; refusing \
                 to write. Another process is indexing the chain.",
                self.chain
            );
        }

        let stalled = self.stalled_for();
        if stalled > self.ttl {
            bail!(
                "chain {}: this process has not been able to write a \
                 heartbeat for {stalled:?} (more than the lease ttl {:?}), \
                 so another process may have taken the chain over. \
                 Refusing to write until a heartbeat succeeds again.",
                self.chain,
                self.ttl
            );
        }

        Ok(())
    }
}

pub struct Lease {
    db: Database,
    instance: String,
    host: String,
    started_ms: i64,
    fence: Fence,
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
/// The instance ids of a role all start with `<role>|`, and a lease only
/// ever looks at its own role. `indexer run` is [`ROLE_RUN`]; `indexer
/// backfill --module X` is a role of its own, so a second backfill of the
/// same module is refused while the documented combination "a backfill
/// next to a live indexer" keeps working.
fn role_of(instance: &str) -> &str {
    instance.split_once('|').map_or(ROLE_RUN, |(role, _)| role)
}

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
             AND startsWith(instance, {}) \
             GROUP BY instance \
             HAVING argMax(released, heartbeat) = 0 \
             AND max(heartbeat) > now64(3) - toIntervalMillisecond({})",
            db.chain_id,
            sql_string(instance),
            sql_string(&format!("{}|", role_of(instance))),
            ttl.as_millis()
        ))
        .fetch_all::<Other>()
        .await
        .context("query the live indexer instances")
}

/// Should this process stop, and why?
///
/// `stalled`: this process' own last successful heartbeat is older than
/// the ttl, so it may have been taken over - then ANY live instance wins,
/// not only an older one (a legitimate takeover starts a YOUNGER process).
/// Otherwise only an instance with precedence - started earlier, ties
/// broken by the instance id - makes this one stop.
fn takeover<'a>(
    stalled: bool,
    others: &'a [Other],
    started_ms: i64,
    instance: &str,
) -> Vec<&'a Other> {
    others
        .iter()
        .filter(|other| {
            stalled
                || (other.started_ms, other.instance.as_str())
                    < (started_ms, instance)
        })
        .collect()
}

/// The role of `indexer run`: the process that streams the chain.
pub const ROLE_RUN: &str = "run";

impl Lease {
    /// Announces this process and makes sure it is alone on the chain.
    /// `fatal` receives the reason when a live older instance shows up
    /// later.
    pub async fn acquire(
        db: &Database,
        options: LeaseOptions,
        fatal: watch::Sender<Option<String>>,
    ) -> Result<Self> {
        Self::acquire_as(db, ROLE_RUN, options, fatal).await
    }

    /// [`Self::acquire`] for a writer that is not `indexer run`.
    ///
    /// A role only excludes OTHER processes of the SAME role. `indexer
    /// backfill --module X` is a writer too - it purges, bumps the epoch
    /// and rebuilds every aggregate - so two of them on one chain corrupt
    /// it exactly as two indexers would (the lower-epoch rebuild ends up
    /// hidden by the higher floor, docs/review-round-4.md, MINOR 16). It
    /// may however run NEXT TO a live indexer, which is documented and
    /// handled (`pipeline::backfill`), so it must not take the run role's
    /// lease.
    pub async fn acquire_as(
        db: &Database,
        role: &str,
        options: LeaseOptions,
        fatal: watch::Sender<Option<String>>,
    ) -> Result<Self> {
        debug_assert!(!role.contains('|'), "'|' separates role and id");

        let instance = format!(
            "{role}|{:016x}{:016x}",
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
                    "another '{role}' process is already writing chain {} \
                     into this database ({}). Two of them on the same \
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

        let fence = Fence::new(db.chain_id, options.ttl);

        let task = {
            let db = db.clone();
            let instance = instance.clone();
            let host = host.clone();
            let fence = fence.clone();

            tokio::spawn(async move {
                let mut tick = tokio::time::interval(options.heartbeat);
                tick.tick().await;

                loop {
                    tick.tick().await;

                    // A beat that fails leaves `last_ok` where it was: as
                    // soon as it is older than the ttl the fence closes,
                    // because from then on another process may have taken
                    // the chain over.
                    if let Err(e) =
                        beat(&db, &instance, &host, started_ms, false)
                            .await
                    {
                        warn!(
                            "Instance heartbeat failed ({:?} since the \
                             last one that worked): {e:#}",
                            fence.stalled_for()
                        );
                        continue;
                    }

                    // The check has to use the state BEFORE this beat: the
                    // takeover the fence protects against happened while
                    // this process was not beating.
                    let stalled = fence.stalled_for() > options.ttl;

                    let others =
                        match others_alive(&db, &instance, options.ttl)
                            .await
                        {
                            Ok(others) => others,
                            Err(e) => {
                                warn!("Instance check failed: {e:#}");
                                continue;
                            }
                        };

                    let wins = takeover(
                        stalled,
                        &others,
                        started_ms,
                        instance.as_str(),
                    );

                    if !wins.is_empty() {
                        let owned: Vec<Other> =
                            wins.into_iter().cloned().collect();
                        fence.give_up();
                        let _ = fatal.send(Some(if stalled {
                            format!(
                                "chain {}: this process could not write a \
                                 heartbeat for longer than the lease ttl \
                                 {:?} and another instance is alive ({}): \
                                 it has taken the chain over. Stopping \
                                 this one.",
                                db.chain_id,
                                options.ttl,
                                describe(&owned)
                            )
                        } else {
                            format!(
                                "another indexer process is indexing \
                                 chain {} into this database ({}) and has \
                                 precedence. Stopping this one.",
                                db.chain_id,
                                describe(&owned)
                            )
                        }));
                        return;
                    }

                    fence.beat_ok();
                }
            })
        };

        Ok(Self {
            db: db.clone(),
            instance,
            host,
            started_ms,
            fence,
            task,
        })
    }

    /// What the writer asks before every flush and every purge.
    pub fn fence(&self) -> Fence {
        self.fence.clone()
    }

    /// Clean shutdown: the next start does not have to wait.
    pub async fn release(self) {
        self.task.abort();
        self.fence.give_up();
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
        self.fence.give_up();
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

    fn other(started_ms: i64, instance: &str) -> Other {
        Other {
            instance: instance.to_string(),
            host: "h".to_string(),
            started_ms,
            heartbeat_ms: started_ms + 1,
        }
    }

    #[test]
    fn a_process_that_still_beats_only_yields_to_an_older_instance() {
        let younger = [other(200, "b")];
        let older = [other(50, "b")];

        assert!(takeover(false, &younger, 100, "a").is_empty());
        assert_eq!(takeover(false, &older, 100, "a").len(), 1);

        // Same start instant: the instance id breaks the tie, both ways.
        assert_eq!(takeover(false, &[other(100, "b")], 100, "c").len(), 1);
        assert!(takeover(false, &[other(100, "c")], 100, "b").is_empty());
    }

    /// What `lease.rs` documents and nothing implemented: a process whose
    /// own heartbeats stalled past the ttl may have been taken over, and a
    /// legitimate takeover is always a YOUNGER process.
    #[test]
    fn a_process_whose_heartbeats_stalled_yields_to_any_live_instance() {
        let younger = [other(200, "b")];

        assert!(takeover(true, &younger, 100, "a").len() == 1);
        // Nobody else alive: nothing to yield to, keep indexing.
        assert!(takeover(true, &[], 100, "a").is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn the_fence_closes_when_the_heartbeats_stall_and_when_it_is_lost(
    ) {
        let ttl = Duration::from_secs(30);
        let fence = Fence::new(7, ttl);

        fence.check().unwrap();

        // Heartbeats stop: from one ttl on, this process can no longer
        // know whether it still owns the chain.
        tokio::time::sleep(ttl + Duration::from_secs(1)).await;
        let error = fence.check().unwrap_err().to_string();
        assert!(error.contains("chain 7"), "{error}");
        assert!(error.contains("heartbeat"), "{error}");

        // A heartbeat that works again reopens it ...
        fence.beat_ok();
        fence.check().unwrap();

        // ... but a lost lease never does.
        fence.give_up();
        let error = fence.check().unwrap_err().to_string();
        assert!(error.contains("lost its indexer lease"), "{error}");
        fence.beat_ok();
        assert!(fence.check().is_err());

        // A process without a lease is never fenced.
        Fence::open().check().unwrap();
    }

    #[test]
    fn ttl_outlasts_several_heartbeats() {
        let options = LeaseOptions::default();
        assert!(options.ttl >= options.heartbeat * 3);
    }
}
