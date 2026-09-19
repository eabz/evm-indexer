//! Prediction market tables, aggregates and views against a REAL
//! ClickHouse. Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test predictions::integration -- --ignored --nocapture
//! ```
//!
//! Every test creates (and drops) its OWN database on that server, applies
//! EVERY embedded migration to it through `crate::db::migrate` (core, DEX,
//! predictions) and never touches the database named in the url. Rows go
//! through [`decode`] and the clickhouse crate's RowBinary inserts, exactly
//! like the pipeline writes them. No test issues a DELETE: reorgs are
//! tombstones + epochs (docs/design.md §2).
//!
//! The data is REAL (see `fixtures_data.rs`) except where a test says
//! "constructed": the resolution and redemption of the traded market (the
//! real ones are months away from the real trade) and the hostile amounts.

// The literals below mirror on-chain amounts digit by digit.
#![allow(clippy::excessive_precision, clippy::inconsistent_digit_grouping)]

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use clickhouse::{Client, Row};
use serde::{Deserialize, Serialize};

use crate::{
    db::{migrate, DatabaseParams},
    predictions::{
        cookbook::{self, Recipe},
        decode,
        derived::rebuild_statements,
        fixtures::{self, address, hash, Place, RawTx},
        models::{PredictionVenue, Protocol, RowSource, VERSION_RPC},
        PredictionRows, BASE_TABLES, PREDICTIONS_DERIVED,
    },
};

const CHAIN: u64 = 137;

const CTF: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
const NEG_RISK_EXCHANGE: &str =
    "0xC5d563A36AE78145C45a50134d48A1215220f80a";
const V2_EXCHANGE: &str = "0xe111180000d2663c0091e4f400237545b87b996b";
const USDC_E: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const PUSD: &str = "0xC011a7E12a19f7B1f670d46F03B03f3342E82DFB";
const WRAPPED_COLLATERAL: &str =
    "0x3A3BD7bb9528E159577F7C2e685CC81A765002E2";

/// Outcome 1 of M1: what the taker bought.
const M1_NO: &str =
    "0xe6f6e528a0a768bd4b5292120b36da79e1724c2bdb89700ac1cd4766024d2f17";
/// The taker of `V1_NEG_RISK_MATCH`.
const TAKER: &str = "0xd218e474776403a330142299f7796e8ba32eb5c9";
/// A maker that bought outcome 0 at 0.059 (mint).
const MAKER: &str = "0x7cc98bd686c5b7941502609826d14baea0d7b8a8";

/// The market of `V2_MINT_MATCH`.
const M2: &str =
    "7d9ba25f111d4adc353e0441fc205c3a39b3e7eef9829999663262055b9911c8";
const V2_TAKER: &str = "0xdc41c39b95453c943174f369926018f6963bdd7e";

struct TestDb {
    admin: Client,
    client: Client,
    name: String,
    /// Request parameters of the cookbook queries, BOUND (sent beside the
    /// statement as `param_x=`), never spliced into its text - that is
    /// what the cookbook promises and what these tests have to exercise.
    /// ClickHouse ignores a parameter a query does not use, so one set
    /// covers every screen.
    params: std::sync::Mutex<Vec<(String, String)>>,
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
        // The clock alone does not tell the tests of one process apart
        // (they start within the same microsecond).
        static SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence =
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!(
            "predictions_it_{}_{nanos}_{sequence}",
            std::process::id()
        );

        let admin = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);

        // The real migrations, through the real runner (it creates the
        // database named in the url).
        let mut target = url::Url::parse(&url).unwrap();
        target.set_path(&format!("/{name}"));
        migrate::run(target.as_str()).await.unwrap();

        let client = admin.clone().with_database(&name);
        Self {
            admin,
            client,
            name,
            params: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Replaces the bound parameters used by every following query.
    fn set(&self, params: &[(&str, &str)]) {
        *self.params.lock().unwrap() = params
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
    }

    fn query(&self, sql: &str) -> clickhouse::query::Query {
        let mut query = self.client.query(&sql.replace('?', "??"));
        for (name, value) in self.params.lock().unwrap().iter() {
            query = query.param(name, value.as_str());
        }
        query
    }

    async fn execute(&self, sql: &str) {
        self.query(sql)
            .execute()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"));
    }

    async fn rows<T>(&self, sql: &str) -> Vec<T>
    where
        T: clickhouse::RowOwned + clickhouse::RowRead,
    {
        self.query(sql)
            .fetch_all::<T>()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"))
    }

    async fn count(&self, sql: &str) -> u64 {
        self.rows::<u64>(sql).await[0]
    }

    /// Every row of `source` as text, sorted.
    async fn snapshot(&self, source: &str) -> Vec<String> {
        self.rows::<String>(&format!(
            "SELECT hex(toString(tuple(*))) AS line FROM ({source}) ORDER BY line"
        ))
        .await
    }

    async fn write<T>(&self, table: &str, rows: &[T])
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        if rows.is_empty() {
            return;
        }

        // Validation off: the crate has no mapping for (U)Int256 (the
        // pipeline inserts the same way, see db::Database::insert_once).
        let client = self.client.clone().with_validation(false);
        let mut insert = client.insert::<T>(table).await.unwrap();
        for row in rows {
            insert.write(row).await.unwrap();
        }
        insert
            .end()
            .await
            .unwrap_or_else(|error| panic!("{table}: {error}"));
    }

    /// In `INSERT_ORDER`, like the pipeline.
    async fn insert(&self, rows: &PredictionRows) {
        self.write("prediction_outcome_tokens", &rows.outcome_tokens)
            .await;
        self.write("prediction_markets", &rows.markets).await;
        self.write("prediction_questions", &rows.questions).await;
        self.write("prediction_resolutions", &rows.resolutions).await;
        self.write("prediction_position_events", &rows.position_events)
            .await;
        self.write("prediction_transfers", &rows.transfers).await;
        self.write("prediction_trades", &rows.trades).await;
    }

    /// What the token worker would have stored.
    async fn token(&self, token: &str, symbol: &str, decimals: u8) {
        self.execute(&format!(
            "INSERT INTO tokens (chain, address, name, symbol, decimals, type) \
             VALUES ({CHAIN}, unhex('{}'), '{symbol}', '{symbol}', {decimals}, 'ERC20')",
            hex::encode(address(token))
        ))
        .await;
    }

    /// What the OPERATOR populates: the contracts it believes. The
    /// indexer ships no address list, and the headline views count
    /// nothing else - an empty `prediction_trusted` means empty screens
    /// (README, "Trusted emitters"), which is why every test that reads a
    /// headline view has to do this first.
    async fn trust(&self, registry: &str, exchanges: &[&str]) {
        self.execute(&format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) \
             VALUES ({CHAIN}, 'registry', unhex('{0}'), unhex('{0}'))",
            id32_hex(registry)
        ))
        .await;

        for exchange in exchanges {
            self.execute(&format!(
                "INSERT INTO prediction_trusted (chain, kind, address, registry) \
                 VALUES ({CHAIN}, 'exchange', unhex('{}'), unhex('{}'))",
                id32_hex(exchange),
                id32_hex(registry)
            ))
            .await;
        }
    }

    /// The adapters that acted for a user on `registry`, as an operator
    /// would add them after reading `prediction_position_events`. They are
    /// the funding side of the leaderboard.
    async fn trust_adapters(&self, registry: &str) {
        self.execute(&format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) \
             SELECT DISTINCT chain, 'adapter', emitter, unhex('{}') \
             FROM prediction_position_events FINAL \
             WHERE chain = {CHAIN} AND is_deleted = 0",
            id32_hex(registry)
        ))
        .await;
    }

    /// What the venue worker would have stored.
    async fn venue(&self, exchange: &str, collateral: &str) {
        self.write(
            "prediction_venues",
            &[PredictionVenue {
                chain: CHAIN,
                exchange: address(exchange),
                protocol: Protocol::CtfExchange,
                collateral_token: address(collateral),
                registry: address(CTF),
                source: RowSource::Rpc,
                _version: VERSION_RPC,
            }],
        )
        .await;
    }

    /// The market list is recomputed by ClickHouse once a minute: tests
    /// do not wait for it.
    async fn refresh_markets(&self) {
        let started =
            self.count("SELECT toUInt64(toUnixTimestamp(now()))").await;
        // The list holds the TRUSTED markets only (0020), so that is what
        // a finished refresh has to contain.
        let markets = self
            .count(
                "SELECT count() FROM prediction_markets_live_v \
                 WHERE (chain, registry) IN ( \
                 SELECT chain, registry FROM prediction_trusted_registries_v)",
            )
            .await;

        self.execute("SYSTEM REFRESH VIEW prediction_market_list").await;

        // REFRESH only schedules and WAIT returns at once when nothing
        // runs yet: poll for the generation computed after `started`.
        for _ in 0..200 {
            self.execute("SYSTEM WAIT VIEW prediction_market_list").await;

            let fresh = self
                .count(&format!(
                    "SELECT count() FROM prediction_market_list \
                     WHERE toUnixTimestamp(computed_at) >= {started}"
                ))
                .await;
            if fresh == markets {
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        panic!("prediction_market_list was not refreshed");
    }

    /// `purge_range(chain, from, ∞)` as docs/design.md §2 describes it,
    /// in plain SQL: tombstones, the `reorgs` row, bucket repair.
    async fn purge_from(&self, from_block: u64, from_ts: u32, epoch: u32) {
        let version = crate::db::next_version();

        for table in BASE_TABLES {
            self.execute(&format!(
                "INSERT INTO {table} SELECT * REPLACE ({version} AS _version, \
                 1 AS is_deleted) FROM {table} FINAL \
                 WHERE chain = {CHAIN} AND block_number >= {from_block}"
            ))
            .await;
        }

        let day = from_ts - from_ts % 86_400;
        self.execute(&format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, fork_block, reason) \
             VALUES ({CHAIN}, {epoch}, {day}, {from_block}, 'reorg')"
        ))
        .await;

        // Month chunked, like the pipeline must run it: one INSERT over
        // more than 100 monthly partitions is refused by ClickHouse.
        for table in PREDICTIONS_DERIVED {
            for sql in rebuild_statements(
                table,
                CHAIN,
                day,
                now() + 86_400,
                epoch,
                (from_block, None),
            ) {
                self.execute(&sql).await;
            }
        }
    }

    async fn drop(self) {
        self.admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await
            .unwrap();
    }
}

fn now() -> u32 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as u32
}

fn bare(hex: &str) -> String {
    hex.trim_start_matches("0x").to_lowercase()
}

/// The 32 byte id of an EVM address as hex, for the columns that store one
/// (docs/design.md §13). Never hand rolled: `format::id32` is the one
/// padding helper.
fn id32_hex(address: &str) -> String {
    hex::encode(crate::utils::format::id32(fixtures::address(address)))
}

/// `tx` decoded as if it had been mined in `block` at `timestamp`.
fn decoded(
    tx: &RawTx,
    block: u64,
    timestamp: u32,
    epoch: u32,
) -> PredictionRows {
    let mut rows = decode(tx.chain, &tx.placed(block, timestamp));
    rows.attach_transactions(|hash| {
        (*hash == tx.hash()).then(|| tx.origin())
    });
    rows.set_epoch(epoch);
    rows.set_version(crate::db::next_version());
    rows
}

