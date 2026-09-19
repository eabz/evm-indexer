//! DEX tables, aggregates and views against a REAL ClickHouse. Ignored by
//! default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test dex::integration -- --ignored
//! ```
//!
//! Every test creates (and drops) its OWN databases on that server, applies
//! minimal `tokens` and `reorgs` tables (owned by the core migrations) plus
//! the DEX migrations to them, and never touches the database named in the
//! url. No test issues a DELETE: reorgs are tombstones + epochs
//! (docs/design.md §2), exactly as `purge_range` does them.
//!
//! Rows go through [`decode`] and are inserted with `INSERT ... VALUES`
//! (`unhex` / `toInt256`): the binary row serializers of the design are
//! not available in this tree yet, so the clickhouse crate's RowBinary
//! inserts are exercised by the pipeline's own tests after the merge.

// The literals below mirror on-chain amounts digit by digit.
#![allow(clippy::excessive_precision, clippy::inconsistent_digit_grouping)]

use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Bytes, B256, I256, U256};
use clickhouse::Client;

use crate::{
    db::{models::log::DatabaseLog, next_version, DatabaseParams},
    dex::{
        block_column, decode,
        derived::{rebuild_statements, render_rebuild},
        events,
        fixtures::{self, address, RawLog},
        models::{
            pool_id_of, DexLiquidity, DexPool, DexSwap, PoolSource,
            Protocol,
        },
        purge_filter,
        sql::{reorg_prerequisites, statements, CHAINS_SQL, MIGRATIONS},
        tombstone_sql, DexRows, BASE_TABLES, DEX_DERIVED, SIDE_TABLES,
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
        // Tests run in parallel and the clock is coarse: number them too.
        static SEQUENCE: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);
        let sequence =
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Unique per process, per call and per instant, and ending in
        // `_test` (the convention of the combined gate, which runs every
        // module's integration tests in parallel on one server).
        let name =
            format!("dex_{}_{nanos}_{sequence}_test", std::process::id());

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
        // `reorgs` and the SHARED epoch_floor_v the aggregate views join
        // (migration 0004, which the migrator applies long before 0011).
        for statement in reorg_prerequisites() {
            database.execute(&statement).await;
        }
        for statement in statements(CHAINS_SQL) {
            database.execute(&statement).await;
        }
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

    /// A single String column.
    async fn lines(&self, sql: &str) -> Vec<String> {
        self.client
            .query(&sql.replace('?', "??"))
            .fetch_all::<String>()
            .await
            .unwrap_or_else(|error| panic!("{error}\n{sql}"))
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
            self.await_part("dex_swaps", rows.swaps[0]._version).await;
        }

        if !rows.liquidity.is_empty() {
            let values: Vec<String> =
                rows.liquidity.iter().map(liquidity_sql).collect();
            self.execute(&format!(
                "INSERT INTO dex_liquidity ({LIQUIDITY_COLUMNS}) VALUES {}",
                values.join(", ")
            ))
            .await;
            self.await_part("dex_liquidity", rows.liquidity[0]._version)
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
            self.await_part("dex_pools", rows.pools[0]._version).await;
        }
    }

    /// Waits until the part `version` just wrote into `table` is readable.
    ///
    /// ClickHouse 25.12 has no read-your-writes ([`SETTLE`]), so a read
    /// issued right after an acknowledged INSERT can miss the new part.
    /// Every row batch of this harness carries its own `_version`
    /// (`next_version` is strictly increasing), which keeps the question
    /// unaffected by what the other chains write meanwhile. One row is
    /// the whole signal: a batch is one part and a part becomes readable
    /// as a whole. Counting rows would NOT work - rows that share a
    /// sorting key collapse inside the part, so the number stored is not
    /// the number written.
    async fn await_part(&self, table: &str, version: u64) {
        let sql = format!(
            "SELECT count() FROM {table} WHERE _version = {version}"
        );
        let started = std::time::Instant::now();

        loop {
            if self.count(&sql).await > 0 {
                return;
            }
            assert!(
                started.elapsed() < SETTLE,
                "{table}: the rows of version {version} never became \
                 visible"
            );
            tokio::time::sleep(RETRY).await;
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

/// An identity column: the address left padded to 32 bytes
/// (docs/design.md §13).
fn id(value: &Address) -> String {
    bytes(crate::utils::format::id32(*value).as_slice())
}

fn word(value: &B256) -> String {
    bytes(value.as_slice())
}

/// A `tx_id` column: the raw transaction id bytes.
fn tx(value: &Bytes) -> String {
    bytes(value.as_ref())
}

/// What `lower(hex(<an identity column>))` returns for an EVM address: 24
/// zeros then the 40 hex digits of the address.
fn id_hex(value: &str) -> String {
    format!("{}{}", "0".repeat(24), value.trim_start_matches("0x"))
        .to_lowercase()
}

fn int(value: &I256) -> String {
    format!("toInt256('{value}')")
}

fn uint(value: &U256) -> String {
    format!("toUInt256('{value}')")
}

fn ids(values: &[Address]) -> String {
    let items: Vec<String> = values.iter().map(id).collect();
    format!("[{}]", items.join(", "))
}

const SWAP_COLUMNS: &str = "chain, block_number, timestamp, \
    tx_id, tx_index, ordinal, pool_id, emitter, protocol, sender, \
    recipient, tx_from, tx_to, trader, amount0, amount1, token_in, \
    token_out, amount_in, amount_out, verified_in, verified_out, reserve0, \
    reserve1, coin_in, coin_out, underlying, \
    sqrt_price_x96, liquidity, tick, fee, epoch, _version";

fn swap_sql(swap: &DexSwap) -> String {
    format!(
        "({}, {}, {}, {}, {}, {}, {}, {}, '{}', {}, {}, {}, {}, {}, {}, {}, \
         {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
        swap.chain,
        swap.block_number,
        swap.timestamp,
        tx(&swap.tx_id),
        swap.tx_index,
        swap.ordinal,
        word(&swap.pool_id),
        id(&swap.emitter),
        swap.protocol,
        id(&swap.sender),
        id(&swap.recipient),
        id(&swap.tx_from),
        id(&swap.tx_to),
        id(&swap.trader),
        int(&swap.amount0),
        int(&swap.amount1),
        id(&swap.token_in),
        id(&swap.token_out),
        uint(&swap.amount_in),
        uint(&swap.amount_out),
        id(&swap.verified_in),
        id(&swap.verified_out),
        uint(&swap.reserve0),
        uint(&swap.reserve1),
        swap.coin_in,
        swap.coin_out,
        swap.underlying,
        uint(&swap.sqrt_price_x96),
        uint(&swap.liquidity),
        swap.tick,
        swap.fee,
        swap.epoch,
        swap._version,
    )
}

const LIQUIDITY_COLUMNS: &str = "chain, block_number, timestamp, \
    tx_id, tx_index, ordinal, pool_id, emitter, protocol, kind, sender, \
    owner, tx_from, tx_to, amount0, amount1, reserve0, reserve1, \
    liquidity_delta, tick_lower, tick_upper, epoch, _version";

fn liquidity_sql(row: &DexLiquidity) -> String {
    format!(
        "({}, {}, {}, {}, {}, {}, {}, {}, '{}', '{}', {}, {}, {}, {}, {}, \
         {}, {}, {}, {}, {}, {}, {}, {})",
        row.chain,
        row.block_number,
        row.timestamp,
        tx(&row.tx_id),
        row.tx_index,
        row.ordinal,
        word(&row.pool_id),
        id(&row.emitter),
        row.protocol,
        row.kind,
        id(&row.sender),
        id(&row.owner),
        id(&row.tx_from),
        id(&row.tx_to),
        int(&row.amount0),
        int(&row.amount1),
        uint(&row.reserve0),
        uint(&row.reserve1),
        int(&row.liquidity_delta),
        row.tick_lower,
        row.tick_upper,
        row.epoch,
        row._version,
    )
}

const POOL_COLUMNS: &str = "chain, pool_id, emitter, factory, protocol, \
    token0, token1, tokens, underlying_tokens, fee, tick_spacing, hooks, \
    stable, created_block, timestamp, tx_id, tx_index, ordinal, source, \
    attempts, epoch, _version";

fn pool_sql(pool: &DexPool) -> String {
    format!(
        "({}, {}, {}, {}, '{}', {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, \
         {}, {}, '{}', {}, {}, {})",
        pool.chain,
        word(&pool.pool_id),
        id(&pool.emitter),
        id(&pool.factory),
        pool.protocol,
        id(&pool.token0),
        id(&pool.token1),
        ids(&pool.tokens),
        ids(&pool.underlying_tokens),
        pool.fee,
        pool.tick_spacing,
        id(&pool.hooks),
        pool.stable,
        pool.created_block,
        pool.timestamp,
        tx(&pool.tx_id),
        pool.tx_index,
        pool.ordinal,
        pool.source,
        pool.attempts,
        pool.epoch,
        pool._version,
    )
}

// ------------------------------------------------------------- the scenario

fn number(value: u128) -> Vec<u8> {
    U256::from(value).to_be_bytes::<32>().to_vec()
}

use crate::dex::fixtures::{
    build as constructed, same_transaction, transfer,
};

const TRADER_X: Address = Address::repeat_byte(0x71);
const ROUTER: Address = Address::repeat_byte(0x70);

/// Log index of the `offset`-th log of the transaction in `slot`.
fn at(slot: u16, offset: u16) -> u16 {
    slot * 10 + offset
}

/// A bare V2 shaped swap event (nothing proves it): (amount0In, amount1In,
/// amount0Out, amount1Out).
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
        &[events::V2_SWAP.topic0, ROUTER.into_word(), to.into_word()],
        amounts.iter().flat_map(|amount| number(*amount)).collect(),
        block,
        log_index,
        timestamp,
    )
}

/// A whole V2 trade the way a pair really emits it: the token transfers,
/// `Sync` with the reserves after the swap, then `Swap` - one transaction.
#[allow(clippy::too_many_arguments)]
fn v2_trade(
    pair: Address,
    tokens: (Address, Address),
    to: Address,
    amounts: [u128; 4],
    reserves: (u128, u128),
    block: u32,
    slot: u16,
    timestamp: u32,
) -> Vec<DatabaseLog> {
    let mut logs = Vec::new();
    let legs = [
        (tokens.0, amounts[0], true),
        (tokens.1, amounts[1], true),
        (tokens.0, amounts[2], false),
        (tokens.1, amounts[3], false),
    ];

    for (token, amount, incoming) in legs {
        if amount == 0 {
            continue;
        }
        let (from, recipient) =
            if incoming { (ROUTER, pair) } else { (pair, to) };
        logs.push(transfer(
            token,
            from,
            recipient,
            U256::from(amount),
            block,
            at(slot, logs.len() as u16),
            timestamp,
        ));
    }

    logs.push(constructed(
        pair,
        &[events::V2_SYNC.topic0],
        [number(reserves.0), number(reserves.1)].concat(),
        block,
        at(slot, 2),
        timestamp,
    ));
    logs.push(v2_swap(pair, to, amounts, block, at(slot, 3), timestamp));

    same_transaction(logs, u64::from(block) * 1_000 + u64::from(slot))
}

fn usdc_weth() -> (Address, Address) {
    (address(fixtures::USDC), address(fixtures::WETH))
}

/// A trade on the scenario's V2 USDC/WETH pair.
fn pair_trade(
    amounts: [u128; 4],
    reserves: (u128, u128),
    block: u32,
    slot: u16,
    timestamp: u32,
) -> Vec<DatabaseLog> {
    v2_trade(
        address(fixtures::V2_USDC_WETH),
        usdc_weth(),
        TRADER_X,
        amounts,
        reserves,
        block,
        slot,
        timestamp,
    )
}

fn placed(
    logs: &[(&RawLog, u16)],
    block: u32,
    slot: u16,
    timestamp: u32,
) -> Vec<DatabaseLog> {
    logs.iter()
        .map(|(raw, offset)| {
            raw.placed(block, at(slot, *offset), timestamp)
        })
        .collect()
}

/// Real transactions, moved to a scenario position with their transfers.
fn v2_real(block: u32, slot: u16, timestamp: u32) -> Vec<DatabaseLog> {
    placed(
        &[
            (&fixtures::V2_SWAP_WETH_IN, 0),
            (&fixtures::V2_SWAP_USDC_OUT, 1),
            (&fixtures::V2_SYNC, 2),
            (&fixtures::V2_SWAP, 3),
        ],
        block,
        slot,
        timestamp,
    )
}

fn v3_real(block: u32, slot: u16, timestamp: u32) -> Vec<DatabaseLog> {
    placed(
        &[
            (&fixtures::V3_SWAP_USDC_OUT, 0),
            (&fixtures::V3_SWAP_WETH_IN, 1),
            (&fixtures::V3_SWAP, 2),
        ],
        block,
        slot,
        timestamp,
    )
}

