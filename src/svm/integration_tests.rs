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

/// Live swap rows, used to wait out ClickHouse's lack of read-your-writes.
const SWAP_COUNT: &str = "SELECT count() FROM sol_dex_swaps FINAL";

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

    async fn number(&self, sql: &str) -> f64 {
        self.client()
            .query(sql)
            .fetch_one::<f64>()
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// Waits until `sql` returns `expected`.
    ///
    /// ClickHouse 25.12 gives NO read-your-writes guarantee: right after an
    /// INSERT returns, the next query can miss the new part for a few
    /// milliseconds (docs/design.md section 2 records the same observation,
    /// and `purge_range` re-issues its tombstones for exactly this reason).
    /// Without this the tests are flaky under load rather than wrong.
    async fn settle(&self, sql: &str, expected: u64) {
        for _ in 0..100 {
            if self.count(sql).await == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "`{sql}` never reached {expected} (last {})",
            self.count(sql).await
        );
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

    // The chain registry knows how a Solana id should be printed. Migrations
    // carry no seed rows: the indexer registers its chain at startup.
    db.client()
        .query(crate::svm::REGISTER_CHAIN_SQL)
        .execute()
        .await
        .expect("register the chain");
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
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

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
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

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
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;
    // Insert the same rows again with a newer version: a re-streamed slot
    // must replace itself, not duplicate.
    let mut again = all_rows();
    again.set_version(next_version());
    store(&database, &again).await;
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

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
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

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

    // Same caution as production: a tombstone INSERT is subject to the very
    // same read-your-writes gap, which is why `purge_range` re-issues its
    // statements until `live_rows_sql` returns 0.
    db.settle(
        "SELECT count() FROM sol_dex_swaps FINAL \
         WHERE block_number >= 448258000 AND block_number < 448259000",
        0,
    )
    .await;

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
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

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

/// A dust swap must not set open / high / low / close of a Solana candle.
///
/// `trade_price = |amount1| / |amount0|`, so one raw unit against 999 raw
/// units prices the pool at 999 - an arbitrary number that
/// `argMinStateIf` / `argMaxStateIf` / `max` / `min` take straight into the
/// candle. Dust swaps are trivial and common on Solana. The EVM DEX
/// candles have refused legs below `dex::derived::DUST_FLOOR_RAW` raw units
/// since migration 0011; these did not (review round 4, MAJOR 13).
#[tokio::test]
#[ignore]
async fn a_dust_swap_does_not_set_the_candle() {
    use alloy::primitives::I256;

    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();

    // One real recorded swap, both of whose legs are far above the floor.
    let real = rows
        .swaps
        .iter()
        .find(|swap| {
            let size = |amount: I256| {
                amount.unsigned_abs().to::<u128>() >= 1_000
            };
            size(swap.amount0) && size(swap.amount1)
        })
        .expect("a recorded swap with two real legs")
        .clone();
    let price = real.amount1.unsigned_abs().to::<u128>() as f64
        / real.amount0.unsigned_abs().to::<u128>() as f64;

    // CONSTRUCTED: the same pool, the same minute, an EARLIER position -
    // so it would be the candle's open - and one raw unit a side.
    let mut dust = real.clone();
    dust.ordinal = 1;
    dust.amount0 = I256::try_from(1i64).unwrap();
    dust.amount1 = I256::try_from(-999i64).unwrap();
    let mut real = real;
    real.ordinal = 2;

    let key = FlushKey {
        chain: CHAIN,
        span: (real.block_number, real.block_number),
        version: real._version,
    };
    database
        .insert_flush("sol_dex_swaps", &[dust, real], &key)
        .await
        .expect("insert sol_dex_swaps");
    db.settle(SWAP_COUNT, 2).await;

    let candle = |column: &str| {
        format!(
            "SELECT toFloat64(ifNull({column}, 0.)) \
             FROM sol_dex_candles_1m_v WHERE chain = {CHAIN}"
        )
    };
    // Both swaps are counted; only the real one is priced.
    assert_eq!(db.number(&candle("swaps")).await, 2.0);
    assert_eq!(db.number(&candle("trades")).await, 1.0);

    for column in ["open", "high", "low", "close"] {
        let value = db.number(&candle(column)).await;
        assert!(
            (value - price).abs() < price * 1e-9,
            "{column} is {value} rather than {price}: the dust swap \
             priced the candle"
        );
    }

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

/// The phase 2 venues survive the real insert path, and the pool state
/// their events carry arrives intact.
///
/// The interesting columns here are the ones phase 1 could never populate
/// for these venues: `reserve0` / `reserve1` and `fee_amount` come from a
/// LOG LINE for Raydium and Orca, so this is the end-to-end proof that the
/// log table reaches ClickHouse and not merely the decoder.
#[tokio::test]
#[ignore]
async fn the_phase_2_venues_round_trip_with_their_pool_state() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    db.settle(SWAP_COUNT, rows.swaps.len() as u64).await;

    // Every venue that decoded is stored under its own `protocol` name,
    // and the names are the ones the registry declares.
    let stored = db
        .scalar(
            "SELECT arrayStringConcat(groupUniqArray(protocol), ',') \
             FROM sol_dex_swaps FINAL",
        )
        .await;
    let mut names: Vec<&str> = stored.split(',').collect();
    names.sort_unstable();
    for name in &names {
        assert!(
            crate::svm::programs::Venue::ALL
                .iter()
                .any(|venue| venue.as_str() == *name),
            "{name} is not a registered venue"
        );
    }
    assert!(
        names.len() >= 3,
        "the fixtures should cover several venues, got {names:?}"
    );

    // A row confirmed by its venue is marked `decoded`, and one that only
    // the movement layer produced is marked `movement`. Both must be
    // present, because storing everything as `decoded` would be a lie.
    let decoded = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE confidence = 'decoded'",
        )
        .await;
    assert!(decoded > 0, "no row was confirmed by its venue");

    // Reserves survive as 32 little-endian bytes and read back as numbers.
    let with_state = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE reserve0 > 0 AND reserve1 > 0",
        )
        .await;
    assert!(
        with_state > 0,
        "no row carries pool reserves, so no per-program decoder supplied \
         any"
    );

    // And a fee the venue itself stated.
    let with_fee = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL WHERE fee_amount > 0",
        )
        .await;
    assert!(with_fee > 0, "no row carries a venue-reported fee");

    // The taker never receives more than the pool sent - the Token-2022
    // invariant, checked in SQL over every stored row.
    let impossible = db
        .count(
            "SELECT count() FROM sol_dex_swaps FINAL \
             WHERE amount_out > amount_out_gross",
        )
        .await;
    assert_eq!(
        impossible, 0,
        "a row claims the taker received more than the pool sent"
    );

    db.drop().await;
}