fn close(left: f64, right: f64) -> bool {
    (left - right).abs() <= 1e-9 * right.abs().max(1.0)
}

/// The real condition id of M1 (from the PositionSplit of the match).
fn m1() -> B256 {
    let rows = decode(CHAIN, &fixtures::V1_NEG_RISK_MATCH.logs());
    rows.outcome_tokens[0].market_id
}

/// CONSTRUCTED: M1 resolves to outcome 1 and the taker redeems.
fn constructed_resolution_and_redemption(
    resolved_at: u32,
    redeemed_at: u32,
) -> PredictionRows {
    let ctf = address(CTF);
    let taker = address(TAKER);
    let place = |block_number, log_index, timestamp| Place {
        chain: CHAIN,
        block_number,
        log_index,
        timestamp,
        transaction_hash: B256::repeat_byte(block_number as u8),
    };

    let logs = vec![
        fixtures::constructed_resolution(
            place(3_000, 0, resolved_at),
            ctf,
            m1(),
            Address::repeat_byte(0x0a),
            B256::repeat_byte(0x0b),
            &[0, 1],
        ),
        // redeemPositions burns the stake, then reports the payout.
        fixtures::constructed_transfer(
            place(3_100, 0, redeemed_at),
            ctf,
            taker,
            taker,
            Address::ZERO,
            U256::from_be_bytes(hash(M1_NO).0),
            U256::from(1_346_420_000u64),
        ),
        fixtures::constructed_redemption(
            place(3_100, 1, redeemed_at),
            ctf,
            taker,
            address(WRAPPED_COLLATERAL),
            m1(),
            U256::from(1_346_420_000u64),
        ),
    ];

    let mut rows = decode(CHAIN, &logs);
    rows.set_version(crate::db::next_version());
    rows
}

#[derive(Debug, Row, Deserialize)]
struct MarketLine {
    market_id: String,
    title: String,
    event_title: String,
    outcomes: Vec<String>,
    outcome_prices: Vec<f64>,
    volume_24h: f64,
    volume_total: f64,
    open_interest: f64,
    traders: u64,
    status: String,
}

const MARKET_PROJECTION: &str = "lower(hex(market_id)) AS market_id, \
    ifNull(title, '<null>') AS title, ifNull(event_title, '<null>') AS event_title, \
    outcomes, arrayMap(x -> ifNull(x, -1.), outcome_prices) AS outcome_prices, \
    ifNull(volume_24h, -1.) AS volume_24h, ifNull(volume_total, -1.) AS volume_total, \
    ifNull(open_interest, -1.) AS open_interest, toUInt64(traders) AS traders, \
    toString(status) AS status";

#[derive(Debug, Row, Deserialize)]
struct PositionLine {
    #[allow(dead_code)]
    outcome: String,
    status: String,
    shares: f64,
    avg_entry_price: f64,
    current_price: f64,
    value: f64,
    unrealized_pnl: f64,
    realized_pnl: f64,
    redeemable: f64,
}

const POSITION_PROJECTION: &str = "ifNull(outcome, '<null>') AS outcome, \
    toString(status) AS status, ifNull(shares, -1.) AS shares, \
    ifNull(avg_entry_price, -1.) AS avg_entry_price, \
    ifNull(current_price, -1.) AS current_price, ifNull(value, -1.) AS value, \
    ifNull(unrealized_pnl, -999.) AS unrealized_pnl, \
    ifNull(realized_pnl, -999.) AS realized_pnl, \
    ifNull(redeemable, -1.) AS redeemable";