/// The real two swap V4 transaction (one settlement per currency).
fn v4_real(block: u32, slot: u16, timestamp: u32) -> Vec<DatabaseLog> {
    let mut logs = placed(
        &[
            (&fixtures::V4_TX_USDC_TAKEN, 0),
            (&fixtures::V4_TX_WETH_TAKEN, 1),
            (&fixtures::V4_SWAP_USDC_IN, 2),
            (&fixtures::V4_SWAP_USDT_IN, 3),
            (&fixtures::V4_TX_USDC_SETTLED, 4),
            (&fixtures::V4_TX_USDT_SETTLED, 5),
        ],
        block,
        slot,
        timestamp,
    );
    // The scenario's pools have ids of their own (see `v4_pool_id`).
    logs[2].topic1 = Some(v4_pool_id(usdc_weth().0, usdc_weth().1));
    logs[3].topic1 =
        Some(v4_pool_id(usdc_weth().1, address(fixtures::USDT)));
    logs
}

fn balancer_real(
    block: u32,
    slot: u16,
    timestamp: u32,
) -> Vec<DatabaseLog> {
    placed(
        &[
            (&fixtures::BALANCER_SWAP, 0),
            (&fixtures::BALANCER_SWAP_TOKEN_IN, 1),
            (&fixtures::BALANCER_SWAP_WETH_OUT, 2),
        ],
        block,
        slot,
        timestamp,
    )
}

fn curve_real(block: u32, slot: u16, timestamp: u32) -> Vec<DatabaseLog> {
    placed(
        &[
            (&fixtures::CURVE_3POOL_USDT_IN, 0),
            (&fixtures::CURVE_3POOL_USDC_OUT, 1),
            (&fixtures::CURVE_3POOL_EXCHANGE, 2),
        ],
        block,
        slot,
        timestamp,
    )
}

/// PoolKey (fee 500, tick spacing 10, no hooks) of the scenario's V4 pools.
fn v4_key(currency0: Address, currency1: Address) -> Vec<u8> {
    [
        currency0.into_word().to_vec(),
        currency1.into_word().to_vec(),
        number(500),
        number(10),
        number(0),
    ]
    .concat()
}

fn v4_pool_id(currency0: Address, currency1: Address) -> B256 {
    alloy::primitives::keccak256(v4_key(currency0, currency1))
}

fn pools() -> Vec<DatabaseLog> {
    let (usdc, weth) = usdc_weth();
    let usdt = address(fixtures::USDT);

    let v4 = |currency0: Address, currency1: Address, index: u16| {
        let key = v4_key(currency0, currency1);
        constructed(
            address(fixtures::V4_POOL_MANAGER),
            &[
                events::V4_INITIALIZE.topic0,
                v4_pool_id(currency0, currency1),
                currency0.into_word(),
                currency1.into_word(),
            ],
            [key[64..].to_vec(), number(1 << 96), number(0)].concat(),
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
        v4(usdc, weth, 2),
        v4(weth, usdt, 3),
    ]
}

/// Reserves of the scenario pair after A (real), B, C and D.
const RESERVES_A: (u128, u128) =
    (10_391_705_448_638, 3_946_924_532_103_308_992_521);
const RESERVES_B: (u128, u128) =
    (RESERVES_A.0 + 5_000_000, RESERVES_A.1 - 1_900_000_000_000_000);
const RESERVES_C: (u128, u128) =
    (RESERVES_B.0 + 1_000_000, RESERVES_B.1 - 400_000_000_000_000);
const RESERVES_D: (u128, u128) =
    (RESERVES_C.0 + 2_000_000, RESERVES_C.1 - 900_000_000_000_000);

fn ratio(reserves: (u128, u128)) -> f64 {
    reserves.1 as f64 / reserves.0 as f64
}

/// Block 100, first minute of the day: A (real) and B on the V2 pair.
fn block_100() -> Vec<DatabaseLog> {
    let mut logs = v2_real(100, 0, DAY + 10);
    logs.extend(pair_trade(
        [5_000_000, 0, 0, 1_900_000_000_000_000],
        RESERVES_B,
        100,
        1,
        DAY + 10,
    ));
    logs
}

/// Block 101, second minute: C, then real swaps of the other families, a
/// Curve pool nobody knows and a pair nobody announced (nothing proves
/// either of them).
fn block_101() -> Vec<DatabaseLog> {
    let mut logs = pair_trade(
        [1_000_000, 0, 0, 400_000_000_000_000],
        RESERVES_C,
        101,
        0,
        DAY + 70,
    );
    logs.extend(v3_real(101, 1, DAY + 70));
    logs.extend(v4_real(101, 2, DAY + 70));
    logs.extend(curve_real(101, 3, DAY + 70));
    logs.push(fixtures::CURVE_UNDERLYING_EXCHANGE.placed(
        101,
        at(4, 0),
        DAY + 70,
    ));
    logs.push(v2_swap(
        Address::repeat_byte(0x99),
        TRADER_X,
        [7_000, 0, 0, 9_000],
        101,
        at(5, 0),
        DAY + 70,
    ));
    logs
}

/// Block 102, the next hour: D and the real Balancer swap (unknown token
/// in, WETH out - valued with the native price of the hour before).
fn block_102() -> Vec<DatabaseLog> {
    let mut logs = pair_trade(
        [2_000_000, 0, 0, 900_000_000_000_000],
        RESERVES_D,
        102,
        0,
        DAY + 3_700,
    );
    logs.extend(balancer_real(102, 1, DAY + 3_700));
    logs
}

/// The 11 swaps of the scenario.
fn swaps() -> Vec<DatabaseLog> {
    [block_100(), block_101(), block_102()].concat()
}

/// A resolver row: what the pool itself answered.
fn rpc_pool(
    chain: u64,
    pool: Address,
    protocol: Protocol,
    tokens: Vec<Address>,
) -> DexPool {
    let two = tokens.len() == 2 && protocol != Protocol::Curve;

    DexPool {
        chain,
        pool_id: pool_id_of(pool),
        emitter: pool,
        factory: Address::ZERO,
        protocol,
        token0: if two { tokens[0] } else { Address::ZERO },
        token1: if two { tokens[1] } else { Address::ZERO },
        tokens,
        underlying_tokens: Vec::new(),
        fee: 0,
        tick_spacing: 0,
        hooks: Address::ZERO,
        stable: false,
        created_block: 0,
        timestamp: 0,
        tx_id: Bytes::new(),
        tx_index: 0,
        ordinal: 0,
        source: PoolSource::Rpc,
        attempts: 0,
        epoch: 0,
        _version: 0,
    }
}

/// Tokens, quote tokens, trusted singletons and pools of `chain`: what both
/// a reorged and a clean index know before the first swap.
async fn seed_reference(database: &TestDb, chain: u64) {
    let token = |hex: &str, symbol: &str, decimals: u8| {
        format!(
            "({chain}, {}, '{symbol}', '{symbol}', {decimals}, 'ERC20', 1)",
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
            "INSERT INTO quote_tokens (chain, token, kind, _version) VALUES \
             ({chain}, {}, 'stable', 1), ({chain}, {}, 'stable', 1), \
             ({chain}, {}, 'native', 1), ({chain}, {}, 'stable', 1)",
            id(&address(fixtures::USDC)),
            id(&address(fixtures::USDT)),
            id(&address(fixtures::WETH)),
            // A "stable" nobody knows the decimals of (no tokens row, NULL
            // decimals): must stay unpriceable instead of assuming 18.
            id(&address(BALANCER_TOKEN_IN)),
        ))
        .await;

    // The operator vouches for the real singletons.
    database
        .execute(&format!(
            "INSERT INTO dex_trusted_emitters (chain, emitter, protocol, \
             _version) VALUES ({chain}, {}, 'uniswap_v4', 1), \
             ({chain}, {}, 'balancer_v2', 1)",
            id(&address(fixtures::V4_POOL_MANAGER)),
            id(&address(fixtures::BALANCER_VAULT)),
        ))
        .await;

    let mut created = decode(chain, &pools());
    assert_eq!(created.pools.len(), 4);

    // What the background resolver wrote: the contract pools answered
    // token0() / token1() / coins(i) themselves.
    let (usdc, weth) = usdc_weth();
    created.pools.extend([
        rpc_pool(
            chain,
            address(fixtures::V2_USDC_WETH),
            Protocol::UniswapV2,
            vec![usdc, weth],
        ),
        rpc_pool(
            chain,
            address(fixtures::V3_USDC_WETH),
            Protocol::UniswapV3,
            vec![usdc, weth],
        ),
        rpc_pool(
            chain,
            address(fixtures::CURVE_3POOL_EXCHANGE.address),
            Protocol::Curve,
            vec![address(DAI), usdc, address(fixtures::USDT)],
        ),
    ]);

    created.set_version(next_version());
    database.insert(&created).await;
}

/// Decodes and inserts `logs` the way the pipeline flushes them. Returns
/// how many swaps they held.
async fn insert_logs(
    database: &TestDb,
    chain: u64,
    logs: &[DatabaseLog],
    epoch: u32,
) -> usize {
    let mut rows = decode(chain, logs);
    rows.set_version(next_version());
    rows.set_epoch(epoch);
    database.insert(&rows).await;
    rows.swaps.len()
}

async fn seed(database: &TestDb) {
    seed_reference(database, CHAIN).await;

    // Two inserts: the aggregate states of the views must merge.
    assert_eq!(insert_logs(database, CHAIN, &block_100(), 0).await, 2);
    let rest = [block_101(), block_102()].concat();
    assert_eq!(insert_logs(database, CHAIN, &rest, 0).await, 9);
}

fn close(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() <= expected.abs() * 1e-12
}

/// USD per WETH of hour 1: the only pool with both legs verified AND at
/// least 1000 stable units of volume is the V3 pool.
fn native_price_hour_1() -> f64 {
    32_942.903993 / 12.572_743_894_898_124_8
}

const BALANCER_WETH_OUT: f64 = 0.001_658_625_973_383_8;

type Candle = (u32, f64, f64, f64, f64, f64, f64, f64, u64);

/// (bucket, open, high, low, close, pool_open, pool_close, volume0, swaps)
async fn candles(
    database: &TestDb,
    table: &str,
    pool: &str,
) -> Vec<Candle> {
    database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), ifNull(open, -1), ifNull(high, -1), \
             ifNull(low, -1), ifNull(close, -1), ifNull(pool_open, -1), \
             ifNull(pool_close, -1), volume0, swaps FROM {table} \
             WHERE chain = {CHAIN} AND pool_id = {pool} ORDER BY bucket"
        ))
        .fetch_all::<Candle>()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn candles_volumes_and_usd_match_hand_computed_numbers() {
    let database = TestDb::create().await;
    seed(&database).await;

    let pair = word(&pool_id_of(address(fixtures::V2_USDC_WETH)));
    let price_a = 1e15 / 2_624_963.0;

    // ---- 1m candles of the V2 pair: trade prices AND pool prices (the
    // reserves the pair reported right before each swap).
    let minutes = candles(&database, "dex_candles_1m_v", &pair).await;
    assert_eq!(minutes.len(), 3);

    let first = minutes[0];
    assert_eq!(first.0, DAY);
    assert!(close(first.1, price_a), "{first:?}");
    assert!(close(first.2, price_a));
    assert!(close(first.3, 3.8e8));
    assert!(close(first.4, 3.8e8));
    assert!(close(first.5, ratio(RESERVES_A)), "{first:?}");
    assert!(close(first.6, ratio(RESERVES_B)), "{first:?}");
    assert_eq!((first.7, first.8), (7_624_963.0, 2));

    assert_eq!(minutes[1].0, DAY + 60);
    assert_eq!((minutes[1].1, minutes[1].4), (4e8, 4e8));
    assert!(close(minutes[1].6, ratio(RESERVES_C)));
    assert_eq!(minutes[2].0, DAY + 3_660);

    // ---- 1h and 1d.
    let hours = candles(&database, "dex_candles_1h_v", &pair).await;
    assert_eq!(hours.len(), 2);
    assert_eq!(hours[0].0, DAY);
    assert!(close(hours[0].1, price_a));
    assert_eq!((hours[0].2, hours[0].3, hours[0].4), (4e8, 3.8e8, 4e8));
    assert!(close(hours[0].6, ratio(RESERVES_C)));
    assert_eq!((hours[0].7, hours[0].8), (8_624_963.0, 3));
    assert_eq!((hours[1].0, hours[1].4), (DAY + 3_600, 4.5e8));

    let days = candles(&database, "dex_candles_1d_v", &pair).await;
    assert_eq!(days.len(), 1);
    assert!(close(days[0].1, price_a));
    assert_eq!((days[0].2, days[0].3, days[0].4), (4.5e8, 3.8e8, 4.5e8));
    assert!(close(days[0].5, ratio(RESERVES_A)));
    assert!(close(days[0].6, ratio(RESERVES_D)));
    assert_eq!((days[0].7, days[0].8), (10_624_963.0, 4));

    // ---- V3: the pool price is (sqrtPriceX96 / 2^96)^2, the trade price
    // the amounts.
    let v3 = candles(
        &database,
        "dex_candles_1h_v",
        &word(&pool_id_of(address(fixtures::V3_USDC_WETH))),
    )
    .await;
    let sqrt = 1_547_521_364_678_359_767_176_169_597_843_369f64
        / 79_228_162_514_264_337_593_543_950_336f64;
    assert!(close(v3[0].6, sqrt * sqrt), "{v3:?}");
    assert!(close(
        v3[0].4,
        12_572_743_894_898_124_800f64 / 32_942_903_993f64
    ));

    // ---- decimals adjusted candle of a TRUSTED pool: pool price series.
    let adjusted = database
        .client
        .query(&format!(
            "SELECT symbol0, symbol1, price_source, ifNull(close, -1), \
             ifNull(volume0_adj, -1), ifNull(volume1_adj, -1) \
             FROM dex_pool_prices_1d_v WHERE chain = {CHAIN} \
             AND pool_id = {pair}"
        ))
        .fetch_one::<(String, String, String, f64, f64, f64)>()
        .await
        .unwrap();
    assert_eq!(
        (adjusted.0.as_str(), adjusted.1.as_str(), adjusted.2.as_str()),
        ("USDC", "WETH", "pool")
    );
    assert!(close(adjusted.3, ratio(RESERVES_D) * 1e-12), "{adjusted:?}");
    assert!(close(adjusted.4, 10.624963));
    assert!(close(adjusted.5, 0.0042));

    // ---- native price: only verified native/stable swaps of pools above
    // the volume floor vote. Hour 1: the V3 pool alone (the V2 pair traded
    // 8.6 USDC, the V4 swaps are not proven on both legs). Hour 2: nobody.
    let native = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), toUInt32(valid_from), ifNull(price, -1), \
             pools \
             FROM dex_native_price_1h_v WHERE chain = {CHAIN} ORDER BY bucket"
        ))
        .fetch_all::<(u32, u32, f64, u64)>()
        .await
        .unwrap();

    assert_eq!(native.len(), 1);
    assert_eq!(
        (native[0].0, native[0].1, native[0].3),
        (DAY, DAY + 3_600, 1)
    );
    assert!(close(native[0].2, native_price_hour_1()), "{native:?}");

    // ---- per swap USD.
    let usd = database
        .client
        .query(&format!(
            "SELECT toUInt64(block_number), ordinal, protocol, symbol_in, \
             symbol_out, toUInt8(token_in_verified), \
             toUInt8(token_out_verified), ifNull(amount_in_adj, -1), \
             ifNull(amount_usd, -1) FROM dex_swaps_usd_v \
             WHERE chain = {CHAIN} ORDER BY block_number, ordinal"
        ))
        .fetch_all::<(u64, u64, String, String, String, u8, u8, f64, f64)>(
        )
        .await
        .unwrap();

    assert_eq!(usd.len(), 11);
    let price = native_price_hour_1();

    // A: WETH in, USDC out, both proven -> the stable side: 2.624963 USD.
    assert_eq!((usd[0].0, usd[0].1), (100, 3));
    assert_eq!((usd[0].3.as_str(), usd[0].4.as_str()), ("WETH", "USDC"));
    assert_eq!((usd[0].5, usd[0].6), (1, 1));
    assert!(close(usd[0].7, 0.001));
    assert!(close(usd[0].8, 2.624963));
    // B, C: USDC in.
    assert!(close(usd[1].8, 5.0));
    assert!(close(usd[2].8, 1.0));
    // V3: WETH in, 32,942.903993 USDC out.
    assert_eq!(usd[3].2, "uniswap_v3");
    assert!(close(usd[3].8, 32_942.903993));
    // V4, first swap: nothing proves it (netted settlement) -> NULL. The
    // tokens are still KNOWN (trusted pool row), just not valued.
    assert_eq!((usd[4].3.as_str(), usd[4].4.as_str()), ("USDC", "WETH"));
    assert_eq!((usd[4].5, usd[4].6, usd[4].8), (0, 0, NULL));
    // V4, second swap: the USDT settlement proves the input.
    assert_eq!((usd[5].3.as_str(), usd[5].5, usd[5].6), ("USDT", 1, 0));
    assert!(close(usd[5].8, 1_428.368405));
    // Curve 3pool: the transfers name the coins, USDT in.
    assert_eq!((usd[6].3.as_str(), usd[6].4.as_str()), ("USDT", "USDC"));
    assert!(close(usd[6].8, 0.099206));
    // A Curve pool and a pair nobody knows, nothing proven: NULL, never 0.
    assert_eq!((usd[7].5, usd[7].8), (0, NULL));
    assert_eq!((usd[8].5, usd[8].8), (0, NULL));
    // D, next hour: 2 USDC in.
    assert!(close(usd[9].8, 2.0));
    // Balancer, next hour: a "stable" of unknown decimals in (NULL, not
    // 1e18 of anything), WETH out at the price of the hour BEFORE.
    assert_eq!(usd[10].2, "balancer_v2");
    assert_eq!((usd[10].5, usd[10].6, usd[10].7), (1, 1, NULL));
    assert!(close(usd[10].8, BALANCER_WETH_OUT * price), "{:?}", usd[10]);

    // ---- daily USD per pool = the sum of its swaps' values.
    let pool_usd = database
        .client
        .query(&format!(
            "SELECT protocol, lower(hex(pool_id)), ifNull(volume_usd, -1), \
             swaps, priced_swaps, traders FROM dex_pool_volume_usd_1d_v \
             WHERE chain = {CHAIN} ORDER BY protocol, pool_id"
        ))
        .fetch_all::<(String, String, f64, u64, u64, u64)>()
        .await
        .unwrap();

    let of = |protocol: &str, id: &str| {
        pool_usd
            .iter()
            .find(|row| row.0 == protocol && row.1.ends_with(id))
            .unwrap_or_else(|| panic!("{protocol} {id}"))
    };

    let v2 = of("uniswap_v2", &fixtures::V2_USDC_WETH[2..]);
    assert!(close(v2.2, 10.624963), "{v2:?}");
    assert_eq!((v2.3, v2.4, v2.5), (4, 4, 2));
    let unknown = of("uniswap_v2", &"99".repeat(20));
    assert_eq!((unknown.2, unknown.3, unknown.4), (NULL, 1, 0));
    let three = of("curve", &fixtures::CURVE_3POOL_EXCHANGE.address[2..]);
    assert!(close(three.2, 0.099206));
    let balancer =
        of("balancer_v2", &fixtures::BALANCER_SWAP.topics[1][2..]);
    assert!(close(balancer.2, BALANCER_WETH_OUT * price));

    let swap_total =
        usd.iter().filter(|row| row.8 >= 0.0).map(|row| row.8);
    let pool_total =
        pool_usd.iter().filter(|row| row.2 >= 0.0).map(|row| row.2);
    assert!(close(pool_total.sum::<f64>(), swap_total.sum::<f64>()));

    // ---- per protocol.
    let protocols = database
        .client
        .query(&format!(
            "SELECT protocol, ifNull(volume_usd, -1), priced_swaps, pools, \
             swaps, traders FROM dex_protocol_volume_usd_1d_v \
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
    assert_eq!(
        (protocols[1].2, protocols[1].3, protocols[1].4),
        (1, 2, 2)
    );
    assert!(close(protocols[2].1, 10.624963));
    assert_eq!((protocols[2].3, protocols[2].4), (2, 5));
    assert!(close(protocols[4].1, 1_428.368405));
    assert_eq!((protocols[4].2, protocols[4].4), (1, 2));

    // ---- per token: verified legs only.
    let usdc = database
        .client
        .query(&format!(
            "SELECT symbol, ifNull(volume_adj, -1), ifNull(volume_usd, -1), \
             swaps, pools FROM dex_token_volume_1d_v WHERE chain = {CHAIN} \
             AND token = {}",
            id(&address(fixtures::USDC))
        ))
        .fetch_one::<(String, f64, f64, u64, u64)>()
        .await
        .unwrap();

    assert_eq!(usdc.0, "USDC");
    assert!(
        close(usdc.1, 10.624963 + 32_942.903993 + 0.099112),
        "{usdc:?}"
    );
    assert!(close(usdc.2, 10.624963 + 32_942.903993 + 0.099206));
    assert_eq!((usdc.3, usdc.4), (6, 3));

    // ---- read path tables.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pools_by_token FINAL \
                 WHERE chain = {CHAIN} AND token = {}",
                id(&address(fixtures::USDC))
            ))
            .await,
        6
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
                id(&TRADER_X)
            ))
            .await,
        4
    );

    // ---- the resolver's work list: traded, contract pool, never answered.
    let missing = crate::dex::MISSING_POOLS_SQL
        .replace("{chain}", &CHAIN.to_string())
        .replace("{limit}", "100");
    let missing = database
        .client
        .query(&format!(
            "SELECT lower(hex(emitter)), protocol, attempts FROM ({missing})"
        ))
        .fetch_all::<(String, String, u32)>()
        .await
        .unwrap();
    assert_eq!(
        missing,
        vec![
            (id_hex(&"99".repeat(20)), "uniswap_v2".to_string(), 0),
            (
                id_hex(fixtures::CURVE_UNDERLYING_EXCHANGE.address),
                "curve".to_string(),
                0
            ),
        ]
    );

    database.drop().await;
}

