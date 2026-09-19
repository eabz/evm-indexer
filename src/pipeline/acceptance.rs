//! THE PROOF: the real pipeline (`run_with`, the same function the binary
//! calls) against an in-memory chain and a REAL ClickHouse.
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@127.0.0.1:8123/pipeline_test \
//!   cargo test --lib pipeline::acceptance -- --ignored
//! ```
//!
//! The database name in the url is only a PREFIX: every test owns (drops
//! and migrates) its own databases, `<prefix>_<scenario>_test`, so the
//! tests run in parallel without sharing a table. The prefix must end in
//! `_test`.
//!
//! The configuration of every scenario comes out of the real command line
//! parser, so "zero flags" means zero flags.

use super::*;
use crate::{
    configs::Command,
    db::{self, migrate, next_version, DatabaseParams, FlushKey},
    dex,
    pipeline::{backfill, modules::ALL_MODULES, verify},
    tokens::{
        discovery::{
            build_caller_with, CallerOptions, ChainRegistry, Connector,
        },
        multicall::testing::{FakeChain, FakeToken},
    },
    utils::events::TRANSFER_EVENT_SIGNATURE,
};
use alloy::primitives::{Address, B256, I256, U256};
use clickhouse::Client;
use hypersync_client::{
    format::{
        Address as HsAddress, Data, Hash, LogArgument, Quantity,
        TransactionStatus, UInt,
    },
    simple_types::{Block, Log, Transaction},
};
use std::{collections::BTreeMap, sync::atomic::AtomicBool};
use tokio::sync::mpsc;

const CHAIN: u64 = 1;
const TOKEN: &str = "00000000-0000-0000-0000-000000000000";
const BASE_TIMESTAMP: u32 = 1_700_000_000;

const TOKEN0: Address = Address::repeat_byte(0xa0);
const TOKEN1: Address = Address::repeat_byte(0xa1);
const V2_FACTORY: Address = Address::repeat_byte(0xf2);
const V3_FACTORY: Address = Address::repeat_byte(0xf3);
const V2_PAIR: Address = Address::repeat_byte(0xb2);
const V3_POOL: Address = Address::repeat_byte(0xb3);
const ROUTER: Address = Address::repeat_byte(0xc0);
const TRADER: Address = Address::repeat_byte(0xd0);

// ------------------------------------------------------------ the chain

