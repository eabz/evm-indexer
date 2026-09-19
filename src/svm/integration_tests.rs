//! The `sol_*` tables against a REAL ClickHouse. Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test svm::integration -- --ignored
//! ```
//!
//! Unlike the other modules' integration tests these apply the FULL embedded
//! migration set through `crate::db::migrate::run`, so they also prove that
//! `0040` / `0041` compose with everything before them. Each test creates and
//! drops its own `..._test` database and never touches the one named in the
//! url.
//!
//! Rows go in through the real insert path (`Database::insert_flush`), i.e.
//! the clickhouse crate's RowBinary with this module's own serializers - a
//! 32-byte pubkey as `FixedString(32)`, a 64-byte signature as
//! `FixedString(64)`, a `UInt256` as four little endian `u64` limbs. That is
//! the part a hand written `INSERT ... VALUES` would not exercise.

use std::time::{SystemTime, UNIX_EPOCH};

use clickhouse::Client;

use crate::{
    db::{migrate, next_version, Database, DatabaseParams, FlushKey},
    svm::{
        self, fixtures,
        models::{Pubkey, SOLANA_CHAIN},
        programs::{pubkey, to_base58, Registry, Venue},
        SvmRows, SvmSlotBatch,
    },
};

const CHAIN: u64 = SOLANA_CHAIN;
const WSOL: &str = "So11111111111111111111111111111111111111112";

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
        let name = format!("svm_{nanos}_{sequence}_test");

        let admin = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);
        admin
            .query(&format!("CREATE DATABASE {name}"))
            .execute()
            .await
            .expect("create test database");

        // The whole embedded set, in order, exactly as `indexer migrate`
        // applies it.
        let db_url = replace_database(&url, &name);
        migrate::run(&db_url).await.expect("apply migrations");

        Self { admin, url: db_url, name }
    }

    async fn database(&self) -> Database {
        Database::new(&self.url, CHAIN)
            .await
            .expect("connect to the test database")
    }

    fn client(&self) -> Client {
        let params = DatabaseParams::parse(&self.url).unwrap();
        Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password)
            .with_database(&self.name)
    }

    async fn scalar(&self, sql: &str) -> String {
        self.client()
            .query(sql)
            .fetch_one::<String>()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn count(&self, sql: &str) -> u64 {
        self.client()
            .query(sql)
            .fetch_one::<u64>()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    async fn drop(self) {
        let _ = self
            .admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await;
    }
}

/// Swaps the database out of a ClickHouse url.
fn replace_database(url: &str, name: &str) -> String {
    let (head, _) = url.rsplit_once('/').unwrap_or((url, ""));
    format!("{head}/{name}")
}

/// Every recorded fixture, decoded into rows ready to insert.
fn all_rows() -> SvmRows {
    let batches: Vec<SvmSlotBatch> = fixtures::all()
        .iter()
        .map(|fixture| SvmSlotBatch {
            slot: fixture.slot,
            blockhash: fixture.blockhash,
            parent_slot: fixture.parent_slot,
            parent_blockhash: fixture.parent_blockhash,
            block_height: 0,
            timestamp: fixture.timestamp(),
            transactions: vec![fixture.transaction.clone()],
        })
        .collect();

    let mut rows = svm::decode(CHAIN, &batches);
    rows.set_version(next_version());
    rows.set_epoch(0);
    rows
}

/// Writes `rows` through the real insert path, children before the commit
/// marker, exactly as a flush does.
async fn store(db: &Database, rows: &SvmRows) {
    let key = FlushKey {
        chain: CHAIN,
        span: (
            rows.slots.iter().map(|s| s.block_number).min().unwrap(),
            rows.slots.iter().map(|s| s.block_number).max().unwrap(),
        ),
        version: rows.slots[0]._version,
    };
    db.insert_flush("sol_dex_swaps", &rows.swaps, &key)
        .await
        .expect("insert sol_dex_swaps");
    db.insert_flush("sol_transactions", &rows.transactions, &key)
        .await
        .expect("insert sol_transactions");
    db.insert_flush("sol_slots", &rows.slots, &key)
        .await
        .expect("insert sol_slots");
    db.insert_rows("sol_tokens", &rows.tokens)
        .await
        .expect("insert sol_tokens");
}

// --- the schema ----------------------------------------------------------

/// The Solana migrations apply cleanly on top of the whole embedded set.
#[tokio::test]
#[ignore]
async fn the_migrations_apply_on_top_of_every_other_module() {
    let db = TestDb::create().await;

    for table in [
        "sol_slots",
        "sol_transactions",
        "sol_tokens",
        "sol_dex_swaps",
        "chains",
    ] {
        let exists = db
            .count(&format!(
                "SELECT count() FROM system.tables WHERE database = '{}' \
                 AND name = '{table}'",
                db.name
            ))
            .await;
        assert_eq!(exists, 1, "{table} was not created");
    }

    // Every sol_* base table is insert-only and reorg aware.
    for table in svm::BASE_TABLES {
        let engine = db
            .scalar(&format!(
                "SELECT engine_full FROM system.tables WHERE database = \
                 '{}' AND name = '{table}'",
                db.name
            ))
            .await;
        assert!(
            engine.contains("ReplacingMergeTree(_version, is_deleted)"),
            "{table} must be tombstone capable: {engine}"
        );
        // Base tables are partitioned by MONTH, never by chain: the target
        // is 50+ chains in one database, and `chain` is the first sorting
        // key column, which is what prunes reads.
        if *table != "sol_tokens" {
            assert!(
                engine.contains("PARTITION BY toYYYYMM(timestamp)"),
                "{table}: {engine}"
            );
        }
        // NB: `do_not_merge_across_partitions_select_final` is accepted in
        // the DDL but NOT persisted into `engine_full` - ClickHouse treats
        // it as a query level setting. The existing `dex_swaps` behaves the
        // same way, so this is consistent with the rest of the schema
        // rather than something Solana does differently.
    }

    // The chain registry knows how a Solana id should be printed.
    let family = db
        .scalar(&format!(
            "SELECT family FROM chains FINAL WHERE chain = {CHAIN}"
        ))
        .await;
    assert_eq!(family, "svm");

    db.drop().await;
}

/// Every `sol_*` table with a block number is listed in
/// [`svm::BASE_TABLES`], or a rollback would leave its rows behind.
#[test]
fn every_solana_table_with_a_block_number_is_classified() {
    let sql = format!(
        "{}\n{}",
        include_str!("../../migrations/0040_solana_core.sql"),
        include_str!("../../migrations/0041_solana_dex.sql")
    );
    let listed: std::collections::HashSet<&str> =
        svm::BASE_TABLES.iter().chain(svm::SIDE_TABLES).copied().collect();

    let mut found = 0;
    for table in crate::db::schema::tables_with_block_number(&sql) {
        assert!(
            listed.contains(table.as_str()),
            "{table} has a block_number column but is in neither \
             svm::BASE_TABLES nor svm::SIDE_TABLES: a rollback would leave \
             its rows behind"
        );
        found += 1;
    }
    assert_eq!(found, svm::BASE_TABLES.len() + svm::SIDE_TABLES.len());
    // The commit marker is tombstoned LAST, mirroring the insert order.
    assert_eq!(svm::BASE_TABLES.last(), Some(&"sol_slots"));
}

// --- the insert path -----------------------------------------------------

/// Decoded rows survive the real binary insert path unchanged.
///
/// This is what a hand written `INSERT ... VALUES` would not prove: 32-byte
/// pubkeys as `FixedString(32)`, a 64-byte signature as `FixedString(64)`,
/// and `UInt256` / `Int256` as four little endian `u64` limbs.
#[tokio::test]
#[ignore]
async fn decoded_rows_round_trip_through_the_binary_insert_path() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    assert!(rows.swaps.len() >= 4, "fixtures should decode to swaps");
    store(&database, &rows).await;

    let stored = db.count("SELECT count() FROM sol_dex_swaps FINAL").await;
    assert_eq!(stored as usize, rows.swaps.len());

    // A pubkey comes back as the SAME 32 bytes, and base58Encode is what a
    // reader uses to print it (no hex strings are stored anywhere).
    let pool = db
        .scalar(
            "SELECT base58Encode(pool_id) FROM sol_dex_swaps FINAL \
             WHERE protocol = 'pumpswap' AND block_number = 448310216 \
             AND tx_index = 139",
        )
        .await;
    assert_eq!(pool, "EJTBQyiF4GMwXSjucW1qnBMVa7iJD21yyvCvMDFrkSrR");

    // A 64-byte signature survives as raw bytes in a String column.
    let signature = db
        .scalar(
            "SELECT base58Encode(tx_id) FROM sol_dex_swaps FINAL \
             WHERE block_number = 448310216 AND tx_index = 139",
        )
        .await;
    assert_eq!(
        signature,
        "yXsvnnBi1LQLYzQ1AKW435otvRFN3t2HV66Cx9Ra7FUewB2YyCCbvu3uQ6vh2LUmf8RY1B4SsGDZYf1Qt6aF9wC"
    );
    let length = db
        .count(
            "SELECT length(tx_id) FROM sol_dex_swaps FINAL \
             WHERE block_number = 448310216 AND tx_index = 139",
        )
        .await;
    assert_eq!(length, 64);

    // The amounts are exact, and the signs are pool relative.
    let amounts = db
        .scalar(
            "SELECT concat(toString(amount_in), '/', \
             toString(amount_out), '/', toString(amount0), '/', \
             toString(amount1)) FROM sol_dex_swaps FINAL \
             WHERE block_number = 448310216 AND tx_index = 139",
        )
        .await;
    // WSOL sorts before the base mint by raw bytes, so token0 is WSOL:
    // amount0 is POSITIVE (into the pool) and amount1 negative (out of it).
    assert_eq!(amounts, "122702894/4157620114/122702894/-4157620114");

    // Decimals came with the stream, so a swap is valuable with no RPC.
    let decimals = db
        .count(&format!(
            "SELECT toUInt64(decimals) FROM sol_tokens FINAL \
             WHERE mint = base58Decode('{WSOL}')"
        ))
        .await;
    assert_eq!(decimals, 9);

    db.drop().await;
}