/// Runs a recipe `runs` times and returns the median latency in ms.
async fn latency(database: &TestDb, sql: &str, runs: usize) -> f64 {
    let mut samples = Vec::new();
    for _ in 0..runs {
        let started = Instant::now();
        database
            .execute(&format!("SELECT * FROM ({sql}) FORMAT Null"))
            .await;
        samples.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn the_cookbook_serves_every_screen_from_real_polymarket_data() {
    let database = TestDb::create().await;
    let now = now();
    let traded_at = now - 3 * 3_600;

    database.token(USDC_E, "USDC.e", 6).await;
    database.token(WRAPPED_COLLATERAL, "WCOL", 6).await;
    database.token(PUSD, "pUSD", 6).await;
    database.venue(NEG_RISK_EXCHANGE, USDC_E).await;
    database.venue(V2_EXCHANGE, PUSD).await;
    database.trust(CTF, &[NEG_RISK_EXCHANGE, V2_EXCHANGE]).await;

    // Titles first (as on chain), then the trades, a few hours ago.
    let real: [(&RawTx, u64, u32); 12] = [
        (&fixtures::NEG_RISK_MARKET_PREPARED, 900, traded_at - 7_200),
        (&fixtures::NEG_RISK_QUESTION_PREPARED, 901, traded_at - 7_100),
        (&fixtures::UMA_QUESTION_INITIALIZED, 902, traded_at - 7_000),
        (&fixtures::V1_NEG_RISK_MATCH, 1_000, traded_at),
        (&fixtures::V2_MINT_MATCH, 2_000, traded_at + 600),
        (&fixtures::USER_SPLIT, 2_001, traded_at + 610),
        (&fixtures::USER_MERGE, 2_002, traded_at + 620),
        (&fixtures::NEG_RISK_MERGE, 2_003, traded_at + 630),
        (&fixtures::RESOLUTION, 2_004, traded_at + 640),
        (&fixtures::USER_REDEMPTION, 2_005, traded_at + 650),
        (&fixtures::NEG_RISK_REDEMPTION, 2_006, traded_at + 660),
        (&fixtures::NEG_RISK_CONVERSION, 2_007, traded_at + 670),
    ];

    let mut expected = PredictionRows::default();
    for (tx, block, timestamp) in real {
        let mut rows = decoded(tx, block, timestamp, 0);
        database.insert(&rows).await;
        expected.append(&mut rows);
    }
    // The adapters exist only once their events are in.
    database.trust_adapters(CTF).await;

    // Every table round trips through RowBinary.
    for (table, rows) in [
        ("prediction_markets", expected.markets.len()),
        ("prediction_questions", expected.questions.len()),
        ("prediction_resolutions", expected.resolutions.len()),
        ("prediction_position_events", expected.position_events.len()),
        ("prediction_transfers", expected.transfers.len()),
        ("prediction_trades", expected.trades.len()),
    ] {
        assert_eq!(
            database
                .count(&format!("SELECT count() FROM {table} FINAL"))
                .await,
            rows as u64,
            "{table}"
        );
    }
    // 256 bit values keep every digit.
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT toString(outcome_token_id) FROM prediction_trades FINAL \
                 WHERE chain = {CHAIN} AND block_number = 1000 LIMIT 1"
            ))
            .await[0],
        U256::from_be_bytes(hash(M1_NO).0).to_string()
    );

    database.refresh_markets().await;

    let chain = CHAIN.to_string();
    let m1_hex = hex::encode(m1());
    let event_id = hex::encode(
        decode(CHAIN, &fixtures::NEG_RISK_QUESTION_PREPARED.logs())
            .questions[0]
            .event_id,
    );
    let token = U256::from_be_bytes(hash(M1_NO).0).to_string();
    let from_day = "2020-01-01";

    // Everything the cookbook screens ask for, BOUND: the recipes carry
    // `{name:Type}` and the value travels beside the statement, so a
    // search box full of quotes is data, never SQL.
    let parameters: [(&str, &str); 9] = [
        ("chain", &chain),
        ("market_id", &m1_hex),
        ("event_id", &event_id),
        ("registry", &bare(CTF)),
        ("token", &token),
        ("holder", &bare(TAKER)),
        ("text", "golden state"),
        ("from_day", from_day),
        ("to_day", "2100-01-01"),
    ];
    database.set(&parameters);

    // ------------------------------------------------------- market list
    let list = cookbook::MARKET_LIST.sql;
    let markets: Vec<MarketLine> = database
        .rows(&format!(
            "SELECT {MARKET_PROJECTION} FROM (SELECT * FROM prediction_markets_v \
             WHERE chain = {CHAIN} AND market_id IN (SELECT market_id FROM ({list}))) \
             ORDER BY volume_total DESC"
        ))
        .await;
    assert!(markets.len() >= 5, "{markets:#?}");

    // M1: six real fills at 0.941 / 0.059. Volume = the collateral that
    // changed hands: 1266.98122 (taker legs) + 52.59968 (the makers' side
    // of the five mints).
    let first = &markets[0];
    assert_eq!(first.market_id, m1_hex);
    assert_eq!(first.status, "open");
    assert_eq!(first.outcome_prices.len(), 2);
    assert!(close(first.outcome_prices[0], 0.059), "{first:?}");
    assert!(close(first.outcome_prices[1], 0.941), "{first:?}");
    assert!(close(first.volume_total, 1_319.580_9), "{first:?}");
    assert!(close(first.volume_24h, 1_319.580_9), "{first:?}");
    // Five sets were minted inside the match: 891.52 locked.
    assert!(close(first.open_interest, 891.52), "{first:?}");
    // 4 distinct makers + the taker.
    assert_eq!(first.traders, 5);
    // Its preparation is older than the slice: no title, never a fake one.
    assert_eq!(first.title, "<null>");

    // M2: one V2 mint at 0.35 / 0.65, 181 collateral, 181 locked.
    let second = markets.iter().find(|line| line.market_id == M2).unwrap();
    assert!(close(second.outcome_prices[0], 0.35), "{second:?}");
    assert!(close(second.outcome_prices[1], 0.65), "{second:?}");
    assert!(close(second.volume_total, 181.0), "{second:?}");
    assert!(close(second.open_interest, 181.0), "{second:?}");
    assert_eq!(second.traders, 2);

    // Titles, outcome labels and the event grouping come from the chain.
    let search = cookbook::MARKET_SEARCH.sql;
    let found: Vec<MarketLine> = database
        .rows(&format!(
            "SELECT {MARKET_PROJECTION} FROM (SELECT * FROM prediction_markets_v \
             WHERE chain = {CHAIN} AND market_id IN (SELECT market_id FROM ({search})))"
        ))
        .await;
    assert_eq!(found.len(), 1);
    assert_eq!(
        found[0].title,
        "Will the Golden State Warriors win the 2025–2026 NBA Pacific Division?"
    );
    assert_eq!(found[0].event_title, "NBA Pacific Division Winner");
    assert_eq!(found[0].outcomes, vec!["Yes", "No"]);
    // Never traded: no price, no volume - NULL / empty, not zero prices.
    assert!(found[0].outcome_prices.is_empty());

    let uma: Vec<MarketLine> = database
        .rows(&format!(
            "SELECT {MARKET_PROJECTION} FROM (SELECT * FROM prediction_markets_v \
             WHERE chain = {CHAIN} AND title LIKE 'Kansas City Royals vs.%')"
        ))
        .await;
    assert_eq!(uma.len(), 1);
    assert_eq!(uma[0].outcomes, vec!["Over", "Under"]);

    let event = cookbook::EVENT_MARKETS.sql;
    assert_eq!(
        database.count(&format!("SELECT count() FROM ({event})")).await,
        1
    );

    // ------------------------------------------------------------ header
    let header = cookbook::MARKET_HEADER.sql;
    assert_eq!(
        database.count(&format!("SELECT count() FROM ({header})")).await,
        1
    );
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT toString(ifNull(collateral_symbol, '')) FROM ({header})"
            ))
            .await[0],
        "WCOL"
    );

    // ------------------------------------------------------------- chart
    #[derive(Debug, Row, Deserialize)]
    struct Candle {
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
        shares: f64,
        trades: u64,
        traders: u64,
    }
    let chart = cookbook::PRICE_CHART.sql;
    let candles: Vec<Candle> = database
        .rows(&format!(
            "SELECT open, high, low, close, ifNull(volume, -1.) AS volume, \
             ifNull(shares, -1.) AS shares, trades, traders FROM ({chart})"
        ))
        .await;
    assert_eq!(candles.len(), 1, "{candles:?}");
    let candle = &candles[0];
    for price in [candle.open, candle.high, candle.low, candle.close] {
        assert!(close(price, 0.941), "{candle:?}");
    }
    assert!(close(candle.volume, 1_266.981_22), "{candle:?}");
    assert!(close(candle.shares, 1_346.42), "{candle:?}");
    assert_eq!(candle.trades, 6);
    assert_eq!(candle.traders, 5);

    // -------------------------------------------------------------- tape
    #[derive(Debug, Row, Deserialize)]
    struct Print {
        outcome_index: u16,
        side: String,
        price: f64,
        shares: f64,
        collateral: f64,
        trader: String,
    }
    let tape = cookbook::TRADES_TAPE.sql;
    let prints: Vec<Print> = database
        .rows(&format!(
            "SELECT outcome_index, side, price, ifNull(shares, -1.) AS shares, \
             ifNull(collateral, -1.) AS collateral, \
             concat('0x', lower(hex(substring(trader, 13)))) AS trader FROM ({tape})"
        ))
        .await;
    assert_eq!(prints.len(), 6);
    // Newest first: the last maker order, 58.53 shares.
    assert!(close(prints[0].shares, 58.53), "{prints:?}");
    assert!(close(prints[0].collateral, 55.076_73), "{prints:?}");
    for print in &prints {
        assert_eq!(print.outcome_index, 1);
        assert_eq!(print.side, "buy");
        assert_eq!(print.trader, TAKER);
        assert!(close(print.price, 0.941), "{print:?}");
    }

    // ----------------------------------------------------------- holders
    #[derive(Debug, Row, Deserialize)]
    struct Holder {
        outcome_index: u16,
        holder: String,
        shares: f64,
        avg_entry_price: f64,
    }
    let holders = cookbook::HOLDERS.sql;
    let holders: Vec<Holder> = database
        .rows(&format!(
            "SELECT outcome_index, concat('0x', lower(hex(substring(holder, 13)))) AS holder, \
             ifNull(shares, -1.) AS shares, \
             ifNull(avg_entry_price, -1.) AS avg_entry_price FROM ({holders})"
        ))
        .await;
    let taker = holders.iter().find(|line| line.holder == TAKER).unwrap();
    assert_eq!(taker.outcome_index, 1);
    assert!(close(taker.shares, 1_346.42), "{taker:?}");
    assert!(close(taker.avg_entry_price, 0.941), "{taker:?}");
    let maker = holders.iter().find(|line| line.holder == MAKER).unwrap();
    assert_eq!(maker.outcome_index, 0);
    assert!(close(maker.shares, 500.0), "{maker:?}");
    assert!(close(maker.avg_entry_price, 0.059), "{maker:?}");
    // Exchanges and adapters pass shares on: nobody holds a zero balance.
    assert!(holders.iter().all(|line| line.shares > 0.0));

    // --------------------------------------------------------- portfolio
    let portfolio = format!(
        "SELECT {POSITION_PROJECTION} FROM ({})",
        cookbook::PORTFOLIO.sql
    );
    // Only the bound `holder` changes between wallets - the statement is
    // the same bytes every time.
    fn for_holder<'a>(
        parameters: &[(&'a str, &'a str); 9],
        holder: &'a str,
    ) -> [(&'a str, &'a str); 9] {
        let mut bound = *parameters;
        bound[5] = ("holder", holder);
        bound
    }
    let taker = bare(TAKER);
    let v2_taker = bare(V2_TAKER);
    let maker = bare(MAKER);

    database.set(&for_holder(&parameters, &taker));
    let open: Vec<PositionLine> = database.rows(&portfolio).await;
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].status, "open");
    assert!(close(open[0].shares, 1_346.42), "{open:?}");
    assert!(close(open[0].avg_entry_price, 0.941), "{open:?}");
    assert!(close(open[0].current_price, 0.941), "{open:?}");
    assert!(close(open[0].value, 1_266.981_22), "{open:?}");
    assert!(open[0].unrealized_pnl.abs() < 1e-6, "{open:?}");
    assert!(open[0].realized_pnl.abs() < 1e-6, "{open:?}");
    assert!(open[0].redeemable.abs() < 1e-9, "{open:?}");

    // The V2 taker paid 63.35 + 2.05887 fee for 181 shares.
    database.set(&for_holder(&parameters, &v2_taker));
    let v2: Vec<PositionLine> = database.rows(&portfolio).await;
    assert_eq!(v2.len(), 1, "{v2:?}");
    assert!(close(v2[0].shares, 181.0), "{v2:?}");
    assert!(close(v2[0].avg_entry_price, 65.408_87 / 181.0), "{v2:?}");
    assert!(close(v2[0].current_price, 0.35), "{v2:?}");
    assert!(
        close(v2[0].unrealized_pnl, 181.0 * 0.35 - 65.408_87),
        "{v2:?}"
    );

    // CONSTRUCTED: M1 resolves to outcome 1 ...
    let lifecycle =
        constructed_resolution_and_redemption(now - 3_600, now - 1_800);
    let mut resolution = lifecycle.clone();
    resolution.transfers.clear();
    resolution.position_events.clear();
    resolution.outcome_tokens.clear();
    database.insert(&resolution).await;
    database.refresh_markets().await;

    database.set(&for_holder(&parameters, &taker));
    let won: Vec<PositionLine> = database.rows(&portfolio).await;
    assert_eq!(won[0].status, "resolved");
    assert!(close(won[0].value, 1_346.42), "{won:?}");
    assert!(close(won[0].unrealized_pnl, 79.438_78), "{won:?}");
    assert!(close(won[0].redeemable, 1_346.42), "{won:?}");

    database.set(&for_holder(&parameters, &maker));
    let lost: Vec<PositionLine> = database.rows(&portfolio).await;
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(close(lost[0].shares, 500.0), "{lost:?}");
    assert!(close(lost[0].unrealized_pnl, -29.5), "{lost:?}");
    assert!(lost[0].redeemable.abs() < 1e-9, "{lost:?}");

    // ... and the taker redeems: the profit is realized, exactly.
    let mut redemption = lifecycle;
    redemption.resolutions.clear();
    database.insert(&redemption).await;
    database.refresh_markets().await;

    database.set(&for_holder(&parameters, &taker));
    let done: Vec<PositionLine> = database.rows(&portfolio).await;
    assert_eq!(done.len(), 1, "{done:?}");
    assert!(done[0].shares.abs() < 1e-9, "{done:?}");
    assert!(close(done[0].realized_pnl, 79.438_78), "{done:?}");
    assert!(done[0].redeemable.abs() < 1e-9, "{done:?}");
    assert_eq!(
        database
            .rows::<String>(
                "SELECT toString(balance) FROM prediction_positions_v(\
                 chain = {chain:UInt64}, holder = {holder:String})"
            )
            .await,
        vec!["0"]
    );

    let resolved: Vec<MarketLine> = database
        .rows(&format!(
            "SELECT {MARKET_PROJECTION} FROM (SELECT * FROM prediction_markets_v \
             WHERE chain = {CHAIN} AND market_id = unhex('{m1_hex}'))"
        ))
        .await;
    assert_eq!(resolved[0].status, "resolved");
    // Open interest only knows the indexed history: 891.52 were locked
    // inside the slice, 1346.42 left it (454.9 of them were minted before
    // the first indexed block).
    assert!(close(resolved[0].open_interest, 891.52 - 1_346.42));
    #[derive(Debug, PartialEq, Row, Deserialize)]
    struct Outcome {
        winning_outcome: u16,
        payouts: Vec<f64>,
    }
    assert_eq!(
        database
            .rows::<Outcome>(&format!(
                "SELECT ifNull(winning_outcome, 999) AS winning_outcome, payouts \
                 FROM ({header})"
            ))
            .await,
        vec![Outcome { winning_outcome: 1, payouts: vec![0.0, 1.0] }]
    );

    // ------------------------------------------------ wallet trade history
    #[derive(Debug, Row, Deserialize)]
    struct Activity {
        action: String,
        role: String,
        price: f64,
        #[allow(dead_code)]
        shares: f64,
    }
    let history = cookbook::WALLET_TRADES.sql;
    let history: Vec<Activity> = database
        .rows(&format!(
            "SELECT action, role, ifNull(price, -1.) AS price, \
             ifNull(shares, -1.) AS shares FROM ({history})"
        ))
        .await;
    assert_eq!(history.len(), 6);
    for line in &history {
        assert_eq!(
            (line.action.as_str(), line.role.as_str()),
            ("buy", "taker")
        );
        assert!(close(line.price, 0.941), "{line:?}");
    }

    // ------------------------------------------------------- leaderboard
    #[derive(Debug, Row, Deserialize)]
    struct Leader {
        trader: String,
        collateral: String,
        volume: f64,
        net_cash_flow: f64,
        trades: u64,
    }
    let leaders = cookbook::LEADERBOARD.sql;
    let leaders: Vec<Leader> = database
        .rows(&format!(
            "SELECT concat('0x', lower(hex(substring(trader, 13)))) AS trader, \
             concat('0x', lower(hex(substring(collateral_token, 13)))) AS collateral, \
             ifNull(volume, -1.) AS volume, ifNull(net_cash_flow, -999.) AS net_cash_flow, \
             trades FROM ({leaders}) ORDER BY volume DESC"
        ))
        .await;
    let line = |trader: &str, collateral: &str| {
        leaders
            .iter()
            .find(|line| {
                line.trader == trader.to_lowercase()
                    && line.collateral == collateral.to_lowercase()
            })
            .unwrap_or_else(|| {
                panic!("{trader} / {collateral}: {leaders:#?}")
            })
    };

    // ONE ROW PER COLLATERAL TOKEN, never one number over both: the
    // taker's six fills are priced in the exchange's USDC.e ...
    let traded = line(TAKER, USDC_E);
    assert!(close(traded.volume, 1_266.981_22), "{traded:?}");
    assert!(close(traded.net_cash_flow, -1_266.981_22), "{traded:?}");
    assert_eq!(traded.trades, 6);

    // ... and the redemption it paid for is in the market's wrapped
    // collateral. 1346.42 - 1266.98122 = 79.43878 is the trader's profit
    // ONLY if the two tokens are worth the same, which this module has no
    // price feed to know - so it never adds them up.
    let redeemed = line(TAKER, WRAPPED_COLLATERAL);
    assert!(close(redeemed.volume, -1.0), "{redeemed:?}");
    assert!(close(redeemed.net_cash_flow, 1_346.42), "{redeemed:?}");
    assert_eq!(redeemed.trades, 0);

    // Volume is counted once per party: the taker's equals the makers'
    // counterpart legs of M1 plus nothing else.
    let v2 = line(V2_TAKER, PUSD);
    assert!(close(v2.volume, 63.35), "{v2:?}");
    assert!(close(v2.net_cash_flow, -65.408_87), "{v2:?}");

    // A labelled address (the exchange) is not a trader.
    database
        .execute(&format!(
            "INSERT INTO prediction_venue_labels (chain, address, venue) VALUES \
             ({CHAIN}, unhex('{}'), 'polymarket'), ({CHAIN}, unhex('{}'), 'polymarket')",
            id32_hex(CTF),
            id32_hex(NEG_RISK_EXCHANGE)
        ))
        .await;
    database.refresh_markets().await;
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT toString(venue) FROM ({header})"
            ))
            .await[0],
        "polymarket"
    );

    // An external enricher fills what the chain does not have: the UI
    // query does not change.
    database
        .execute(&format!(
            "INSERT INTO prediction_market_metadata \
             (chain, market_id, title, category, tags, source) VALUES \
             ({CHAIN}, unhex('{m1_hex}'), 'Enriched title', 'Sports', ['nba'], 'gamma')"
        ))
        .await;
    database.refresh_markets().await;
    assert_eq!(
        database
            .rows::<(String, String)>(&format!(
                "SELECT toString(ifNull(title, '')), toString(ifNull(category, '')) \
                 FROM ({header})"
            ))
            .await[0],
        ("Enriched title".to_owned(), "Sports".to_owned())
    );

    // ---------------------------------------------------------- latency
    database.set(&parameters);
    println!("screen -> median latency over 9 runs (fixture sized data)");
    for Recipe { screen, sql } in cookbook::COOKBOOK {
        println!(
            "  {screen:24} {:7.2} ms",
            latency(&database, sql, 9).await
        );
    }

    database.drop().await;
}