// ------------------------------------------------------------- forgeries

/// The numbers a forgery must not move: (view, rows as text).
async fn headlines(database: &TestDb) -> Vec<(String, Vec<String>)> {
    let mut state = Vec::new();

    for (name, sql) in [
        ("native price", "SELECT * FROM dex_native_price_1h_v".to_string()),
        ("token volumes", "SELECT * FROM dex_token_volume_1d_v".to_string()),
        (
            "valued swaps",
            "SELECT chain, block_number, ordinal, amount_usd \
             FROM dex_swaps_usd_v WHERE amount_usd IS NOT NULL"
                .to_string(),
        ),
        (
            "pool usd",
            "SELECT chain, pool_id, emitter, bucket, volume_usd, priced_swaps \
             FROM dex_pool_volume_usd_1d_v WHERE volume_usd IS NOT NULL"
                .to_string(),
        ),
        (
            "protocol usd",
            "SELECT chain, protocol, bucket, volume_usd, priced_swaps \
             FROM dex_protocol_volume_usd_1d_v"
                .to_string(),
        ),
        (
            "trusted pool prices",
            "SELECT * FROM dex_pool_prices_1h_v".to_string(),
        ),
    ] {
        let rows = database
            .lines(&format!(
                "SELECT hex(toString(tuple(*))) AS line FROM ({sql}) \
                 ORDER BY line"
            ))
            .await;
        assert!(!rows.is_empty(), "{name}");
        state.push((name.to_string(), rows));
    }

    state
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn forged_events_do_not_move_any_headline() {
    let database = TestDb::create().await;
    seed(&database).await;
    let before = headlines(&database).await;

    let (usdc, weth) = usdc_weth();
    let junk_vault = Address::repeat_byte(0xb1);
    let junk_pair = Address::repeat_byte(0xb2);

    let forged = vec![
        // Path A: a Balancer shaped swap, USDC -> WETH, 1e30 for 1, from a
        // contract that is not the Vault and moved nothing.
        constructed(
            junk_vault,
            &[
                events::BALANCER_SWAP.topic0,
                B256::repeat_byte(0x0b),
                usdc.into_word(),
                weth.into_word(),
            ],
            [number(10u128.pow(30)), number(1)].concat(),
            101,
            at(60, 0),
            DAY + 70,
        ),
        // Path B: anyone announces PairCreated(USDC, WETH, junk)...
        constructed(
            Address::repeat_byte(0xf2),
            &[
                events::V2_PAIR_CREATED.topic0,
                usdc.into_word(),
                weth.into_word(),
            ],
            [junk_pair.into_word().to_vec(), number(9)].concat(),
            101,
            at(61, 0),
            DAY + 70,
        ),
        // ... and the junk contract emits a Sync and a Swap: one USDC for
        // a million WETH.
        constructed(
            junk_pair,
            &[events::V2_SYNC.topic0],
            [number(1), number(10u128.pow(24))].concat(),
            101,
            at(61, 1),
            DAY + 70,
        ),
        v2_swap(
            junk_pair,
            TRADER_X,
            [10u128.pow(12), 0, 0, 10u128.pow(24)],
            101,
            at(61, 2),
            DAY + 70,
        ),
        // A pre-announced / contradicting PairCreated for the REAL pair,
        // positioned before the real creation: (NEWTOKEN, USDC).
        constructed(
            Address::repeat_byte(0xf9),
            &[
                events::V2_PAIR_CREATED.topic0,
                Address::repeat_byte(0x01).into_word(),
                usdc.into_word(),
            ],
            [
                address(fixtures::V2_USDC_WETH).into_word().to_vec(),
                number(1),
            ]
            .concat(),
            80,
            0,
            DAY - 2_000,
        ),
    ];

    let rows = decode(CHAIN, &forged);
    assert_eq!((rows.swaps.len(), rows.pools.len()), (2, 2));
    insert_logs(&database, CHAIN, &forged, 0).await;

    let after = headlines(&database).await;
    assert_same_state(&after, &before, "with the forged rows");

    // The forgeries are IN the data - shown, never valued.
    let seen = database
        .client
        .query(&format!(
            "SELECT lower(hex(emitter)), symbol_in, \
             toUInt8(token_in_verified), ifNull(amount_usd, -1) \
             FROM dex_swaps_usd_v WHERE chain = {CHAIN} AND emitter IN \
             ({}, {}) ORDER BY emitter",
            id(&junk_vault),
            id(&junk_pair)
        ))
        .fetch_all::<(String, String, u8, f64)>()
        .await
        .unwrap();
    assert_eq!(
        seen,
        vec![
            // The event NAMES USDC: displayed as a claim, not verified.
            (id_hex(&"b1".repeat(20)), "USDC".to_string(), 0, NULL),
            // The junk pair is 'unverified': its tokens are not even shown.
            (id_hex(&"b2".repeat(20)), String::new(), 0, NULL),
        ]
    );

    // The real pair is contested by the pre-announced event - and still
    // resolved by what the pair itself answered.
    let pair = database
        .client
        .query(&format!(
            "SELECT status, lower(hex(token0)), toUInt64(created_block), \
             candidates FROM dex_pool_current_v WHERE chain = {CHAIN} AND \
             pool_id = {}",
            word(&pool_id_of(address(fixtures::V2_USDC_WETH)))
        ))
        .fetch_one::<(String, String, u64, u64)>()
        .await
        .unwrap();
    assert_eq!(
        pair,
        ("verified".to_string(), id_hex(fixtures::USDC), 90, 3)
    );

    // Even a real looking swap THROUGH the fake vault, with real transfers,
    // is not valued: the emitter is not a trusted singleton.
    let mut washed = vec![
        constructed(
            junk_vault,
            &[
                events::BALANCER_SWAP.topic0,
                B256::repeat_byte(0x0b),
                usdc.into_word(),
                weth.into_word(),
            ],
            [number(5_000_000_000), number(2_000_000_000_000_000_000)]
                .concat(),
            101,
            at(62, 0),
            DAY + 70,
        ),
        transfer(
            usdc,
            TRADER_X,
            junk_vault,
            U256::from(5_000_000_000u64),
            101,
            at(62, 1),
            DAY + 70,
        ),
        transfer(
            weth,
            junk_vault,
            TRADER_X,
            U256::from(2_000_000_000_000_000_000u64),
            101,
            at(62, 2),
            DAY + 70,
        ),
    ];
    washed = same_transaction(washed, 777);
    let rows = decode(CHAIN, &washed);
    assert_eq!(rows.swaps[0].verified_in, usdc);
    insert_logs(&database, CHAIN, &washed, 0).await;

    let after = headlines(&database).await;
    assert_same_state(&after, &before, "with a washed fake vault swap");

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn pool_metadata_is_what_the_pool_answers() {
    let database = TestDb::create().await;
    seed_reference(&database, CHAIN).await;

    let (usdc, weth) = usdc_weth();
    let pair = Address::repeat_byte(0xc1);
    let pool_id = word(&pool_id_of(pair));

    let creation = |token0: Address, token1: Address, block: u32| {
        constructed(
            Address::repeat_byte(0xf2),
            &[
                events::V2_PAIR_CREATED.topic0,
                token0.into_word(),
                token1.into_word(),
            ],
            [pair.into_word().to_vec(), number(1)].concat(),
            block,
            0,
            DAY,
        )
    };

    let current = |database: &TestDb| {
        let sql = format!(
            "SELECT status, toUInt8(trusted), lower(hex(token0)), source, \
             toUInt64(created_block), candidates FROM dex_pool_current_v \
             WHERE chain = {CHAIN} AND pool_id = {pool_id}"
        );
        let client = database.client.clone();
        async move {
            client
                .query(&sql)
                .fetch_all::<(String, u8, String, String, u64, u64)>()
                .await
                .unwrap()
        }
    };

    let usdc_hex = id_hex(fixtures::USDC);

    // 1. A PRE-ANNOUNCED forgery: V2 pair addresses are predictable, so
    //    PairCreated(NEWTOKEN, USDC, pair) can be emitted before the pair
    //    exists. Alone it is a claim: 'unverified', not trusted.
    insert_logs(
        &database,
        CHAIN,
        &[creation(Address::repeat_byte(0x01), usdc, 80)],
        0,
    )
    .await;
    assert_eq!(
        current(&database).await,
        vec![(
            "unverified".into(),
            0,
            id_hex(&"01".repeat(20)),
            "event".into(),
            80,
            1
        )]
    );

    // A swap of that pool without proof: the claimed tokens are NOT used.
    insert_logs(
        &database,
        CHAIN,
        &[v2_swap(
            pair,
            TRADER_X,
            [5_000_000, 0, 0, 2_000_000],
            95,
            0,
            DAY + 5,
        )],
        0,
    )
    .await;
    let unproven = database
        .client
        .query(&format!(
            "SELECT toUInt8(token_in_known), symbol_in, \
             ifNull(amount_usd, -1) FROM dex_swaps_usd_v WHERE chain = \
             {CHAIN} AND pool_id = {pool_id}"
        ))
        .fetch_one::<(u8, String, f64)>()
        .await
        .unwrap();
    assert_eq!(unproven, (0, String::new(), NULL));

    // 2. The real creation arrives: two token sets -> 'contested'.
    insert_logs(&database, CHAIN, &[creation(usdc, weth, 90)], 0).await;
    assert_eq!(
        current(&database).await,
        vec![(
            "contested".into(),
            0,
            id_hex(&"01".repeat(20)),
            "event".into(),
            80,
            2
        )]
    );

    // A PROVEN swap of the contested pool is valued all the same: its
    // token identity comes from the transfers, not from dex_pools.
    insert_logs(
        &database,
        CHAIN,
        &v2_trade(
            pair,
            (usdc, weth),
            TRADER_X,
            [7_000_000, 0, 0, 2_000_000_000_000_000],
            (1, 1),
            96,
            0,
            DAY + 6,
        ),
        0,
    )
    .await;
    let proven = database
        .client
        .query(&format!(
            "SELECT symbol_in, symbol_out, ifNull(amount_usd, -1) \
             FROM dex_swaps_usd_v WHERE chain = {CHAIN} AND pool_id = \
             {pool_id} AND block_number = 96"
        ))
        .fetch_one::<(String, String, f64)>()
        .await
        .unwrap();
    assert_eq!(proven, ("USDC".to_string(), "WETH".to_string(), 7.0));
    // ... but there are no decimals adjusted candles of an untrusted pool.
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pool_prices_1h_v WHERE chain = \
                 {CHAIN} AND pool_id = {pool_id}"
            ))
            .await,
        0
    );

    // 3. The pool answers token0() / token1(): that WINS, and the first
    //    event that agrees with it supplies factory / created_block.
    let mut answered = DexRows {
        pools: vec![rpc_pool(
            CHAIN,
            pair,
            Protocol::UniswapV2,
            vec![usdc, weth],
        )],
        ..DexRows::default()
    };
    answered.set_version(next_version());
    database.insert(&answered).await;
    assert_eq!(
        current(&database).await,
        vec![(
            "verified".into(),
            1,
            usdc_hex.clone(),
            "event".into(),
            90,
            3
        )]
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pool_prices_1h_v WHERE chain = \
                 {CHAIN} AND pool_id = {pool_id}"
            ))
            .await,
        1
    );

    // 4. A reorg drops BOTH creation events: the pool is still what it
    //    answered (resolver rows are chain state, no purge touches them).
    purge(&database, CHAIN, 0, 1, DAY).await;
    assert_eq!(
        current(&database).await,
        vec![("verified".into(), 1, usdc_hex.clone(), "rpc".into(), 0, 1)]
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_pools_by_token FINAL WHERE chain = \
                 {CHAIN} AND pool_id = {pool_id} AND source = 'event'"
            ))
            .await,
        0
    );

    // 5. Re-created on the canonical chain at the SAME position, in the new
    //    epoch: alive again (beats its own tombstone).
    insert_logs(&database, CHAIN, &[creation(usdc, weth, 90)], 1).await;
    assert_eq!(
        current(&database).await,
        vec![(
            "verified".into(),
            1,
            usdc_hex.clone(),
            "event".into(),
            90,
            2
        )]
    );

    // 6. A forged event can not re-tokenise an rpc resolved pool, however
    //    early it claims to be.
    insert_logs(
        &database,
        CHAIN,
        &[creation(Address::repeat_byte(0x02), usdc, 10)],
        1,
    )
    .await;
    assert_eq!(
        current(&database).await,
        vec![("verified".into(), 1, usdc_hex, "event".into(), 90, 3)]
    );

    // 7. Protocol attribution follows the POOL: a Solidly V1 fork emits the
    //    V2 swap, its pool says 'solidly'.
    let fork = Address::repeat_byte(0xc2);
    let mut solidly =
        rpc_pool(CHAIN, fork, Protocol::Solidly, vec![usdc, weth]);
    solidly.stable = true;
    let mut rows = DexRows { pools: vec![solidly], ..DexRows::default() };
    rows.set_version(next_version());
    database.insert(&rows).await;
    insert_logs(
        &database,
        CHAIN,
        &v2_trade(
            fork,
            (usdc, weth),
            TRADER_X,
            [3_000_000, 0, 0, 1_000_000_000_000_000],
            (50_000_000_000, 60_000_000_000_000_000_000),
            97,
            0,
            DAY + 7,
        ),
        1,
    )
    .await;
    let attributed = database
        .client
        .query(&format!(
            "SELECT protocol, ifNull(volume_usd, -1), swaps \
             FROM dex_protocol_volume_usd_1d_v WHERE chain = {CHAIN} \
             AND protocol = 'solidly'"
        ))
        .fetch_all::<(String, f64, u64)>()
        .await
        .unwrap();
    assert_eq!(attributed, vec![("solidly".to_string(), 3.0, 1)]);
    // ... and a STABLE pool's candle uses trades: reserves say nothing
    // about the price on the x3y + y3x curve.
    let stable = database
        .client
        .query(&format!(
            "SELECT price_source, ifNull(close, -1) FROM \
             dex_pool_prices_1h_v WHERE chain = {CHAIN} AND pool_id = {}",
            word(&pool_id_of(fork))
        ))
        .fetch_one::<(String, f64)>()
        .await
        .unwrap();
    assert_eq!(stable.0, "trades");
    assert!(close(stable.1, 1e15 / 3e6 * 1e-12));

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn native_price_is_a_median_without_look_ahead_or_stale_prices() {
    let database = TestDb::create().await;
    seed_reference(&database, CHAIN).await;

    let (usdc, weth) = usdc_weth();
    // Three honest pools around 2600 USD, one wash traded at 2 USD with
    // MORE volume than all of them together.
    let pools: [(u8, u128, u128); 4] = [
        (0xd1, 2_600_000_000, 1_000_000_000_000_000_000),
        (0xd2, 2_620_000_000, 1_000_000_000_000_000_000),
        (0xd3, 2_640_000_000, 1_000_000_000_000_000_000),
        (0xd4, 20_000_000_000, 10_000_000_000_000_000_000_000),
    ];

    let mut hour_1 = Vec::new();
    for (slot, (pool, usdc_in, weth_out)) in pools.iter().enumerate() {
        hour_1.extend(v2_trade(
            Address::repeat_byte(*pool),
            (usdc, weth),
            TRADER_X,
            [*usdc_in, 0, 0, *weth_out],
            (1, 1),
            100,
            slot as u16,
            DAY + 10,
        ));
    }
    // A dust pool far below the volume floor, at an absurd price.
    hour_1.extend(v2_trade(
        Address::repeat_byte(0xd5),
        (usdc, weth),
        TRADER_X,
        [1_000_000, 0, 0, 1_000_000_000_000_000_000_000],
        (1, 1),
        100,
        9,
        DAY + 10,
    ));
    // A WETH -> junk swap in the SAME hour: only a native leg to value it.
    let junk = Address::repeat_byte(0xe0);
    let native_only = |block: u32, timestamp: u32| {
        v2_trade(
            Address::repeat_byte(0xd6),
            (junk, weth),
            TRADER_X,
            [0, 2_000_000_000_000_000_000, 5_000, 0],
            (1, 1),
            block,
            20,
            timestamp,
        )
    };
    hour_1.extend(native_only(100, DAY + 20));
    insert_logs(&database, CHAIN, &hour_1, 0).await;

    let price = database
        .client
        .query(&format!(
            "SELECT ifNull(price, -1), pools FROM dex_native_price_1h_v \
             WHERE chain = {CHAIN}"
        ))
        .fetch_one::<(f64, u64)>()
        .await
        .unwrap();
    // The wash traded pool is ONE vote of four, the dust pool none.
    assert_eq!(price.1, 4);
    assert!((2_600.0..=2_640.0).contains(&price.0), "{price:?}");

    // Later hours: +1 h (priced), +25 h (priced: the hour ended 24 h ago),
    // +26 h (stale: NULL).
    for (block, offset) in
        [(200u32, 3_600u32), (300, 25 * 3_600), (400, 26 * 3_600)]
    {
        insert_logs(
            &database,
            CHAIN,
            &native_only(block, DAY + offset + 5),
            0,
        )
        .await;
    }

    let valued = database
        .client
        .query(&format!(
            "SELECT toUInt64(block_number), ifNull(native_price, -1), \
             ifNull(amount_usd, -1) FROM dex_swaps_usd_v WHERE chain = \
             {CHAIN} AND emitter = {} ORDER BY block_number",
            id(&Address::repeat_byte(0xd6))
        ))
        .fetch_all::<(u64, f64, f64)>()
        .await
        .unwrap();

    assert_eq!(valued.len(), 4);
    // Same hour as the price's own bucket: no look ahead.
    assert_eq!((valued[0].0, valued[0].1, valued[0].2), (100, NULL, NULL));
    assert!(close(valued[1].2, 2.0 * price.0), "{valued:?}");
    assert!(close(valued[2].2, 2.0 * price.0), "{valued:?}");
    assert_eq!((valued[3].1, valued[3].2), (NULL, NULL));

    // The hourly aggregate values the same swaps the same way.
    let rolled = database
        .client
        .query(&format!(
            "SELECT ifNull(sum(volume_usd), -1), toUInt64(sum(priced_swaps)) \
             FROM dex_pool_volume_usd_1h_v WHERE chain = {CHAIN} \
             AND emitter = {}",
            id(&Address::repeat_byte(0xd6))
        ))
        .fetch_one::<(f64, u64)>()
        .await
        .unwrap();
    assert!(close(rolled.0, 4.0 * price.0), "{rolled:?}");
    assert_eq!(rolled.1, 2);

    // A fee-on-transfer token: its leg is not proven (the Transfer says
    // more than the pair received), the WETH leg is, and values the swap.
    let fee_token = Address::repeat_byte(0xe1);
    let fot_pair = Address::repeat_byte(0xd7);
    let mut fot = vec![
        transfer(
            fee_token,
            ROUTER,
            fot_pair,
            U256::from(1_000_000u64),
            200,
            at(30, 0),
            DAY + 3_700,
        ),
        transfer(
            weth,
            fot_pair,
            TRADER_X,
            U256::from(500_000_000_000_000_000u64),
            200,
            at(30, 1),
            DAY + 3_700,
        ),
        v2_swap(
            fot_pair,
            TRADER_X,
            [990_000, 0, 0, 500_000_000_000_000_000],
            200,
            at(30, 3),
            DAY + 3_700,
        ),
    ];
    fot = same_transaction(fot, 4_242);
    insert_logs(&database, CHAIN, &fot, 0).await;
    let fot_usd = database
        .client
        .query(&format!(
            "SELECT toUInt8(token_in_verified), toUInt8(token_out_verified), \
             ifNull(amount_usd, -1) FROM dex_swaps_usd_v WHERE chain = \
             {CHAIN} AND emitter = {}",
            id(&fot_pair)
        ))
        .fetch_one::<(u8, u8, f64)>()
        .await
        .unwrap();
    assert_eq!((fot_usd.0, fot_usd.1), (0, 1));
    assert!(close(fot_usd.2, 0.5 * price.0));

    // The operator names the price sources: only they vote.
    database
        .execute(&format!(
            "INSERT INTO dex_trusted_emitters (chain, emitter, protocol, \
             price_source) VALUES ({CHAIN}, {}, 'uniswap_v2', 1), \
             ({CHAIN}, {}, 'uniswap_v2', 1), ({CHAIN}, {}, 'uniswap_v2', 1)",
            id(&Address::repeat_byte(0xd1)),
            id(&Address::repeat_byte(0xd2)),
            id(&Address::repeat_byte(0xd3)),
        ))
        .await;
    let listed = database
        .client
        .query(&format!(
            "SELECT ifNull(price, -1), pools FROM dex_native_price_1h_v \
             WHERE chain = {CHAIN}"
        ))
        .fetch_one::<(f64, u64)>()
        .await
        .unwrap();
    assert_eq!(listed, (2_620.0, 3));

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn dust_swaps_do_not_make_prices() {
    let database = TestDb::create().await;
    seed_reference(&database, CHAIN).await;

    // 10 units for 1: a "price" of 0.1 where the pool stands at 4e8.
    let dust = pair_trade(
        [10, 0, 0, 1],
        (10_000_000_000, 4_000_000_000_000_000_000),
        100,
        0,
        DAY + 10,
    );
    insert_logs(&database, CHAIN, &dust, 0).await;

    let pair = word(&pool_id_of(address(fixtures::V2_USDC_WETH)));
    let candle = candles(&database, "dex_candles_1m_v", &pair).await;

    assert_eq!(candle.len(), 1);
    // No trade price at all, the pool price from the reserves, the volume
    // counted like in dex_pool_volume_1h.
    assert_eq!((candle[0].1, candle[0].4), (NULL, NULL));
    assert_eq!((candle[0].5, candle[0].6), (4e8, 4e8));
    assert_eq!((candle[0].7, candle[0].8), (10.0, 1));

    let adjusted = database
        .client
        .query(&format!(
            "SELECT price_source, ifNull(close, -1) FROM \
             dex_pool_prices_1m_v WHERE chain = {CHAIN} AND pool_id = {pair}"
        ))
        .fetch_one::<(String, f64)>()
        .await
        .unwrap();
    assert_eq!(adjusted.0, "pool");
    assert!(close(adjusted.1, 4e-4));

    database.drop().await;
}

// ------------------------------------------------ reorgs without DELETE

/// Exclusive upper bound of every rebuild of the tests.
const REBUILD_TO: u32 = DAY + 40 * 86_400;

/// How long a read is given to catch up with an acknowledged INSERT.
///
/// ClickHouse 25.12 has no read-your-writes: measured on this build, 3 %
/// of the reads issued right after an acknowledged INSERT miss the new
/// part, and heal within milliseconds (docs/design.md §2, "No
/// read-your-writes"). Every read that decides what to write next - and
/// every final comparison - is therefore repeated until it settles.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Between two attempts of a settling read.
const RETRY: std::time::Duration = std::time::Duration::from_millis(5);

/// Tombstones `table` from `fork_block` on, and keeps re-issuing the
/// tombstone until no live row of the range is left.
///
/// One `INSERT .. SELECT` is not enough: the SELECT can miss rows that a
/// just-acknowledged INSERT wrote, and the survivors would then be
/// counted by every reader for ever. This is exactly what
/// `reorg::Purger::tombstone_until_gone` does in production, and what
/// `db::integration_tests::tombstone` does in its harness.
async fn tombstone_until_gone(
    database: &TestDb,
    table: &str,
    chain: u64,
    fork_block: u64,
) {
    let column = block_column(table);
    let mut live = format!(
        "SELECT count() FROM {table} FINAL \
         WHERE chain = {chain} AND {column} >= {fork_block}"
    );
    // The tombstone carries the same extra predicate, so the count must
    // too - otherwise a row it deliberately spares never lets it stop.
    if let Some(filter) = purge_filter(table) {
        live.push_str(&format!(" AND {filter}"));
    }

    let started = std::time::Instant::now();

    loop {
        database
            .execute(&tombstone_sql(
                table,
                chain,
                fork_block,
                None,
                next_version(),
            ))
            .await;

        let alive = database.count(&live).await;
        if alive == 0 {
            return;
        }

        assert!(
            started.elapsed() < SETTLE,
            "{table}: {alive} rows survive their tombstones"
        );
        tokio::time::sleep(RETRY).await;
    }
}

/// What `purge_range` does to the DEX tables, in its order (docs/design.md
/// §2): tombstone the base tables from `fork_block` on, record the reorg,
/// repair every aggregate under the new epoch (month by month). Only
/// INSERTs.
async fn purge(
    database: &TestDb,
    chain: u64,
    fork_block: u64,
    new_epoch: u32,
    from_ts: u32,
) {
    for table in BASE_TABLES {
        tombstone_until_gone(database, table, chain, fork_block).await;
    }

    // `to_ts`: the exclusive end of the window this repair covers. It
    // must match the range the rebuild below writes - the validity rule
    // hides exactly [from_ts, to_ts).
    database
        .execute(&format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts) VALUES \
             ({chain}, {new_epoch}, toDateTime({from_ts}), \
              toDateTime({REBUILD_TO}))"
        ))
        .await;

    for table in DEX_DERIVED {
        for statement in rebuild_statements(
            table,
            chain,
            from_ts,
            REBUILD_TO,
            new_epoch,
            // This helper emulates a purge that tombstoned nothing: the
            // statement must count every live row of the range.
            (u64::MAX, None),
        ) {
            database.execute(&statement).await;
        }
    }
}