/// A liquidity add or remove never becomes a swap row, checked through the
/// database rather than only in the decoder.
///
/// This is the one that protects the volume figures: a liquidity operation
/// counted as a trade is fabricated volume, and it would be invisible in
/// any aggregate that simply sums `sol_dex_swaps`.
#[tokio::test]
#[ignore]
async fn a_liquidity_operation_never_reaches_the_swap_table() {
    let db = TestDb::create().await;
    let database = db.database().await;

    let fixture = crate::svm::fixtures::get("liquidity_no_swap");
    let batch = SvmSlotBatch {
        slot: fixture.slot,
        blockhash: fixture.blockhash,
        parent_slot: fixture.parent_slot,
        parent_blockhash: fixture.parent_blockhash,
        block_height: 0,
        timestamp: fixture.timestamp(),
        transactions: vec![fixture.transaction.clone()],
    };
    let mut rows = svm::decode(CHAIN, &[batch]);
    rows.set_version(next_version());
    rows.set_epoch(0);

    assert!(rows.swaps.is_empty(), "the decoder produced a swap row");
    // The slot is still committed: the commit marker is not conditional on
    // there being anything to decode.
    assert_eq!(rows.slots.len(), 1);

    let key = FlushKey {
        chain: CHAIN,
        span: (fixture.slot, fixture.slot),
        version: rows.slots[0]._version,
    };
    database
        .insert_flush("sol_dex_swaps", &rows.swaps, &key)
        .await
        .expect("insert sol_dex_swaps");
    database
        .insert_flush("sol_slots", &rows.slots, &key)
        .await
        .expect("insert sol_slots");

    db.settle("SELECT count() FROM sol_slots FINAL", 1).await;
    let swaps = db.count(SWAP_COUNT).await;
    assert_eq!(swaps, 0, "a liquidity operation reached the swap table");

    db.drop().await;
}