/// Everything a consumer can see, as text.
async fn everything(database: &TestDb) -> Vec<(String, Vec<String>)> {
    database.refresh_markets().await;

    let chain = CHAIN.to_string();
    let token = U256::from_be_bytes(
        hash("0xa344da41ad7e4f483728a9b14e71b33db2732266c4d02854df24bee138fdc7d2").0,
    )
    .to_string();
    let parameters: [(&str, &str); 9] = [
        ("chain", &chain),
        ("market_id", M2),
        ("event_id", M2),
        ("registry", &bare(CTF)),
        ("token", &token),
        ("holder", &bare(V2_TAKER)),
        ("text", ""),
        ("from_day", "2020-01-01"),
        ("to_day", "2100-01-01"),
    ];
    database.set(&parameters);

    let mut seen = Vec::new();

    for recipe in cookbook::COOKBOOK {
        let sql = recipe.sql;
        // computed_at is the wall clock of the refresh.
        let sql = if sql.starts_with("SELECT *") {
            sql.replacen("SELECT *", "SELECT * EXCEPT (computed_at)", 1)
        } else {
            sql.to_owned()
        };
        seen.push((
            recipe.screen.to_owned(),
            database.snapshot(&sql).await,
        ));
    }

    // The other holder's portfolio, the other market's tape.
    for (name, sql) in [
        (
            "portfolio of the V1 taker",
            format!(
                "SELECT * FROM prediction_positions_v(chain = {CHAIN}, holder = '{}')",
                bare(TAKER)
            ),
        ),
        (
            "tape of M1",
            format!(
                "SELECT * FROM prediction_trades_v(chain = {CHAIN}, market_id = '{}')",
                hex::encode(m1())
            ),
        ),
        (
            "all markets",
            "SELECT * EXCEPT (computed_at) FROM prediction_markets_live_v".to_owned(),
        ),
    ] {
        seen.push((name.to_owned(), database.snapshot(&sql).await));
    }

    // Base and side tables, without the columns a repair is allowed to
    // change.
    for table in crate::predictions::BLOCK_SCOPED_TABLES {
        seen.push((
            table.to_string(),
            database
                .snapshot(&format!(
                    "SELECT * EXCEPT (_version, epoch, is_deleted) FROM {table} FINAL"
                ))
                .await,
        ));
    }

    seen
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn a_reorg_leaves_every_view_equal_to_a_clean_index() {
    let day = now() - now() % 86_400;
    let fork_time = day + 600;

    // The orphaned block 1000 held the six trades of the V1 match AND the
    // V2 match. The canonical block 1000 only holds the V2 match: fewer
    // trades, fewer transfers, a market less.
    let reorged = TestDb::create().await;
    let clean = TestDb::create().await;

    for database in [&reorged, &clean] {
        database.token(USDC_E, "USDC.e", 6).await;
        database.token(WRAPPED_COLLATERAL, "WCOL", 6).await;
        database.token(PUSD, "pUSD", 6).await;
        database.venue(NEG_RISK_EXCHANGE, USDC_E).await;
        database.venue(V2_EXCHANGE, PUSD).await;
        database.trust(CTF, &[NEG_RISK_EXCHANGE, V2_EXCHANGE]).await;
        // Before the fork: untouched by the purge.
        database
            .insert(&decoded(
                &fixtures::USER_SPLIT,
                999,
                fork_time - 300,
                0,
            ))
            .await;
    }

    reorged
        .insert(&decoded(
            &fixtures::V1_NEG_RISK_MATCH,
            1_000,
            fork_time,
            0,
        ))
        .await;
    reorged
        .insert(&decoded(&fixtures::V2_MINT_MATCH, 1_000, fork_time, 0))
        .await;
    reorged
        .insert(&decoded(&fixtures::USER_MERGE, 1_001, fork_time + 2, 0))
        .await;

    let before = everything(&reorged).await;

    // Rollback to the fork point, then the canonical chain under epoch 1.
    reorged.purge_from(1_000, fork_time, 1).await;
    assert_eq!(
        reorged
            .count(&format!(
                "SELECT count() FROM prediction_trades FINAL WHERE chain = {CHAIN}"
            ))
            .await,
        0
    );
    // Nothing was deleted: the orphaned rows are still there, dead.
    assert!(
        reorged
            .count(&format!(
                "SELECT count() FROM prediction_trades WHERE chain = {CHAIN}"
            ))
            .await
            >= 7
    );

    let canonical = [
        (&fixtures::V2_MINT_MATCH, 1_000, fork_time + 1),
        (&fixtures::USER_REDEMPTION, 1_001, fork_time + 3),
    ];
    for (tx, block, timestamp) in canonical {
        reorged.insert(&decoded(tx, block, timestamp, 1)).await;
        clean.insert(&decoded(tx, block, timestamp, 0)).await;
    }

    let after = everything(&reorged).await;
    let expected = everything(&clean).await;

    assert_ne!(before, after);
    for ((name, repaired), (_, clean)) in after.iter().zip(&expected) {
        assert_eq!(repaired, clean, "{name}");
    }
    // The comparison is not vacuous.
    let filled =
        expected.iter().filter(|(_, rows)| !rows.is_empty()).count();
    assert!(filled >= 12, "{filled}");

    // A second reorg of the same day on top of the first one, and a crash
    // in the middle of a purge (an abandoned epoch that was never
    // rebuilt) change nothing.
    reorged.purge_from(1_001, fork_time + 3, 2).await;
    reorged
        .insert(&decoded(
            &fixtures::USER_REDEMPTION,
            1_001,
            fork_time + 3,
            2,
        ))
        .await;
    reorged
        .execute(&format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, reason) VALUES \
             ({CHAIN}, 2, {day}, 'reorg')"
        ))
        .await;
    let again = everything(&reorged).await;
    for ((name, repaired), (_, clean)) in again.iter().zip(&expected) {
        assert_eq!(repaired, clean, "second reorg: {name}");
    }

    // Another chain is not affected by this chain's reorgs.
    assert_eq!(
        reorged
            .count("SELECT count() FROM epoch_floor_v WHERE chain != 137")
            .await,
        0
    );

    reorged.drop().await;
    clean.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn hostile_amounts_do_not_wrap_aggregates() {
    let database = TestDb::create().await;
    let now = now();

    let exchange = Address::repeat_byte(0xe1);
    let registry = Address::repeat_byte(0xc7);
    let taker = Address::repeat_byte(0x7a);
    // One outcome token per maker: the shares bound of `verify_shares` is
    // per (registry, token), and a registry cannot move more than
    // 2^256-1 of ONE token however many transfers it emits.
    let tokens = [U256::from(77u8), U256::from(78u8)];
    let place = |transaction: u8, log_index| Place {
        chain: CHAIN,
        block_number: 10,
        log_index,
        timestamp: now - 60,
        transaction_hash: B256::repeat_byte(transaction),
    };

    // CONSTRUCTED: TWO transactions, each one maker order selling 2^256-1
    // shares for 2^256-1 collateral of its own outcome token, and each
    // preceded by the registry really moving 2^256-1 of it. Two
    // transactions because the shares bound of `verify_shares` is per
    // transaction and per (registry, token): a registry cannot move more
    // than 2^256-1 of one token in one transaction however many transfers
    // it emits, which is exactly the property under test elsewhere.
    let mut logs = Vec::new();
    for (index, token) in tokens.into_iter().enumerate() {
        let tx = 1 + index as u8;
        let maker = Address::repeat_byte(0xa1 + index as u8);

        logs.push(fixtures::constructed_transfer(
            place(tx, 0),
            registry,
            exchange,
            maker,
            taker,
            token,
            U256::MAX,
        ));
        logs.push(fixtures::constructed_v2_fill(
            place(tx, 1),
            exchange,
            B256::repeat_byte(0xa1 + index as u8),
            maker,
            taker,
            true,
            token,
            U256::MAX,
            U256::MAX,
            U256::ZERO,
        ));
        logs.push(fixtures::constructed_v2_fill(
            place(tx, 2),
            exchange,
            B256::repeat_byte(0x7a),
            taker,
            exchange,
            false,
            token,
            U256::MAX,
            U256::MAX,
            U256::ZERO,
        ));
    }

    let mut rows = decode(CHAIN, &logs);
    assert_eq!(rows.trades.len(), 2);
    // 2^256-1 shares really were moved for each, so both fills are proven
    // and DO reach the aggregates: this test is about the arithmetic, not
    // about the proof.
    assert!(rows.trades.iter().all(|trade| trade.verified == 1));
    rows.set_version(crate::db::next_version());
    database.insert(&rows).await;
    database
        .execute(&format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) \
             VALUES ({CHAIN}, 'registry', unhex('{0}'), unhex('{0}')), \
             ({CHAIN}, 'exchange', unhex('{1}'), unhex('{0}'))",
            hex::encode(crate::utils::format::id32(registry)),
            hex::encode(crate::utils::format::id32(exchange))
        ))
        .await;

    // The exact values survive the round trip ...
    assert_eq!(
        database
            .rows::<String>(
                "SELECT toString(share_amount) FROM prediction_trades FINAL LIMIT 1"
            )
            .await[0],
        U256::MAX.to_string()
    );

    // ... and two of them sum to 2 * (2^256-1) ~ 2.3e77 instead of
    // wrapping around to 2^256-2 (or to zero).
    let expected = 2.0 * 1.157_920_892_373_162e77;
    for table in ["prediction_candles_1m", "prediction_candles_1d"] {
        let (volume, shares, trades): (f64, f64, u64) = database
            .rows(&format!(
                "SELECT toFloat64(sum(volume)), toFloat64(sum(shares)), \
                 toUInt64(sum(trades)) FROM {table} WHERE chain = {CHAIN}"
            ))
            .await[0];
        assert!(close(volume, expected), "{table}: {volume}");
        assert!(close(shares, expected), "{table}: {shares}");
        assert_eq!(trades, 2);
    }

    let (bought, sold): (f64, f64) = database
        .rows(&format!(
            "SELECT toFloat64(sum(bought)), toFloat64(sum(sold)) \
             FROM prediction_trader_trades_1d WHERE chain = {CHAIN}"
        ))
        .await[0];
    assert!(close(bought, expected), "{bought}");
    assert!(close(sold, expected), "{sold}");

    // A print above 1 collateral per share is not a probability: forged
    // prices never reach a chart.
    let forged = fixtures::constructed_v2_fill(
        place(9, 9),
        exchange,
        B256::repeat_byte(0xa3),
        Address::repeat_byte(0xa3),
        taker,
        true,
        tokens[0],
        U256::from(10u8),
        U256::from(1_000u64),
        U256::ZERO,
    );
    // The shares ARE proven (the registry moves exactly ten), so what
    // keeps this print off the chart is the price bound alone.
    let moved = fixtures::constructed_transfer(
        place(9, 8),
        registry,
        exchange,
        Address::repeat_byte(0xa3),
        taker,
        tokens[0],
        U256::from(10u8),
    );
    let mut rows = decode(CHAIN, &[moved, forged]);
    assert_eq!(rows.trades[0].verified, 1);
    rows.set_version(crate::db::next_version());
    database.insert(&rows).await;
    assert_eq!(
        database
            .count(&format!(
                "SELECT toUInt64(sum(trades)) FROM prediction_candles_1m WHERE chain = {CHAIN}"
            ))
            .await,
        2
    );

    // The views still answer.
    database.refresh_markets().await;
    let _ = database
        .snapshot(&format!(
            "SELECT * FROM prediction_positions_v(chain = {CHAIN}, holder = '{}')",
            hex::encode(taker)
        ))
        .await;

    database.drop().await;
}

