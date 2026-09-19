//! `fleet_chains` against a REAL ClickHouse. Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test fleet::integration_tests:: -- --ignored --test-threads=1
//! ```
//!
//! What is proved here and cannot be proved without a server: that
//! migration 0007 applies on top of the whole embedded set, that a settings
//! map survives the round trip through a JSON column, that the
//! ReplacingMergeTree really does keep only the newest row per chain (the
//! supervisor writes one row per command, for ever), and that the read of
//! chains indexed by ANOTHER process sees real heartbeats.
//!
//! Each test creates and drops its own `..._test` database and never
//! touches the one named in the url.

use super::chains::{foreign, load, save, DesiredChain};
use crate::{
    configs::{ChainSettings, Desired},
    db::{migrate, Database, DatabaseParams},
};
use clickhouse::Client;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL_MS: u64 = 35_000;

struct TestDb {
    admin: Client,
    url: String,
    name: String,
}

impl TestDb {
    async fn create() -> Self {
        let url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL must be set for the ignored tests");
        let params = DatabaseParams::parse(&url).unwrap();

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static SEQUENCE: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);
        let sequence =
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("fleet_{nanos}_{sequence}_test");

        let admin = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);
        admin
            .query(&format!("CREATE DATABASE {name}"))
            .execute()
            .await
            .expect("create test database");

        let db_url = replace_database(&url, &name);
        migrate::run(&db_url).await.expect("apply migrations");

        Self { admin, url: db_url, name }
    }

    async fn database(&self) -> Database {
        Database::new(&self.url, 0)
            .await
            .expect("connect to the test database")
    }

    async fn drop(self) {
        let _ = self
            .admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await;
    }
}

fn replace_database(url: &str, name: &str) -> String {
    let (head, _) = url.rsplit_once('/').unwrap_or((url, ""));
    format!("{head}/{name}")
}