#[derive(Debug, Clone)]
struct TestLog {
    address: Address,
    topics: Vec<B256>,
    data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct TestTx {
    from: Address,
    to: Address,
    logs: Vec<TestLog>,
}

#[derive(Debug, Clone)]
struct TestBlock {
    number: u64,
    hash: B256,
    parent_hash: B256,
    timestamp: u32,
    txs: Vec<TestTx>,
}

fn word(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

fn signed_word(value: i64) -> [u8; 32] {
    I256::try_from(value).unwrap().to_be_bytes::<32>()
}

fn topic(address: Address) -> B256 {
    address.into_word()
}

fn transfer(
    token: Address,
    from: Address,
    to: Address,
    amount: u64,
) -> TestLog {
    TestLog {
        address: token,
        topics: vec![TRANSFER_EVENT_SIGNATURE, topic(from), topic(to)],
        data: word(amount).to_vec(),
    }
}

/// Pool creations: one Uniswap V2 pair, one Uniswap V3 pool.
fn creations() -> TestTx {
    let mut pair = topic(V2_PAIR).to_vec();
    pair.extend_from_slice(&word(1));

    let mut pool = signed_word(60).to_vec();
    pool.extend_from_slice(topic(V3_POOL).as_slice());

    TestTx {
        from: TRADER,
        to: V2_FACTORY,
        logs: vec![
            TestLog {
                address: V2_FACTORY,
                topics: vec![
                    dex::events::V2_PAIR_CREATED.topic0,
                    topic(TOKEN0),
                    topic(TOKEN1),
                ],
                data: pair,
            },
            TestLog {
                address: V3_FACTORY,
                topics: vec![
                    dex::events::V3_POOL_CREATED.topic0,
                    topic(TOKEN0),
                    topic(TOKEN1),
                    B256::from(word(3_000)),
                ],
                data: pool,
            },
        ],
    }
}

/// A V2 swap as a router produces it: token in, Sync, Swap, token out.
/// Amounts are small integers, so every Float64 sum is exact whatever the
/// order of addition (the comparisons below are exact).
fn v2_swap(amount_in: u64, amount_out: u64) -> TestTx {
    let mut sync = word(1_000_000 + amount_in).to_vec();
    sync.extend_from_slice(&word(2_000_000 - amount_out));

    let mut swap = word(amount_in).to_vec();
    swap.extend_from_slice(&word(0));
    swap.extend_from_slice(&word(0));
    swap.extend_from_slice(&word(amount_out));

    TestTx {
        from: TRADER,
        to: ROUTER,
        logs: vec![
            transfer(TOKEN0, TRADER, V2_PAIR, amount_in),
            TestLog {
                address: V2_PAIR,
                topics: vec![dex::events::V2_SYNC.topic0],
                data: sync,
            },
            TestLog {
                address: V2_PAIR,
                topics: vec![
                    dex::events::V2_SWAP.topic0,
                    topic(ROUTER),
                    topic(TRADER),
                ],
                data: swap,
            },
            transfer(TOKEN1, V2_PAIR, TRADER, amount_out),
        ],
    }
}

fn v3_swap(amount_in: i64, amount_out: i64) -> TestTx {
    let mut swap = signed_word(amount_in).to_vec();
    swap.extend_from_slice(&signed_word(-amount_out));
    // sqrtPriceX96 = 2^96 (price 1), liquidity, tick.
    let price_one: U256 = U256::from(1u8) << 96;
    swap.extend_from_slice(&price_one.to_be_bytes::<32>());
    swap.extend_from_slice(&word(5_000_000));
    swap.extend_from_slice(&signed_word(0));

    TestTx {
        from: TRADER,
        to: ROUTER,
        logs: vec![
            transfer(TOKEN0, TRADER, V3_POOL, amount_in as u64),
            TestLog {
                address: V3_POOL,
                topics: vec![
                    dex::events::V3_SWAP.topic0,
                    topic(ROUTER),
                    topic(TRADER),
                ],
                data: swap,
            },
            transfer(TOKEN1, V3_POOL, TRADER, amount_out as u64),
        ],
    }
}

/// What block `number` holds on the ORIGINAL chain.
fn busy_block(number: u64) -> Vec<TestTx> {
    match number {
        0 => vec![],
        1 => vec![creations()],
        n => {
            let mut txs = vec![TestTx {
                from: TRADER,
                to: TOKEN0,
                logs: vec![transfer(TOKEN0, TRADER, ROUTER, n)],
            }];
            txs.push(v2_swap(100 * n, 50 * n));
            if n % 2 == 0 {
                txs.push(v3_swap(10 * n as i64, 9 * n as i64));
            }
            txs
        }
    }
}

/// The same height after a reorg: FEWER swaps (no V3 swap, another V2
/// amount), so orphan keys must die and aggregates must go DOWN.
fn quiet_block(number: u64) -> Vec<TestTx> {
    vec![v2_swap(7 * number, 3 * number)]
}

#[derive(Clone)]
struct TestChain {
    blocks: Arc<Mutex<Vec<TestBlock>>>,
    blocks_per_response: u64,
    block_seconds: u32,
}

impl TestChain {
    /// 12 s blocks: the whole chain within one UTC day.
    fn new(length: u64) -> Self {
        Self::with_block_time(length, 12)
    }

    fn with_block_time(length: u64, block_seconds: u32) -> Self {
        let chain = Self {
            blocks: Arc::default(),
            blocks_per_response: 4,
            block_seconds,
        };
        chain.extend(length, 0, busy_block);
        chain
    }

    fn head(&self) -> u64 {
        self.blocks.lock().unwrap().len() as u64
    }

    /// Appends `count` blocks. `salt` makes the hashes of a fork differ.
    fn extend(
        &self,
        count: u64,
        salt: u8,
        content: fn(u64) -> Vec<TestTx>,
    ) {
        let mut blocks = self.blocks.lock().unwrap();

        for _ in 0..count {
            let number = blocks.len() as u64;
            let mut hash = [salt; 32];
            hash[..8].copy_from_slice(&number.to_be_bytes());
            hash[31] = 1;

            let parent_hash =
                blocks.last().map_or(B256::ZERO, |parent| parent.hash);

            blocks.push(TestBlock {
                number,
                hash: B256::from(hash),
                parent_hash,
                timestamp: BASE_TIMESTAMP
                    + number as u32 * self.block_seconds,
                txs: content(number),
            });
        }
    }

    /// Replaces the newest `depth` blocks by quiet ones and grows the new
    /// fork by `extra` blocks.
    fn reorg(&self, depth: u64, extra: u64) {
        {
            let mut blocks = self.blocks.lock().unwrap();
            let keep = blocks.len() - depth as usize;
            blocks.truncate(keep);
        }
        self.extend(depth + extra, 0x77, quiet_block);
    }

    fn response(&self, range: BlockRange) -> ResponseRows {
        let blocks = self.blocks.lock().unwrap();
        let mut data = ResponseRows::default();

        for block in &blocks[range.from as usize..range.to as usize] {
            data.blocks.push(vec![Block {
                number: Some(block.number),
                hash: Some(Hash::from(block.hash.0)),
                parent_hash: Some(Hash::from(block.parent_hash.0)),
                timestamp: Some(Quantity::from(u64::from(
                    block.timestamp,
                ))),
                gas_used: Some(Quantity::from(21_000u64)),
                gas_limit: Some(Quantity::from(30_000_000u64)),
                size: Some(Quantity::from(1_000u64)),
                ..Default::default()
            }]);

            let mut log_index = 0u64;

            for (index, tx) in block.txs.iter().enumerate() {
                let mut hash = block.hash.0;
                hash[30] = index as u8 + 1;
                hash[31] = 2;

                data.transactions.push(vec![Transaction {
                    block_number: Some(UInt::from(block.number)),
                    block_hash: Some(Hash::from(block.hash.0)),
                    transaction_index: Some(UInt::from(index as u64)),
                    hash: Some(Hash::from(hash)),
                    from: Some(HsAddress::from(tx.from.0 .0)),
                    to: Some(HsAddress::from(tx.to.0 .0)),
                    gas: Some(Quantity::from(100_000u64)),
                    gas_used: Some(Quantity::from(21_000u64)),
                    gas_price: Some(Quantity::from(9u64)),
                    effective_gas_price: Some(Quantity::from(9u64)),
                    status: Some(TransactionStatus::Success),
                    ..Default::default()
                }]);

                for log in &tx.logs {
                    let mut row = Log {
                        block_number: Some(UInt::from(block.number)),
                        log_index: Some(UInt::from(log_index)),
                        transaction_index: Some(UInt::from(index as u64)),
                        transaction_hash: Some(Hash::from(hash)),
                        address: Some(HsAddress::from(log.address.0 .0)),
                        data: Some(Data::from(log.data.clone())),
                        ..Default::default()
                    };
                    for topic in &log.topics {
                        row.topics.push(Some(LogArgument::from(topic.0)));
                    }
                    data.logs.push(vec![row]);
                    log_index += 1;
                }
            }
        }

        data
    }
}

impl BlockSource for TestChain {
    async fn head(&self) -> Result<u64> {
        Ok(TestChain::head(self))
    }

    async fn stream(
        &self,
        range: BlockRange,
    ) -> Result<Receiver<Result<SourceResponse>>> {
        let (tx, rx) = mpsc::channel(2);
        let chain = self.clone();

        tokio::spawn(async move {
            let mut from = range.from;
            while from < range.to {
                let next_block =
                    (from + chain.blocks_per_response).min(range.to);

                // The chain got shorter under the stream.
                if next_block > chain.head() {
                    let _ = tx
                        .send(Err(anyhow::anyhow!("block not found")))
                        .await;
                    return;
                }

                let response = SourceResponse {
                    next_block,
                    data: chain
                        .response(BlockRange::new(from, next_block)),
                    rollback_guard: None,
                };
                if tx.send(Ok(response)).await.is_err() {
                    return;
                }
                from = next_block;
            }
        });

        Ok(rx)
    }
}

impl CanonicalChain for TestChain {
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, Result<Vec<BlockHeader>>> {
        Box::pin(async move {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .iter()
                .filter(|block| (from..to).contains(&block.number))
                .map(|block| BlockHeader {
                    number: block.number,
                    hash: block.hash,
                    parent_hash: block.parent_hash,
                    timestamp: block.timestamp,
                })
                .collect())
        })
    }
}

// ------------------------------------------------------------ the fake RPC

/// Stands in for `https://chainid.network/chains.json`.
struct FakeRegistry;

impl ChainRegistry for FakeRegistry {
    fn rpc_urls(&self, _: u64) -> BoxFuture<'_, Result<Vec<String>>> {
        Box::pin(async {
            // Public endpoints are untrusted: an answer is only stored
            // when two independent providers agree.
            Ok(vec![
                "https://rpc.one.example".to_string(),
                "https://rpc.two.example".to_string(),
                "https://rpc.three.example".to_string(),
            ])
        })
    }
}