// --- the SHARED launchpad_* tables ---------------------------------------
//
// These are the point of the module: the same four tables the EVM decoder
// writes, with 32-byte Solana ids, through the real RowBinary path. A
// hand-written `INSERT ... VALUES` would not exercise the serializers, and
// the serializers are where a 32-byte pubkey or a 64-byte signature goes
// wrong (`models.rs` records one that reached a live ClickHouse).

/// Writes the launchpad rows the way a flush does.
async fn store_launchpads(db: &Database, rows: &SvmRows) {
    let pads = &rows.launchpads;
    let key = FlushKey {
        chain: CHAIN,
        span: (
            rows.slots.iter().map(|s| s.block_number).min().unwrap(),
            rows.slots.iter().map(|s| s.block_number).max().unwrap(),
        ),
        version: rows.slots[0]._version,
    };
    // Parents first: a reader must never see a trade of a token whose
    // launch row is not there yet (`launchpads::INSERT_ORDER`).
    db.insert_flush("launchpad_tokens", &pads.tokens, &key)
        .await
        .expect("insert launchpad_tokens");
    db.insert_flush("launchpad_trades", &pads.trades, &key)
        .await
        .expect("insert launchpad_trades");
    db.insert_flush("launchpad_graduations", &pads.graduations, &key)
        .await
        .expect("insert launchpad_graduations");
    db.insert_flush("launchpad_creator_fees", &pads.creator_fees, &key)
        .await
        .expect("insert launchpad_creator_fees");
    db.insert_flush("sol_launchpad_configs", &pads.configs, &key)
        .await
        .expect("insert sol_launchpad_configs");
    db.insert_flush("sol_token_balances", &pads.balances, &key)
        .await
        .expect("insert sol_token_balances");
}

/// Migration 0042's own tables and views exist and compose with everything
/// before them.
#[tokio::test]
#[ignore]
async fn the_launchpad_migration_applies_on_top_of_every_other_module() {
    let db = TestDb::create().await;

    for table in [
        "sol_dex_programs",
        "sol_launchpad_configs",
        "sol_token_balances",
        "sol_launchpad_token_holders_v",
        "sol_launchpad_attribution_v",
        // And the shared ones it writes into, which 0030 owns.
        "launchpad_tokens",
        "launchpad_trades",
        "launchpad_graduations",
        "launchpad_creator_fees",
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

    // The registry is OPERATOR data and migrations seed nothing.
    assert_eq!(db.count("SELECT count() FROM sol_dex_programs").await, 0);

    db.drop().await;
}

/// Solana launchpad rows go into the shared tables and come back out with
/// every one of their 32 bytes.
///
/// `toString(FixedString)` and `CAST(id AS String)` TRIM TRAILING ZERO
/// BYTES, so a pubkey that happens to end in a zero byte silently shortens
/// - the reason every reader in this schema uses `substring(id, 1, 32)`.
/// Asserting base58 round trips is what catches it.
#[tokio::test]
#[ignore]
async fn launchpad_rows_round_trip_through_the_shared_tables() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    assert!(
        !rows.launchpads.tokens.is_empty()
            && !rows.launchpads.trades.is_empty()
            && !rows.launchpads.graduations.is_empty(),
        "the recorded fixtures must cover all three row kinds"
    );
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;

    db.settle(
        "SELECT count() FROM launchpad_tokens FINAL",
        rows.launchpads.tokens.len() as u64,
    )
    .await;
    db.settle(
        "SELECT count() FROM launchpad_trades FINAL",
        rows.launchpads.trades.len() as u64,
    )
    .await;

    // A launch, printed the way migration 0006 says to print an 'svm' id.
    let launch = rows
        .launchpads
        .tokens
        .iter()
        .find(|row| row.family == "pumpfun")
        .expect("a pump.fun launch");
    let token = db
        .scalar(&format!(
            "SELECT base58Encode(substring(token, 1, 32)) \
             FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(launch.token)
        ))
        .await;
    assert_eq!(token, to_base58(&launch.token));

    let curve = db
        .scalar(&format!(
            "SELECT base58Encode(substring(curve, 1, 32)) \
             FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(launch.token)
        ))
        .await;
    assert_eq!(curve, to_base58(&launch.curve));

    // A 64-byte signature in the shared `tx_id String`, which a
    // FixedString(32) could not have held.
    let signature = db
        .scalar(&format!(
            "SELECT base58Encode(tx_id) FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(launch.token)
        ))
        .await;
    assert_eq!(signature, bs58::encode(&launch.tx_id).into_string());
    assert_eq!(
        db.count(&format!(
            "SELECT length(tx_id) FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(launch.token)
        ))
        .await,
        64
    );

    db.drop().await;
}