/// ClickHouse gives no read-your-writes guarantee: wait for what was
/// written to become readable rather than assuming it is.
async fn settle(
    db: &Database,
    mut ready: impl FnMut(&[DesiredChain]) -> bool,
) -> Vec<DesiredChain> {
    for _ in 0..100 {
        let rows = load(db).await.expect("read fleet_chains");
        if ready(&rows) {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("fleet_chains never reached the expected state");
}

fn settings(pairs: &[(&str, &str)]) -> ChainSettings {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_desired_state_of_a_chain_survives_a_round_trip() {
    let test = TestDb::create().await;
    let db = test.database().await;

    // A brand new database knows no chain, and that is not an error.
    assert!(load(&db).await.unwrap().is_empty());

    let written = DesiredChain {
        chain: 8453,
        desired: Desired::Running,
        settings: settings(&[
            ("confirmations", "12"),
            ("no-predictions", "true"),
            // A value with the characters that would break a hand written
            // INSERT, to prove the escaping. (It is not a value the
            // command line would accept, but this test is about the
            // ClickHouse round trip, not about validation.)
            ("max-reorg-depth", "o'neil\\"),
        ]),
    };

    save(&db, &written).await.unwrap();

    let rows = settle(&db, |rows| rows.len() == 1).await;
    assert_eq!(rows[0], written);

    test.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn only_the_newest_row_of_a_chain_is_read_back() {
    let test = TestDb::create().await;
    let db = test.database().await;

    // The supervisor writes one row per command, for ever: the table is
    // insert only and nothing is ever deleted from it.
    for confirmations in ["1", "2", "3", "4", "5"] {
        save(
            &db,
            &DesiredChain {
                chain: 1,
                desired: Desired::Running,
                settings: settings(&[("confirmations", confirmations)]),
            },
        )
        .await
        .unwrap();
        // `_version` is a millisecond clock: two rows inside one
        // millisecond are the same version and the tie is arbitrary.
        tokio::time::sleep(Duration::from_millis(3)).await;
    }

    save(
        &db,
        &DesiredChain {
            chain: 1,
            desired: Desired::Stopped,
            settings: settings(&[("confirmations", "6")]),
        },
    )
    .await
    .unwrap();

    let rows = settle(&db, |rows| {
        rows.len() == 1 && rows[0].desired == Desired::Stopped
    })
    .await;

    assert_eq!(rows.len(), 1, "the view must collapse the history");
    assert_eq!(rows[0].settings["confirmations"], "6");

    // And nothing was DELETED to get there. The old rows disappear because
    // the ReplacingMergeTree collapses them when it merges, which is the
    // engine's own housekeeping; the indexer never issues a DELETE or an
    // ALTER DELETE anywhere (docs/design.md section 2), and a mutation is
    // what that would leave behind.
    let mutations: u64 = db
        .db
        .query(
            "SELECT count() FROM system.mutations \
             WHERE database = currentDatabase()",
        )
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(mutations, 0, "something issued an ALTER DELETE");

    test.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_chain_with_no_settings_at_all_round_trips() {
    let test = TestDb::create().await;
    let db = test.database().await;

    let written = DesiredChain {
        chain: crate::pipeline::solana::SOLANA_CHAIN_ID,
        desired: Desired::Running,
        settings: ChainSettings::new(),
    };
    save(&db, &written).await.unwrap();

    let rows = settle(&db, |rows| rows.len() == 1).await;
    assert_eq!(rows[0], written);
    assert!(rows[0].settings.is_empty());

    test.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn several_chains_come_back_in_order_and_unmixed() {
    let test = TestDb::create().await;
    let db = test.database().await;

    for (chain, desired) in [
        (8453, Desired::Running),
        (1, Desired::Running),
        (10, Desired::Stopped),
    ] {
        save(
            &db,
            &DesiredChain {
                chain,
                desired,
                settings: settings(&[(
                    "confirmations",
                    &chain.to_string(),
                )]),
            },
        )
        .await
        .unwrap();
    }

    let rows = settle(&db, |rows| rows.len() == 3).await;

    assert_eq!(
        rows.iter().map(|row| row.chain).collect::<Vec<_>>(),
        // `ORDER BY chain`: numeric order, not the order they arrived in.
        [1_u64, 10, 8453].to_vec()
    );
    for row in &rows {
        assert_eq!(row.settings["confirmations"], row.chain.to_string());
    }
    assert_eq!(
        rows.iter().find(|row| row.chain == 10).unwrap().desired,
        Desired::Stopped
    );

    test.drop().await;
}

/// The panel shows chains that a DIFFERENT process is indexing, read only.
/// They come from the heartbeats of `indexer_instances` (migration 0005),
/// which is what a live `indexer run` elsewhere keeps writing.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn chains_another_process_is_indexing_are_seen_through_heartbeats() {
    let test = TestDb::create().await;
    let db = test.database().await;

    let beat = |chain: u64, instance: &str, released: u8, age: &str| {
        let instance = instance.to_string();
        let age = age.to_string();
        let db = db.clone();
        async move {
            db.db
                .query(&format!(
                    "INSERT INTO indexer_instances \
                     (chain, instance, host, started_at, heartbeat, \
                      released) \
                     SELECT {chain}, '{instance}', 'other-host', now64(3), \
                     now64(3) - {age}, {released}"
                ))
                .execute()
                .await
                .unwrap();
        }
    };

    // Alive, and not ours.
    beat(137, "run|aaaa", 0, "toIntervalSecond(1)").await;
    // Alive, but this process indexes it: never listed twice.
    beat(1, "run|bbbb", 0, "toIntervalSecond(1)").await;
    // Shut down cleanly: gone.
    beat(42161, "run|cccc", 1, "toIntervalSecond(1)").await;
    // Long silent: gone.
    beat(56, "run|dddd", 0, "toIntervalSecond(600)").await;
    // A backfill, not an indexer: a different role, never listed.
    beat(999, "backfill|eeee", 0, "toIntervalSecond(1)").await;

    let mut seen = Vec::new();
    for _ in 0..100 {
        seen = foreign(&db, TTL_MS, &[1]).await.unwrap();
        if seen.len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        seen.iter().map(|other| other.chain).collect::<Vec<_>>(),
        [137],
        "{seen:?}"
    );
    assert_eq!(seen[0].host, "other-host");

    // With nothing of our own, the same live chain is still the only one.
    let all = foreign(&db, TTL_MS, &[]).await.unwrap();
    assert_eq!(
        all.iter().map(|other| other.chain).collect::<Vec<_>>(),
        [1, 137]
    );

    test.drop().await;
}