struct FakeRpc {
    node: Arc<FakeChain>,
    /// A connection was asked for: discovery reached the endpoints.
    connected: Arc<AtomicBool>,
}

impl FakeRpc {
    fn new() -> Self {
        let node = FakeChain::new();
        node.chain_id.store(CHAIN, std::sync::atomic::Ordering::SeqCst);
        node.height.store(10_000_000, std::sync::atomic::Ordering::SeqCst);
        node.add(TOKEN0, FakeToken::erc20("Token Zero", "TK0", 6));
        node.add(TOKEN1, FakeToken::erc20("Token One", "TK1", 18));
        Self { node, connected: Arc::default() }
    }

    /// THE SEAM: `tokens::build_caller` with its two outside-world
    /// dependencies (the registry fetch and the HTTP connector) replaced.
    /// Everything else - what `--rpc` unset / `none` / a list means, the
    /// discovery, the trust rules - is the production code.
    async fn caller(
        &self,
        rpc_arg: Option<&str>,
    ) -> Option<Arc<dyn EthCaller>> {
        let node = self.node.clone();
        let connected = self.connected.clone();

        let connect: Arc<Connector> =
            Arc::new(move |_url, _discovered| {
                connected.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(node.clone() as Arc<dyn EthCaller>)
            });

        let mut options = CallerOptions::default();
        options.discovery.refresh_check_interval =
            Duration::from_millis(200);
        options.discovery.jitter = false;

        build_caller_with(
            CHAIN,
            rpc_arg,
            options,
            Arc::new(FakeRegistry),
            Some(connect),
        )
        .await
        .unwrap()
    }
}

fn fast_workers() -> WorkerOptions {
    let mut options = WorkerOptions::default();
    options.tokens.batch_linger = Duration::from_millis(20);
    options.tokens.retry_delay = Duration::from_millis(200);
    options.tokens.backfill_interval = Duration::from_millis(300);
    options.tokens.backfill_min_interval = Duration::from_millis(100);
    options.pools.batch_linger = Duration::from_millis(20);
    options.pools.backfill_interval = Duration::from_millis(300);
    options.venues.batch_linger = Duration::from_millis(20);
    options
}

fn fast_lease() -> LeaseOptions {
    LeaseOptions {
        heartbeat: Duration::from_millis(100),
        ttl: Duration::from_millis(400),
    }
}

// ------------------------------------------------------------ the harness

/// One database per scenario (and per role), dropped and migrated here.
struct Scenario {
    url: String,
    db: Database,
}