/// Everything a reader can see of `chain`, as sorted text per source:
/// base and side tables through FINAL (without the bookkeeping columns),
/// every aggregate and analyst view as is.
async fn visible_state(
    database: &TestDb,
    chain: u64,
) -> Vec<(String, Vec<String>)> {
    let mut state = Vec::new();

    for table in BASE_TABLES.iter().chain(SIDE_TABLES) {
        let rows = database
            .lines(&format!(
                "SELECT hex(toString(tuple(* EXCEPT (_version, epoch, \
                 is_deleted)))) AS line FROM {table} FINAL \
                 WHERE chain = {chain} ORDER BY line"
            ))
            .await;
        state.push((table.to_string(), rows));
    }

    for view in READER_VIEWS {
        let rows = database
            .lines(&format!(
                "SELECT hex(toString(tuple(*))) AS line FROM {view} \
                 WHERE chain = {chain} ORDER BY line"
            ))
            .await;
        state.push((view.to_string(), rows));
    }

    state
}

type State = Vec<(String, Vec<String>)>;

/// Reads `chain` out of both databases until the two agree, then asserts
/// on the last pair read.
///
/// Either side can be short for a few milliseconds after the inserts that
/// filled it ([`SETTLE`]), so a single read of each would fail about one
/// run in ten with a row count off by one or two. Returns the settled
/// `(actual, expected)` pair, so a caller that needs either of them does
/// not read it a third time (and short) right afterwards.
async fn assert_same_state_eventually(
    database: &TestDb,
    clean: &TestDb,
    chain: u64,
    what: &str,
) -> (State, State) {
    let started = std::time::Instant::now();

    loop {
        let actual = visible_state(database, chain).await;
        let expected = visible_state(clean, chain).await;

        if actual == expected || started.elapsed() >= SETTLE {
            assert_same_state(&actual, &expected, what);
            return (actual, expected);
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// The same, against a state read earlier instead of a second database.
async fn assert_state_settles_to(
    database: &TestDb,
    chain: u64,
    expected: &[(String, Vec<String>)],
    what: &str,
) -> State {
    let started = std::time::Instant::now();

    loop {
        let actual = visible_state(database, chain).await;

        if actual == expected || started.elapsed() >= SETTLE {
            assert_same_state(&actual, expected, what);
            return actual;
        }
        tokio::time::sleep(RETRY).await;
    }
}

fn assert_same_state(
    actual: &[(String, Vec<String>)],
    expected: &[(String, Vec<String>)],
    what: &str,
) {
    assert_eq!(actual.len(), expected.len());

    for ((name, rows), (_, clean)) in actual.iter().zip(expected) {
        assert_eq!(
            rows.len(),
            clean.len(),
            "{what}: {name} has another number of rows"
        );
        assert!(rows == clean, "{what}: {name} differs");
    }
}

/// Views a consumer reads (everything except the trailing-30-days one,
/// which depends on now()).
const READER_VIEWS: &[&str] = &[
    "dex_pool_current_v",
    "dex_pools_v",
    "dex_swaps_v",
    "dex_swaps_usd_v",
    "dex_candles_1m_v",
    "dex_candles_1h_v",
    "dex_candles_1d_v",
    "dex_pool_prices_1m_v",
    "dex_pool_prices_1h_v",
    "dex_pool_prices_1d_v",
    "dex_pool_volume_1h_v",
    "dex_pool_stats_1d_v",
    "dex_native_price_1h_v",
    "dex_pool_volume_usd_1h_v",
    "dex_pool_volume_usd_1d_v",
    "dex_protocol_stats_1d_v",
    "dex_protocol_volume_usd_1d_v",
    "dex_token_volume_1d_v",
];

/// A pool announced in block `block` (a creation inside a reorged range).
fn late_pair(pair: u8, block: u32, log_index: u16) -> DatabaseLog {
    constructed(
        Address::repeat_byte(0xf2),
        &[
            events::V2_PAIR_CREATED.topic0,
            Address::repeat_byte(0x0a).into_word(),
            Address::repeat_byte(0x0b).into_word(),
        ],
        [Address::repeat_byte(pair).into_word().to_vec(), number(2)]
            .concat(),
        block,
        log_index,
        DAY + 70,
    )
}

/// The canonical blocks 101 and 102 after the reorg: FEWER swaps than the
/// orphaned ones (positions of the old fork stay dead), other amounts on
/// the position that is reused (101/3), a native/stable swap big enough to
/// set the native price, a pool creation, and a native-only swap in the
/// next hour that needs that price.
fn canonical_tail() -> Vec<DatabaseLog> {
    let mut logs = pair_trade(
        [3_000_000, 0, 0, 1_100_000_000_000_000],
        RESERVES_C,
        101,
        0,
        DAY + 70,
    );
    logs.extend(v3_real(101, 1, DAY + 70));
    logs.push(late_pair(0x97, 101, at(7, 0)));
    logs.extend(pair_trade(
        [0, 500_000_000_000_000, 1_200_000, 0],
        RESERVES_D,
        102,
        0,
        DAY + 3_700,
    ));
    logs.extend(balancer_real(102, 1, DAY + 3_700));
    logs
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reorg_leaves_exactly_a_clean_index() {
    // The index that lived through the reorg...
    let database = TestDb::create().await;
    seed(&database).await;
    insert_logs(
        &database,
        CHAIN,
        &[
            fixtures::V2_MINT.placed(101, at(8, 0), DAY + 70),
            fixtures::V3_MINT.placed(102, at(8, 0), DAY + 3_700),
            // Created on the fork that loses.
            late_pair(0x98, 101, at(9, 0)),
        ],
        0,
    )
    .await;

    let before = visible_state(&database, CHAIN).await;

    purge(&database, CHAIN, 101, 1, DAY).await;

    // Between purge and re-stream: only block 100 (swaps A and B) is left,
    // in the base tables, the side tables and the aggregates alike.
    for table in ["dex_swaps", "dex_swaps_by_pool", "dex_swaps_by_trader"]
    {
        assert_eq!(
            database
                .count(&format!(
                    "SELECT count() FROM {table} FINAL WHERE chain = {CHAIN}"
                ))
                .await,
            2,
            "{table}"
        );
    }
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_liquidity FINAL \
                 WHERE chain = {CHAIN}"
            ))
            .await,
        2,
        "the two Syncs of block 100"
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM dex_candles_1d_v \
                 WHERE chain = {CHAIN}"
            ))
            .await,
        2
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM dex_pool_volume_1h_v \
                 WHERE chain = {CHAIN}"
            ))
            .await,
        2
    );
    // Nothing was deleted: the 9 orphans are tombstones, FINAL hides them.
    assert!(
        database
            .count(&format!(
                "SELECT countIf(is_deleted = 1) FROM dex_swaps \
                 WHERE chain = {CHAIN}"
            ))
            .await
            >= 9
    );

    insert_logs(&database, CHAIN, &canonical_tail(), 1).await;

    // ... and the index that only ever saw the canonical chain.
    let clean = TestDb::create().await;
    seed_reference(&clean, CHAIN).await;
    let canonical = [block_100(), canonical_tail()].concat();
    assert_eq!(insert_logs(&clean, CHAIN, &canonical, 0).await, 6);

    let (after, expected) = assert_same_state_eventually(
        &database,
        &clean,
        CHAIN,
        "after the reorg",
    )
    .await;
    assert!(before != after);

    // Spot checks by hand: 101/3 carries the NEW amounts, the day candle
    // of the V2 pair is A, B, the new C and the new D, and the Balancer
    // swap of hour 2 is valued with hour 1's (rebuilt) native price.
    let pair = word(&pool_id_of(address(fixtures::V2_USDC_WETH)));
    let day = database
        .client
        .query(&format!(
            "SELECT volume0, volume1, swaps FROM dex_candles_1d_v \
             WHERE chain = {CHAIN} AND pool_id = {pair}"
        ))
        .fetch_one::<(f64, f64, u64)>()
        .await
        .unwrap();
    assert_eq!(
        day,
        (
            2_624_963.0 + 5e6 + 3e6 + 1_200_000.0,
            1e15 + 1.9e15 + 1.1e15 + 5e14,
            4
        )
    );
    let balancer = database
        .client
        .query(&format!(
            "SELECT ifNull(amount_usd, -1) FROM dex_swaps_usd_v \
             WHERE chain = {CHAIN} AND protocol = 'balancer_v2'"
        ))
        .fetch_one::<f64>()
        .await
        .unwrap();
    assert!(close(balancer, BALANCER_WETH_OUT * native_price_hour_1()));

    // A second reorg on top, deeper than the first (fork 100), then the
    // same canonical chain again: still a clean index.
    purge(&database, CHAIN, 100, 2, DAY).await;
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps FINAL WHERE chain = {CHAIN}"
            ))
            .await,
        0
    );
    insert_logs(&database, CHAIN, &canonical, 2).await;
    assert_state_settles_to(
        &database,
        CHAIN,
        &expected,
        "after the second reorg",
    )
    .await;

    clean.drop().await;
    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_validity_rule_keeps_old_buckets_and_later_additions() {
    let database = TestDb::create().await;
    seed_reference(&database, CHAIN).await;

    let pair = address(fixtures::V2_USDC_WETH);
    let yesterday = DAY - 86_400;
    let swap = |block: u32, timestamp: u32, usdc: u128| {
        v2_swap(
            pair,
            TRADER_X,
            [usdc, 0, 0, usdc * 1_000],
            block,
            0,
            timestamp,
        )
    };

    // Epoch 0: one swap yesterday, one today.
    insert_logs(
        &database,
        CHAIN,
        &[
            swap(50, yesterday + 5, 1_000_000),
            swap(100, DAY + 5, 2_000_000),
        ],
        0,
    )
    .await;

    // Reorg of today's block only: from_ts = start of today.
    purge(&database, CHAIN, 100, 1, DAY).await;
    insert_logs(&database, CHAIN, &[swap(100, DAY + 5, 4_000_000)], 1)
        .await;

    // A later gap heal writes an OLD block of yesterday under epoch 1:
    // it must ADD to yesterday's epoch 0 contribution, not replace it.
    insert_logs(
        &database,
        CHAIN,
        &[swap(60, yesterday + 9, 8_000_000)],
        1,
    )
    .await;

    let days = database
        .client
        .query(&format!(
            "SELECT toUInt32(bucket), volume0, swaps FROM dex_candles_1d_v \
             WHERE chain = {CHAIN} ORDER BY bucket"
        ))
        .fetch_all::<(u32, f64, u64)>()
        .await
        .unwrap();

    assert_eq!(days, vec![(yesterday, 9e6, 2), (DAY, 4e6, 1)]);

    // Open / close come from valid epochs only: a stale epoch 0 row of
    // today with another price and an earlier position must not leak.
    insert_logs(
        &database,
        CHAIN,
        &[v2_swap(
            pair,
            TRADER_X,
            [1_000_000, 0, 0, 7_000],
            99,
            0,
            DAY + 1,
        )],
        0,
    )
    .await;
    let open = database
        .client
        .query(&format!(
            "SELECT ifNull(open, -1), ifNull(close, -1), swaps \
             FROM dex_candles_1d_v WHERE chain = {CHAIN} \
             AND bucket = toDateTime({DAY}, 'UTC')"
        ))
        .fetch_one::<(f64, f64, u64)>()
        .await
        .unwrap();
    assert_eq!(open, (1_000.0, 1_000.0, 1));

    // join_use_nulls = 1 in a user profile must not hide the buckets that
    // have no reorg at or before them (yesterday).
    let with_nulls = database
        .client
        .query(&format!(
            "SELECT toUInt64(sum(swaps)) FROM dex_candles_1d_v WHERE chain = \
             {CHAIN} SETTINGS join_use_nulls = 1"
        ))
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert_eq!(with_nulls, 3);

    // Another chain is not affected by this chain's reorgs.
    seed_reference(&database, 2).await;
    insert_logs(&database, 2, &[swap(100, DAY + 5, 2_000_000)], 0).await;
    assert_eq!(
        database
            .count(
                "SELECT toUInt64(sum(swaps)) FROM dex_candles_1d_v \
                 WHERE chain = 2"
            )
            .await,
        1
    );

    database.drop().await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_rebuild_deeper_than_100_months_is_chunked() {
    let database = TestDb::create().await;
    seed_reference(&database, CHAIN).await;

    // One swap every 31 days from 2013-01-01: 130 swaps in 130 different
    // months (base tables are monthly too: insert them in small batches).
    let pair = address(fixtures::V2_USDC_WETH);
    let first: u32 = 1_356_998_400;
    let logs: Vec<DatabaseLog> = (0..130u32)
        .map(|step| {
            v2_swap(
                pair,
                TRADER_X,
                [1_000_000, 0, 0, 2_000_000],
                1_000 + step,
                0,
                first + step * 31 * 86_400,
            )
        })
        .collect();
    for batch in logs.chunks(40) {
        insert_logs(&database, CHAIN, batch, 0).await;
    }

    // One INSERT over everything is refused by ClickHouse...
    let table = &DEX_DERIVED[2];
    let whole = render_rebuild(
        table,
        CHAIN,
        first,
        REBUILD_TO,
        1,
        (u64::MAX, None),
    );
    let refused = database.client.query(&whole).execute().await;
    assert!(
        format!("{refused:?}").contains("TOO_MANY_PARTS")
            || format!("{refused:?}").contains("Too many partitions"),
        "{refused:?}"
    );

    // ... month by month it is not, and the result is complete.
    purge(&database, CHAIN, 5_000, 1, first).await;
    assert_eq!(
        database
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM dex_candles_1d_v \
                 WHERE chain = {CHAIN}"
            ))
            .await,
        130
    );

    database.drop().await;
}