/// A 256-bit amount above 2^128 round-trips exactly (docs/design.md
/// section 1: every table's integration test does this).
#[tokio::test]
#[ignore]
async fn a_256_bit_amount_round_trips() {
    use alloy::primitives::U256;

    let db = TestDb::create().await;
    let database = db.database().await;
    let mut rows = all_rows();

    let huge =
        U256::from(2u8).pow(U256::from(200u8)) + U256::from(12345u64);
    rows.swaps[0].amount_in = huge;
    store(&database, &rows).await;

    let stored = db
        .scalar(&format!(
            "SELECT toString(amount_in) FROM sol_dex_swaps FINAL \
             WHERE block_number = {} AND tx_index = {} AND ordinal = {}",
            rows.swaps[0].block_number,
            rows.swaps[0].tx_index,
            rows.swaps[0].ordinal
        ))
        .await;
    assert_eq!(stored, huge.to_string());

    db.drop().await;
}

/// Two swaps of ONE transaction stay two rows.
///
/// They share (chain, block_number, tx_index) and differ only in `ordinal`,
/// so if the ordinal were not part of the sorting key - or if it collided -
/// the `ReplacingMergeTree` would silently collapse them into one and the
/// netting case would be lost in storage instead of in the decoder.
#[tokio::test]
#[ignore]
async fn the_two_opposite_swaps_stay_two_rows_after_replacement() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    // Insert the same rows again with a newer version: a re-streamed slot
    // must replace itself, not duplicate.
    let mut again = all_rows();
    again.set_version(next_version());
    store(&database, &again).await;

    let stored = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE block_number = 448258071 AND tx_index = 423",
        )
        .await;
    assert_eq!(stored, 2, "the netting transaction holds two swaps");

    let total = db.count("SELECT count() FROM sol_dex_swaps FINAL").await;
    assert_eq!(
        total as usize,
        rows.swaps.len(),
        "re-inserting a slot must replace, never duplicate"
    );

    db.drop().await;
}