// ---------------------------------------------------------- chain neutral

/// A chain that is not EVM: the reserved Solana id (docs/design.md §14).
const SVM_CHAIN: u64 = 1_399_811_149;

/// 32 byte ids that are NOT left padded EVM addresses - every one has
/// non-zero bytes in the 12 byte prefix an EVM address leaves empty, so any
/// code still assuming "the last 20 bytes are the address" mangles them
/// visibly.
const SVM_REGISTRY: &str =
    "b3f1a90c5d2e7481aa6fc03d94e5178b2c6d0f43a97e15bc8d2043fe6719ac85";
const SVM_EXCHANGE: &str =
    "7c2d5e8a41f0b96d3ae7c184fb5029d6e3a8710c45bd92fe6018a3c7d54b09ef";
const SVM_TAKER: &str =
    "e41a7b0396d5c82f14ae6d093b7c52801faa36d9c4e0b71852fd6a3c09e7b418";
const SVM_MAKER: &str =
    "2d90fa4c7b18e635a0cd472e918bf3067ac54d21e8b0937fca6d152048e3b7c9";
const SVM_COLLATERAL: &str =
    "5a8c3f19d02b47e6ba71cd8340f29e5b16d7a04c93e281fb60ac57d9138e4b2f";
const SVM_ORACLE: &str =
    "cd47e0a8153b96f27ea40d1c85b3097fe2461da05c8bf37962a0e4d81753cb6a";
const SVM_CREATOR: &str =
    "81f350ce9a274db6083fac51e7d29b640a5c8371fe4092bd6ac1573e8b04d9f2";
const SVM_MARKET: &str =
    "3fa07c15e9b8246d0cf37a5be1948d02c76ba31d905e8437b26cfa0d5187e93a";
/// A Solana signature is 64 bytes: `tx_id` is a String, never a hash column.
const SVM_TX: &str = concat!(
    "9a4c0f71e3b58d26ac190fe74b3d0825c6a1fb39d07e42b85cf1360ad9e274bb",
    "1f83e0d94a26cb705e3df182ac9460b7d5301fe8ba27c46d90f3581ea7b02d4c",
);