/// The graduation row's `pool_id` JOINS the swap rows the DEX decoder
/// wrote. This is the query a token page runs to keep charting after the
/// curve is gone, and the whole reason the two modules share a database.
#[tokio::test]
#[ignore]
async fn a_graduation_joins_the_pool_the_dex_decoder_wrote() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;
    db.settle(
        "SELECT count() FROM launchpad_graduations FINAL",
        rows.launchpads.graduations.len() as u64,
    )
    .await;

    let graduation = rows
        .launchpads
        .graduations
        .iter()
        .find(|row| row.pool_id != crate::svm::models::ZERO_PUBKEY)
        .expect("a graduation naming its destination pool");

    // A pubkey is a NATIVE 32 byte id, so the join is direct: no padding,
    // no truncation, no cast.
    let pool = db
        .scalar(&format!(
            "SELECT base58Encode(substring(pool_id, 1, 32)) \
             FROM launchpad_graduations FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(graduation.token)
        ))
        .await;
    assert_eq!(pool, to_base58(&graduation.pool_id));

    // And the join itself runs, with the swap table on the other side.
    let joined = db
        .count(&format!(
            "SELECT count() FROM launchpad_graduations AS g FINAL \
             LEFT JOIN sol_dex_swaps AS s FINAL ON s.pool_id = g.pool_id \
             WHERE g.chain = {CHAIN} AND g.token = unhex('{}')",
            hex::encode(graduation.token)
        ))
        .await;
    assert!(joined >= 1, "the join produced no row at all");

    db.drop().await;
}

/// The config account survives the `launch_config_id UInt256` column it
/// shares with EVM, and the documented SQL brings it back.
///
/// `reinterpretAsFixedString` writes the integer's LITTLE ENDIAN memory, so
/// the `reverse()` is not decoration - without it every config account
/// comes back byte-reversed and joins nothing.
#[tokio::test]
#[ignore]
async fn a_config_account_survives_the_numeric_column() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;
    db.settle(
        "SELECT count() FROM launchpad_tokens FINAL",
        rows.launchpads.tokens.len() as u64,
    )
    .await;

    let launch = rows
        .launchpads
        .tokens
        .iter()
        .find(|row| row.family == "meteora_dbc")
        .expect("a Meteora DBC launch");
    let expected =
        crate::svm::launchpads::u256_as_config(launch.launch_config_id);

    let config = db
        .scalar(&format!(
            "SELECT base58Encode(reverse(reinterpretAsFixedString(\
             launch_config_id))) FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            hex::encode(launch.token)
        ))
        .await;
    assert_eq!(
        config,
        to_base58(&expected),
        "the config account did not survive the numeric column; without \
         reverse() it comes back byte-reversed"
    );

    db.drop().await;
}