const CONCURRENT_CHAINS: u64 = 8;
const REORG_ROUNDS: u32 = 10;

/// Round `round` of a chain: the block that gets orphaned (3 proven swaps)
/// and the canonical one that replaces it (1 swap, other amounts).
fn round_blocks(
    chain: u64,
    round: u32,
) -> (Vec<DatabaseLog>, Vec<DatabaseLog>) {
    let block = 1_000 + round;
    let timestamp = DAY + 60 * round + 10;
    let unit = 1_000_000 * u128::from(chain) * u128::from(round);

    let orphan = (0..3u16)
        .flat_map(|slot| {
            pair_trade(
                [unit * 7, 0, 0, unit * 3],
                RESERVES_B,
                block,
                slot,
                timestamp,
            )
        })
        .collect();

    let canonical = pair_trade(
        [unit, 0, 0, unit * 2],
        RESERVES_C,
        block,
        0,
        timestamp,
    );

    (orphan, canonical)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn eight_chains_reorg_concurrently_on_the_same_tables() {
    let database = std::sync::Arc::new(TestDb::create().await);
    let clean = TestDb::create().await;

    for chain in 1..=CONCURRENT_CHAINS {
        seed_reference(&database, chain).await;
        seed_reference(&clean, chain).await;
    }

    let mut tasks = Vec::new();

    for chain in 1..=CONCURRENT_CHAINS {
        let database = database.clone();

        tasks.push(tokio::spawn(async move {
            insert_logs(&database, chain, &swaps(), 0).await;

            for round in 1..=REORG_ROUNDS {
                let (orphan, canonical) = round_blocks(chain, round);

                insert_logs(&database, chain, &orphan, round - 1).await;
                purge(
                    &database,
                    chain,
                    u64::from(1_000 + round),
                    round,
                    DAY,
                )
                .await;
                insert_logs(&database, chain, &canonical, round).await;
            }
        }));
    }

    for task in tasks {
        task.await.unwrap();
    }

    for chain in 1..=CONCURRENT_CHAINS {
        let mut canonical = swaps();
        for round in 1..=REORG_ROUNDS {
            canonical.extend(round_blocks(chain, round).1);
        }
        insert_logs(&clean, chain, &canonical, 0).await;
    }

    for chain in 1..=CONCURRENT_CHAINS {
        assert_same_state_eventually(
            &database,
            &clean,
            chain,
            &format!("chain {chain}"),
        )
        .await;

        // By hand: 11 scenario swaps + one canonical swap per round.
        assert_eq!(
            database
                .count(&format!(
                    "SELECT count() FROM dex_swaps FINAL WHERE chain = {chain}"
                ))
                .await,
            11 + u64::from(REORG_ROUNDS)
        );

        let rounds: u64 = (1..=u64::from(REORG_ROUNDS)).sum();
        let volume = database
            .client
            .query(&format!(
                "SELECT volume0, swaps FROM dex_candles_1d_v WHERE chain = \
                 {chain} AND pool_id = {}",
                word(&pool_id_of(address(fixtures::V2_USDC_WETH)))
            ))
            .fetch_one::<(f64, u64)>()
            .await
            .unwrap();
        assert_eq!(
            volume,
            (
                10_624_963.0 + (1_000_000 * chain * rounds) as f64,
                4 + u64::from(REORG_ROUNDS)
            ),
            "chain {chain}"
        );

        // USD of the pair: the scenario's 10.624963 + the USDC input of
        // every canonical round swap, none of the orphans.
        let usd = database
            .client
            .query(&format!(
                "SELECT ifNull(sum(volume_usd), -1) FROM \
                 dex_pool_volume_usd_1d_v WHERE chain = {chain} AND \
                 pool_id = {}",
                word(&pool_id_of(address(fixtures::V2_USDC_WETH)))
            ))
            .fetch_one::<f64>()
            .await
            .unwrap();
        assert!(
            close(usd, 10.624963 + (chain * rounds) as f64),
            "chain {chain}: {usd}"
        );
    }

    // Every purge of every chain was recorded, nothing was lost.
    assert_eq!(
        database.count("SELECT count() FROM reorgs").await,
        CONCURRENT_CHAINS * u64::from(REORG_ROUNDS)
    );

    clean.drop().await;
    std::sync::Arc::try_unwrap(database)
        .unwrap_or_else(|_| panic!("tasks are done"))
        .drop()
        .await;
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
    // Int256 extremes on a V3 shaped swap (-2^255 itself is refused by the
    // decoder: it has no absolute value).
    let extreme = |index: u16| {
        constructed(
            Address::repeat_byte(0x5b),
            &[
                events::V3_SWAP.topic0,
                TRADER_X.into_word(),
                TRADER_X.into_word(),
            ],
            [
                I256::MAX.to_be_bytes::<32>().to_vec(),
                (I256::MIN + I256::ONE).to_be_bytes::<32>().to_vec(),
                number(1 << 96),
                number(1),
                number(0),
            ]
            .concat(),
            100,
            index,
            DAY,
        )
    };

    let logs = [
        // 2^256 - 1, then 10 more: a UInt256 sum would wrap to 9.
        exchange(U256::MAX, 0),
        exchange(U256::from(10u8), 1),
        extreme(2),
        extreme(3),
    ];
    assert_eq!(decode(CHAIN, &logs).swaps[0].amount_in, U256::MAX);
    assert_eq!(insert_logs(&database, CHAIN, &logs, 0).await, 4);

    let max = 1.157_920_892_373_162e77;

    let check = |database: &TestDb| {
        let client = database.client.clone();
        async move {
            let leg = client
                .query(&format!(
                    "SELECT sum(volume_in) FROM dex_pool_volume_1h_v WHERE \
                     chain = {CHAIN} AND protocol = 'curve'"
                ))
                .fetch_one::<f64>()
                .await
                .unwrap();
            assert!(close(leg, max), "{leg}");

            let candle = client
                .query(&format!(
                    "SELECT volume0, volume1 FROM dex_candles_1d_v \
                     WHERE chain = {CHAIN}"
                ))
                .fetch_one::<(f64, f64)>()
                .await
                .unwrap();
            // 2 * (2^255 - 1), twice: finite, positive, not wrapped.
            assert!(close(candle.0, max), "{candle:?}");
            assert!(close(candle.1, max), "{candle:?}");

            // The swap level view takes absolute values of Int256 too.
            let swaps = client
                .query(&format!(
                    "SELECT count() FROM dex_swaps_usd_v WHERE chain = {CHAIN}"
                ))
                .fetch_one::<u64>()
                .await
                .unwrap();
            assert_eq!(swaps, 4);
        }
    };

    check(&database).await;

    // The rebuild (another code path over the same amounts) agrees.
    let before = visible_state(&database, CHAIN).await;
    purge(&database, CHAIN, 1_000, 1, DAY).await;
    check(&database).await;
    assert_state_settles_to(
        &database,
        CHAIN,
        &before,
        "after the rebuild",
    )
    .await;

    database.drop().await;
}

// ------------------------------------------- chain neutrality (design §13)

/// Chain id reserved for Solana (docs/design.md §14).
const SVM_CHAIN: u64 = 1_399_811_149;

/// 32 bytes that are NOT an EVM address. Every one of them has a non-zero
/// byte among the first 12 (so `substring(id, 13)` would lose information)
/// AND a trailing zero byte (so `toString(FixedString)`, and the implicit
/// conversion `base58Encode(id)` performs, would silently shorten it). A
/// Solana pubkey looks exactly like this.
fn svm_id(tag: u8) -> B256 {
    let mut id = [0u8; 32];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = tag.wrapping_add(index as u8).wrapping_mul(7) | 0x11;
    }
    id[0] = tag;
    id[11] = 0xc6;
    id[31] = 0x00;
    B256::from(id)
}