/// Every `prediction_*` table, materialized view, aggregate, parameterized
/// view and cookbook query carries a 32 byte NON-EVM id and a 64 byte
/// transaction id through unmangled (docs/design.md §13).
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn a_non_evm_32_byte_id_round_trips_through_every_table_and_query() {
    let database = TestDb::create().await;
    let traded_at = now() - 3_600;
    let version = crate::db::next_version();
    // The ERC-1155 style token id is a UInt256 on every chain.
    let token_id = (U256::MAX - U256::from(1u8)).to_string();

    // A non-EVM front end writes these columns from its own row type: the
    // EVM decoder's models are `Address` based on purpose (§13), the TABLES
    // are not. So this test inserts the way that front end would.
    for sql in [
        format!(
            "INSERT INTO prediction_markets (chain, market_id, registry, protocol, \
             oracle, question_id, outcome_count, block_number, timestamp, tx_id, \
             tx_index, ordinal, tx_from, source, epoch, _version) VALUES \
             ({SVM_CHAIN}, unhex('{SVM_MARKET}'), unhex('{SVM_REGISTRY}'), 'ctf', \
             unhex('{SVM_ORACLE}'), unhex('{SVM_MARKET}'), 2, 10, {traded_at}, \
             unhex('{SVM_TX}'), 3, 77, unhex('{SVM_CREATOR}'), 'event', 0, {version})"
        ),
        format!(
            "INSERT INTO prediction_questions (chain, question_id, emitter, kind, \
             protocol, event_id, question_index, title, description, outcomes, data, \
             creator, oracle, reward_token, reward, proposal_bond, fee_bips, \
             block_number, timestamp, tx_id, tx_index, ordinal, epoch, _version) \
             VALUES ({SVM_CHAIN}, unhex('{SVM_MARKET}'), unhex('{SVM_ORACLE}'), \
             'uma_question', 'uma', unhex('{SVM_MARKET}'), 0, 'A non-EVM market', '', \
             ['Yes', 'No'], '', unhex('{SVM_CREATOR}'), unhex('{SVM_ORACLE}'), \
             unhex('{SVM_COLLATERAL}'), 0, 0, 0, 10, {traded_at}, unhex('{SVM_TX}'), \
             3, 78, 0, {version})"
        ),
        format!(
            "INSERT INTO prediction_outcome_tokens (chain, registry, \
             outcome_token_id, market_id, outcome_index, collateral_token, \
             first_seen_block, first_seen_timestamp, _version) VALUES \
             ({SVM_CHAIN}, unhex('{SVM_REGISTRY}'), toUInt256('{token_id}'), \
             unhex('{SVM_MARKET}'), 0, unhex('{SVM_COLLATERAL}'), 10, {traded_at}, \
             {version})"
        ),
        format!(
            "INSERT INTO prediction_position_events (chain, block_number, timestamp, \
             tx_id, tx_index, ordinal, protocol, emitter, kind, stakeholder, \
             market_id, collateral_token, parent_collection_id, index_sets, amount, \
             tx_from, epoch, _version) VALUES \
             ({SVM_CHAIN}, 10, {traded_at}, unhex('{SVM_TX}'), 3, 79, 'ctf', \
             unhex('{SVM_REGISTRY}'), 'split', unhex('{SVM_MAKER}'), \
             unhex('{SVM_MARKET}'), unhex('{SVM_COLLATERAL}'), toFixedString('', 32), \
             [1, 2], 1000000, unhex('{SVM_CREATOR}'), 0, {version})"
        ),
        // The split mints a full set to the maker ...
        format!(
            "INSERT INTO prediction_transfers (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, batch_index, registry, operator, `from`, `to`, \
             outcome_token_id, amount, from_reason, to_reason, priced_collateral, \
             epoch, _version) VALUES \
             ({SVM_CHAIN}, 10, {traded_at}, unhex('{SVM_TX}'), 3, 80, 0, \
             unhex('{SVM_REGISTRY}'), unhex('{SVM_REGISTRY}'), toFixedString('', 32), \
             unhex('{SVM_MAKER}'), toUInt256('{token_id}'), 1000000, 'split', 'split', \
             500000, 0, {version})"
        ),
        // ... and the maker sells 400000 of one outcome to the taker at 0.6.
        format!(
            "INSERT INTO prediction_transfers (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, batch_index, registry, operator, `from`, `to`, \
             outcome_token_id, amount, from_reason, to_reason, priced_collateral, \
             epoch, _version) VALUES \
             ({SVM_CHAIN}, 11, {traded_at}, unhex('{SVM_TX}'), 4, 12, 0, \
             unhex('{SVM_REGISTRY}'), unhex('{SVM_EXCHANGE}'), unhex('{SVM_MAKER}'), \
             unhex('{SVM_TAKER}'), toUInt256('{token_id}'), 400000, 'trade', 'trade', \
             0, 0, {version})"
        ),
        format!(
            "INSERT INTO prediction_trades (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, protocol, exchange, registry, order_hash, maker, \
             taker, tx_from, tx_to, outcome_token_id, side, share_amount, \
             collateral_amount, match_type, verified, maker_outcome_token_id, maker_side, \
             maker_collateral_amount, maker_fee_amount, maker_fee_unit, \
             taker_fee_amount, taker_fee_unit, epoch, _version) VALUES \
             ({SVM_CHAIN}, 11, {traded_at}, unhex('{SVM_TX}'), 4, 13, 'ctf_exchange', \
             unhex('{SVM_EXCHANGE}'), unhex('{SVM_REGISTRY}'), unhex('{SVM_MARKET}'), \
             unhex('{SVM_MAKER}'), unhex('{SVM_TAKER}'), unhex('{SVM_CREATOR}'), \
             unhex('{SVM_EXCHANGE}'), toUInt256('{token_id}'), 'buy', 400000, 240000, \
             'complementary', 1, toUInt256('{token_id}'), 'sell', 240000, 0, \
             'collateral', 0, 'collateral', 0, {version})"
        ),
        format!(
            "INSERT INTO prediction_venues (chain, exchange, protocol, \
             collateral_token, registry, source, _version) VALUES \
             ({SVM_CHAIN}, unhex('{SVM_EXCHANGE}'), 'ctf_exchange', \
             unhex('{SVM_COLLATERAL}'), unhex('{SVM_REGISTRY}'), 'rpc', {version})"
        ),
        format!(
            "INSERT INTO prediction_venue_labels (chain, address, venue) VALUES \
             ({SVM_CHAIN}, unhex('{SVM_EXCHANGE}'), 'a non-evm venue')"
        ),
        // The operator trusts this chain's registry, exchange and the
        // registry as its own adapter (it emitted the split).
        format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) VALUES \
             ({SVM_CHAIN}, 'registry', unhex('{SVM_REGISTRY}'), unhex('{SVM_REGISTRY}')), \
             ({SVM_CHAIN}, 'exchange', unhex('{SVM_EXCHANGE}'), unhex('{SVM_REGISTRY}')), \
             ({SVM_CHAIN}, 'adapter', unhex('{SVM_REGISTRY}'), unhex('{SVM_REGISTRY}'))"
        ),
        format!(
            "INSERT INTO prediction_market_metadata (chain, market_id, title, source) \
             VALUES ({SVM_CHAIN}, unhex('{SVM_MARKET}'), 'A non-EVM market', 'gamma')"
        ),
    ] {
        database.execute(&sql).await;
    }

    // ------------------------------- every stored id comes back unmangled
    for (table, column, expected) in [
        ("prediction_markets", "registry", SVM_REGISTRY),
        ("prediction_markets", "oracle", SVM_ORACLE),
        ("prediction_markets", "tx_from", SVM_CREATOR),
        ("prediction_questions", "emitter", SVM_ORACLE),
        ("prediction_questions", "creator", SVM_CREATOR),
        ("prediction_questions", "reward_token", SVM_COLLATERAL),
        ("prediction_outcome_tokens", "registry", SVM_REGISTRY),
        ("prediction_outcome_tokens", "collateral_token", SVM_COLLATERAL),
        ("prediction_outcome_tokens_by_market", "registry", SVM_REGISTRY),
        (
            "prediction_outcome_tokens_by_market",
            "collateral_token",
            SVM_COLLATERAL,
        ),
        ("prediction_position_events", "emitter", SVM_REGISTRY),
        ("prediction_position_events", "stakeholder", SVM_MAKER),
        ("prediction_position_events", "collateral_token", SVM_COLLATERAL),
        ("prediction_transfers", "registry", SVM_REGISTRY),
        ("prediction_trades", "exchange", SVM_EXCHANGE),
        ("prediction_trades", "maker", SVM_MAKER),
        ("prediction_trades", "taker", SVM_TAKER),
        ("prediction_trades", "tx_to", SVM_EXCHANGE),
        ("prediction_trades_by_token", "registry", SVM_REGISTRY),
        ("prediction_trades_by_token", "taker", SVM_TAKER),
        ("prediction_ledger_by_holder", "registry", SVM_REGISTRY),
        ("prediction_ledger_by_token", "registry", SVM_REGISTRY),
        ("prediction_venues", "exchange", SVM_EXCHANGE),
        ("prediction_venue_labels", "address", SVM_EXCHANGE),
        ("prediction_candles_1m", "registry", SVM_REGISTRY),
        ("prediction_candles_1d", "registry", SVM_REGISTRY),
        ("prediction_market_flows_1d", "registry", SVM_REGISTRY),
        ("prediction_trader_trades_1d", "exchange", SVM_EXCHANGE),
        ("prediction_trader_flows_1d", "collateral_token", SVM_COLLATERAL),
    ] {
        assert_eq!(
            database
                .rows::<String>(&format!(
                    "SELECT DISTINCT lower(hex({column})) FROM {table} \
                     WHERE chain = {SVM_CHAIN}"
                ))
                .await,
            vec![expected.to_owned()],
            "{table}.{column}"
        );
    }

    // Both parties of the fill and both legs of the mint kept their ids.
    let mut both = vec![SVM_MAKER.to_owned(), SVM_TAKER.to_owned()];
    both.sort();
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT DISTINCT lower(hex(holder)) FROM prediction_ledger_by_token \
                 WHERE chain = {SVM_CHAIN} ORDER BY 1"
            ))
            .await,
        both
    );

    // A 64 byte transaction id is not truncated to 32 anywhere.
    assert_eq!(SVM_TX.len(), 128);
    for table in [
        "prediction_markets",
        "prediction_questions",
        "prediction_position_events",
        "prediction_transfers",
        "prediction_trades",
        "prediction_trades_by_token",
        "prediction_ledger_by_holder",
        "prediction_ledger_by_token",
    ] {
        assert_eq!(
            database
                .rows::<String>(&format!(
                    "SELECT DISTINCT lower(hex(tx_id)) FROM {table} \
                     WHERE chain = {SVM_CHAIN}"
                ))
                .await,
            vec![SVM_TX.to_owned()],
            "{table}.tx_id"
        );
    }

    // The 256 bit token id survives too (it is not an identity column).
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT DISTINCT toString(outcome_token_id) FROM prediction_trades \
                 WHERE chain = {SVM_CHAIN}"
            ))
            .await,
        vec![token_id.clone()]
    );

    // ---------------------------------------------- every cookbook query
    database.refresh_markets().await;

    let chain = SVM_CHAIN.to_string();
    let parameters: [(&str, &str); 9] = [
        ("chain", &chain),
        ("market_id", SVM_MARKET),
        ("event_id", SVM_MARKET),
        ("registry", SVM_REGISTRY),
        ("token", &token_id),
        ("holder", SVM_TAKER),
        ("text", "non-EVM"),
        ("from_day", "2020-01-01"),
        ("to_day", "2100-01-01"),
    ];
    database.set(&parameters);
    for recipe in cookbook::COOKBOOK {
        let sql = recipe.sql;
        assert!(
            database.count(&format!("SELECT count() FROM ({sql})")).await
                >= 1,
            "{}: no row",
            recipe.screen
        );
    }

    // ... and the ids the screens PRINT are the stored bytes.
    let header = cookbook::MARKET_HEADER.sql;
    assert_eq!(
        database
            .rows::<(String, String)>(&format!(
                "SELECT lower(hex(registry)), lower(hex(market_id)) FROM ({header})"
            ))
            .await,
        vec![(SVM_REGISTRY.to_owned(), SVM_MARKET.to_owned())]
    );

    let tape = cookbook::TRADES_TAPE.sql;
    assert_eq!(
        database
            .rows::<(String, String)>(&format!(
                "SELECT lower(hex(trader)), lower(hex(tx_id)) FROM ({tape})"
            ))
            .await,
        vec![(SVM_TAKER.to_owned(), SVM_TX.to_owned())]
    );

    let holders = cookbook::HOLDERS.sql;
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT lower(hex(holder)) FROM ({holders}) ORDER BY 1"
            ))
            .await,
        both
    );

    // The portfolio balance is exact even though the collateral has no
    // decimals: `tokens` is the EVM-only core table, so the Float64 columns
    // stay NULL rather than guessing (README, chain-neutral section).
    assert_eq!(
        database
            .rows::<(String, String)>(&format!(
                "SELECT lower(hex(market_id)), toString(balance) FROM \
                 (SELECT * FROM prediction_positions_v(chain = {SVM_CHAIN}, \
                 holder = '{SVM_TAKER}'))"
            ))
            .await,
        vec![(SVM_MARKET.to_owned(), "400000".to_owned())]
    );

    let leaders = cookbook::LEADERBOARD.sql;
    // The labelled exchange is not a trader, both parties of the fill are.
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT lower(hex(trader)) FROM ({leaders}) ORDER BY 1"
            ))
            .await,
        both
    );

    // The padding of a 40 character parameter is a real distinction: the
    // last 20 bytes of a Solana id are NOT that id.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM prediction_positions_v(chain = {SVM_CHAIN}, \
                 holder = '{}')",
                &SVM_TAKER[24..]
            ))
            .await,
        0
    );

    database.drop().await;
}

/// Review round 3, item 2. The `tokens` join of the candle views used to
/// TRUNCATE the analytics side - `toFixedString(substring(collateral_token,
/// 13, 20), 20)` - instead of padding `tokens.address` up to 32 bytes. That
/// maps EVERY 32 byte id onto some EVM address, so a non-EVM collateral
/// whose last 20 bytes happen to equal a real token's address picked up
/// that token's decimals and silently rescaled its amounts by 10^decimals.
///
/// The collision is planted deliberately here: `COLLIDING_COLLATERAL` is a
/// 32 byte id with a non-zero 12 byte prefix whose last 20 bytes ARE
/// `COLLIDING_ADDRESS`, the address of a real `tokens` row with 9 decimals.
/// The decimals-adjusted columns must stay NULL (the honest "not known"),
/// while the `_raw` ones keep the on-chain integer. The EVM control in the
/// same test proves the join still works when it is supposed to.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn a_32_byte_collateral_never_borrows_a_truncated_tokens_row() {
    let database = TestDb::create().await;
    let traded_at = now() - 3_600;
    let version = crate::db::next_version();
    let token_id = U256::from(7u8).to_string();
    let evm_token_id = U256::from(8u8).to_string();

    // The last 20 bytes of the non-EVM collateral, and the collateral.
    const COLLIDING_ADDRESS: &str =
        "40f29e5b16d7a04c93e281fb60ac57d9138e4b2f";
    const COLLIDING_COLLATERAL: &str = concat!(
        "5a8c3f19d02b47e6ba71cd83",
        "40f29e5b16d7a04c93e281fb60ac57d9138e4b2f",
    );
    assert_eq!(COLLIDING_COLLATERAL.len(), 64);
    assert_eq!(&COLLIDING_COLLATERAL[24..], COLLIDING_ADDRESS);
    // The prefix is NOT zero, so this id is not a padded EVM address.
    assert_ne!(&COLLIDING_COLLATERAL[..24], "000000000000000000000000");

    // A genuine EVM collateral on the same chain: the control.
    const EVM_ADDRESS: &str = "1111111111111111111111111111111122223333";
    let evm_collateral = format!("000000000000000000000000{EVM_ADDRESS}");

    for sql in [
        // The real token the forged id collides with, and the control's.
        format!(
            "INSERT INTO tokens (chain, address, name, symbol, decimals, type) \
             VALUES ({SVM_CHAIN}, unhex('{COLLIDING_ADDRESS}'), 'Nine', 'NINE', 9, \
             'ERC20'), ({SVM_CHAIN}, unhex('{EVM_ADDRESS}'), 'Six', 'SIX', 6, 'ERC20')"
        ),
        format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) VALUES \
             ({SVM_CHAIN}, 'registry', unhex('{SVM_REGISTRY}'), unhex('{SVM_REGISTRY}'))"
        ),
        // (registry, outcome_token_id) -> collateral, for both legs.
        format!(
            "INSERT INTO prediction_outcome_tokens (chain, registry, \
             outcome_token_id, market_id, outcome_index, collateral_token, \
             first_seen_block, first_seen_timestamp, _version) VALUES \
             ({SVM_CHAIN}, unhex('{SVM_REGISTRY}'), toUInt256('{token_id}'), \
             unhex('{SVM_MARKET}'), 0, unhex('{COLLIDING_COLLATERAL}'), 10, \
             {traded_at}, {version}), \
             ({SVM_CHAIN}, unhex('{SVM_REGISTRY}'), toUInt256('{evm_token_id}'), \
             unhex('{SVM_MARKET}'), 1, unhex('{evm_collateral}'), 10, {traded_at}, \
             {version})"
        ),
        // One verified trade per leg: 240000 collateral for 400000 shares.
        format!(
            "INSERT INTO prediction_trades (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, protocol, exchange, registry, order_hash, maker, \
             taker, tx_from, tx_to, outcome_token_id, side, share_amount, \
             collateral_amount, match_type, verified, maker_outcome_token_id, \
             maker_side, maker_collateral_amount, maker_fee_amount, maker_fee_unit, \
             taker_fee_amount, taker_fee_unit, epoch, _version) VALUES \
             ({SVM_CHAIN}, 11, {traded_at}, unhex('{SVM_TX}'), 4, 13, 'ctf_exchange', \
             unhex('{SVM_EXCHANGE}'), unhex('{SVM_REGISTRY}'), unhex('{SVM_MARKET}'), \
             unhex('{SVM_MAKER}'), unhex('{SVM_TAKER}'), unhex('{SVM_CREATOR}'), \
             unhex('{SVM_EXCHANGE}'), toUInt256('{token_id}'), 'buy', 400000, 240000, \
             'complementary', 1, toUInt256('{token_id}'), 'sell', 240000, 0, \
             'collateral', 0, 'collateral', 0, {version}), \
             ({SVM_CHAIN}, 12, {traded_at}, unhex('{SVM_TX}'), 5, 14, 'ctf_exchange', \
             unhex('{SVM_EXCHANGE}'), unhex('{SVM_REGISTRY}'), unhex('{SVM_MARKET}'), \
             unhex('{SVM_MAKER}'), unhex('{SVM_TAKER}'), unhex('{SVM_CREATOR}'), \
             unhex('{SVM_EXCHANGE}'), toUInt256('{evm_token_id}'), 'buy', 400000, \
             240000, 'complementary', 1, toUInt256('{evm_token_id}'), 'sell', 240000, \
             0, 'collateral', 0, 'collateral', 0, {version})"
        ),
    ] {
        database.execute(&sql).await;
    }

    // Truncating really would have found the colliding row: this is the
    // lookup the views used to do, and it returns the planted 9 decimals.
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT ifNull(toString(any(decimals)), '<null>') FROM tokens FINAL \
                 WHERE chain = {SVM_CHAIN} AND address IN (SELECT \
                 toFixedString(substring(unhex('{COLLIDING_COLLATERAL}'), 13, 20), 20))"
            ))
            .await,
        vec!["9".to_owned()],
        "the collision is not planted correctly"
    );

    // ... and padding, which is what the views do now, finds nothing.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM tokens FINAL WHERE chain = {SVM_CHAIN} \
                 AND toFixedString(concat(toFixedString('', 12), address), 32) \
                 = unhex('{COLLIDING_COLLATERAL}')"
            ))
            .await,
        0
    );

    // THE ASSERTION. The non-EVM leg: raw volume is the on-chain integer
    // and the decimals-adjusted columns are NULL. With the truncating join
    // volume was 240000 / 10^9 = 0.00024 instead.
    for view in [
        "prediction_candles_1m_v",
        "prediction_candles_1h_v",
        "prediction_candles_1d_v",
    ] {
        assert_eq!(
            database
                .rows::<(String, String, String)>(&format!(
                    "SELECT ifNull(toString(volume_raw), '<null>'), \
                     ifNull(toString(volume), '<null>'), \
                     ifNull(toString(shares), '<null>') FROM {view}(\
                     chain = {SVM_CHAIN}, registry = '{SVM_REGISTRY}', \
                     outcome_token_id = {token_id})"
                ))
                .await,
            vec![(
                "240000".to_owned(),
                "<null>".to_owned(),
                "<null>".to_owned()
            )],
            "{view} borrowed the decimals of a truncated tokens row"
        );

        // The control: a genuine EVM collateral on the same chain still
        // resolves, so the join is scoped, not simply broken.
        assert_eq!(
            database
                .rows::<(String, String)>(&format!(
                    "SELECT ifNull(toString(volume_raw), '<null>'), \
                     ifNull(toString(volume), '<null>') FROM {view}(\
                     chain = {SVM_CHAIN}, registry = '{SVM_REGISTRY}', \
                     outcome_token_id = {evm_token_id})"
                ))
                .await,
            vec![("240000".to_owned(), "0.24".to_owned())],
            "{view} lost a real EVM collateral"
        );
    }

    database.drop().await;
}

