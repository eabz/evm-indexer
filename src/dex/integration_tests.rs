//! DEX tables, aggregates and views against a REAL ClickHouse. Ignored by
//! default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test dex::integration -- --ignored
//! ```
//!
//! Every test creates (and drops) its OWN database on that server, applies
//! a minimal `tokens` table plus the DEX migrations to it, and never
//! touches the database named in the url.
//!
//! Rows go through [`decode`] and are inserted with `INSERT ... VALUES`
//! (`unhex` / `toInt256`): the binary row serializers of the design are
//! not available in this tree yet, so the clickhouse crate's RowBinary
//! inserts are exercised by the pipeline's own tests after the merge.

// The literals below mirror on-chain amounts digit by digit.
#![allow(clippy::excessive_precision, clippy::inconsistent_digit_grouping)]

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, I256, U256};
use clickhouse::Client;

use crate::{
    db::{models::log::DatabaseLog, DatabaseParams},
    dex::{
        block_column, decode,
        derived::render_rebuild,
        events,
        fixtures::{self, address, hash, RawLog},
        models::{
            pool_id_of, DexLiquidity, DexPool, DexSwap, PoolSource,
            Protocol, POOL_VERSION_RPC,
        },
        sql::{statements, MIGRATIONS},
        DexRows, BLOCK_SCOPED_TABLES, DEX_DERIVED,
    },
};

const CHAIN: u64 = 1;
/// Start of a UTC day (and hour, and minute).
const DAY: u32 = 1_700_006_400;

const TOKENS_DDL: &str = "CREATE TABLE IF NOT EXISTS tokens (\
    chain UInt64, address FixedString(20), name String, symbol String, \
    decimals UInt8, type LowCardinality(String), _version UInt64) \
    ENGINE = ReplacingMergeTree(_version) ORDER BY (chain, address)";

/// What the queries turn SQL NULL into (every real value is >= 0).
const NULL: f64 = -1.0;

/// `tokenIn` of the Balancer fixture swap.
const BALANCER_TOKEN_IN: &str =
    "0x0f2d719407fdbeff09d87557abb7232601fd9f29";

