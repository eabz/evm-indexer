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
        derived::render_rebuild,
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
        Self { admin, client, name }
    }

    async fn execute(&self, sql: &str) {
        self.client
            .query(&sql.replace('?', "??"))
            .execute()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"));
    }

    async fn rows<T>(&self, sql: &str) -> Vec<T>
    where
        T: clickhouse::RowOwned + clickhouse::RowRead,
    {
        self.client
            .query(&sql.replace('?', "??"))
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
        let markets = self
            .count("SELECT count() FROM prediction_markets_live_v")
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

        for table in PREDICTIONS_DERIVED {
            self.execute(&render_rebuild(
                table,
                CHAIN,
                day,
                epoch,
                (from_block, None),
            ))
            .await;
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

    // ------------------------------------------------------- market list
    let list = cookbook::MARKET_LIST.render(&[("chain", &chain)]);
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
    let search = cookbook::MARKET_SEARCH
        .render(&[("chain", &chain), ("text", "golden state")]);
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

    let event_id = hex::encode(
        decode(CHAIN, &fixtures::NEG_RISK_QUESTION_PREPARED.logs())
            .questions[0]
            .event_id,
    );
    let event = cookbook::EVENT_MARKETS
        .render(&[("chain", &chain), ("event_id", &event_id)]);
    assert_eq!(
        database.count(&format!("SELECT count() FROM ({event})")).await,
        1
    );

    // ------------------------------------------------------------ header
    let header = cookbook::MARKET_HEADER
        .render(&[("chain", &chain), ("market_id", &m1_hex)]);
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
    let token = U256::from_be_bytes(hash(M1_NO).0).to_string();
    let chart = cookbook::PRICE_CHART.render(&[
        ("chain", &chain),
        ("registry", &bare(CTF)),
        ("token", &token),
    ]);
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
    let tape = cookbook::TRADES_TAPE
        .render(&[("chain", &chain), ("market_id", &m1_hex)]);
    let prints: Vec<Print> = database
        .rows(&format!(
            "SELECT outcome_index, side, price, ifNull(shares, -1.) AS shares, \
             ifNull(collateral, -1.) AS collateral, \
             concat('0x', lower(hex(trader))) AS trader FROM ({tape})"
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
    let holders = cookbook::HOLDERS
        .render(&[("chain", &chain), ("market_id", &m1_hex)]);
    let holders: Vec<Holder> = database
        .rows(&format!(
            "SELECT outcome_index, concat('0x', lower(hex(holder))) AS holder, \
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
    let portfolio = |holder: &str| {
        let sql = cookbook::PORTFOLIO
            .render(&[("chain", &chain), ("holder", &bare(holder))]);
        format!("SELECT {POSITION_PROJECTION} FROM ({sql})")
    };

    let open: Vec<PositionLine> = database.rows(&portfolio(TAKER)).await;
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
    let v2: Vec<PositionLine> = database.rows(&portfolio(V2_TAKER)).await;
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

    let won: Vec<PositionLine> = database.rows(&portfolio(TAKER)).await;
    assert_eq!(won[0].status, "resolved");
    assert!(close(won[0].value, 1_346.42), "{won:?}");
    assert!(close(won[0].unrealized_pnl, 79.438_78), "{won:?}");
    assert!(close(won[0].redeemable, 1_346.42), "{won:?}");

    let lost: Vec<PositionLine> = database.rows(&portfolio(MAKER)).await;
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(close(lost[0].shares, 500.0), "{lost:?}");
    assert!(close(lost[0].unrealized_pnl, -29.5), "{lost:?}");
    assert!(lost[0].redeemable.abs() < 1e-9, "{lost:?}");

    // ... and the taker redeems: the profit is realized, exactly.
    let mut redemption = lifecycle;
    redemption.resolutions.clear();
    database.insert(&redemption).await;
    database.refresh_markets().await;

    let done: Vec<PositionLine> = database.rows(&portfolio(TAKER)).await;
    assert_eq!(done.len(), 1, "{done:?}");
    assert!(done[0].shares.abs() < 1e-9, "{done:?}");
    assert!(close(done[0].realized_pnl, 79.438_78), "{done:?}");
    assert!(done[0].redeemable.abs() < 1e-9, "{done:?}");
    assert_eq!(
        database
            .rows::<String>(&format!(
                "SELECT toString(balance) FROM prediction_positions_v(\
                 chain = {CHAIN}, holder = unhex('{}'))",
                bare(TAKER)
            ))
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
    let history = cookbook::WALLET_TRADES
        .render(&[("chain", &chain), ("holder", &bare(TAKER))]);
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
        volume: f64,
        net_cash_flow: f64,
        trades: u64,
    }
    let from_day = "2020-01-01";
    let leaders = cookbook::LEADERBOARD.render(&[
        ("chain", &chain),
        ("from_day", from_day),
        ("to_day", "2100-01-01"),
    ]);
    let leaders: Vec<Leader> = database
        .rows(&format!(
            "SELECT concat('0x', lower(hex(trader))) AS trader, volume, \
             net_cash_flow, trades FROM ({leaders}) ORDER BY volume DESC"
        ))
        .await;
    // The taker: 1266.98122 paid in six fills, 1346.42 redeemed.
    assert_eq!(leaders[0].trader, TAKER);
    assert!(close(leaders[0].volume, 1_266.981_22), "{leaders:?}");
    assert!(close(leaders[0].net_cash_flow, 79.438_78), "{leaders:?}");
    assert_eq!(leaders[0].trades, 6);
    // Volume is counted once per party: the taker's equals the makers'
    // counterpart legs of M1 plus nothing else.
    let v2 = leaders.iter().find(|line| line.trader == V2_TAKER).unwrap();
    assert!(close(v2.volume, 63.35), "{v2:?}");
    assert!(close(v2.net_cash_flow, -65.408_87), "{v2:?}");

    // A labelled address (the exchange) is not a trader.
    database
        .execute(&format!(
            "INSERT INTO prediction_venue_labels (chain, address, venue) VALUES \
             ({CHAIN}, unhex('{}'), 'polymarket'), ({CHAIN}, unhex('{}'), 'polymarket')",
            bare(CTF),
            bare(NEG_RISK_EXCHANGE)
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
    println!("screen -> median latency over 9 runs (fixture sized data)");
    for Recipe { screen, .. } in cookbook::COOKBOOK {
        let recipe = cookbook::COOKBOOK
            .iter()
            .find(|recipe| recipe.screen == *screen)
            .unwrap();
        let sql = recipe.render(&parameters);
        assert!(!sql.contains('{'), "{sql}");
        println!(
            "  {screen:24} {:7.2} ms",
            latency(&database, &sql, 9).await
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

    let mut seen = Vec::new();

    for recipe in cookbook::COOKBOOK {
        let sql = recipe.render(&parameters);
        // computed_at is the wall clock of the refresh.
        let sql = if sql.starts_with("SELECT *") {
            sql.replacen("SELECT *", "SELECT * EXCEPT (computed_at)", 1)
        } else {
            sql
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
                "SELECT * FROM prediction_positions_v(chain = {CHAIN}, holder = unhex('{}'))",
                bare(TAKER)
            ),
        ),
        (
            "tape of M1",
            format!(
                "SELECT * FROM prediction_trades_v(chain = {CHAIN}, market_id = unhex('{}'))",
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
    let token = U256::from(77u8);
    let place = |log_index| Place {
        chain: CHAIN,
        block_number: 10,
        log_index,
        timestamp: now - 60,
        transaction_hash: B256::repeat_byte(1),
    };

    // CONSTRUCTED: two maker orders selling 2^256-1 shares for 2^256-1
    // collateral each, a registry moving 2^256-1 shares twice.
    let mut logs = vec![fixtures::constructed_transfer(
        place(0),
        registry,
        exchange,
        Address::repeat_byte(0xa1),
        taker,
        token,
        U256::MAX,
    )];
    for (index, maker) in [0xa1u8, 0xa2].into_iter().enumerate() {
        logs.push(fixtures::constructed_v2_fill(
            place(1 + index as u32),
            exchange,
            B256::repeat_byte(maker),
            Address::repeat_byte(maker),
            taker,
            true,
            token,
            U256::MAX,
            U256::MAX,
            U256::ZERO,
        ));
    }
    logs.push(fixtures::constructed_v2_fill(
        place(3),
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

    let mut rows = decode(CHAIN, &logs);
    assert_eq!(rows.trades.len(), 2);
    rows.set_version(crate::db::next_version());
    database.insert(&rows).await;

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
        place(9),
        exchange,
        B256::repeat_byte(0xa3),
        Address::repeat_byte(0xa3),
        taker,
        true,
        token,
        U256::from(10u8),
        U256::from(1_000u64),
        U256::ZERO,
    );
    let mut rows = decode(CHAIN, &[forged]);
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
            "SELECT * FROM prediction_positions_v(chain = {CHAIN}, holder = unhex('{}'))",
            hex::encode(taker)
        ))
        .await;

    database.drop().await;
}