impl Scenario {
    async fn new(name: &str) -> Self {
        if std::env::var("TEST_LOG").is_ok() {
            let _ = simple_logger::SimpleLogger::new()
                .with_level(log::LevelFilter::Info)
                .init();
        }

        let base = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL must be set for the ignored tests");
        let params = DatabaseParams::parse(&base).unwrap();

        assert!(
            params.database.ends_with("_test"),
            "TEST_DATABASE_URL names database '{}': these tests DROP \
             databases derived from it, so it must end in '_test'",
            params.database
        );

        let prefix = params.database.trim_end_matches("_test");
        let database = format!("{prefix}_{name}_test");
        let url = base.replacen(
            &format!("/{}", params.database),
            &format!("/{database}"),
            1,
        );

        Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password)
            .query(&format!("DROP DATABASE IF EXISTS `{database}`"))
            .execute()
            .await
            .unwrap();

        migrate::run(&url).await.unwrap();

        let db = Database::new(&url, CHAIN).await.unwrap();
        Self { url, db }
    }

    /// The configuration exactly as the binary would parse it.
    fn config(&self, flags: &[&str]) -> Config {
        let mut argv = vec![
            "indexer",
            "--database",
            &self.url,
            "--hypersync-token",
            TOKEN,
        ];
        argv.extend_from_slice(flags);

        match Command::try_parse_from(argv).unwrap() {
            Command::Run(config) => *config,
            other => panic!("{other:?}"),
        }
    }

    async fn count(&self, sql: &str) -> u64 {
        self.db
            .db
            .query(sql)
            .fetch_one::<u64>()
            .await
            .unwrap_or_else(|e| panic!("{e}\n{sql}"))
    }

    async fn rows(&self, table: &str) -> u64 {
        self.count(&format!(
            "SELECT toUInt64(count()) FROM `{table}` FINAL"
        ))
        .await
    }

    /// Runs the real pipeline until `done` says so (or `--end-block`).
    async fn run<F, Fut>(
        &self,
        config: Config,
        chain: &TestChain,
        rpc: &FakeRpc,
        done: F,
    ) -> Result<()>
    where
        F: Fn(Database) -> Fut + Send + 'static,
        Fut: Future<Output = bool> + Send,
    {
        let caller = rpc.caller(config.rpc_url.as_deref()).await;
        let db = self.db.clone();

        let shutdown = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if done(db.clone()).await {
                    return;
                }
            }
        };

        let runtime = Runtime {
            source: chain.clone(),
            canonical: Arc::new(chain.clone()),
            caller,
            workers: fast_workers(),
            lease: fast_lease(),
            shutdown: Box::pin(shutdown),
        };

        tokio::time::timeout(
            Duration::from_secs(600),
            run_with(config, runtime),
        )
        .await
        .expect("the pipeline did not finish in time")
    }

    /// Indexes `[0, end)` and stops (`--end-block`).
    async fn index_until(
        &self,
        chain: &TestChain,
        end: u64,
        flags: &[&str],
    ) {
        let end = end.to_string();
        let mut all = vec!["--end-block", end.as_str(), "--rpc", "none"];
        all.extend_from_slice(flags);

        self.run(self.config(&all), chain, &FakeRpc::new(), |_| async {
            false
        })
        .await
        .unwrap();
    }

    /// Everything a reader can see, as strings: every base and side table
    /// (FINAL, without the per-flush stamps) and every aggregate view.
    async fn snapshot(&self) -> BTreeMap<String, Vec<String>> {
        let mut tables: Vec<&str> = db::BASE_TABLES.to_vec();
        tables.extend_from_slice(db::SIDE_TABLES);
        tables.push("seen_tokens");
        for spec in ALL_MODULES {
            tables.extend_from_slice(spec.base_tables);
        }
        tables.extend_from_slice(dex::SIDE_TABLES);

        let views = AGGREGATE_VIEWS;

        let mut snapshot = BTreeMap::new();

        for (name, is_table) in tables
            .iter()
            .map(|t| (*t, true))
            .chain(views.iter().map(|v| (*v, false)))
        {
            let columns: Vec<String> = self
                .db
                .db
                .query(&format!(
                    "SELECT name FROM system.columns WHERE database = \
                     currentDatabase() AND table = '{name}' \
                     AND name NOT IN ('_version', 'epoch', 'is_deleted') \
                     ORDER BY position"
                ))
                .fetch_all()
                .await
                .unwrap();
            assert!(!columns.is_empty(), "{name} does not exist");

            let tuple = columns
                .iter()
                .map(|column| format!("`{column}`"))
                .collect::<Vec<_>>()
                .join(", ");

            let sql = format!(
                "SELECT hex(toString(tuple({tuple}))) AS row FROM `{name}`{} \
                 ORDER BY row",
                if is_table { " FINAL" } else { "" }
            );

            let rows: Vec<String> = self
                .db
                .db
                .query(&sql)
                .fetch_all()
                .await
                .unwrap_or_else(|e| panic!("{e}\n{sql}"));

            snapshot.insert(name.to_string(), rows);
        }

        snapshot
    }

    async fn assert_consistent(&self) {
        let report = verify::verify(&self.db, 0, 0).await.unwrap();
        assert!(report.is_consistent(), "{report}");
    }
}

fn assert_same(
    what: &str,
    actual: &BTreeMap<String, Vec<String>>,
    clean: &BTreeMap<String, Vec<String>>,
) {
    for (name, expected) in clean {
        assert_eq!(
            &actual[name], expected,
            "{what}: '{name}' differs from a clean index"
        );
    }
}

/// A clean index of `chain` as it is now, in its own database.
async fn clean_index(name: &str, chain: &TestChain) -> Scenario {
    let clean = Scenario::new(name).await;
    clean.index_until(chain, chain.head(), &[]).await;
    clean
}

