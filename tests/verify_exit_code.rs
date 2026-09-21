//! The exit code of `indexer verify`, through the REAL binary.
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@127.0.0.1:8123/cli_test \
//!   cargo test --test verify_exit_code -- --ignored
//! ```
//!
//! Everything else about the coverage floor is tested inside the library
//! (`pipeline::acceptance`, `pipeline::solana_acceptance`). This file
//! exists for the one thing a library test cannot reach: what a shell
//! script, a cron job or a CI step actually sees. `verify` is meant to be
//! wired into monitoring, and before the floor was taught to `verify` the
//! answer on a perfectly healthy database was **1**, for ever - which is
//! the fastest way to teach an operator to ignore an alert.
//!
//! The fixture is hand written SQL rather than a pipeline run on purpose:
//! this test is about the process's exit status, so the less of the
//! indexer stands between the database and the assertion, the better.

use clickhouse::Client;
use evm_indexer::db::{migrate, DatabaseParams};
use std::process::Command;

const CHAIN: u64 = 1;

/// A whole day before midnight, so the fixture stays inside ONE UTC day:
/// the aggregate cross-check then has no complete day to compare and says
/// so, which is a "not fully checked" verdict and still exit code 0.
const BASE_TIMESTAMP: u64 = 1_700_000_000;

/// The chain's coverage floor. Far enough above zero that a check starting
/// at 0 cannot possibly be mistaken for one starting at the floor.
const FLOOR: u64 = 23_399_283;

/// Blocks stored above the floor.
const BLOCKS: u64 = 40;

/// `<prefix>_<name>_test`, dropped and migrated. Returns the url.
async fn database(name: &str) -> Option<String> {
    let base = std::env::var("TEST_DATABASE_URL").ok()?;
    let params = DatabaseParams::parse(&base).unwrap();

    assert!(
        params.database.ends_with("_test"),
        "TEST_DATABASE_URL names database '{}': this test DROPs databases \
         derived from it, so it must end in '_test'",
        params.database
    );

    let prefix = params.database.trim_end_matches("_test");
    let db = format!("{prefix}_{name}_test");
    let url = base.replacen(
        &format!("/{}", params.database),
        &format!("/{db}"),
        1,
    );

    Client::default()
        .with_url(&params.endpoint)
        .with_user(&params.user)
        .with_password(&params.password)
        .query(&format!("DROP DATABASE IF EXISTS `{db}`"))
        .execute()
        .await
        .unwrap();

    migrate::run(&url).await.unwrap();

    Some(url)
}

fn client(url: &str) -> Client {
    let params = DatabaseParams::parse(url).unwrap();
    Client::default()
        .with_url(&params.endpoint)
        .with_user(&params.user)
        .with_password(&params.password)
        .with_database(&params.database)
}

/// A healthy floor-to-head chain: blocks `[FLOOR, FLOOR + BLOCKS)`, one
/// checkpoint claiming exactly that, and the floor written down.
async fn healthy(url: &str) {
    let client = client(url);

    let values: Vec<String> = (0..BLOCKS)
        .map(|offset| {
            format!(
                "({CHAIN}, {}, toDateTime({}), {})",
                FLOOR + offset,
                BASE_TIMESTAMP + offset * 60,
                offset + 1
            )
        })
        .collect();

    client
        .query(&format!(
            "INSERT INTO blocks (chain, number, timestamp, _version) \
             VALUES {}",
            values.join(", ")
        ))
        .execute()
        .await
        .unwrap();

    client
        .query(&format!(
            "INSERT INTO checkpoints (chain, from_block, to_block, epoch, \
             _version, is_deleted) VALUES ({CHAIN}, {FLOOR}, {}, 0, 1, 0)",
            FLOOR + BLOCKS
        ))
        .execute()
        .await
        .unwrap();

    client
        .query(&format!(
            "INSERT INTO chain_coverage (chain, coverage_from_block, \
             coverage_from_ts, reason, _version) VALUES \
             ({CHAIN}, {FLOOR}, {BASE_TIMESTAMP}, 'default-1y', {})",
            u64::MAX - FLOOR
        ))
        .execute()
        .await
        .unwrap();
}

/// Runs the real binary and returns `(exit code, stdout)`.
fn verify(url: &str, extra: &[&str]) -> (i32, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_indexer"));
    command.arg("verify").arg("--database").arg(url);
    command.args(extra);

    // The binary reads these from the environment when the flag is absent,
    // and the answer must come from the database, not from whatever the
    // machine running the tests happens to export.
    for name in ["START_BLOCK", "END_BLOCK", "CHAIN_ID", "DATABASE_URL"] {
        command.env_remove(name);
    }

    let output = command.output().expect("run `indexer verify`");

    (
        output.status.code().expect("indexer verify was killed"),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Exit code 0 on a healthy floor-to-head database, 1 when the operator
/// asks about the range below the floor and is told the truth about it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn verify_exits_zero_on_a_healthy_floor_to_head_database() {
    let Some(url) = database("cli_exit").await else {
        panic!("TEST_DATABASE_URL must be set for the ignored tests");
    };

    healthy(&url).await;

    let (code, out) = verify(&url, &[]);
    assert_eq!(code, 0, "`indexer verify` said PROBLEMS FOUND:\n{out}");
    assert!(!out.contains("PROBLEMS FOUND"), "{out}");
    assert!(
        out.contains(&format!("blocks [{FLOOR}, {}", FLOOR + BLOCKS)),
        "the check did not start at the floor:\n{out}"
    );
    assert!(out.contains("Gaps: none."), "{out}");

    // Asked about the whole chain, it answers about the whole chain: the
    // blocks below the floor really are not there.
    let (code, out) = verify(&url, &["--start-block", "0"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("PROBLEMS FOUND"), "{out}");
    assert!(out.contains("BELOW this chain's coverage floor"), "{out}");
}

/// And a database that really is broken still exits 1 with no flags: the
/// floor moves where the check starts, it does not soften what it finds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn verify_exits_one_when_something_above_the_floor_is_missing() {
    let Some(url) = database("cli_exit_gap").await else {
        panic!("TEST_DATABASE_URL must be set for the ignored tests");
    };

    healthy(&url).await;

    // One block above the floor goes away, leaving a hole inside the
    // window this database promises.
    let missing = FLOOR + BLOCKS / 2;
    client(&url)
        .query(&format!(
            "INSERT INTO blocks (chain, number, timestamp, _version, \
             is_deleted) VALUES ({CHAIN}, {missing}, toDateTime({}), \
             1000, 1)",
            BASE_TIMESTAMP
        ))
        .execute()
        .await
        .unwrap();

    let (code, out) = verify(&url, &[]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("PROBLEMS FOUND"), "{out}");
    assert!(
        out.contains(&format!("[{missing}, {})", missing + 1)),
        "{out}"
    );
}