/// A rollback removes rows by TOMBSTONE, never by DELETE.
#[tokio::test]
#[ignore]
async fn a_purge_tombstones_solana_rows_without_a_delete() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;

    let before = db.count("SELECT count() FROM sol_dex_swaps FINAL").await;
    assert!(before > 0);

    // Exactly what `purge_range` issues: the rows again, with a newer
    // version and is_deleted = 1. No DELETE anywhere.
    // `tombstone_sql` reads its column list from the EMBEDDED migrations,
    // so it already knows the sol_* tables: the shared purge primitive
    // needs no change for Solana.
    for table in svm::BASE_TABLES {
        let sql = crate::db::schema::tombstone_sql(
            table,
            CHAIN,
            448_258_000,
            Some(448_259_000),
            next_version(),
        )
        .expect("tombstone sql");
        db.client().query(&sql).execute().await.expect("tombstone");
    }

    let after = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE block_number >= 448258000 AND block_number < 448259000",
        )
        .await;
    assert_eq!(after, 0, "purged slots must be hidden by FINAL");

    // The slots above the purge are untouched.
    let kept = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE block_number = 448310216",
        )
        .await;
    assert!(kept > 0, "a purge must not reach past its range");

    db.drop().await;
}

// --- what the data is FOR ------------------------------------------------