// ------------------------------------------------------------ (a)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn zero_flags_index_dex_and_resolve_tokens() {
    let scenario = Scenario::new("a_zero_flags").await;
    let chain = TestChain::new(12);
    let rpc = FakeRpc::new();

    // Nothing but the database and the HyperSync token.
    let config = scenario.config(&[]);
    assert!(config.dex && config.predictions);
    assert_eq!(config.rpc_url, None);

    scenario
        .run(config, &chain, &rpc, |db| async move {
            let count = |sql: &'static str| {
                let db = db.clone();
                async move {
                    db.db.query(sql).fetch_one::<u64>().await.unwrap_or(0)
                }
            };
            count("SELECT toUInt64(count()) FROM blocks FINAL").await == 12
                && count(
                    "SELECT toUInt64(count()) FROM tokens FINAL \
                     WHERE symbol != ''",
                )
                .await
                    >= 2
        })
        .await
        .unwrap();

    // DEX by default: pools from the creation events, swaps, liquidity
    // (Sync), candles.
    assert_eq!(scenario.rows("blocks").await, 12);
    assert_eq!(scenario.rows("dex_swaps").await, 10 + 5);
    assert_eq!(scenario.rows("dex_liquidity").await, 10);
    assert_eq!(
        scenario
            .count(
                "SELECT toUInt64(count()) FROM dex_pools FINAL \
                 WHERE source = 'event'"
            )
            .await,
        2
    );
    for view in
        ["dex_candles_1m_v", "dex_candles_1h_v", "dex_candles_1d_v"]
    {
        assert_eq!(
            scenario
                .count(&format!("SELECT toUInt64(sum(swaps)) FROM {view}"))
                .await,
            15,
            "{view}"
        );
    }

    // Who traded / seeded: the transaction sender, not the router.
    assert_eq!(
        scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM dex_swaps FINAL WHERE \
                 tx_from = unhex('{}') AND tx_to = unhex('{}')",
                "d0".repeat(20),
                "c0".repeat(20)
            ))
            .await,
        15
    );
    assert_eq!(
        scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM dex_liquidity FINAL WHERE \
                 tx_from = unhex('{}')",
                "d0".repeat(20)
            ))
            .await,
        10
    );

    // Token metadata with NO --rpc: `auto` discovered the (fake) public
    // endpoints and the background worker resolved both tokens.
    assert!(rpc.connected.load(std::sync::atomic::Ordering::SeqCst));
    let tokens: Vec<(String, String, u8)> = scenario
        .db
        .db
        .query(
            "SELECT name, symbol, decimals FROM tokens FINAL \
             WHERE symbol != '' ORDER BY symbol",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(
        tokens,
        vec![
            ("Token Zero".to_string(), "TK0".to_string(), 6),
            ("Token One".to_string(), "TK1".to_string(), 18),
        ]
    );

    // Checkpoints were written and cover everything.
    assert_eq!(verify::resume_point(&scenario.db, 0).await.unwrap(), 12);
    scenario.assert_consistent().await;
}

// ------------------------------------------------------------ (b)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn no_dex_and_rpc_none_produce_none_of_it() {
    let scenario = Scenario::new("b_opt_out").await;
    let chain = TestChain::new(12);
    let rpc = FakeRpc::new();

    let config = scenario.config(&[
        "--no-dex",
        "--no-predictions",
        "--rpc",
        "none",
        "--end-block",
        "12",
    ]);

    scenario.run(config, &chain, &rpc, |_| async { false }).await.unwrap();

    // Give a (wrongly) running worker the time to show itself.
    tokio::time::sleep(Duration::from_millis(800)).await;

    assert_eq!(scenario.rows("blocks").await, 12);
    assert_eq!(scenario.rows("erc20_transfers").await, 40);
    for table in [
        "dex_swaps",
        "dex_liquidity",
        "dex_pools",
        "dex_candles_1m",
        "dex_pool_volume_1h",
        "tokens",
    ] {
        assert_eq!(
            scenario
                .count(&format!("SELECT toUInt64(count()) FROM {table}"))
                .await,
            0,
            "{table}"
        );
    }
    assert!(!rpc.connected.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        rpc.node.attempts.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

// ------------------------------------------------------------ (c)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_flush_killed_before_blocks_is_healed_on_restart() {
    let scenario = Scenario::new("c_crash").await;
    let chain = TestChain::new(12);

    // First run: blocks [0, 5) committed.
    scenario.index_until(&chain, 5, &[]).await;

    // Then a flush of [5, 9) dies after its children and before `blocks`:
    // exactly what `Database::store` does, minus its last two inserts.
    let mut batch = transform::transform_with(
        CHAIN,
        &chain.response(BlockRange::new(5, 9)),
        BlockRange::new(5, 9),
        EnabledModules::default(),
        &mut DecodeState::default(),
    )
    .unwrap()
    .rows;
    batch.set_version(next_version());
    batch.set_epoch(0);

    let key =
        FlushKey { chain: CHAIN, span: (5, 8), version: batch.version() };
    let db = &scenario.db;
    db.insert_flush("logs", &batch.logs, &key).await.unwrap();
    db.insert_flush("transactions", &batch.transactions, &key)
        .await
        .unwrap();
    db.insert_flush("erc20_transfers", &batch.erc20_transfers, &key)
        .await
        .unwrap();
    batch.modules.store(db, &key).await.unwrap();

    // The orphans are there, and they already inflated the aggregates.
    let report = verify::verify(db, 0, 0).await.unwrap();
    assert!(!report.is_consistent());
    assert_eq!(scenario.rows("blocks").await, 5);
    assert!(scenario.rows("dex_swaps").await > 4);

    // Restart.
    scenario.index_until(&chain, 12, &[]).await;

    let clean = clean_index("c_crash_clean", &chain).await;
    assert_same(
        "after the gap heal",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );

    let reasons: Vec<String> = db
        .db
        .query("SELECT toString(reason) FROM reorgs")
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(reasons, vec!["gap_heal".to_string()]);
    scenario.assert_consistent().await;
}