const DAI: &str = "0x6b175474e89094c44da98b954eedeac495271d0f";

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
        let name = format!("dex_it_{}_{nanos}", std::process::id());

        let admin = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);

        admin
            .query(&format!("CREATE DATABASE {name}"))
            .execute()
            .await
            .unwrap();

        let client = admin.clone().with_database(&name);
        let database = Self { admin, client, name };

        database.execute(TOKENS_DDL).await;
        for (file, sql) in MIGRATIONS {
            for statement in statements(sql) {
                database
                    .client
                    .query(&statement.replace('?', "??"))
                    .execute()
                    .await
                    .unwrap_or_else(|error| panic!("{file}: {error}"));
            }
        }

        database
    }

    async fn execute(&self, sql: &str) {
        self.client
            .query(&sql.replace('?', "??"))
            .execute()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"));
    }

    async fn count(&self, sql: &str) -> u64 {
        self.client
            .query(&sql.replace('?', "??"))
            .fetch_one::<u64>()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"))
    }

    /// Every row of `source` as text, sorted.
    async fn snapshot(&self, source: &str) -> Vec<String> {
        self.client
            .query(&format!(
                "SELECT hex(toString(tuple(*))) AS line FROM {source} \
                 ORDER BY line"
            ))
            .fetch_all::<String>()
            .await
            .unwrap()
    }

    async fn insert(&self, rows: &DexRows) {
        if !rows.swaps.is_empty() {
            let values: Vec<String> =
                rows.swaps.iter().map(swap_sql).collect();
            self.execute(&format!(
                "INSERT INTO dex_swaps ({SWAP_COLUMNS}) VALUES {}",
                values.join(", ")
            ))
            .await;
        }

        if !rows.liquidity.is_empty() {
            let values: Vec<String> =
                rows.liquidity.iter().map(liquidity_sql).collect();
            self.execute(&format!(
                "INSERT INTO dex_liquidity ({LIQUIDITY_COLUMNS}) VALUES {}",
                values.join(", ")
            ))
            .await;
        }

        if !rows.pools.is_empty() {
            let values: Vec<String> =
                rows.pools.iter().map(pool_sql).collect();
            self.execute(&format!(
                "INSERT INTO dex_pools ({POOL_COLUMNS}) VALUES {}",
                values.join(", ")
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

// ------------------------------------------------------------ SQL rendering

fn bytes(raw: &[u8]) -> String {
    format!("unhex('{}')", hex::encode(raw))
}

fn addr(value: &Address) -> String {
    bytes(value.as_slice())
}

fn word(value: &B256) -> String {
    bytes(value.as_slice())
}

fn int(value: &I256) -> String {
    format!("toInt256('{value}')")
}

fn uint(value: &U256) -> String {
    format!("toUInt256('{value}')")
}

fn addresses(values: &[Address]) -> String {
    let items: Vec<String> = values.iter().map(addr).collect();
    format!("[{}]", items.join(", "))
}

const SWAP_COLUMNS: &str = "chain, block_number, timestamp, \
    transaction_hash, log_index, pool_id, emitter, protocol, sender, \
    recipient, tx_from, tx_to, trader, amount0, amount1, token_in, \
    token_out, amount_in, amount_out, coin_in, coin_out, underlying, \
    sqrt_price_x96, liquidity, tick, fee, _version";

fn swap_sql(swap: &DexSwap) -> String {
    format!(
        "({}, {}, {}, {}, {}, {}, {}, '{}', {}, {}, {}, {}, {}, {}, {}, {}, \
         {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
        swap.chain,
        swap.block_number,
        swap.timestamp,
        word(&swap.transaction_hash),
        swap.log_index,
        word(&swap.pool_id),
        addr(&swap.emitter),
        swap.protocol,
        addr(&swap.sender),
        addr(&swap.recipient),
        addr(&swap.tx_from),
        addr(&swap.tx_to),
        addr(&swap.trader),
        int(&swap.amount0),
        int(&swap.amount1),
        addr(&swap.token_in),
        addr(&swap.token_out),
        uint(&swap.amount_in),
        uint(&swap.amount_out),
        swap.coin_in,
        swap.coin_out,
        swap.underlying,
        uint(&swap.sqrt_price_x96),
        uint(&swap.liquidity),
        swap.tick,
        swap.fee,
        swap._version,
    )
}

const LIQUIDITY_COLUMNS: &str = "chain, block_number, timestamp, \
    transaction_hash, log_index, pool_id, emitter, protocol, kind, sender, \
    owner, tx_from, tx_to, amount0, amount1, reserve0, reserve1, \
    liquidity_delta, tick_lower, tick_upper, _version";

fn liquidity_sql(row: &DexLiquidity) -> String {
    format!(
        "({}, {}, {}, {}, {}, {}, {}, '{}', '{}', {}, {}, {}, {}, {}, {}, \
         {}, {}, {}, {}, {}, {})",
        row.chain,
        row.block_number,
        row.timestamp,
        word(&row.transaction_hash),
        row.log_index,
        word(&row.pool_id),
        addr(&row.emitter),
        row.protocol,
        row.kind,
        addr(&row.sender),
        addr(&row.owner),
        addr(&row.tx_from),
        addr(&row.tx_to),
        int(&row.amount0),
        int(&row.amount1),
        uint(&row.reserve0),
        uint(&row.reserve1),
        int(&row.liquidity_delta),
        row.tick_lower,
        row.tick_upper,
        row._version,
    )
}

const POOL_COLUMNS: &str = "chain, pool_id, emitter, factory, protocol, \
    token0, token1, tokens, underlying_tokens, fee, tick_spacing, hooks, \
    stable, created_block, timestamp, transaction_hash, log_index, source, \
    _version";

fn pool_sql(pool: &DexPool) -> String {
    format!(
        "({}, {}, {}, {}, '{}', {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, \
         {}, '{}', {})",
        pool.chain,
        word(&pool.pool_id),
        addr(&pool.emitter),
        addr(&pool.factory),
        pool.protocol,
        addr(&pool.token0),
        addr(&pool.token1),
        addresses(&pool.tokens),
        addresses(&pool.underlying_tokens),
        pool.fee,
        pool.tick_spacing,
        addr(&pool.hooks),
        pool.stable,
        pool.created_block,
        pool.timestamp,
        word(&pool.transaction_hash),
        pool.log_index,
        pool.source,
        pool._version,
    )
}

// ------------------------------------------------------------- the scenario

fn number(value: u128) -> Vec<u8> {
    U256::from(value).to_be_bytes::<32>().to_vec()
}

use crate::dex::fixtures::build as constructed;

/// A V2 shaped swap on `pair`: (amount0In, amount1In, amount0Out,
/// amount1Out).
fn v2_swap(
    pair: Address,
    to: Address,
    amounts: [u128; 4],
    block: u32,
    log_index: u16,
    timestamp: u32,
) -> DatabaseLog {
    constructed(
        pair,
        &[
            events::V2_SWAP.topic0,
            Address::repeat_byte(0x70).into_word(),
            to.into_word(),
        ],
        amounts.iter().flat_map(|amount| number(*amount)).collect(),
        block,
        log_index,
        timestamp,
    )
}

fn pools() -> Vec<DatabaseLog> {
    let usdc = address(fixtures::USDC);
    let weth = address(fixtures::WETH);
    let usdt = address(fixtures::USDT);

    let v4 =
        |id: &str, currency0: Address, currency1: Address, index: u16| {
            constructed(
                address(fixtures::V4_POOL_MANAGER),
                &[
                    events::V4_INITIALIZE.topic0,
                    hash(id),
                    currency0.into_word(),
                    currency1.into_word(),
                ],
                [
                    number(500),
                    number(10),
                    number(0),
                    number(1 << 96),
                    number(0),
                ]
                .concat(),
                90,
                index,
                DAY - 1_000,
            )
        };

    vec![
        constructed(
            Address::repeat_byte(0xf2),
            &[
                events::V2_PAIR_CREATED.topic0,
                usdc.into_word(),
                weth.into_word(),
            ],
            [
                address(fixtures::V2_USDC_WETH).into_word().to_vec(),
                number(1),
            ]
            .concat(),
            90,
            0,
            DAY - 1_000,
        ),
        constructed(
            Address::repeat_byte(0xf3),
            &[
                events::V3_POOL_CREATED.topic0,
                usdc.into_word(),
                weth.into_word(),
                B256::from(U256::from(500u16)),
            ],
            [
                number(10),
                address(fixtures::V3_USDC_WETH).into_word().to_vec(),
            ]
            .concat(),
            90,
            1,
            DAY - 1_000,
        ),
        v4(fixtures::V4_SWAP_USDC_IN.topics[1], usdc, weth, 2),
        v4(fixtures::V4_SWAP_USDT_IN.topics[1], weth, usdt, 3),
    ]
}

const TRADER_X: Address = Address::repeat_byte(0x71);

/// Block 100 (first minute), block 101 (second minute), block 102 (next
/// hour). Everything inside one UTC day.
fn swaps() -> Vec<DatabaseLog> {
    let pair = address(fixtures::V2_USDC_WETH);

    let at = |raw: &RawLog, block: u32, index: u16, timestamp: u32| {
        raw.placed(block, index, timestamp)
    };

    vec![
        // V2 USDC/WETH: A (real), B, C.
        at(&fixtures::V2_SWAP, 100, 0, DAY + 10),
        v2_swap(
            pair,
            TRADER_X,
            [5_000_000, 0, 0, 1_900_000_000_000_000],
            100,
            1,
            DAY + 10,
        ),
        v2_swap(
            pair,
            TRADER_X,
            [1_000_000, 0, 0, 400_000_000_000_000],
            101,
            0,
            DAY + 70,
        ),
        // Real swaps of the other families, same hour.
        at(&fixtures::V3_SWAP, 101, 1, DAY + 70),
        at(&fixtures::V4_SWAP_USDC_IN, 101, 2, DAY + 70),
        at(&fixtures::V4_SWAP_USDT_IN, 101, 3, DAY + 70),
        at(&fixtures::BALANCER_SWAP, 101, 4, DAY + 70),
        at(&fixtures::CURVE_3POOL_EXCHANGE, 101, 5, DAY + 70),
        at(&fixtures::CURVE_UNDERLYING_EXCHANGE, 101, 6, DAY + 70),
        // A pair nobody announced: unpriceable.
        v2_swap(
            Address::repeat_byte(0x99),
            TRADER_X,
            [7, 0, 0, 9],
            101,
            7,
            DAY + 70,
        ),
        // Next hour: D on the V2 pair.
        v2_swap(
            pair,
            TRADER_X,
            [2_000_000, 0, 0, 900_000_000_000_000],
            102,
            0,
            DAY + 3_700,
        ),
    ]
}

async fn seed(database: &TestDb) {
    let token = |hex: &str, symbol: &str, decimals: u8| {
        format!(
            "({CHAIN}, {}, '{symbol}', '{symbol}', {decimals}, 'ERC20', 1)",
            addr(&address(hex))
        )
    };

    database
        .execute(&format!(
            "INSERT INTO tokens (chain, address, name, symbol, decimals, \
             type, _version) VALUES {}, {}, {}, {}",
            token(fixtures::USDC, "USDC", 6),
            token(fixtures::WETH, "WETH", 18),
            token(fixtures::USDT, "USDT", 6),
            token(DAI, "DAI", 18),
        ))
        .await;

    database
        .execute(&format!(
            "INSERT INTO quote_tokens (chain, token, kind) VALUES \
             ({CHAIN}, {}, 'stable'), ({CHAIN}, {}, 'stable'), \
             ({CHAIN}, {}, 'native'), ({CHAIN}, {}, 'stable')",
            addr(&address(fixtures::USDC)),
            addr(&address(fixtures::USDT)),
            addr(&address(fixtures::WETH)),
            // A "stable" nobody knows the decimals of (no tokens row, NULL
            // decimals): must stay unpriceable instead of assuming 18.
            addr(&address(BALANCER_TOKEN_IN)),
        ))
        .await;

    let mut created = decode(CHAIN, &pools());
    assert_eq!(created.pools.len(), 4);

    // Curve has no creation event: the row the RPC resolver would write.
    let three_pool = address(fixtures::CURVE_3POOL_EXCHANGE.address);
    created.pools.push(DexPool {
        pool_id: pool_id_of(three_pool),
        emitter: three_pool,
        protocol: Protocol::Curve,
        tokens: vec![
            address(DAI),
            address(fixtures::USDC),
            address(fixtures::USDT),
        ],
        token0: Address::ZERO,
        token1: Address::ZERO,
        factory: Address::ZERO,
        created_block: 0,
        timestamp: 0,
        source: PoolSource::Rpc,
        _version: POOL_VERSION_RPC,
        ..created.pools[0].clone()
    });

    database.insert(&created).await;

    // Two inserts: the aggregate states of the views must merge.
    let logs = swaps();
    let (first, second) = logs.split_at(4);

    for part in [first, second] {
        let mut rows = decode(CHAIN, part);
        assert_eq!(rows.swaps.len(), part.len());
        rows.set_version(1_000);
        database.insert(&rows).await;
    }
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= expected.abs() * 1e-12
}

/// USD per WETH implied by the scenario's native/stable pools in the first
/// hour (stable volume / native volume, decimals adjusted).
fn expected_native_price(include_second_hour: bool) -> f64 {
    let mut stable = 2.624963 + 5.0 + 1.0 // V2 A, B, C
        + 32_942.903993 // V3
        + 1_793.58876 // V4 USDC/WETH
        + 1_428.368405; // V4 WETH/USDT
    let mut native = 0.001
        + 0.0019
        + 0.0004
        + 12.572_743_894_898_124_8
        + 0.685_766_237_544_796_922
        + 0.546_044_876_340_273_314;

    if include_second_hour {
        stable += 2.0;
        native += 0.0009;
    }

    stable / native
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn candles_volumes_and_usd_match_hand_computed_numbers() {
    let database = TestDb::create().await;
    seed(&database).await;

    let pair = word(&pool_id_of(address(fixtures::V2_USDC_WETH)));

    // ---- 1m candles of the V2 pair (raw price = WETH wei per USDC unit).
    let candles = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), open, high, low, close, volume0, \
             volume1, swaps, traders FROM dex_candles_1m_v \
             WHERE chain = {CHAIN} AND pool_id = {pair} ORDER BY bucket"
        ))
        .fetch_all::<(u32, f64, f64, f64, f64, f64, f64, u64, u64)>()
        .await
        .unwrap();

    let price_a = 1e15 / 2_624_963.0;
    assert_eq!(candles.len(), 3);

    let first = candles[0];
    assert_eq!(first.0, DAY);
    assert!(close(first.1, price_a), "{first:?}");
    assert!(close(first.2, price_a));
    assert!(close(first.3, 3.8e8));
    assert!(close(first.4, 3.8e8));
    assert_eq!((first.5, first.6), (7_624_963.0, 2.9e15));
    assert_eq!((first.7, first.8), (2, 2));

    let second = candles[1];
    assert_eq!(second.0, DAY + 60);
    assert_eq!(
        (second.1, second.2, second.3, second.4),
        (4e8, 4e8, 4e8, 4e8)
    );
    assert_eq!(
        (second.5, second.6, second.7, second.8),
        (1e6, 4e14, 1, 1)
    );

    assert_eq!(candles[2].0, DAY + 3_660);

    // ---- 1h and 1d candles.
    let hour = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), open, high, low, close, volume0, \
             volume1, swaps, traders FROM dex_candles_1h_v \
             WHERE chain = {CHAIN} AND pool_id = {pair} ORDER BY bucket"
        ))
        .fetch_all::<(u32, f64, f64, f64, f64, f64, f64, u64, u64)>()
        .await
        .unwrap();

    assert_eq!(hour.len(), 2);
    assert_eq!(hour[0].0, DAY);
    assert!(close(hour[0].1, price_a));
    assert_eq!((hour[0].2, hour[0].3, hour[0].4), (4e8, 3.8e8, 4e8));
    assert_eq!((hour[0].5, hour[0].6), (8_624_963.0, 3.3e15));
    assert_eq!((hour[0].7, hour[0].8), (3, 2));
    assert_eq!(hour[1].0, DAY + 3_600);
    assert_eq!(hour[1].4, 4.5e8);

    let day = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), open, high, low, close, volume0, \
             volume1, swaps, traders FROM dex_candles_1d_v \
             WHERE chain = {CHAIN} AND pool_id = {pair}"
        ))
        .fetch_all::<(u32, f64, f64, f64, f64, f64, f64, u64, u64)>()
        .await
        .unwrap();

    assert_eq!(day.len(), 1);
    assert_eq!(day[0].0, DAY);
    assert!(close(day[0].1, price_a));
    assert_eq!((day[0].2, day[0].3, day[0].4), (4.5e8, 3.8e8, 4.5e8));
    assert_eq!((day[0].5, day[0].6), (10_624_963.0, 4.2e15));
    assert_eq!((day[0].7, day[0].8), (4, 2));

    // ---- sqrt price candles (V3): (sqrtPriceX96 / 2^96)^2.
    let v3 = database
        .client
        .query(&format!(
            "SELECT close FROM dex_candles_1h_v WHERE chain = {CHAIN} \
             AND pool_id = {}",
            word(&pool_id_of(address(fixtures::V3_USDC_WETH)))
        ))
        .fetch_one::<f64>()
        .await
        .unwrap();
    let sqrt = 1_547_521_364_678_359_767_176_169_597_843_369f64
        / 79_228_162_514_264_337_593_543_950_336f64;
    assert!(close(v3, sqrt * sqrt), "{v3}");

    // ---- decimals adjusted candle: WETH per USDC.
    let adjusted = database
        .client
        .query(&format!(
            "SELECT symbol0, symbol1, ifNull(close, -1), ifNull(volume0_adj, -1), ifNull(volume1_adj, -1) \
             FROM dex_pool_prices_1d_v WHERE chain = {CHAIN} \
             AND pool_id = {pair}"
        ))
        .fetch_one::<(String, String, f64, f64, f64)>()
        .await
        .unwrap();
    assert_eq!(
        (adjusted.0.as_str(), adjusted.1.as_str()),
        ("USDC", "WETH")
    );
    assert!(close(adjusted.2, 4.5e-4));
    assert!(close(adjusted.3, 10.624963));
    assert!(close(adjusted.4, 0.0042));

    // ---- native price: volume weighted over the four native/stable pools.
    let native = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), ifNull(price, -1), pools FROM \
             dex_native_price_1h_v WHERE chain = {CHAIN} ORDER BY bucket"
        ))
        .fetch_all::<(u32, f64, u64)>()
        .await
        .unwrap();

    assert_eq!(native.len(), 2);
    assert_eq!((native[0].0, native[0].2), (DAY, 4));
    assert!(
        close(native[0].1, expected_native_price(false)),
        "{native:?}"
    );
    assert!(close(native[1].1, 2.0 / 0.0009));

    // ---- per swap USD.
    let usd = database
        .client
        .query(&format!(
            "SELECT toUInt64(block_number), log_index, protocol, symbol_in, \
             symbol_out, toUInt8(token_in_known), ifNull(amount_in_adj, -1), \
             ifNull(amount_out_adj, -1), ifNull(amount_usd, -1) FROM dex_swaps_usd_v \
             WHERE chain = {CHAIN} ORDER BY block_number, log_index"
        ))
        .fetch_all::<(
            u64,
            u32,
            String,
            String,
            String,
            u8,
            f64,
            f64,
            f64,
        )>()
        .await
        .unwrap();

    assert_eq!(usd.len(), 11);
    let price = expected_native_price(false);

    // A: WETH in, USDC out -> the stable side wins: 2.624963 USD.
    assert_eq!((usd[0].3.as_str(), usd[0].4.as_str()), ("WETH", "USDC"));
    assert!(close(usd[0].6, 0.001));
    assert!(close(usd[0].8, 2.624963));
    // B: 5 USDC in.
    assert!(close(usd[1].8, 5.0));
    // V3: WETH in, 32,942.903993 USDC out.
    assert_eq!(usd[3].2, "uniswap_v3");
    assert!(close(usd[3].8, 32_942.903993));
    // V4 (negated): USDC in / USDT in.
    assert_eq!((usd[4].3.as_str(), usd[4].4.as_str()), ("USDC", "WETH"));
    assert!(close(usd[4].8, 1_793.58876));
    assert_eq!((usd[5].3.as_str(), usd[5].4.as_str()), ("USDT", "WETH"));
    assert!(close(usd[5].8, 1_428.368405));
    // Balancer: unknown token in, WETH out -> native priced.
    assert_eq!(usd[6].2, "balancer_v2");
    assert_eq!((usd[6].5, usd[6].6), (1, NULL));
    assert!(close(usd[6].8, 0.0016586259733838 * price));
    // Curve 3pool: coin 2 (USDT) -> coin 1 (USDC).
    assert_eq!((usd[7].3.as_str(), usd[7].4.as_str()), ("USDT", "USDC"));
    assert!(close(usd[7].8, 0.099206));
    // Curve pool without a dex_pools row, unknown pair: NULL, never 0.
    assert_eq!((usd[8].5, usd[8].8), (0, NULL));
    assert_eq!((usd[9].5, usd[9].8), (0, NULL));
    // D, next hour: 2 USDC in.
    assert!(close(usd[10].8, 2.0));

    // ---- daily USD volume per pool.
    let pool_usd = database
        .client
        .query(&format!(
            "SELECT protocol, lower(hex(pool_id)), ifNull(volume_usd, -1), swaps, \
             traders FROM dex_pool_volume_usd_1d_v WHERE chain = {CHAIN} \
             ORDER BY protocol, pool_id"
        ))
        .fetch_all::<(String, String, f64, u64, u64)>()
        .await
        .unwrap();

    let daily = expected_native_price(true);
    let of = |protocol: &str, id: &str| {
        pool_usd
            .iter()
            .find(|row| row.0 == protocol && row.1.ends_with(id))
            .unwrap_or_else(|| panic!("{protocol} {id}"))
    };

    // Both legs priced: USDC inputs (5 + 1 + 2) + WETH input (0.001).
    let v2 = of("uniswap_v2", &fixtures::V2_USDC_WETH[2..]);
    assert!(close(v2.2, 8.0 + 0.001 * daily), "{v2:?}");
    assert_eq!((v2.3, v2.4), (4, 2));
    // Unknown pair: NULL.
    let unknown = of("uniswap_v2", &"99".repeat(20));
    assert_eq!((unknown.2, unknown.3), (NULL, 1));
    // 3pool: USDT input.
    let three = of("curve", &fixtures::CURVE_3POOL_EXCHANGE.address[2..]);
    assert!(close(three.2, 0.099206));
    // Balancer: only the output leg is priced.
    let balancer =
        of("balancer_v2", &fixtures::BALANCER_SWAP.topics[1][2..]);
    assert!(close(balancer.2, 0.0016586259733838 * daily));

    // ---- per protocol.
    let protocols = database
        .client
        .query(&format!(
            "SELECT protocol, ifNull(volume_usd, -1), priced_pools, pools, swaps, \
             traders FROM dex_protocol_volume_usd_1d_v \
             WHERE chain = {CHAIN} ORDER BY protocol"
        ))
        .fetch_all::<(String, f64, u64, u64, u64, u64)>()
        .await
        .unwrap();

    let names: Vec<&str> =
        protocols.iter().map(|row| row.0.as_str()).collect();
    assert_eq!(
        names,
        ["balancer_v2", "curve", "uniswap_v2", "uniswap_v3", "uniswap_v4"]
    );
    // curve: two pools traded, one priced.
    assert_eq!(
        (protocols[1].2, protocols[1].3, protocols[1].4),
        (1, 2, 2)
    );
    assert!(close(protocols[2].1, 8.0 + 0.001 * daily));
    assert_eq!((protocols[2].3, protocols[2].4), (2, 5));
    assert!(close(protocols[4].1, 1_793.58876 + 1_428.368405));

    // ---- per token.
    let usdc = database
        .client
        .query(&format!(
            "SELECT symbol, ifNull(volume_adj, -1), ifNull(volume_usd, -1), swaps, pools \
             FROM dex_token_volume_1d_v WHERE chain = {CHAIN} \
             AND token = {}",
            addr(&address(fixtures::USDC))
        ))
        .fetch_one::<(String, f64, f64, u64, u64)>()
        .await
        .unwrap();

    let usdc_volume = 10.624963 + 32_942.903993 + 1_793.58876 + 0.099112;
    assert_eq!(usdc.0, "USDC");
    assert!(close(usdc.1, usdc_volume), "{usdc:?}");
    assert!(close(usdc.2, usdc_volume));
    assert_eq!((usdc.3, usdc.4), (7, 4));

    let token_usd = database
        .client
        .query(&format!(
            "SELECT ifNull(volume_usd, -1), swaps, unpriced_swaps \
             FROM dex_token_volume_usd_1d_v WHERE chain = {CHAIN} \
             AND token = {}",
            addr(&address(fixtures::WETH))
        ))
        .fetch_one::<(f64, u64, u64)>()
        .await
        .unwrap();
    assert_eq!((token_usd.1, token_usd.2), (8, 0));

    // ---- pools of a token, top pools.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pools_by_token FINAL \
                 WHERE chain = {CHAIN} AND token = {}",
                addr(&address(fixtures::USDC))
            ))
            .await,
        4
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps_by_pool FINAL \
                 WHERE chain = {CHAIN} AND pool_id = {pair}"
            ))
            .await,
        4
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps_by_trader FINAL \
                 WHERE chain = {CHAIN} AND trader = {}",
                addr(&TRADER_X)
            ))
            .await,
        4
    );

    // ---- the backfill query: traded, resolvable, no dex_pools row.
    let missing = crate::dex::MISSING_POOLS_SQL
        .replace("{chain}", &CHAIN.to_string())
        .replace("{limit}", "100");
    let missing = database
        .client
        .query(&format!(
            "SELECT lower(hex(emitter)), protocol FROM ({missing}) \
             ORDER BY protocol"
        ))
        .fetch_all::<(String, String)>()
        .await
        .unwrap();
    assert_eq!(
        missing,
        vec![
            (
                fixtures::CURVE_UNDERLYING_EXCHANGE.address[2..]
                    .to_string(),
                "curve".to_string()
            ),
            ("99".repeat(20), "uniswap_v2".to_string()),
        ]
    );

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_first_creation_event_wins_and_rpc_rows_lose() {
    let database = TestDb::create().await;

    let pair = address(fixtures::V2_USDC_WETH);
    let real = decode(CHAIN, &pools()).pools.remove(0);

    // A forged PairCreated for the same pair, 10 blocks later.
    let forged = DexPool {
        token0: Address::repeat_byte(0x66),
        tokens: vec![Address::repeat_byte(0x66), real.token1],
        created_block: real.created_block + 10,
        _version: crate::dex::pool_event_version(
            real.created_block + 10,
            0,
        ),
        ..real.clone()
    };
    let rpc = DexPool {
        token0: Address::repeat_byte(0x77),
        source: PoolSource::Rpc,
        created_block: 0,
        _version: POOL_VERSION_RPC,
        ..real.clone()
    };

    // Worst insertion order: the real row first.
    for pool in [real.clone(), forged, rpc] {
        database
            .insert(&DexRows { pools: vec![pool], ..DexRows::default() })
            .await;
    }

    let stored = database
        .client
        .query(&format!(
            "SELECT lower(hex(token0)), source FROM dex_pools FINAL \
             WHERE chain = {CHAIN} AND pool_id = {}",
            word(&pool_id_of(pair))
        ))
        .fetch_all::<(String, String)>()
        .await
        .unwrap();

    assert_eq!(
        stored,
        vec![(fixtures::USDC[2..].to_string(), "event".to_string())]
    );

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn purge_and_bucket_repair_reproduce_the_views() {
    let database = TestDb::create().await;
    seed(&database).await;

    // Liquidity rows so every block scoped table has something to purge.
    let mut liquidity = decode(
        CHAIN,
        &[
            fixtures::V2_SYNC.at(DAY + 10),
            fixtures::V3_MINT.at(DAY + 3_700),
        ],
    );
    liquidity.liquidity[0].block_number = 100;
    liquidity.liquidity[1].block_number = 102;
    database.insert(&liquidity).await;

    // A pool created inside the range that will be purged.
    let late = decode(
        CHAIN,
        &[constructed(
            Address::repeat_byte(0xf2),
            &[
                events::V2_PAIR_CREATED.topic0,
                Address::repeat_byte(0x0a).into_word(),
                Address::repeat_byte(0x0b).into_word(),
            ],
            [Address::repeat_byte(0x98).into_word().to_vec(), number(2)]
                .concat(),
            101,
            9,
            DAY + 70,
        )],
    );
    assert_eq!(late.pools.len(), 1);
    database.insert(&late).await;

    // ---- 1. delete bucket + rebuild == what the views wrote.
    for table in DEX_DERIVED {
        let view = format!("{}_v", table.name);
        let before = database.snapshot(&view).await;
        assert!(!before.is_empty(), "{view}");

        database
            .execute(&format!(
                "DELETE FROM {} WHERE chain = {CHAIN} AND {} >= \
                 toDateTime({DAY}) SETTINGS lightweight_deletes_sync = 2",
                table.name, table.bucket_column
            ))
            .await;
        assert!(database.snapshot(&view).await.is_empty(), "{view}");

        database.execute(&render_rebuild(table, CHAIN, DAY)).await;
        assert_eq!(database.snapshot(&view).await, before, "{view}");
    }

    // ---- 2. reorg at block 101: lightweight delete on every table.
    for table in BLOCK_SCOPED_TABLES {
        let column = block_column(table);
        let total = database
            .count(&format!(
                "SELECT count() FROM {table} WHERE chain = {CHAIN}"
            ))
            .await;
        let doomed = database
            .count(&format!(
                "SELECT count() FROM {table} WHERE chain = {CHAIN} \
                 AND {column} >= 101"
            ))
            .await;
        assert!(total > doomed && doomed > 0, "{table}: {doomed}/{total}");

        database
            .execute(&format!(
                "DELETE FROM {table} WHERE chain = {CHAIN} AND {column} >= \
                 101 SETTINGS lightweight_deletes_sync = 2"
            ))
            .await;

        assert_eq!(
            database
                .count(&format!(
                    "SELECT count() FROM {table} WHERE chain = {CHAIN}"
                ))
                .await,
            total - doomed,
            "{table}"
        );
        assert_eq!(
            database
                .count(&format!(
                    "SELECT count() FROM {table} WHERE chain = {CHAIN} \
                     AND {column} >= 101"
                ))
                .await,
            0,
            "{table}"
        );
    }

    // Swaps A and B (block 100) survive, pools (block 90 / rpc) too.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps FINAL WHERE chain = {CHAIN}"
            ))
            .await,
        2
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pools FINAL WHERE chain = {CHAIN}"
            ))
            .await,
        5
    );

    // ---- 3. bucket repair from the bucket of the first purged block.
    for table in DEX_DERIVED {
        let from = (DAY + 70) - (DAY + 70) % table.bucket_seconds;

        database
            .execute(&format!(
                "DELETE FROM {} WHERE chain = {CHAIN} AND {} >= \
                 toDateTime({from}) SETTINGS lightweight_deletes_sync = 2",
                table.name, table.bucket_column
            ))
            .await;
        database.execute(&render_rebuild(table, CHAIN, from)).await;
    }

    let hour = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), high, low, close, volume0, swaps \
             FROM dex_candles_1h_v WHERE chain = {CHAIN}"
        ))
        .fetch_all::<(u32, f64, f64, f64, f64, u64)>()
        .await
        .unwrap();

    // Only the V2 pair is left, with swaps A and B.
    assert_eq!(hour.len(), 1);
    assert_eq!(hour[0].0, DAY);
    assert_eq!((hour[0].2, hour[0].3), (3.8e8, 3.8e8));
    assert_eq!((hour[0].4, hour[0].5), (7_624_963.0, 2));

    assert_eq!(
        database
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM dex_protocol_stats_1d_v \
                 WHERE chain = {CHAIN}"
            ))
            .await,
        2
    );

    // ---- 4. re-streaming the purged blocks restores everything.
    let logs = swaps();
    let mut again = decode(CHAIN, &logs[2..]);
    again.set_version(2_000);
    database.insert(&again).await;

    let pair = word(&pool_id_of(address(fixtures::V2_USDC_WETH)));
    let day = database
        .client
        .query(&format!(
            "SELECT close, volume0, swaps, traders FROM dex_candles_1d_v \
             WHERE chain = {CHAIN} AND pool_id = {pair}"
        ))
        .fetch_one::<(f64, f64, u64, u64)>()
        .await
        .unwrap();
    assert_eq!(day, (4.5e8, 10_624_963.0, 4, 2));

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn maximal_amounts_do_not_wrap_the_aggregates() {
    let database = TestDb::create().await;

    let spam = Address::repeat_byte(0x5a);
    let exchange = |sold: U256, index: u16| {
        constructed(
            spam,
            &[
                events::CURVE_CRYPTO_TOKEN_EXCHANGE.topic0,
                TRADER_X.into_word(),
            ],
            [
                number(0),
                sold.to_be_bytes::<32>().to_vec(),
                number(1),
                number(10),
            ]
            .concat(),
            100,
            index,
            DAY,
        )
    };

    let mut rows = decode(
        CHAIN,
        &[
            // 2^256 - 1, then 10 more: a UInt256 sum would wrap to 9.
            exchange(U256::MAX, 0),
            exchange(U256::from(10u8), 1),
            // Int256 extremes on a V3 shaped swap.
            constructed(
                Address::repeat_byte(0x5b),
                &[
                    events::V3_SWAP.topic0,
                    TRADER_X.into_word(),
                    TRADER_X.into_word(),
                ],
                [
                    I256::MAX.to_be_bytes::<32>().to_vec(),
                    I256::MIN.to_be_bytes::<32>().to_vec(),
                    number(1 << 96),
                    number(1),
                    number(0),
                ]
                .concat(),
                100,
                2,
                DAY,
            ),
            constructed(
                Address::repeat_byte(0x5b),
                &[
                    events::V3_SWAP.topic0,
                    TRADER_X.into_word(),
                    TRADER_X.into_word(),
                ],
                [
                    I256::MAX.to_be_bytes::<32>().to_vec(),
                    I256::MIN.to_be_bytes::<32>().to_vec(),
                    number(1 << 96),
                    number(1),
                    number(0),
                ]
                .concat(),
                100,
                3,
                DAY,
            ),
        ],
    );
    assert_eq!(rows.swaps.len(), 4);
    assert_eq!(rows.swaps[0].amount_in, U256::MAX);
    rows.set_version(1);
    database.insert(&rows).await;

    let max = 1.157_920_892_373_162e77;

    let leg = database
        .client
        .query(&format!(
            "SELECT volume_in FROM dex_pool_volume_1d_v WHERE chain = \
             {CHAIN} AND protocol = 'curve' AND leg_index = 0"
        ))
        .fetch_one::<f64>()
        .await
        .unwrap();
    assert!(close(leg, max), "{leg}");

    let candle = database
        .client
        .query(&format!(
            "SELECT volume0, volume1 FROM dex_candles_1d_v \
             WHERE chain = {CHAIN}"
        ))
        .fetch_one::<(f64, f64)>()
        .await
        .unwrap();
    // 2 * (2^255 - 1) and 2 * 2^255: finite, positive, not wrapped.
    assert!(close(candle.0, max), "{candle:?}");
    assert!(close(candle.1, max), "{candle:?}");

    // The rebuild agrees.
    for table in DEX_DERIVED {
        let view = format!("{}_v", table.name);
        let before = database.snapshot(&view).await;
        database
            .execute(&format!(
                "DELETE FROM {} WHERE chain = {CHAIN} AND {} >= \
                 toDateTime({DAY}) SETTINGS lightweight_deletes_sync = 2",
                table.name, table.bucket_column
            ))
            .await;
        database.execute(&render_rebuild(table, CHAIN, DAY)).await;
        assert_eq!(database.snapshot(&view).await, before, "{view}");
    }

    database.drop().await;
}