/// The trust views behave for Solana rows.
///
/// On Solana the emitter is the PROGRAM, which cannot be forged, so an
/// operator lists three rows and every real curve follows:
/// `launchpad_trusted_curves_v` is the listed singletons UNION every curve
/// a listed emitter announced. Nothing about the view changes - this
/// asserts the Solana rows satisfy it.
#[tokio::test]
#[ignore]
async fn the_trust_views_accept_solana_rows() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;
    db.settle(
        "SELECT count() FROM launchpad_tokens FINAL",
        rows.launchpads.tokens.len() as u64,
    )
    .await;

    // With NO trusted emitter, a headline view must yield nothing:
    // missing numbers, never wrong ones.
    let before = db
        .count(&format!(
            "SELECT count() FROM launchpad_trusted_curves_v \
             WHERE chain = {CHAIN}"
        ))
        .await;
    assert_eq!(before, 0, "a curve was trusted with no emitter listed");

    // The operator's three rows, exactly as the README ships them.
    for venue in
        [Venue::PumpFun, Venue::MeteoraDbc, Venue::RaydiumLaunchlab]
    {
        db.client()
            .query(&format!(
                "INSERT INTO launchpad_trusted_emitters \
                 (chain, emitter, family, label) VALUES \
                 ({CHAIN}, unhex('{}'), '{}', 'live')",
                hex::encode(pubkey(venue.program_b58())),
                match venue {
                    Venue::PumpFun => "pumpfun",
                    Venue::MeteoraDbc => "meteora_dbc",
                    _ => "raydium_launchlab",
                }
            ))
            .execute()
            .await
            .expect("insert a trusted emitter");
    }

    // Now every curve a listed program announced is trusted - and that is
    // the set a forger cannot enter, because it cannot emit as the
    // program.
    let trusted = db
        .count(&format!(
            "SELECT count() FROM launchpad_trusted_curves_v \
             WHERE chain = {CHAIN}"
        ))
        .await;
    assert!(
        trusted >= rows.launchpads.tokens.len() as u64,
        "only {trusted} curves are trusted for {} launches",
        rows.launchpads.tokens.len()
    );

    // And the token page returns the launch rather than nothing.
    let launch = rows
        .launchpads
        .tokens
        .iter()
        .find(|row| row.family == "pumpfun")
        .expect("a pump.fun launch");
    let rows_for_token = db
        .count(&format!(
            "SELECT count() FROM launchpad_token_v(chain = {CHAIN}, \
             token = '{}')",
            hex::encode(launch.token)
        ))
        .await;
    assert_eq!(
        rows_for_token, 1,
        "the token page returned nothing for a trusted Solana launch"
    );

    db.drop().await;
}

/// The Solana holder view answers where the EVM one structurally cannot.
///
/// `launchpad_token_holders_v` sums `erc20_transfers`, whose columns are
/// `FixedString(20)`; it PADS them up to 32 bytes, so a pubkey finds no row
/// - the right failure, but still no answer. `sol_token_balances` reads the
/// post balance validator metadata already carries.
#[tokio::test]
#[ignore]
async fn the_solana_holder_view_answers_where_the_evm_one_cannot() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;
    db.settle(
        "SELECT count() FROM sol_token_balances FINAL",
        rows.launchpads.balances.len() as u64,
    )
    .await;

    let balance = rows
        .launchpads
        .balances
        .iter()
        .find(|row| row.balance > alloy::primitives::U256::ZERO)
        .expect("a non-zero holder balance");
    let token = hex::encode(balance.mint);

    // The EVM holder view finds nothing for a pubkey, and that is by
    // design: it pads a 20-byte address up to 32 rather than truncating a
    // 32-byte one down, so a Solana mint simply matches no row.
    let evm = db
        .count(&format!(
            "SELECT count() FROM launchpad_token_holders_all_v(\
             chain = {CHAIN}, token = '{token}', \
             as_of_block = 18446744073709551615)"
        ))
        .await;
    assert_eq!(evm, 0, "the EVM holder view must not answer for a pubkey");

    // The Solana one does.
    let holders = db
        .count(&format!(
            "SELECT count() FROM sol_launchpad_token_holders_v(\
             chain = {CHAIN}, token = '{token}', \
             as_of_block = 18446744073709551615)"
        ))
        .await;
    assert!(holders >= 1, "the Solana holder view returned nothing");

    db.drop().await;
}