/// The point of the whole module: a candle over Solana swaps.
///
/// Amounts are summed as `toFloat64(amount) / pow(10, decimals)` and never
/// as raw integers - `sum()` over `UInt256` wraps silently and hostile
/// tokens really do emit `2^256-1` (docs/design.md section 1).
#[tokio::test]
#[ignore]
async fn a_candle_query_over_solana_swaps_works() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;

    #[derive(clickhouse::Row, serde::Deserialize, Debug)]
    #[allow(dead_code)]
    struct Candle {
        bucket: u32,
        pool: String,
        trades: u64,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume_quote: f64,
        traders: u64,
    }

    // One minute candles per pool, priced in the quote (WSOL) leg, with
    // both sides decimals adjusted through sol_tokens. Open and close are
    // taken at the exact position key, which is what makes the ordering
    // well defined inside a slot and inside a transaction.
    let sql = format!(
        "
        WITH swaps AS (
          SELECT
            toUnixTimestamp(toStartOfMinute(s.timestamp)) AS bucket,
            base58Encode(s.pool_id) AS pool,
            (s.block_number, s.tx_index, s.ordinal) AS position,
            s.trader AS trader,
            toFloat64(s.amount_in) / pow(10, ti.decimals) AS in_amount,
            toFloat64(s.amount_out) / pow(10, to_.decimals) AS out_amount,
            if(s.token_in = base58Decode('{WSOL}'),
               in_amount / out_amount, out_amount / in_amount) AS price,
            if(s.token_in = base58Decode('{WSOL}'), in_amount, out_amount)
              AS quote_volume
          FROM sol_dex_swaps AS s FINAL
          INNER JOIN sol_tokens AS ti FINAL
            ON ti.chain = s.chain AND ti.mint = s.token_in
          INNER JOIN sol_tokens AS to_ FINAL
            ON to_.chain = s.chain AND to_.mint = s.token_out
          WHERE s.chain = {CHAIN}
            AND (s.token_in = base58Decode('{WSOL}')
                 OR s.token_out = base58Decode('{WSOL}'))
        )
        SELECT
          bucket,
          pool,
          count() AS trades,
          argMin(price, position) AS open,
          max(price) AS high,
          min(price) AS low,
          argMax(price, position) AS close,
          sum(quote_volume) AS volume_quote,
          uniqExact(trader) AS traders
        FROM swaps
        GROUP BY bucket, pool
        ORDER BY bucket, pool
        "
    );

    let candles: Vec<Candle> =
        db.client().query(&sql).fetch_all().await.expect("candles");

    assert!(!candles.is_empty(), "no candles came back");
    println!("\n=== 1m candles over the recorded Solana swaps ===");
    for candle in &candles {
        println!(
            "  {} {:<44} trades={} o={:.10} h={:.10} l={:.10} \
             c={:.10} vol_sol={:.6} traders={}",
            candle.bucket,
            candle.pool,
            candle.trades,
            candle.open,
            candle.high,
            candle.low,
            candle.close,
            candle.volume_quote,
            candle.traders
        );
    }
    for candle in &candles {
        assert!(candle.trades > 0);
        assert!(candle.high >= candle.low, "{candle:?}");
        assert!(candle.open > 0.0 && candle.close > 0.0, "{candle:?}");
        assert!(candle.volume_quote > 0.0, "{candle:?}");
        assert!(candle.traders > 0);
    }

    // The netting transaction: two trades on one pool in one minute, and
    // the candle must show BOTH, with a volume far above the ~0.03 SOL a
    // transaction-level reading would report.
    let netting = candles
        .iter()
        .find(|candle| candle.trades == 2)
        .expect("the two-opposite-swaps pool should have two trades");
    assert!(
        netting.volume_quote > 1.0,
        "transaction-level netting would have hidden this volume: {netting:?}"
    );

    db.drop().await;
}