/// The hex a view must return for [`svm_id`], lower case, all 32 bytes.
fn svm_hex(tag: u8) -> String {
    hex::encode(svm_id(tag))
}

/// Start of the current UTC day, so the trailing-30-days views have data.
fn today() -> u32 {
    let now =
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
            as u32;
    now - now % 86_400
}

/// A 64 byte transaction id: the length of a Solana signature, which is why
/// `tx_id` is a `String` column and never part of a sorting key.
fn svm_tx(tag: u8) -> String {
    bytes(&[tag; 64])
}

#[allow(clippy::too_many_arguments)]
fn svm_swap_sql(
    block: u64,
    timestamp: u32,
    tx_index: u32,
    ordinal: u64,
    amount0: &str,
    amount1: &str,
    amount_in: &str,
    amount_out: &str,
    verified_in: Option<u8>,
    verified_out: Option<u8>,
    version: u64,
) -> String {
    let zero = "toFixedString('', 32)".to_string();
    let leg = |tag: Option<u8>| {
        tag.map_or_else(
            || zero.clone(),
            |tag| bytes(svm_id(tag).as_slice()),
        )
    };

    format!(
        "({SVM_CHAIN}, {block}, {timestamp}, {}, {tx_index}, {ordinal}, {}, \
         {}, 'uniswap_v2', {}, {}, {}, {}, {}, toInt256('{amount0}'), \
         toInt256('{amount1}'), {zero}, {zero}, toUInt256('{amount_in}'), \
         toUInt256('{amount_out}'), {}, {}, toUInt256('7000000000'), \
         toUInt256('9000000'), 0, 0, false, toUInt256('0'), toUInt256('0'), \
         0, 0, 0, {version})",
        svm_tx(tx_index as u8),
        bytes(svm_id(0xa1).as_slice()),
        bytes(svm_id(0xe1).as_slice()),
        bytes(svm_id(0x11).as_slice()),
        bytes(svm_id(0x22).as_slice()),
        bytes(svm_id(0x33).as_slice()),
        bytes(svm_id(0x44).as_slice()),
        bytes(svm_id(0x33).as_slice()),
        leg(verified_in),
        leg(verified_out),
    )
}