// ------------------------------------------------------------ (d)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_retried_insert_does_not_double_count() {
    let scenario = Scenario::new("d_retry").await;
    let chain = TestChain::new(12);

    let mut batch = transform::transform_with(
        CHAIN,
        &chain.response(BlockRange::new(0, 12)),
        BlockRange::new(0, 12),
        EnabledModules::default(),
        &mut DecodeState::default(),
    )
    .unwrap()
    .rows;
    batch.set_version(next_version());

    // The flush is applied, the answer is lost, the flush is retried -
    // as a whole and table by table.
    let db = &scenario.db;
    db.store(&batch).await.unwrap();
    db.store(&batch).await.unwrap();

    let key =
        FlushKey { chain: CHAIN, span: (0, 11), version: batch.version() };
    for _ in 0..2 {
        db.insert_flush("dex_swaps", &batch.modules.dex.swaps, &key)
            .await
            .unwrap();
        db.insert_flush("transactions", &batch.transactions, &key)
            .await
            .unwrap();
        db.insert_flush("blocks", &batch.blocks, &key).await.unwrap();
    }

    // Base tables (no FINAL: not even a second physical copy), side
    // tables and aggregates all hold it once.
    for (sql, expected) in [
        ("SELECT toUInt64(count()) FROM dex_swaps", 15),
        ("SELECT toUInt64(count()) FROM transactions", 1 + 10 * 2 + 5),
        ("SELECT toUInt64(count()) FROM dex_swaps_by_pool", 15),
        ("SELECT toUInt64(count()) FROM tx_lookup", 26),
        ("SELECT toUInt64(sum(swaps)) FROM dex_candles_1m_v", 15),
        ("SELECT toUInt64(sum(swaps)) FROM dex_candles_1d_v", 15),
        ("SELECT toUInt64(sum(transactions)) FROM daily_transaction_stats_v", 26),
        ("SELECT toUInt64(sum(blocks)) FROM daily_block_stats_v", 12),
        ("SELECT toUInt64(sum(transfers)) FROM daily_erc20_transfer_stats_v", 40),
    ] {
        assert_eq!(scenario.count(sql).await, expected, "{sql}");
    }

    // A LATER flush of the same blocks (another `_version`) is not a
    // retry: it is written - that is what re-streaming after a purge does.
    batch.set_version(next_version());
    db.store(&batch).await.unwrap();
    assert_eq!(
        scenario.count("SELECT toUInt64(count()) FROM dex_swaps").await,
        30
    );
    assert_eq!(scenario.rows("dex_swaps").await, 15);
}

// ------------------------------------------------------------ (e)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reorg_of_depth_3_ends_up_equal_to_a_clean_index() {
    let scenario = Scenario::new("e_reorg").await;
    let chain = TestChain::new(12);

    // Blocks [0, 12) of the original chain, busy with swaps.
    scenario.index_until(&chain, 12, &[]).await;
    assert_eq!(scenario.rows("dex_swaps").await, 15);

    // Blocks 9, 10, 11 are replaced by quieter ones, the new fork is 2
    // blocks longer.
    chain.reorg(3, 2);
    scenario.index_until(&chain, 14, &[]).await;

    let clean = clean_index("e_reorg_clean", &chain).await;
    let actual = scenario.snapshot().await;
    assert_same("after the rollback", &actual, &clean.snapshot().await);

    // Fewer swaps than before: orphan keys died, aggregates went DOWN.
    assert_eq!(actual["dex_swaps"].len(), 16);

    let reorgs: Vec<(String, u64, u64, u32)> = scenario
        .db
        .db
        .query(
            "SELECT toString(reason), fork_block, depth, epoch FROM reorgs",
        )
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(reorgs, vec![("reorg".to_string(), 9, 3, 1)]);

    // The epoch advanced: what was streamed after the rollback carries it.
    assert_eq!(scenario.db.current_epoch().await.unwrap(), 1);
    assert_eq!(
        scenario
            .count(
                "SELECT toUInt64(count()) FROM blocks FINAL \
                 WHERE number >= 9 AND epoch != 1"
            )
            .await,
        0
    );
    scenario.assert_consistent().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reorg_deeper_than_max_reorg_depth_is_a_clear_fatal_error() {
    let scenario = Scenario::new("e_too_deep").await;
    let chain = TestChain::new(12);

    scenario.index_until(&chain, 12, &[]).await;
    let before = scenario.snapshot().await;

    chain.reorg(3, 2);

    let config = scenario.config(&[
        "--end-block",
        "14",
        "--rpc",
        "none",
        "--max-reorg-depth",
        "2",
    ]);

    let error = scenario
        .run(config, &chain, &FakeRpc::new(), |_| async { false })
        .await
        .expect_err("a reorg deeper than the limit must stop the indexer");

    let message = format!("{error:#}");
    assert!(message.contains("max-reorg-depth"), "{message}");

    // Nothing was purged, nothing was written.
    assert_same("after the refusal", &scenario.snapshot().await, &before);
    assert_eq!(
        scenario.count("SELECT toUInt64(count()) FROM reorgs").await,
        0
    );
}