/// Review round 3, item 3. An id parameter is hex without `0x` and the
/// views pad a 40 character one, but an EMPTY string went through the
/// same path: `unhex('')` is the empty string and `toFixedString('', 32)`
/// is 32 ZERO BYTES, which is a real, storable value in these tables: an
/// unknown `registry`, `market_id` or `collateral_token` is the 32 zero
/// bytes, never a missing row. So an empty parameter, which is exactly
/// what a UI sends when its field is unset, selected the zero bucket
/// instead of returning nothing; a truncated 39 or 63 character id padded
/// the same way.
///
/// The `holder` views were the one lucky case - the MVs that fill
/// `prediction_ledger_by_holder` drop the zero holder on purpose, so the
/// mint and burn legs of a split never land there - but the guard is
/// applied uniformly rather than resting on that.
///
/// Every parameterized view now carries `AND length({id}) IN (40, 64)`.
/// The positive direction - a valid id still answers - is covered by the
/// other five tests in this file, which read these same views.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn an_empty_or_wrong_length_id_parameter_matches_nothing() {
    let database = TestDb::create().await;
    let traded_at = now() - 3_600;
    let version = crate::db::next_version();
    let token_id = U256::from(5u8).to_string();
    let zero = "0".repeat(64);

    // The premise: plant rows under the 32 zero bytes, on the holder, the
    // market and the registry, so "matches nothing" is a filter doing
    // work rather than an empty table.
    for sql in [
        format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) \
             VALUES ({CHAIN}, 'registry', unhex('{zero}'), unhex('{zero}'))"
        ),
        format!(
            "INSERT INTO prediction_outcome_tokens (chain, registry, \
             outcome_token_id, market_id, outcome_index, collateral_token, \
             first_seen_block, first_seen_timestamp, _version) VALUES \
             ({CHAIN}, unhex('{zero}'), toUInt256('{token_id}'), \
             unhex('{zero}'), 0, unhex('{zero}'), 10, {traded_at}, {version})"
        ),
        // A market and an exchange under the zero id, so the market list
        // really holds a zero-id market ...
        format!(
            "INSERT INTO prediction_trusted (chain, kind, address, registry) \
             VALUES ({CHAIN}, 'exchange', unhex('{zero}'), unhex('{zero}'))"
        ),
        format!(
            "INSERT INTO prediction_markets (chain, market_id, registry, \
             protocol, oracle, question_id, outcome_count, block_number, \
             timestamp, tx_id, tx_index, ordinal, tx_from, source, epoch, \
             _version) VALUES ({CHAIN}, unhex('{zero}'), unhex('{zero}'), 'ctf', \
             unhex('{zero}'), unhex('{zero}'), 2, 10, {traded_at}, unhex('aa'), \
             0, 1, unhex('{zero}'), 'event', 0, {version})"
        ),
        // ... and a verified trade on it, which feeds the candles and the
        // trades tape under the zero registry / zero market.
        format!(
            "INSERT INTO prediction_trades (chain, block_number, timestamp, \
             tx_id, tx_index, ordinal, protocol, exchange, registry, order_hash, \
             maker, taker, tx_from, tx_to, outcome_token_id, side, share_amount, \
             collateral_amount, match_type, verified, maker_outcome_token_id, \
             maker_side, maker_collateral_amount, maker_fee_amount, \
             maker_fee_unit, taker_fee_amount, taker_fee_unit, epoch, _version) \
             VALUES ({CHAIN}, 11, {traded_at}, unhex('aa'), 1, 2, 'ctf_exchange', \
             unhex('{zero}'), unhex('{zero}'), unhex('{zero}'), unhex('{zero}'), \
             unhex('{zero}'), unhex('{zero}'), unhex('{zero}'), \
             toUInt256('{token_id}'), 'buy', 400000, 240000, 'complementary', 1, \
             toUInt256('{token_id}'), 'sell', 240000, 0, 'collateral', 0, \
             'collateral', 0, {version})"
        ),
    ] {
        database.execute(&sql).await;
    }
    database.refresh_markets().await;

    for (table, column) in [
        ("prediction_outcome_tokens", "registry"),
        ("prediction_outcome_tokens_by_market", "market_id"),
    ] {
        assert!(
            database
                .count(&format!(
                    "SELECT count() FROM {table} FINAL WHERE chain = {CHAIN} \
                     AND {column} = toFixedString('', 32)"
                ))
                .await
                > 0,
            "{table}.{column}: the zero bucket is empty, this proves nothing"
        );
    }

    let chain = CHAIN.to_string();
    // Every parameterized view of 0022, with the id it scopes on.
    let views: [(&str, &str); 8] = [
        ("prediction_candles_1m_v", "registry"),
        ("prediction_candles_1h_v", "registry"),
        ("prediction_candles_1d_v", "registry"),
        ("prediction_trades_v", "market_id"),
        ("prediction_trades_all_v", "market_id"),
        ("prediction_holders_v", "market_id"),
        ("prediction_positions_v", "holder"),
        ("prediction_activity_v", "holder"),
    ];

    // An empty field, an address one character short, a 32 byte id one
    // short, and a stray byte. None of them may match anything.
    for bad in ["", &"a".repeat(39), &"a".repeat(63), "00"] {
        let parameters: [(&str, &str); 6] = [
            ("chain", &chain),
            ("registry", bad),
            ("market_id", bad),
            ("holder", bad),
            ("outcome_token_id", &token_id),
            ("from_block", "0"),
        ];
        database.set(&parameters);

        for (view, id) in views {
            let sql = match id {
                "registry" => format!(
                    "SELECT count() FROM {view}(chain = {{chain:UInt64}}, \
                     registry = {{registry:String}}, \
                     outcome_token_id = {{outcome_token_id:UInt256}})"
                ),
                "market_id" => format!(
                    "SELECT count() FROM {view}(chain = {{chain:UInt64}}, \
                     market_id = {{market_id:String}})"
                ),
                _ => format!(
                    "SELECT count() FROM {view}(chain = {{chain:UInt64}}, \
                     holder = {{holder:String}})"
                ),
            };
            assert_eq!(
                database.count(&sql).await,
                0,
                "{view}: id {bad:?} matched rows"
            );
        }
    }

    database.drop().await;
}

/// A contract nobody trusts, emitting the same events the real one does.
const FORGER: &str = "0xF0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0";
/// A worthless ERC-20 the forger splits one unit of.
const JUNK_COLLATERAL: &str = "0xBAdBAdbaDbaDbAdbaDBadBadBADbadBadbADBAd0";