/// A 32 byte id that is not an EVM address survives every `dex_*` table,
/// materialized view, aggregate and analyst view byte for byte - nothing
/// truncates it to 20 bytes, and nothing matches it against an EVM address
/// that shares its last 20 bytes.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_non_evm_id_survives_every_table_and_view() {
    let database = TestDb::create().await;

    let hour0 = today();
    let hour1 = hour0 + 3_600;

    database
        .execute(&format!(
            "INSERT INTO chains (chain, name, family) VALUES \
             ({SVM_CHAIN}, 'solana', 'svm'), ({CHAIN}, 'ethereum', 'evm')"
        ))
        .await;

    // native = svm_id(0x71) with 9 decimals, stable = svm_id(0x51) with 6.
    database
        .execute(&format!(
            "INSERT INTO quote_tokens (chain, token, kind, decimals, \
             symbol, _version) VALUES ({SVM_CHAIN}, {}, 'native', 9, \
             'SOL', 1), ({SVM_CHAIN}, {}, 'stable', 6, 'USDC', 1)",
            bytes(svm_id(0x71).as_slice()),
            bytes(svm_id(0x51).as_slice()),
        ))
        .await;

    // THE trap: an EVM `tokens` row whose 20 byte address is exactly the
    // last 20 bytes of the Solana stable token. A view that truncated the
    // analytics side with substring(token, 13) would join these two.
    let tail_hex = svm_hex(0x51)[24..].to_string();
    database
        .execute(&format!(
            "INSERT INTO tokens (chain, address, name, symbol, decimals, \
             type, _version) VALUES ({SVM_CHAIN}, unhex('{tail_hex}'), \
             'Impostor', 'FAKE', 18, 'ERC20', 1)"
        ))
        .await;

    // A resolved pool: trusted, so the metadata dependent views run too.
    database
        .execute(&format!(
            "INSERT INTO dex_pools ({POOL_COLUMNS}) VALUES \
             ({SVM_CHAIN}, {pool}, {emitter}, {factory}, 'uniswap_v2', \
             {native}, {stable}, [{native}, {stable}], [], 0, 0, \
             toFixedString('', 32), false, 0, 0, '', 0, 0, 'rpc', 0, 0, 1)",
            pool = bytes(svm_id(0xa1).as_slice()),
            emitter = bytes(svm_id(0xe1).as_slice()),
            factory = bytes(svm_id(0xf1).as_slice()),
            native = bytes(svm_id(0x71).as_slice()),
            stable = bytes(svm_id(0x51).as_slice()),
        ))
        .await;

    database
        .execute(&format!(
            "INSERT INTO dex_liquidity (chain, block_number, timestamp, \
             tx_id, tx_index, ordinal, pool_id, emitter, protocol, kind, \
             sender, owner, tx_from, tx_to, amount0, amount1, reserve0, \
             reserve1, liquidity_delta, tick_lower, tick_upper, epoch, \
             _version) VALUES ({SVM_CHAIN}, 10, {hour0}, {}, 1, 5, {}, {}, \
             'uniswap_v2', 'mint', {}, {}, {}, {}, toInt256('5'), \
             toInt256('7'), toUInt256('0'), toUInt256('0'), toInt256('0'), \
             0, 0, 0, {})",
            svm_tx(1),
            bytes(svm_id(0xa1).as_slice()),
            bytes(svm_id(0xe1).as_slice()),
            bytes(svm_id(0x11).as_slice()),
            bytes(svm_id(0x55).as_slice()),
            bytes(svm_id(0x33).as_slice()),
            bytes(svm_id(0x44).as_slice()),
            next_version(),
        ))
        .await;

    // hour0: 2.0 native for 4000 stable -> the hour's native price is 2000.
    // hour1: 1.0 native for 2000 stable, valued by its stable leg, plus one
    // swap whose in leg is NOT verified (the 32 zero bytes id).
    let version = next_version();
    database
        .execute(&format!(
            "INSERT INTO dex_swaps ({SWAP_COLUMNS}) VALUES {}, {}, {}",
            svm_swap_sql(
                10,
                hour0,
                1,
                7,
                "2000000000",
                "-4000000000",
                "2000000000",
                "4000000000",
                Some(0x71),
                Some(0x51),
                version,
            ),
            svm_swap_sql(
                20,
                hour1,
                2,
                3,
                "1000000000",
                "-2000000000",
                "1000000000",
                "2000000000",
                Some(0x71),
                Some(0x51),
                version,
            ),
            svm_swap_sql(
                20,
                hour1,
                2,
                9,
                "3000",
                "-6000",
                "3000",
                "6000",
                None,
                Some(0x51),
                version,
            ),
        ))
        .await;

    // ---- base tables and the side tables their materialized views feed.
    assert_eq!(
        database
            .lines(&format!(
                "SELECT lower(hex(pool_id)) FROM dex_pools FINAL \
                 WHERE chain = {SVM_CHAIN}"
            ))
            .await,
        vec![svm_hex(0xa1)]
    );

    for (table, column, tag) in [
        ("dex_swaps", "emitter", 0xe1u8),
        ("dex_swaps", "trader", 0x33),
        ("dex_swaps", "sender", 0x11),
        ("dex_swaps", "recipient", 0x22),
        ("dex_swaps", "tx_from", 0x33),
        ("dex_swaps", "tx_to", 0x44),
        ("dex_liquidity", "owner", 0x55),
        ("dex_swaps_by_pool", "verified_out", 0x51),
        ("dex_swaps_by_trader", "trader", 0x33),
        ("dex_pools_by_token", "emitter", 0xe1),
    ] {
        let seen = database
            .lines(&format!(
                "SELECT DISTINCT lower(hex({column})) AS id FROM {table} \
                 FINAL WHERE chain = {SVM_CHAIN} ORDER BY id"
            ))
            .await;
        assert!(
            seen.contains(&svm_hex(tag)),
            "{table}.{column}: {seen:?}"
        );
        assert!(
            seen.iter().all(|value| value.len() == 64),
            "{table}.{column} was truncated: {seen:?}"
        );
    }

    // Both tokens of the pool reached dex_pools_by_token, whole.
    let mut tokens = database
        .lines(&format!(
            "SELECT lower(hex(token)) AS id FROM dex_pools_by_token FINAL \
             WHERE chain = {SVM_CHAIN} ORDER BY id"
        ))
        .await;
    tokens.sort();
    let mut both = vec![svm_hex(0x51), svm_hex(0x71)];
    both.sort();
    assert_eq!(tokens, both);

    // A 64 byte transaction id round trips, and the position columns too.
    let position = database
        .client
        .query(&format!(
            "SELECT toUInt64(length(tx_id)), lower(hex(tx_id)), tx_index, \
             ordinal FROM dex_swaps FINAL WHERE chain = {SVM_CHAIN} \
             AND block_number = 10"
        ))
        .fetch_one::<(u64, String, u32, u64)>()
        .await
        .unwrap();
    assert_eq!((position.0, position.2, position.3), (64, 1, 7));
    assert_eq!(position.1, "01".repeat(64));

    // ---- the impostor must NOT be joined: padding, never truncation.
    let info = database
        .client
        .query(&format!(
            "SELECT symbol, ifNull(decimals, 255), kind FROM \
             dex_token_info_v WHERE chain = {SVM_CHAIN} AND token = {}",
            bytes(svm_id(0x51).as_slice())
        ))
        .fetch_one::<(String, u8, String)>()
        .await
        .unwrap();
    assert_eq!(info, ("USDC".to_string(), 6, "stable".to_string()));

    // The EVM row is still there, under its own PADDED 32 byte id.
    let impostor = database
        .client
        .query(&format!(
            "SELECT symbol FROM dex_token_info_v WHERE chain = \
             {SVM_CHAIN} AND token = unhex('{}')",
            id_hex(&tail_hex)
        ))
        .fetch_one::<String>()
        .await
        .unwrap();
    assert_eq!(impostor, "FAKE");

    // ---- pools, candles, volumes, USD: ids intact and numbers right.
    let pool = database
        .client
        .query(&format!(
            "SELECT status, toUInt8(trusted), lower(hex(pool_id)), \
             lower(hex(emitter)), lower(hex(factory)), lower(hex(token0)), \
             lower(hex(token1)) FROM dex_pools_v WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, u8, String, String, String, String, String)>()
        .await
        .unwrap();
    assert_eq!((pool.0.as_str(), pool.1), ("verified", 1));
    assert_eq!(pool.2, svm_hex(0xa1));
    assert_eq!(pool.3, svm_hex(0xe1));
    assert_eq!(pool.4, svm_hex(0xf1));
    assert_eq!(pool.5, svm_hex(0x71));
    assert_eq!(pool.6, svm_hex(0x51));

    let shown = database
        .client
        .query(&format!(
            "SELECT symbol0, symbol1, pool FROM dex_pools_v \
             WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, String, String)>()
        .await
        .unwrap();
    assert_eq!((shown.0.as_str(), shown.1.as_str()), ("SOL", "USDC"));
    // A pool id is not an address: all 32 bytes are printed.
    assert_eq!(shown.2, format!("0x{}", svm_hex(0xa1)));

    let candles = database
        .client
        .query(&format!(
            "SELECT lower(hex(pool_id)), lower(hex(emitter)), \
             toUInt64(trades), toUInt64(swaps), toUInt64(traders) \
             FROM dex_candles_1d_v WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, String, u64, u64, u64)>()
        .await
        .unwrap();
    assert_eq!(candles.0, svm_hex(0xa1));
    assert_eq!(candles.1, svm_hex(0xe1));
    assert_eq!((candles.2, candles.3, candles.4), (3, 3, 1));

    let volume = database
        .client
        .query(&format!(
            "SELECT lower(hex(token_in)), lower(hex(token_out)), \
             toUInt64(swaps) FROM dex_pool_volume_1h_v WHERE chain = \
             {SVM_CHAIN} ORDER BY token_in, bucket"
        ))
        .fetch_all::<(String, String, u64)>()
        .await
        .unwrap();
    // The unverified in leg is the 32 ZERO bytes, not 20.
    assert_eq!(
        volume,
        vec![
            ("00".repeat(32), svm_hex(0x51), 1),
            (svm_hex(0x71), svm_hex(0x51), 1),
            (svm_hex(0x71), svm_hex(0x51), 1),
        ]
    );

    let price = database
        .client
        .query(&format!(
            "SELECT toUInt64(bucket), ifNull(price, -1), toUInt64(pools) \
             FROM dex_native_price_1h_v WHERE chain = {SVM_CHAIN} \
             ORDER BY bucket"
        ))
        .fetch_all::<(u64, f64, u64)>()
        .await
        .unwrap();
    assert_eq!(price[0].0, u64::from(hour0));
    assert!(close(price[0].1, 2000.0), "{price:?}");

    let usd = database
        .client
        .query(&format!(
            "SELECT toUInt64(block_number), tx_index, ordinal, \
             lower(hex(token_in)), lower(hex(token_out)), \
             toUInt8(token_in_verified), ifNull(amount_usd, -1) \
             FROM dex_swaps_usd_v WHERE chain = {SVM_CHAIN} \
             ORDER BY block_number, tx_index, ordinal"
        ))
        .fetch_all::<(u64, u32, u64, String, String, u8, f64)>()
        .await
        .unwrap();
    assert_eq!(usd.len(), 3);
    assert_eq!((usd[0].0, usd[0].1, usd[0].2), (10, 1, 7));
    assert_eq!(usd[0].3, svm_hex(0x71));
    assert_eq!(usd[0].4, svm_hex(0x51));
    assert!(close(usd[0].6, 4000.0), "{usd:?}");
    // The unverified leg falls back to the TRUSTED pool's token0.
    assert_eq!((usd[2].0, usd[2].1, usd[2].2), (20, 2, 9));
    assert_eq!(usd[2].3, svm_hex(0x71));
    assert_eq!(usd[2].5, 0);

    let daily = database
        .client
        .query(&format!(
            "SELECT lower(hex(pool_id)), ifNull(volume_usd, -1), \
             toUInt64(swaps) FROM dex_pool_volume_usd_1d_v \
             WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, f64, u64)>()
        .await
        .unwrap();
    assert_eq!(daily.0, svm_hex(0xa1));
    assert_eq!(daily.2, 3);
    assert!(close(daily.1, 4000.0 + 2000.0 + 0.006), "{daily:?}");

    let by_token = database
        .client
        .query(&format!(
            "SELECT lower(hex(token)) AS id, symbol, toUInt64(swaps) FROM \
             dex_token_volume_1d_v WHERE chain = {SVM_CHAIN} ORDER BY id"
        ))
        .fetch_all::<(String, String, u64)>()
        .await
        .unwrap();
    let mut seen: Vec<String> =
        by_token.iter().map(|row| row.0.clone()).collect();
    seen.sort();
    assert_eq!(seen, both);

    let prices = database
        .client
        .query(&format!(
            "SELECT lower(hex(pool_id)), lower(hex(token0)), \
             lower(hex(token1)), price_source FROM dex_pool_prices_1h_v \
             WHERE chain = {SVM_CHAIN} ORDER BY bucket"
        ))
        .fetch_all::<(String, String, String, String)>()
        .await
        .unwrap();
    assert!(!prices.is_empty());
    for row in &prices {
        assert_eq!(row.0, svm_hex(0xa1));
        assert_eq!(row.1, svm_hex(0x71));
        assert_eq!(row.2, svm_hex(0x51));
    }

    let top = database
        .client
        .query(&format!(
            "SELECT lower(hex(pool_id)), pool, symbol0, symbol1 FROM \
             dex_top_pools_v WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, String, String, String)>()
        .await
        .unwrap();
    assert_eq!(top.0, svm_hex(0xa1));
    assert_eq!(top.1, format!("0x{}", svm_hex(0xa1)));
    assert_eq!((top.2.as_str(), top.3.as_str()), ("SOL", "USDC"));

    let protocols = database
        .client
        .query(&format!(
            "SELECT protocol, toUInt64(swaps), toUInt64(pools) FROM \
             dex_protocol_stats_1d_v WHERE chain = {SVM_CHAIN}"
        ))
        .fetch_one::<(String, u64, u64)>()
        .await
        .unwrap();
    assert_eq!(protocols, ("uniswap_v2".to_string(), 3, 1));

    // ---- the documented per family formatting, from migration 0006.
    let printed = database
        .lines(&format!(
            "SELECT if(c.family = 'svm', \
             base58Encode(substring(s.emitter, 1, 32)), \
             concat('0x', lower(hex(substring(s.emitter, 13))))) AS text \
             FROM dex_swaps AS s FINAL \
             LEFT JOIN chains_v AS c ON c.chain = s.chain \
             WHERE s.chain = {SVM_CHAIN} GROUP BY text"
        ))
        .await;
    assert_eq!(printed.len(), 1, "{printed:?}");
    // base58 of the WHOLE 32 bytes: decoding gives every byte back. The
    // shape solana-research sketched, base58Encode(id), would not.
    assert_eq!(
        database
            .lines(&format!(
                "SELECT lower(hex(base58Decode('{}'))) AS id",
                printed[0]
            ))
            .await,
        vec![svm_hex(0xe1)]
    );
    assert_ne!(
        database
            .lines(&format!(
                "SELECT base58Encode(toString(emitter)) AS id FROM \
                 dex_swaps FINAL WHERE chain = {SVM_CHAIN} GROUP BY id"
            ))
            .await,
        printed,
        "toString(FixedString) must be shown to lose the trailing zero"
    );

    database.drop().await;
}

/// Two chains of different families may hold ids that agree on their last
/// 20 bytes. Nothing joins, aggregates or purges them together.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn an_evm_address_and_a_pubkey_sharing_20_bytes_never_collide() {
    let database = TestDb::create().await;

    let hour = today();

    // EVM addresses that are the tails of the Solana emitter and trader:
    // the ids agree on 20 bytes and differ only in the padding.
    let tail = Address::from_slice(&svm_id(0xe1).as_slice()[12..]);
    let tail_trader = Address::from_slice(&svm_id(0x33).as_slice()[12..]);
    assert_eq!(pool_id_of(tail).as_slice()[12..], svm_id(0xe1)[12..]);
    assert_ne!(pool_id_of(tail), svm_id(0xe1));

    database
        .execute(&format!(
            "INSERT INTO dex_swaps ({SWAP_COLUMNS}) VALUES {}",
            svm_swap_sql(
                10,
                hour,
                1,
                7,
                "2000000000",
                "-4000000000",
                "2000000000",
                "4000000000",
                Some(0x71),
                Some(0x51),
                next_version(),
            ),
        ))
        .await;

    let mut evm = DexRows::default();
    let mut swap =
        decode(CHAIN, &[fixtures::V2_SWAP.log()]).swaps.remove(0);
    swap.emitter = tail;
    swap.trader = tail_trader;
    swap.timestamp = hour;
    evm.swaps.push(swap);
    evm.set_version(next_version());
    database.insert(&evm).await;

    // Each chain sees exactly its own row, under its own 32 byte id.
    for (chain, emitter) in [
        (SVM_CHAIN, svm_hex(0xe1)),
        (CHAIN, hex::encode(pool_id_of(tail))),
    ] {
        assert_eq!(
            database
                .count(&format!(
                    "SELECT count() FROM dex_swaps FINAL WHERE chain = \
                     {chain} AND lower(hex(emitter)) = '{emitter}'"
                ))
                .await,
            1,
            "chain {chain}"
        );
    }

    // The two ids differ in the padding alone, and the id-keyed side table
    // keeps them apart.
    let traders = database
        .lines(
            "SELECT DISTINCT lower(hex(trader)) AS id \
             FROM dex_swaps_by_trader FINAL ORDER BY id",
        )
        .await;
    assert_eq!(traders.len(), 2, "{traders:?}");
    assert!(traders.contains(&svm_hex(0x33)), "{traders:?}");
    assert!(
        traders.contains(&hex::encode(pool_id_of(tail_trader))),
        "{traders:?}"
    );

    // A purge of one chain leaves the other alone.
    database
        .execute(&tombstone_sql(
            "dex_swaps",
            CHAIN,
            0,
            None,
            next_version(),
        ))
        .await;
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps FINAL WHERE chain = \
                 {SVM_CHAIN}"
            ))
            .await,
        1
    );
    assert_eq!(
        database
            .count(&format!(
                "SELECT count() FROM dex_swaps FINAL WHERE chain = {CHAIN}"
            ))
            .await,
        0
    );

    database.drop().await;
}