// ------------------------------------------------------------ (f)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn backfill_from_stored_logs_equals_a_fresh_index() {
    let chain = TestChain::new(12);
    let clean = clean_index("f_backfill_clean", &chain).await;
    let expected = clean.snapshot().await;

    // 1. Nothing changed: nothing is written, not even an epoch.
    let report =
        backfill::backfill(&clean.db, "dex", 0, 0, 5).await.unwrap();
    assert_eq!(report.rewritten, None);
    assert_eq!(report.logs, 67);
    assert_same(
        "after a no-op backfill",
        &clean.snapshot().await,
        &expected,
    );
    assert_eq!(
        clean.count("SELECT toUInt64(count()) FROM reorgs").await,
        0
    );
    assert_eq!(
        clean.count("SELECT toUInt64(count()) FROM dex_swaps").await,
        15,
        "no physical row was added either"
    );

    // 2. A chain indexed WITHOUT the module (= a new event family): the
    //    backfill produces the module's rows and aggregates from the
    //    stored logs alone.
    let late = Scenario::new("f_backfill_late").await;
    late.index_until(&chain, 12, &["--no-dex"]).await;
    assert_eq!(late.rows("dex_swaps").await, 0);

    let report =
        backfill::backfill(&late.db, "dex", 0, 0, 5).await.unwrap();
    assert_eq!(report.rewritten, Some(BlockRange::new(0, 12)));
    assert_same(
        "after a late backfill",
        &late.snapshot().await,
        &expected,
    );

    // 3. A decoder fix: stored rows that the decoder no longer produces
    //    (a forged swap) are replaced, the aggregates go back down, and
    //    the aggregates of EVERYTHING ELSE survive the new epoch.
    let fixed = Scenario::new("f_backfill_fixed").await;
    fixed.index_until(&chain, 12, &[]).await;

    let mut forged = transform::transform_with(
        CHAIN,
        &chain.response(BlockRange::new(6, 7)),
        BlockRange::new(6, 7),
        EnabledModules::default(),
        &mut DecodeState::default(),
    )
    .unwrap()
    .rows
    .modules;
    forged.dex.liquidity.clear();
    forged.dex.pools.clear();
    for swap in &mut forged.dex.swaps {
        swap.log_index += 1_000;
    }
    forged.set_version(next_version());
    let key =
        FlushKey { chain: CHAIN, span: (6, 6), version: next_version() };
    forged.store(&fixed.db, &key).await.unwrap();
    assert_eq!(fixed.rows("dex_swaps").await, 17);

    let report =
        backfill::backfill(&fixed.db, "dex", 0, 0, 5).await.unwrap();
    assert_eq!(report.rewritten, Some(BlockRange::new(5, 10)));
    assert_eq!(report.epoch, 1);
    assert_same(
        "after a fixing backfill",
        &fixed.snapshot().await,
        &expected,
    );

    let reasons: Vec<String> = fixed
        .db
        .db
        .query("SELECT toString(reason) FROM reorgs")
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(reasons, vec!["redecode".to_string()]);

    // Idempotent.
    let report =
        backfill::backfill(&fixed.db, "dex", 0, 0, 5).await.unwrap();
    assert_eq!(report.rewritten, None);

    // A live indexer on the same chain adopts the new epoch by itself.
    chain.extend(2, 0, busy_block);
    fixed.index_until(&chain, 14, &[]).await;
    let clean = clean_index("f_backfill_clean_longer", &chain).await;
    assert_same(
        "after indexing on top of a backfill",
        &fixed.snapshot().await,
        &clean.snapshot().await,
    );
}

// ------------------------------------------------------------ deep purge

/// A purge deep in history repairs more than 100 monthly partitions of
/// every aggregate. ClickHouse refuses ONE insert over that many (code
/// 252), which would wedge the chain for ever (the purge fails at the
/// rebuild on every restart): the rebuilds are one INSERT per month.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_purge_eleven_years_deep_can_finish() {
    const MONTH: u32 = 30 * 86_400;

    // Small flushes: ONE insert may not span more than 100 monthly
    // partitions either (not a concern on a real chain, where a flush of
    // 100k rows covers hours or days, never eight years).
    const SMALL_FLUSHES: [&str; 2] = ["--flush-rows", "300"];

    let scenario = Scenario::new("deep").await;
    let chain = TestChain::with_block_time(135, MONTH);
    scenario.index_until(&chain, 135, &SMALL_FLUSHES).await;

    // Block 3 (11 years before the head) has to go: every aggregate is
    // rebuilt from its day on, 132 months.
    let purger = Purger::new(
        Arc::new(ClickhouseReorgStore::new(
            scenario.db.clone(),
            Scope::Chain,
        )),
        Arc::new(crate::pipeline::backfill::EpochOnly::new(
            scenario.db.clone(),
        )),
        Arc::new(NoBlockKeyedCaches),
        Arc::new(Metrics::disabled()),
    );
    let report = purger
        .purge_range(CHAIN, 3, Some(4), PurgeReason::GapHeal)
        .await
        .expect("the rebuild must be sliced by month");
    assert_eq!(report.blocks_tombstoned, 1);
    assert_eq!(report.epoch, 1);

    // The hole is streamed again and everything equals a clean index.
    scenario.index_until(&chain, 135, &SMALL_FLUSHES).await;

    let clean = Scenario::new("deep_clean").await;
    clean.index_until(&chain, 135, &SMALL_FLUSHES).await;
    assert_same(
        "after a deep purge",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );
    scenario.assert_consistent().await;
}