/// Everything a UI screen can see, as text. `computed_at` is the wall
/// clock of the last refresh, so it is left out.
async fn headline(database: &TestDb) -> Vec<(String, Vec<String>)> {
    database.refresh_markets().await;

    let mut seen = Vec::new();
    for recipe in cookbook::COOKBOOK {
        let sql = if recipe.sql.starts_with("SELECT *") {
            recipe.sql.replacen(
                "SELECT *",
                "SELECT * EXCEPT (computed_at)",
                1,
            )
        } else {
            recipe.sql.to_owned()
        };

        // The leaderboard is per COLLATERAL TOKEN, and anyone may really
        // split a worthless ERC-20 at a trusted registry - that is a true
        // fact about a real contract, reported under that token and
        // nowhere near the tokens a UI ranks by. So the screens are
        // compared for the collaterals that carry the market, and the
        // junk line is asserted separately below.
        let sql = if recipe.screen == "Leaderboard" {
            format!(
                "SELECT * FROM ({sql}) \
                 WHERE lower(hex(collateral_token)) != '{}'",
                id32_hex(JUNK_COLLATERAL)
            )
        } else {
            sql
        };

        seen.push((
            recipe.screen.to_owned(),
            database.snapshot(&sql).await,
        ));
    }

    // The aggregates the screens are built on, directly - for the
    // registries a screen can reach. A candle row of an UNTRUSTED
    // registry exists (the materialized view cannot know a trust table
    // the operator may populate later), and no view reads it: every
    // prediction_candles_*_v guards on prediction_trusted_registries_v.
    for table in ["prediction_candles_1m", "prediction_candles_1d"] {
        seen.push((
            table.to_owned(),
            database
                .snapshot(&format!(
                    "SELECT chain, lower(hex(registry)) AS registry, \
                     toString(outcome_token_id) AS token, bucket, \
                     toFloat64(sum(volume)) AS volume, \
                     toFloat64(sum(shares)) AS shares, \
                     toUInt64(sum(trades)) AS trades, \
                     argMaxMerge(close) AS close, uniqMerge(traders) AS traders \
                     FROM {table} WHERE (chain, registry) IN ( \
                     SELECT chain, registry FROM prediction_trusted_registries_v) \
                     GROUP BY chain, registry, token, bucket"
                ))
                .await,
        ));
    }

    seen
}

/// The four forgeries review-c reproduced (findings 1, 2, 3 and 7). Each
/// one is a real, permissionless transaction pattern; none of them may
/// move a single number on a headline screen.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL (a real ClickHouse)"]
async fn forged_markets_trades_and_collaterals_never_reach_a_screen() {
    let database = TestDb::create().await;
    let now = now();
    let traded_at = now - 3 * 3_600;

    database.token(USDC_E, "USDC.e", 6).await;
    database.token(WRAPPED_COLLATERAL, "WCOL", 6).await;
    database.token(JUNK_COLLATERAL, "JUNK", 6).await;
    database.venue(NEG_RISK_EXCHANGE, USDC_E).await;
    database.trust(CTF, &[NEG_RISK_EXCHANGE]).await;

    for (tx, block, timestamp) in [
        (&fixtures::UMA_QUESTION_INITIALIZED, 900u64, traded_at - 7_000),
        (&fixtures::V1_NEG_RISK_MATCH, 1_000, traded_at),
        (&fixtures::USER_SPLIT, 1_001, traded_at + 10),
    ] {
        database.insert(&decoded(tx, block, timestamp, 0)).await;
    }
    database.trust_adapters(CTF).await;

    let chain = CHAIN.to_string();
    let m1_hex = hex::encode(m1());
    let token = U256::from_be_bytes(hash(M1_NO).0).to_string();
    database.set(&[
        ("chain", &chain),
        ("market_id", &m1_hex),
        ("event_id", &m1_hex),
        ("registry", &bare(CTF)),
        ("token", &token),
        ("holder", &bare(TAKER)),
        ("text", "the"),
        ("from_day", "2020-01-01"),
        ("to_day", "2100-01-01"),
    ]);

    let honest = headline(&database).await;
    let filled =
        honest.iter().filter(|(_, rows)| !rows.is_empty()).count();
    assert!(filled >= 6, "nothing to protect: {filled}");

    let version = crate::db::next_version();
    let forger = id32_hex(FORGER);
    let junk = id32_hex(JUNK_COLLATERAL);
    let ctf = id32_hex(CTF);
    let oracle = database
        .rows::<String>(&format!(
            "SELECT lower(hex(oracle)) FROM prediction_markets FINAL \
             WHERE chain = {CHAIN} AND market_id = unhex('{m1_hex}') LIMIT 1"
        ))
        .await;
    let oracle = oracle.first().cloned().unwrap_or_else(|| ctf.clone());

    for sql in [
        // FINDING 1. A junk contract prepares the SAME condition, naming
        // the REAL oracle and questionId - the decoder re-derives the
        // conditionId and a forger can compute it too - so the live view
        // joins it to the real title, and the market list would carry a
        // second row for one market, at the top if the forger prints
        // enough volume.
        format!(
            "INSERT INTO prediction_markets (chain, market_id, registry, protocol, \
             oracle, question_id, outcome_count, block_number, timestamp, tx_id, \
             tx_index, ordinal, tx_from, source, epoch, _version) VALUES \
             ({CHAIN}, unhex('{m1_hex}'), unhex('{forger}'), 'ctf', \
             unhex('{oracle}'), unhex('{m1_hex}'), 2, 900, {traded_at}, \
             unhex('{m1_hex}'), 0, 0, unhex('{forger}'), 'event', 0, {version})"
        ),
        // FINDING 3. The junk registry puts itself in the token map of the
        // real market, so a tape or holders query that resolves by
        // market_id alone reads its rows.
        format!(
            "INSERT INTO prediction_outcome_tokens (chain, registry, \
             outcome_token_id, market_id, outcome_index, collateral_token, \
             first_seen_block, first_seen_timestamp, _version) VALUES \
             ({CHAIN}, unhex('{forger}'), toUInt256('{token}'), \
             unhex('{m1_hex}'), 1, unhex('{junk}'), 1, {traded_at}, {version})"
        ),
        // FINDING 7. A one wei split of a worthless ERC-20 against the
        // REAL registry, mined earlier than anything honest: with a
        // first-seen argMin it becomes the market's primary collateral and
        // the real outcome tokens fall out of the market entirely.
        format!(
            "INSERT INTO prediction_outcome_tokens (chain, registry, \
             outcome_token_id, market_id, outcome_index, collateral_token, \
             first_seen_block, first_seen_timestamp, _version) VALUES \
             ({CHAIN}, unhex('{ctf}'), toUInt256('999'), \
             unhex('{m1_hex}'), 0, unhex('{junk}'), 1, {traded_at}, {version})"
        ),
        format!(
            "INSERT INTO prediction_position_events (chain, block_number, timestamp, \
             tx_id, tx_index, ordinal, protocol, emitter, kind, stakeholder, \
             market_id, collateral_token, parent_collection_id, index_sets, amount, \
             tx_from, epoch, _version) VALUES \
             ({CHAIN}, 1, {traded_at}, unhex('{m1_hex}'), 0, 0, 'ctf', \
             unhex('{ctf}'), 'split', unhex('{forger}'), unhex('{m1_hex}'), \
             unhex('{junk}'), toFixedString('', 32), [1, 2], 1, \
             unhex('{forger}'), 0, {version})"
        ),
        // FINDING 2. A lone OrderFilled from the forger's own contract,
        // naming a REAL outcome token id and 10^24 shares at 0.99. Its
        // registry is the genuine CTF, because the genuine CTF really did
        // move that token id in the same transaction (one unit, from the
        // permissionless splitPosition above) - `movers` cannot tell. Only
        // `verified` can, and it is 0.
        format!(
            "INSERT INTO prediction_trades (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, protocol, exchange, registry, order_hash, maker, \
             taker, tx_from, tx_to, outcome_token_id, side, share_amount, \
             collateral_amount, match_type, verified, maker_outcome_token_id, \
             maker_side, maker_collateral_amount, maker_fee_amount, maker_fee_unit, \
             taker_fee_amount, taker_fee_unit, epoch, _version) VALUES \
             ({CHAIN}, 1002, {traded_at}, unhex('{m1_hex}'), 0, 0, 'ctf_exchange', \
             unhex('{forger}'), unhex('{ctf}'), unhex('{m1_hex}'), unhex('{forger}'), \
             unhex('{forger}'), unhex('{forger}'), unhex('{forger}'), \
             toUInt256('{token}'), 'buy', 1000000000000000000000000, \
             990000000000000000000000, 'complementary', 0, toUInt256('{token}'), \
             'sell', 990000000000000000000000, 0, 'collateral', 0, 'collateral', \
             0, {version})"
        ),
        // The same fill, but on the forger's own registry AND marked
        // verified: its own transfers really do back it. Trust, not proof,
        // is what has to keep this one out.
        format!(
            "INSERT INTO prediction_trades (chain, block_number, timestamp, tx_id, \
             tx_index, ordinal, protocol, exchange, registry, order_hash, maker, \
             taker, tx_from, tx_to, outcome_token_id, side, share_amount, \
             collateral_amount, match_type, verified, maker_outcome_token_id, \
             maker_side, maker_collateral_amount, maker_fee_amount, maker_fee_unit, \
             taker_fee_amount, taker_fee_unit, epoch, _version) VALUES \
             ({CHAIN}, 1003, {traded_at}, unhex('{m1_hex}'), 0, 1, 'ctf_exchange', \
             unhex('{forger}'), unhex('{forger}'), unhex('{m1_hex}'), \
             unhex('{forger}'), unhex('{forger}'), unhex('{forger}'), \
             unhex('{forger}'), toUInt256('{token}'), 'buy', \
             1000000000000000000000000, 990000000000000000000000, 'complementary', \
             1, toUInt256('{token}'), 'sell', 990000000000000000000000, 0, \
             'collateral', 0, 'collateral', 0, {version})"
        ),
    ] {
        database.execute(&sql).await;
    }

    // Every screen shows exactly what it showed before the forgeries.
    let forged = headline(&database).await;
    assert_eq!(forged.len(), honest.len());
    for ((screen, after), (_, before)) in forged.iter().zip(&honest) {
        assert_eq!(after, before, "{screen}");
    }

    // The forger's ONLY presence on a leaderboard is the worthless token
    // it really did lock one unit of, under that token's own line.
    let forged_lines: Vec<(String, f64)> = database
        .rows(&format!(
            "SELECT lower(hex(collateral_token)), ifNull(net_cash_flow, -999.) \
             FROM ({}) WHERE lower(hex(trader)) = '{forger}'",
            cookbook::LEADERBOARD.sql
        ))
        .await;
    assert_eq!(forged_lines.len(), 1, "{forged_lines:?}");
    assert_eq!(forged_lines[0].0, id32_hex(JUNK_COLLATERAL));

    // The rows were KEPT, and the forensic views show them: this is a
    // trust boundary, not a deletion.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM prediction_markets_all_v \
                 WHERE chain = {CHAIN} AND market_id = unhex('{m1_hex}')"
            ))
            .await,
        2,
        "the forged market must still be visible to an operator"
    );
    let all: Vec<(String, u8, bool)> = database
        .rows(&format!(
            "SELECT lower(hex(exchange)), verified, trusted \
             FROM prediction_trades_all_v(chain = {{chain:UInt64}}, \
             market_id = {{market_id:String}}) \
             WHERE lower(hex(exchange)) = '{forger}'"
        ))
        .await;
    assert!(!all.is_empty(), "the forged fills must still be there");
    assert!(all
        .iter()
        .all(|(_, verified, trusted)| *verified == 0 || !*trusted));

    // And once the operator DOES trust the forger, its market appears -
    // the boundary is the table, not a hard coded address list.
    database.trust(FORGER, &[FORGER]).await;
    database.refresh_markets().await;
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM prediction_markets_v \
                 WHERE chain = {CHAIN} AND market_id = unhex('{m1_hex}')"
            ))
            .await,
        2
    );

    database.drop().await;
}