/// The whole point of the chain-neutral shape: the swap rows are already
/// `dex_swaps` rows, so the merge is a column-for-column copy.
#[tokio::test]
#[ignore]
async fn the_swap_table_has_exactly_the_chain_neutral_columns() {
    let db = TestDb::create().await;

    let columns: Vec<String> = db
        .client()
        .query(&format!(
            "SELECT name FROM system.columns WHERE database = '{}' \
             AND table = 'sol_dex_swaps' ORDER BY position",
            db.name
        ))
        .fetch_all()
        .await
        .expect("columns");

    let expected: Vec<String> =
        crate::svm::models::SvmSwap::DEX_SWAP_COLUMNS
            .iter()
            .map(|column| (*column).to_owned())
            .collect();
    assert_eq!(
        columns, expected,
        "TODO(merge): the table and DEX_SWAP_COLUMNS drifted apart, so \
         `INSERT INTO dex_swaps SELECT ... FROM sol_dex_swaps` would break"
    );

    db.drop().await;
}

// --- a name that must never appear ---------------------------------------

/// A guard, not a test of behaviour: these tables are a program-filtered
/// SUBSET of Solana, and a table named like the chain's transactions would
/// invite exactly the wrong query.
#[test]
fn nothing_claims_to_be_chain_complete() {
    for table in svm::BASE_TABLES {
        assert!(
            table.starts_with("sol_"),
            "{table} should be namespaced so nobody mistakes it for the \
             chain's own data"
        );
    }
    let core = include_str!("../../migrations/0040_solana_core.sql");
    assert!(
        core.contains("ANALYTICS-ONLY"),
        "the migration must say plainly what these tables are not"
    );
}

/// The venue registry and the decoders stay in step.
#[test]
fn every_streamed_venue_is_decodable_at_least_by_movement() {
    let registry = Registry::new();
    for venue in crate::svm::programs::VENUES {
        let program: Pubkey = pubkey(venue.program_b58());
        assert_eq!(registry.venue(&program), Some(venue));
        assert!(
            registry.router(&program).is_none(),
            "{venue} must not also be a router"
        );
    }
    // Phase 1 streams only the venues that have a per-program decoder.
    for venue in crate::svm::programs::VENUES {
        assert!(
            venue.has_decoder(),
            "{venue} is streamed but has no decoder"
        );
    }
    // And the others are named but deliberately not streamed.
    for venue in Venue::ALL {
        if !venue.has_decoder() {
            assert!(
                !crate::svm::programs::VENUES.contains(&venue),
                "{} would be written with movement confidence only",
                to_base58(&pubkey(venue.program_b58()))
            );
        }
    }
}