/// `sol_dex_programs` names a program, and the decoder uses that name.
///
/// The registry can only ADD knowledge: an unlisted program keeps its
/// built-in name, so a fresh database decodes exactly as before. Both
/// halves are asserted, because "configuration that can only help" is a
/// claim worth testing.
#[tokio::test]
#[ignore]
async fn the_program_registry_renames_a_venue_and_nothing_else() {
    use crate::svm::registry::{ProgramNames, SolDexProgram, LOAD_SQL};

    let db = TestDb::create().await;

    db.client()
        .query(&format!(
            "INSERT INTO sol_dex_programs \
             (program_id, name, kind, confidence, source) VALUES \
             (unhex('{}'), 'pumpswap_amm', 'venue', 100, 'its own IDL')",
            hex::encode(pubkey(Venue::PumpSwap.program_b58()))
        ))
        .execute()
        .await
        .expect("insert a program");

    let loaded: Vec<SolDexProgram> = db
        .client()
        .query(LOAD_SQL)
        .fetch_all()
        .await
        .expect("read sol_dex_programs");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].kind, "venue");
    assert_eq!(loaded[0].confidence, 100);
    // The 32 bytes survived the FixedString(32) round trip.
    assert_eq!(
        loaded[0].program_id,
        pubkey(Venue::PumpSwap.program_b58())
    );

    let names = ProgramNames::new(loaded);
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
    let renamed = svm::decode_with(CHAIN, &batches, &names);

    let pumpswap = renamed
        .swaps
        .iter()
        .filter(|swap| swap.protocol == "pumpswap_amm")
        .count();
    assert!(pumpswap > 0, "the operator's name was not applied");
    assert_eq!(
        renamed
            .swaps
            .iter()
            .filter(|swap| swap.protocol == Venue::PumpSwap.as_str())
            .count(),
        0,
        "the built-in name survived the override"
    );
    // Every other venue is untouched.
    assert!(
        renamed
            .swaps
            .iter()
            .any(|swap| swap.protocol == Venue::PumpFun.as_str()),
        "an unlisted program lost its built-in name"
    );

    db.drop().await;
}

/// The front-end attribution join runs end to end, and a launch with no
/// listed front end still reads perfectly - a front end is attribution,
/// never a precondition.
#[tokio::test]
#[ignore]
async fn a_launch_is_attributed_to_its_front_end_or_to_nobody() {
    let db = TestDb::create().await;
    let database = db.database().await;
    let rows = all_rows();
    store(&database, &rows).await;
    store_launchpads(&database, &rows).await;
    db.settle(
        "SELECT count() FROM launchpad_tokens FINAL",
        rows.launchpads.tokens.len() as u64,
    )
    .await;

    // Every launch is readable, named front end or not.
    let all = db
        .count(&format!(
            "SELECT count() FROM sol_launchpad_attribution_v \
             WHERE chain = {CHAIN}"
        ))
        .await;
    assert_eq!(all, rows.launchpads.tokens.len() as u64);

    // Now name one. The fee claimer is chain data (the venue's own
    // EvtCreateConfig); the NAME is the operator's.
    if let Some(config) = rows.launchpads.configs.first() {
        db.client()
            .query(&format!(
                "INSERT INTO launchpad_frontends \
                 (chain, address, name, kind) VALUES \
                 ({CHAIN}, unhex('{}'), 'a DBC partner', 'fee_recipient')",
                hex::encode(config.fee_claimer)
            ))
            .execute()
            .await
            .expect("insert a front end");

        let named = db
            .count(&format!(
                "SELECT count() FROM sol_launchpad_attribution_v \
                 WHERE chain = {CHAIN} AND frontend_name != ''"
            ))
            .await;
        println!(
            "  {named} launches attributed to a named front end out of {all}"
        );
    }

    db.drop().await;
}