// ------------------------------------------------------------ the views

const AGGREGATE_VIEWS: [&str; 9] = [
    "daily_block_stats_v",
    "daily_transaction_stats_v",
    "daily_erc20_transfer_stats_v",
    "dex_candles_1m_v",
    "dex_candles_1h_v",
    "dex_candles_1d_v",
    "dex_pool_volume_1h_v",
    "dex_pool_stats_1d_v",
    "dex_protocol_stats_1d_v",
];

/// `join_use_nulls = 1` is a per user / per profile setting a BI tool or an
/// ORM may set. The validity rule joins `reorgs` with an ASOF LEFT JOIN: a
/// chain without reorgs has no row there, and a bare `epoch >= epoch_floor`
/// would then compare with NULL and silently drop every row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn aggregate_views_do_not_depend_on_join_use_nulls() {
    let scenario = Scenario::new("views").await;
    let chain = TestChain::new(12);
    scenario.index_until(&chain, 12, &[]).await;

    let nulls = scenario.db.db.clone().with_option("join_use_nulls", "1");

    for view in AGGREGATE_VIEWS {
        let sql = format!("SELECT toUInt64(count()) FROM {view}");
        let default = scenario.count(&sql).await;
        let with_nulls: u64 = nulls.query(&sql).fetch_one().await.unwrap();

        assert!(default > 0, "{view} is empty");
        assert_eq!(with_nulls, default, "{view} under join_use_nulls = 1");
    }

    // ... and after a purge (a `reorgs` row exists) just the same.
    chain.reorg(2, 2);
    scenario.index_until(&chain, 14, &[]).await;

    for view in AGGREGATE_VIEWS {
        let sql = format!("SELECT toUInt64(count()) FROM {view}");
        let with_nulls: u64 = nulls.query(&sql).fetch_one().await.unwrap();
        assert_eq!(with_nulls, scenario.count(&sql).await, "{view}");
    }
}

/// Receipts had no status before Byzantium: `status` is NULL there, and a
/// contract creation without a status succeeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn contracts_view_lists_pre_byzantium_creations() {
    use crate::db::models::transaction::DatabaseTransaction;

    let scenario = Scenario::new("contracts").await;

    let creation = |index: u8, status: Option<TransactionStatus>| {
        let mut row = DatabaseTransaction::from_hypersync(
            &Transaction {
                block_number: Some(UInt::from(100u64)),
                transaction_index: Some(UInt::from(u64::from(index))),
                hash: Some(Hash::from([index; 32])),
                from: Some(HsAddress::from([0x01; 20])),
                contract_address: Some(HsAddress::from(
                    [0xc0 | index; 20],
                )),
                status,
                ..Default::default()
            },
            CHAIN,
            BASE_TIMESTAMP,
            None,
        )
        .unwrap();
        row._version = 1;
        row
    };

    let rows = vec![
        creation(1, None),
        creation(2, Some(TransactionStatus::Success)),
        creation(3, Some(TransactionStatus::Failure)),
    ];
    let key = FlushKey { chain: CHAIN, span: (100, 100), version: 1 };
    scenario.db.insert_flush("transactions", &rows, &key).await.unwrap();

    let listed: Vec<u32> = scenario
        .db
        .db
        .query(
            "SELECT toUInt32(transaction_index) FROM (SELECT c.*, \
             t.transaction_index FROM contracts AS c INNER JOIN \
             transactions AS t ON t.hash = c.transaction_hash) \
             ORDER BY transaction_index",
        )
        .fetch_all()
        .await
        .unwrap();

    assert_eq!(
        listed,
        vec![1, 2],
        "NULL status = success, failure is out"
    );
}

// ------------------------------------------------------------ the lease

/// Generous ttl: the machine running the tests may be saturated, and a
/// heartbeat that takes longer than the ttl IS a dead process.
fn patient_lease() -> LeaseOptions {
    LeaseOptions {
        heartbeat: Duration::from_millis(100),
        ttl: Duration::from_secs(3),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_second_process_on_the_same_chain_refuses_to_start() {
    let scenario = Scenario::new("lease").await;

    let (fatal, _) = watch::channel(None);
    let first =
        Lease::acquire(&scenario.db, patient_lease(), fatal.clone())
            .await
            .unwrap();

    let error =
        Lease::acquire(&scenario.db, patient_lease(), fatal.clone())
            .await
            .err()
            .expect("the second instance must refuse");
    assert!(format!("{error:#}").contains("already indexing chain 1"));

    // After a clean shutdown the next start does not even wait.
    first.release().await;
    let started = std::time::Instant::now();
    let second =
        Lease::acquire(&scenario.db, patient_lease(), fatal.clone())
            .await
            .unwrap();
    assert!(started.elapsed() < patient_lease().ttl);

    // A killed process (dropped: no release row): the next start waits
    // one ttl, sees no new heartbeat and takes over.
    drop(second);
    let started = std::time::Instant::now();
    let third =
        Lease::acquire(&scenario.db, patient_lease(), fatal.clone())
            .await
            .unwrap();
    assert!(started.elapsed() >= patient_lease().ttl);
    third.release().await;
}
