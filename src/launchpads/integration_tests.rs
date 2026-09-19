//! Launchpad tables, aggregates and views against a REAL ClickHouse.
//! Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/anything \
//!   cargo test launchpads::integration -- --ignored --nocapture
//! ```
//!
//! Every test creates its OWN database on that server (name ending in
//! `_test`), applies EVERY embedded migration to it through
//! `crate::db::migrate` (core, DEX, predictions, launchpads, 0090) and
//! never touches the database named in the url. Rows go through
//! [`decode`] and the clickhouse crate's RowBinary inserts, exactly like
//! the pipeline writes them. No test issues a DELETE: reorgs are
//! tombstones + epochs (docs/design.md §2).
//!
//! The data is REAL (see `fixtures_data.rs`) except where a test says
//! "forged" / "constructed": the forgeries and the hostile amounts.

// The literals below mirror on-chain amounts digit by digit.
#![allow(clippy::excessive_precision, clippy::inconsistent_digit_grouping)]

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, U256};
use clickhouse::{Client, Row};
use serde::Serialize;

use crate::{
    db::{
        migrate, models::erc20_transfer::DatabaseERC20Transfer,
        tombstone_sql, DatabaseParams,
    },
    launchpads::{
        cookbook, decode,
        derived::rebuild_statements,
        fixtures::{self, address, Place, RawTx},
        LaunchpadRows, BASE_TABLES, LAUNCHPADS_DERIVED, SIDE_TABLES,
    },
    utils::format::id32,
};

const CHAIN: u64 = 4663;

/// Pons V2 on Robinhood Chain: the singletons an operator trusts.
const PONS_FACTORY: &str = "0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e";
const PONS_HOOK: &str = "0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044";
const FLAP_RH: &str = "0x26605f322f7fF986f381bB9A6e3f5DAb0bEaEb09";
const FLAP_BNB: &str = "0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0";

/// The token that launched and graduated 12 blocks (1.2 s) later.
const TOKEN: &str = "0x95eb2d489fcc07a6f527b3989e6c0d0a7e69e37d";
const CURVE: &str = "0x249915281e027c9559eb018b19784d02fb046ed3";
const CREATOR: &str = "0xf523e8b611d3f003929379fc235de5d7d662c2e5";
/// Its destination Uniswap V4 pool (the `Initialize` of the same tx).
const GRADUATION_POOL: &str =
    "3875d4c5c08fcb207d3db0b096b4c84ca59c2e04828b91eb9d230ea2828ee856";
/// The bundler that bought for 15 recipients one block after the launch.
const BUNDLER: &str = "0x14b9a544e8c179fc2040d3089dcc73baf25aa8f9";

const LAUNCH_BLOCK: u64 = 66_679_543;
const GRADUATION_BLOCK: u64 = 66_679_555;
/// The graduation threshold of that curve, in wei of the native coin.
const THRESHOLD: f64 = 4.2e18;

/// How long a read is given to catch up with an acknowledged INSERT.
///
/// ClickHouse 25.12 has no read-your-writes: measured on this build, 3 %
/// of the reads issued right after an acknowledged INSERT miss the new
/// part, and they heal within milliseconds (docs/design.md §2, "No
/// read-your-writes").
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// Between two attempts of a settling read.
const RETRY: std::time::Duration = std::time::Duration::from_millis(5);

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
    async fn create(tag: &str) -> Self {
        let url = std::env::var("TEST_DATABASE_URL")
            .expect("TEST_DATABASE_URL must be set for the ignored tests");
        let params = DatabaseParams::parse(&url).unwrap();

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        static SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let sequence =
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!(
            "launchpads_{tag}_{}_{nanos}_{sequence}_test",
            std::process::id()
        );

        let admin = Client::default()
            .with_url(&params.endpoint)
            .with_user(&params.user)
            .with_password(&params.password);

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

    async fn number(&self, sql: &str) -> f64 {
        self.rows::<f64>(sql).await[0]
    }

    async fn text(&self, sql: &str) -> String {
        self.rows::<String>(sql).await.pop().unwrap_or_default()
    }

    /// Every row of `source` as text, sorted: two indexes are equal iff
    /// their snapshots are.
    async fn snapshot(&self, source: &str) -> Vec<String> {
        self.rows::<String>(&format!(
            "SELECT hex(toString(tuple(*))) AS line FROM ({source}) \
             ORDER BY line"
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

        // Validation off: the crate has no mapping for (U)Int256, exactly
        // like db::Database::insert_once.
        let client = self.client.clone().with_validation(false);
        let mut insert = client.insert::<T>(table).await.unwrap();
        for row in rows {
            insert.write(row).await.unwrap();
        }
        insert.end().await.unwrap();
    }

    /// Waits until the part `version` just wrote into `table` is readable.
    ///
    /// ClickHouse 25.12 has no read-your-writes ([`SETTLE`]), so a read
    /// issued right after an acknowledged INSERT can miss it. One row is
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

    /// In `INSERT_ORDER`, like the pipeline, and not returning before
    /// every part it wrote can be read back.
    async fn store(&self, rows: &LaunchpadRows) {
        self.write("launchpad_tokens", &rows.tokens).await;
        self.write("launchpad_trades", &rows.trades).await;
        self.write("launchpad_graduations", &rows.graduations).await;
        self.write("launchpad_creator_fees", &rows.creator_fees).await;

        for (table, version) in [
            (
                "launchpad_tokens",
                rows.tokens.first().map(|row| row._version),
            ),
            (
                "launchpad_trades",
                rows.trades.first().map(|row| row._version),
            ),
            (
                "launchpad_graduations",
                rows.graduations.first().map(|row| row._version),
            ),
            (
                "launchpad_creator_fees",
                rows.creator_fees.first().map(|row| row._version),
            ),
        ] {
            if let Some(version) = version {
                self.await_part(table, version).await;
            }
        }
    }

    /// The operator data every headline view depends on.
    async fn trust_the_real_venues(&self) {
        self.execute(&format!(
            "INSERT INTO launchpad_trusted_emitters \
             (chain, emitter, family, label) VALUES \
             ({CHAIN}, {}, 'pons_v2', 'factory'), \
             ({CHAIN}, {}, 'pons_v2', 'hook'), \
             ({CHAIN}, {}, 'flap_portal', 'portal'), \
             (56, {}, 'flap_portal', 'portal')",
            id_literal(PONS_FACTORY),
            id_literal(PONS_HOOK),
            id_literal(FLAP_RH),
            id_literal(FLAP_BNB),
        ))
        .await;
    }

    async fn drop_database(&self) {
        self.admin
            .query(&format!("DROP DATABASE IF EXISTS {}", self.name))
            .execute()
            .await
            .unwrap();
    }
}

/// A `FixedString(32)` identity literal for SQL.
fn id_literal(address_hex: &str) -> String {
    format!("unhex('{}')", id_hex(address_hex))
}

fn id_hex(address_hex: &str) -> String {
    hex::encode(id32(address(address_hex)))
}

/// The bound parameters every cookbook query and every parameterized view
/// of these tests is sent with. ClickHouse ignores the ones a statement
/// does not use, so one set covers every screen.
fn cookbook_parameters<'a>(
    token: &'a str,
    creator: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("chain", "4663"),
        ("since", "0"),
        ("now", "1789999999"),
        ("dead_after", "3600"),
        ("from_block", "0"),
        ("as_of_block", "18446744073709551615"),
        ("blocks", "5"),
        ("token", token),
        ("creator", creator),
    ]
}

/// Every fixture decoded the way the pipeline does it, stamped.
fn rows_of(
    transactions: &[&RawTx],
    version: u64,
    epoch: u32,
) -> LaunchpadRows {
    let mut rows = LaunchpadRows::default();

    for tx in transactions {
        let mut decoded = decode(tx.chain, &tx.logs());
        decoded.attach_transactions(|hash| {
            (*hash == tx.hash()).then(|| tx.origin())
        });
        rows.append(&mut decoded);
    }

    rows.set_version(version);
    rows.set_epoch(epoch);
    rows
}

/// The core `erc20_transfers` rows of the same fixtures: the holder screen
/// reads them, and they are the evidence the corroboration rests on.
fn transfers_of(
    transactions: &[&RawTx],
    version: u64,
    epoch: u32,
) -> Vec<DatabaseERC20Transfer> {
    transactions
        .iter()
        .flat_map(|tx| tx.logs())
        .filter_map(|log| DatabaseERC20Transfer::from_log(&log))
        .map(|mut row| {
            row._version = version;
            row.epoch = epoch;
            row
        })
        .collect()
}

/// A `dex_pools` row for the Uniswap V4 pool the token graduated into, as
/// the DEX module would have written it from the `Initialize` event of the
/// very same transaction. Proves the graduation join key.
async fn store_the_destination_pool(db: &TestDb) {
    db.execute(&format!(
        "INSERT INTO dex_pools (chain, pool_id, emitter, factory, protocol, \
         token0, token1, tokens, underlying_tokens, fee, tick_spacing, hooks, \
         stable, created_block, timestamp, tx_id, tx_index, ordinal, \
         source, _version) VALUES ({CHAIN}, unhex('{GRADUATION_POOL}'), \
         unhex('0000000000000000000000008366a39cc670b4001a1121b8f6a443a643e40951'), \
         unhex('0000000000000000000000008366a39cc670b4001a1121b8f6a443a643e40951'), 'uniswap_v4', \
         unhex('0000000000000000000000000000000000000000000000000000000000000000'), \
         unhex('000000000000000000000000{TOKEN_20}'), [], [], 0, 200, \
         unhex('000000000000000000000000e5e702641ea86f4ae6cc3cdaed2b886f976be044'), false, \
         {GRADUATION_BLOCK}, toDateTime(1789780288), \
         unhex('0000000000000000000000000000000000000000000000000000000000000001'), \
         0, 18, 'event', 1)",
        TOKEN_20 = &TOKEN[2..],
    ))
    .await;
    // A singleton family: dex_pool_current_v only calls it 'event' when the
    // emitter is a trusted one.
    db.execute(&format!(
        "INSERT INTO dex_trusted_emitters (chain, emitter, protocol) VALUES \
         ({CHAIN}, unhex('0000000000000000000000008366a39cc670b4001a1121b8f6a443a643e40951'), \
         'uniswap_v4')"
    ))
    .await;
}

// -------------------------------------------------------------- the tests

#[tokio::test]
#[ignore]
async fn real_rows_round_trip_and_the_side_tables_follow() {
    let db = TestDb::create("roundtrip").await;
    let rows = rows_of(fixtures::ALL, 1, 0);

    assert_eq!(rows.tokens.len(), 9, "launches");
    assert_eq!(rows.trades.len(), 43, "curve trades");
    assert_eq!(rows.graduations.len(), 2, "graduations");
    assert_eq!(rows.creator_fees.len(), 9, "fee components");

    db.store(&rows).await;

    for table in BASE_TABLES {
        let stored =
            db.count(&format!("SELECT count() FROM {table} FINAL")).await;
        assert!(stored > 0, "{table}");
    }

    // Every MV-fed side table carries exactly its parent's rows.
    assert_eq!(
        db.count("SELECT count() FROM launchpad_trades_by_token FINAL")
            .await,
        rows.trades.len() as u64
    );
    assert_eq!(
        db.count("SELECT count() FROM launchpad_launches_by_time FINAL")
            .await,
        rows.tokens.len() as u64
    );
    assert_eq!(
        db.count(
            "SELECT count() FROM launchpad_launches_by_creator FINAL"
        )
        .await,
        rows.tokens.len() as u64
    );

    // 256-bit round trip: the initial supply is 1e27, above 2^53, and the
    // exact integer must survive the wire.
    let supply = db
        .text(&format!(
            "SELECT toString(initial_supply) FROM launchpad_tokens FINAL \
             WHERE chain = {CHAIN} AND token = unhex('{}')",
            id_hex(TOKEN)
        ))
        .await;
    assert_eq!(supply, "1000000000000000000000000000");

    // Every trade of the fixtures has a VERIFIED token leg.
    assert_eq!(
        db.count("SELECT count() FROM launchpad_trades FINAL WHERE token_verified = 1")
            .await,
        rows.trades.len() as u64
    );
    // The native-quoted ones can never be verified, the ERC-20 ones always
    // are. 5 of the 43 are ERC-20 quoted (4 Pons V2 buys + 1 sell on the
    // ERC-20 curve) plus 3 on the Flap portal.
    assert_eq!(
        db.count("SELECT count() FROM launchpad_trades FINAL WHERE quote_verified = 1")
            .await,
        8
    );

    db.drop_database().await;
}

#[tokio::test]
#[ignore]
async fn the_cookbook_runs_on_real_data() {
    let db = TestDb::create("cookbook").await;
    let rows = rows_of(fixtures::ALL, 1, 0);
    db.store(&rows).await;
    db.write("erc20_transfers", &transfers_of(fixtures::ALL, 1, 0)).await;
    db.trust_the_real_venues().await;
    store_the_destination_pool(&db).await;
    db.execute(&format!(
        "INSERT INTO launchpad_frontends (chain, address, name, kind) \
         VALUES ({CHAIN}, {}, 'bundler', 'router')",
        id_literal(BUNDLER)
    ))
    .await;

    let token = id_hex(TOKEN);
    let creator = id_hex(CREATOR);
    db.set(&cookbook_parameters(&token, &creator));

    // Every recipe runs AS WRITTEN, with its values bound beside it.
    for recipe in cookbook::COOKBOOK {
        let started = Instant::now();
        let lines = db.snapshot(recipe.sql).await;
        println!(
            "{:<38} {:>4} rows  {:>6.1} ms",
            recipe.screen,
            lines.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }

    // ---- new launch feed: 7 of the 9 launches are on this chain (one
    // Flap is on BNB Chain, one Clanker on Base) and 3 of those 7 come
    // from a trusted emitter: 2 Pons V2 and 1 Flap portal.
    assert_eq!(
        db.count(&format!(
            "SELECT count() FROM ({})",
            cookbook::NEW_LAUNCHES.sql
        ))
        .await,
        3
    );
    assert_eq!(
        db.count(&format!(
            "SELECT count() FROM ({})",
            cookbook::NEW_LAUNCHES_ALL.sql
        ))
        .await,
        7
    );

    // ---- token page, hand computed against README §1.1.
    let page = |column: &str| {
        format!(
            "SELECT ifNull(toString({column}), '') FROM \
             launchpad_token_v(chain = {{chain:UInt64}}, \
             token = {{token:String}})"
        )
    };
    assert_eq!(
        db.text(&page("launch_block")).await,
        LAUNCH_BLOCK.to_string()
    );
    assert_eq!(db.text(&page("trades")).await, "32");
    assert_eq!(db.text(&page("buys")).await, "32");
    assert_eq!(db.text(&page("trusted")).await, "1");
    assert_eq!(db.text(&page("graduated")).await, "1");
    assert_eq!(
        db.text(&page("hex(pool_id)")).await.to_lowercase(),
        GRADUATION_POOL
    );
    // The 33 buys raised the threshold exactly (4.2e18 + 3 wei).
    let raised = db
        .number(
            "SELECT raised_raw FROM launchpad_token_v(\
             chain = {chain:UInt64}, token = {token:String})",
        )
        .await;
    assert!((raised - THRESHOLD).abs() < 1.0, "raised {raised}");
    assert_eq!(db.text(&page("curve_progress")).await, "1");

    // ---- candles: the curve lived for 13 blocks inside one minute.
    let buckets = db
        .count(
            "SELECT count() FROM launchpad_candles_1m_v(\
             chain = {chain:UInt64}, token = {token:String})",
        )
        .await;
    assert_eq!(buckets, 1);
    let candle = |column: &str| {
        format!(
            "SELECT {column} FROM launchpad_candles_1m_v(\
             chain = {{chain:UInt64}}, token = {{token:String}})"
        )
    };
    assert_eq!(db.number(&candle("toFloat64(trades)")).await, 32.0);
    // The launch buy: 53,565,734,934,637,059 wei for
    // 30,000,000,000,000,005,519,761,904 raw token units.
    let open = db.number(&candle("ifNull(open_raw, 0.)")).await;
    assert!(
        (open
            - 53_565_734_934_637_059.0
                / 30_000_000_000_000_005_519_761_904.0)
            .abs()
            < 1e-30,
        "open {open}"
    );
    assert!(db.number(&candle("ifNull(high_raw, 0.)")).await > open);

    // ---- graduation feed: the DEX join really resolves the pool.
    let graduation = db
        .rows::<(String, String, u8)>(&format!(
            "SELECT ifNull(toString(pool_status), ''), \
             ifNull(toString(pool_protocol), ''), \
             toUInt8(ifNull(pool_trusted, 0)) FROM \
             launchpad_graduations_v(chain = {CHAIN}, since = 0)"
        ))
        .await;
    assert_eq!(graduation.len(), 1);
    assert_eq!(graduation[0].0, "event");
    assert_eq!(graduation[0].1, "uniswap_v4");
    assert_eq!(graduation[0].2, 1);

    // ---- creator page: one launch, it graduated, fees really paid out.
    let creator_row = |column: &str| {
        format!(
            "SELECT ifNull(toString({column}), '') FROM \
             launchpad_creator_v(chain = {{chain:UInt64}}, \
             creator = {{creator:String}}, as_of = {{now:UInt32}}, \
             dead_after = {{dead_after:UInt32}})"
        )
    };
    assert_eq!(db.text(&creator_row("launches")).await, "1");
    assert_eq!(db.text(&creator_row("graduated")).await, "1");
    assert_eq!(db.text(&creator_row("died")).await, "0");
    let fees = db
        .number(
            "SELECT realised_creator_fees_raw FROM launchpad_creator_v(\
             chain = {chain:UInt64}, creator = {creator:String}, \
             as_of = {now:UInt32}, dead_after = {dead_after:UInt32})",
        )
        .await;
    assert!(
        (fees - 147_906_635_318_930_699.0).abs() < 1_000.0,
        "creator fees {fees}"
    );

    // ---- sniper view: the bundle of 15 recipients is visible as such.
    let bundle = db
        .count(
            "SELECT count() FROM launchpad_snipers_v(\
             chain = {chain:UInt64}, token = {token:String}, \
             blocks = {blocks:UInt64}) WHERE bundle_size = 15",
        )
        .await;
    assert_eq!(bundle, 15);
    let funders = db
        .count(
            "SELECT uniqExact(funder) FROM launchpad_snipers_v(\
             chain = {chain:UInt64}, token = {token:String}, \
             blocks = {blocks:UInt64}) WHERE bundle_size = 15",
        )
        .await;
    assert_eq!(funders, 1, "one transaction funded all fifteen");

    // ---- holders: the curve gave the tokens out, so the sum of the
    // balances is the initial supply minus what the curve still holds.
    let holders = db
        .count(
            "SELECT count() FROM launchpad_token_holders_v(\
             chain = {chain:UInt64}, token = {token:String}, \
             as_of_block = {as_of_block:UInt64})",
        )
        .await;
    assert!(holders >= 15, "holders {holders}");

    // ---- front ends: the split adds up to the venue's own volume.
    let venue = db
        .number(&format!(
            "SELECT sum(volume_quote_raw) FROM launchpad_venues_1d_v(chain \
             = {CHAIN}) WHERE family = 'pons_v2'"
        ))
        .await;
    let split = db
        .number(&format!(
            "SELECT sum(volume_quote_raw) FROM launchpad_frontend_volume_v(\
             chain = {CHAIN}, since = 0) WHERE family = 'pons_v2'"
        ))
        .await;
    assert!((venue - split).abs() < 1.0, "{venue} != {split}");
    assert!(
        db.count(&format!(
            "SELECT count() FROM launchpad_frontend_volume_v(chain = \
             {CHAIN}, since = 0) WHERE frontend = 'bundler'"
        ))
        .await
            >= 1
    );

    db.drop_database().await;
}

#[tokio::test]
#[ignore]
async fn a_purge_and_a_rebuild_equal_a_clean_index() {
    // The canonical chain keeps the launch and the graduation but has
    // FEWER trades: four snipes of block 66,679,545 never happened.
    let reorged: Vec<&RawTx> = fixtures::ALL.to_vec();
    let canonical: Vec<&RawTx> = fixtures::ALL
        .iter()
        .copied()
        .filter(|tx| {
            !std::ptr::eq(*tx, &fixtures::PONS_SNIPE_03)
                && !std::ptr::eq(*tx, &fixtures::PONS_SNIPE_04)
                && !std::ptr::eq(*tx, &fixtures::PONS_SNIPE_05)
                && !std::ptr::eq(*tx, &fixtures::PONS_SNIPE_06)
        })
        .collect();
    assert_eq!(canonical.len(), reorged.len() - 4);

    let fork: u64 = 66_679_545;
    let before: Vec<&RawTx> = canonical
        .iter()
        .copied()
        .filter(|tx| tx.chain != CHAIN || tx.block_number < fork)
        .collect();
    let after: Vec<&RawTx> = canonical
        .iter()
        .copied()
        .filter(|tx| tx.chain == CHAIN && tx.block_number >= fork)
        .collect();

    // --- the clean index
    let clean = TestDb::create("clean").await;
    clean.store(&rows_of(&canonical, 1, 0)).await;
    clean.trust_the_real_venues().await;

    // --- the reorged one: everything, then a purge, then the canonical
    // tail. Nothing is ever deleted.
    let db = TestDb::create("reorged").await;
    db.store(&rows_of(&reorged, 1, 0)).await;
    db.trust_the_real_venues().await;

    let epoch: u32 = 1;
    let from_ts = 1_789_780_286 - 1_789_780_286 % 86_400;
    for table in BASE_TABLES {
        let sql = tombstone_sql(table, CHAIN, fork, None, 2).expect(table);
        db.execute(&sql).await;
    }
    // `to_ts`: the exclusive end of the window the validity rule hides,
    // which must be the range the rebuild below covers.
    db.execute(&format!(
        "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, detected_at, \
         fork_block, old_head, depth, rows_tombstoned, reason) \
         VALUES ({CHAIN}, {epoch}, toDateTime({from_ts}), \
         toDateTime(1790000000), now(), {fork}, \
         {GRADUATION_BLOCK}, 12, 0, 'reorg')"
    ))
    .await;
    // `(fork, None)`: the block range the purge above removed. The
    // rebuild leaves it out by itself rather than trusting the tombstones
    // to be readable already (docs/design.md §2, "No read-your-writes");
    // the canonical tail below adds itself through the materialized view.
    for table in LAUNCHPADS_DERIVED {
        for sql in rebuild_statements(
            table,
            CHAIN,
            from_ts,
            1_790_000_000,
            epoch,
            (fork, None),
        ) {
            db.execute(&sql).await;
        }
    }
    db.store(&rows_of(&after, 3, epoch)).await;

    // Sanity: the tombstoned trades really are gone from the live rows and
    // still on disk (insert-only).
    assert!(
        db.count("SELECT count() FROM launchpad_trades").await
            > db.count("SELECT count() FROM launchpad_trades FINAL").await
    );
    let _ = before;

    // --- every table, every side table and every view must agree.
    for table in BASE_TABLES.iter().chain(SIDE_TABLES) {
        let source = format!(
            "SELECT * EXCEPT (_version, epoch) FROM {table} FINAL WHERE \
             is_deleted = 0"
        );
        assert_eq!(
            db.snapshot(&source).await,
            clean.snapshot(&source).await,
            "{table}"
        );
    }

    // Float64 sums are compared at Float32 precision: a rebuild adds the
    // surviving rows in ONE group, the materialized view added them
    // incrementally, and floating point addition is not associative. Every
    // integer, id and timestamp is compared exactly.
    let token = id_hex(TOKEN);
    let creator = id_hex(CREATOR);
    db.set(&cookbook_parameters(&token, &creator));
    clean.set(&cookbook_parameters(&token, &creator));
    let candles = |view: &str| {
        format!(
            "SELECT chain, token, emitter, bucket, toFloat32(open_raw), \
             toFloat32(high_raw), toFloat32(low_raw), toFloat32(close_raw), \
             trades, priced_trades, buys, toFloat32(volume_quote_raw), \
             toFloat32(volume_token_raw), \
             toFloat32(volume_quote_verified_raw), unique_traders, \
             toFloat32(curve_progress) FROM {view} WHERE chain = {CHAIN}"
        )
    };
    for view in [
        candles("launchpad_candles_1m_all_v"),
        candles("launchpad_candles_1h_all_v"),
        format!(
            "SELECT chain, family, emitter, bucket, trades, buys, \
             toFloat32(volume_quote_raw), \
             toFloat32(volume_quote_verified_raw), toFloat32(fees_raw), \
             unique_traders, unique_tokens FROM \
             launchpad_venue_trades_1d_v WHERE chain = {CHAIN}"
        ),
        format!(
            "SELECT * FROM launchpad_launches_1d_v WHERE chain = {CHAIN}"
        ),
        format!(
            "SELECT chain, family, emitter, bucket, graduations, \
             toFloat32(quote_in_raw), unique_tokens FROM \
             launchpad_graduations_1d_v WHERE chain = {CHAIN}"
        ),
        format!(
            "SELECT chain, family, emitter, recipient, kind, phase, \
             bucket, events, toFloat32(amount_raw) FROM \
             launchpad_creator_fees_1d_v WHERE chain = {CHAIN}"
        ),
        format!(
            "SELECT chain, family, emitter, bucket, launches, graduations, \
             trades, buys, toFloat32(volume_quote_raw), \
             toFloat32(volume_quote_verified_raw), toFloat32(fees_raw), \
             toFloat32(graduated_quote_raw), unique_traders, \
             unique_creators, trusted FROM \
             launchpad_venues_1d_all_v(chain = {CHAIN})"
        ),
        "SELECT token, family, emitter, curve, creator, launch_block, \
         launch_time, trades, buys, unique_traders, \
         toFloat32(volume_quote_raw), toFloat32(last_price_raw), \
         toFloat32(curve_progress), graduated, pool_id FROM \
         launchpad_token_v(chain = {chain:UInt64}, token = {token:String})"
            .to_owned(),
    ] {
        assert_eq!(
            db.snapshot(&view).await,
            clean.snapshot(&view).await,
            "{view}"
        );
    }

    // And the repaired index really lost the four trades.
    assert_eq!(
        db.count(
            "SELECT sum(trades) FROM launchpad_candles_1m_v(\
             chain = {chain:UInt64}, token = {token:String})",
        )
        .await,
        28
    );

    db.drop_database().await;
    clean.drop_database().await;
}

#[tokio::test]
#[ignore]
async fn a_forged_venue_moves_no_headline_number() {
    let db = TestDb::create("forgery").await;
    db.store(&rows_of(fixtures::ALL, 1, 0)).await;
    db.trust_the_real_venues().await;

    let headline = |view: &str| {
        format!(
            "SELECT round(sum(volume_quote_raw)) FROM {view}(chain = \
             {CHAIN}) WHERE family = 'pons_v2'"
        )
    };
    let honest_volume =
        db.number(&headline("launchpad_venues_1d_v")).await;
    let honest_launches = db
        .count(&format!(
            "SELECT count() FROM launchpad_new_launches_v(chain = {CHAIN}, \
             since = 0)"
        ))
        .await;
    assert!(honest_volume > 0.0 && honest_launches > 0);

    // A forged launch and a forged trade of 1e30 quote units, fully
    // corroborated by a token the forger also controls: the corroboration
    // is satisfied, only the emitter list is not.
    let forger = Address::repeat_byte(0x66);
    let fake_token = Address::repeat_byte(0x67);
    let fake_curve = Address::repeat_byte(0x68);
    let huge = U256::from(10u64).pow(U256::from(30u64));
    let place = |log_index: u32| Place {
        chain: CHAIN,
        block_number: 66_679_560,
        log_index,
        timestamp: 1_789_780_300,
        transaction_hash: B256::repeat_byte(0x99),
    };

    let logs = vec![
        fixtures::constructed_launch(
            place(0),
            forger,
            fake_token,
            fake_curve,
            forger,
            huge,
        ),
        fixtures::constructed_transfer(
            place(1),
            fake_token,
            fake_curve,
            forger,
            huge,
        ),
        fixtures::constructed_buy(
            place(2),
            fake_curve,
            forger,
            forger,
            huge,
            huge,
            U256::ZERO,
            U256::ZERO,
        ),
    ];

    let mut forged = decode(CHAIN, &logs);
    assert_eq!(forged.trades.len(), 1);
    // The forger DID move a real token through their own curve, so the
    // token leg is verified: corroboration bounds amounts, not identity.
    assert_eq!(forged.trades[0].token_verified, 1);
    forged.set_version(2);
    db.store(&forged).await;

    assert_eq!(
        db.number(&headline("launchpad_venues_1d_v")).await,
        honest_volume,
        "a forged venue changed the trusted volume"
    );
    assert_eq!(
        db.count(&format!(
            "SELECT count() FROM launchpad_new_launches_v(chain = {CHAIN}, \
             since = 0)"
        ))
        .await,
        honest_launches
    );
    assert!(
        db.count(&format!(
            "SELECT count() FROM launchpad_trusted_curves_v WHERE chain = \
             {CHAIN} AND curve = {}",
            id_literal(&format!("{fake_curve:?}"))
        ))
        .await
            == 0
    );

    // The exploration views DO show it, which is the point of having them.
    assert!(
        db.number(&headline("launchpad_venues_1d_all_v")).await
            > honest_volume
    );
    assert_eq!(
        db.count(&format!(
            "SELECT count() FROM launchpad_new_launches_all_v(chain = \
             {CHAIN}, since = 0) WHERE trusted = 0"
        ))
        .await,
        5
    );

    db.drop_database().await;
}

/// Review finding #6. Picking a token is not a trust decision: a forged
/// curve can emit `CurveBuy` naming a REAL token, and a forger who
/// predicts a token address can emit a `TokenLaunched` for it EARLIER
/// than the real venue did. Every token-scoped `_v` view must therefore
/// read exactly the same before and after those rows exist, and every
/// `_all_v` twin must show them.
#[tokio::test]
#[ignore]
async fn a_forged_curve_moves_no_token_page_number() {
    let db = TestDb::create("tokenpage").await;
    db.store(&rows_of(fixtures::ALL, 1, 0)).await;
    db.write("erc20_transfers", &transfers_of(fixtures::ALL, 1, 0)).await;
    db.trust_the_real_venues().await;

    let token = id_hex(TOKEN);
    let creator = id_hex(CREATOR);
    db.set(&cookbook_parameters(&token, &creator));

    // Every screen of the real token, before the forgery.
    let screens: Vec<&str> = vec![
        // The whole header row, so nothing in it can move unnoticed.
        "SELECT * FROM launchpad_token_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT * FROM launchpad_candles_1m_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT * FROM launchpad_candles_1h_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT * FROM launchpad_token_trades_v(chain = {chain:UInt64}, \
         token = {token:String}, from_block = {from_block:UInt64})",
        "SELECT * FROM launchpad_snipers_v(chain = {chain:UInt64}, \
         token = {token:String}, blocks = {blocks:UInt64})",
        "SELECT * FROM launchpad_token_holders_v(chain = {chain:UInt64}, \
         token = {token:String}, as_of_block = {as_of_block:UInt64})",
        // The feeds, which were already filtered - a regression guard.
        "SELECT * FROM launchpad_new_launches_v(chain = {chain:UInt64}, \
         since = {since:UInt32})",
        "SELECT * FROM launchpad_venues_1d_v(chain = {chain:UInt64})",
    ];
    let mut before = Vec::new();
    for screen in &screens {
        let rows = db.snapshot(screen).await;
        assert!(!rows.is_empty(), "nothing to protect: {screen}");
        before.push(rows);
    }

    // The exploration twins, which MUST move.
    let twins: Vec<&str> = vec![
        "SELECT * FROM launchpad_token_all_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT * FROM launchpad_token_trades_all_v(\
         chain = {chain:UInt64}, token = {token:String}, \
         from_block = {from_block:UInt64})",
        "SELECT * FROM launchpad_snipers_all_v(chain = {chain:UInt64}, \
         token = {token:String}, blocks = {blocks:UInt64})",
        "SELECT * FROM launchpad_token_holders_all_v(\
         chain = {chain:UInt64}, token = {token:String}, \
         as_of_block = {as_of_block:UInt64})",
    ];
    let mut twins_before = Vec::new();
    for twin in &twins {
        twins_before.push(db.snapshot(twin).await);
    }

    // ---- the forgery, all of it naming the REAL token.
    let real_token = address(TOKEN);
    let forger = Address::repeat_byte(0x77);
    let fake_curve = Address::repeat_byte(0x78);
    let fake_factory = Address::repeat_byte(0x79);
    let huge = U256::from(10u64).pow(U256::from(30u64));
    let place = |block: u64, log_index: u32| Place {
        chain: CHAIN,
        block_number: block,
        log_index,
        timestamp: 1_789_780_286,
        transaction_hash: B256::repeat_byte(0x88),
    };

    let logs = vec![
        // A mint of the real token to the forger, in the same
        // transaction as the forged launch: `initial_supply` is the
        // largest Transfer from the zero address, so this is what makes
        // everyone's share_of_initial_supply collapse.
        fixtures::constructed_transfer(
            place(LAUNCH_BLOCK - 3, 0),
            real_token,
            Address::ZERO,
            forger,
            huge,
        ),
        // A TokenLaunched for the real token, EARLIER than the real
        // launch: without the trust filter this wins every argMin, so
        // the header would show the forger as creator and `trusted` 0.
        fixtures::constructed_launch(
            place(LAUNCH_BLOCK - 3, 1),
            fake_factory,
            real_token,
            fake_curve,
            forger,
            huge,
        ),
        // A real movement of the real token through the forger's curve,
        // so the corroboration PASSES and the trade is `verified`.
        fixtures::constructed_transfer(
            place(LAUNCH_BLOCK - 2, 0),
            real_token,
            fake_curve,
            forger,
            huge,
        ),
        // ... and the buy itself: quote >= the graduation threshold, the
        // "this real token is about to graduate" lie.
        fixtures::constructed_buy(
            place(LAUNCH_BLOCK - 2, 1),
            fake_curve,
            forger,
            forger,
            huge,
            huge,
            U256::ZERO,
            U256::ZERO,
        ),
    ];

    let mut forged = decode(CHAIN, &logs);
    assert_eq!(forged.tokens.len(), 1);
    assert_eq!(forged.trades.len(), 1);
    assert_eq!(forged.trades[0].token, real_token, "it names the token");
    assert_eq!(forged.trades[0].token_verified, 1, "corroboration passes");
    assert!(forged.tokens[0].block_number < LAUNCH_BLOCK);
    assert_eq!(forged.tokens[0].initial_supply, huge, "a forged supply");
    forged.set_version(2);
    db.store(&forged).await;

    // A forged ERC-20 transfer of the real token would be the token
    // contract's own claim, so the holder BALANCES are left alone: what
    // the forgery attacks there is the supply it is divided by.
    for (screen, expected) in screens.iter().zip(&before) {
        assert_eq!(&db.snapshot(screen).await, expected, "{screen}");
    }

    // The twins see all of it - that is what they are for.
    let mut moved = 0;
    for (twin, was) in twins.iter().zip(&twins_before) {
        if &db.snapshot(twin).await != was {
            moved += 1;
        }
    }
    assert_eq!(moved, twins.len(), "an _all_v twin hid the forgery");

    // And the specific lies, spelled out against the trusted header.
    let page = |column: &str| {
        format!(
            "SELECT ifNull(toString({column}), '') FROM \
             launchpad_token_v(chain = {{chain:UInt64}}, \
             token = {{token:String}})"
        )
    };
    assert_eq!(db.text(&page("trades")).await, "32");
    assert_eq!(db.text(&page("trusted")).await, "1");
    assert_eq!(db.text(&page("curve_progress")).await, "1");
    assert_eq!(
        db.text(&page("launch_block")).await,
        LAUNCH_BLOCK.to_string(),
        "the earlier forged launch won the argMin"
    );
    assert_eq!(
        db.text(&page("hex(creator)")).await.to_lowercase(),
        id_hex(CREATOR)
    );

    // The untrusted twin shows every one of them instead.
    let all = |column: &str| {
        format!(
            "SELECT ifNull(toString({column}), '') FROM \
             launchpad_token_all_v(chain = {{chain:UInt64}}, \
             token = {{token:String}})"
        )
    };
    assert_eq!(db.text(&all("trusted")).await, "0");
    assert_eq!(db.text(&all("trades")).await, "33");
    assert_eq!(
        db.text(&all("launch_block")).await,
        (LAUNCH_BLOCK - 3).to_string()
    );

    db.drop_database().await;
}

/// Review round 3, item 1: the creator page had the forgery hole the token
/// page just had fixed, and its victim is a wallet that did nothing at
/// all. A launch names its creator in the event, so anyone can emit a
/// `TokenLaunched` naming a stranger: unfiltered it lands on that
/// stranger's page, never graduates, and so inflates `launches`, inflates
/// `died` and tanks `graduation_rate` - the serial-rugger signal the
/// screen exists to report. A forged `CurveBuy` on the same token moves
/// `trades` / `volume_quote_raw` / `last_trade_time`, and a forged fee
/// sweep naming the wallet as `recipient` inflates
/// `realised_creator_fees_raw`.
///
/// Both creator screens must therefore read byte for byte the same before
/// and after those rows exist, and both `_all_v` twins must show them.
#[tokio::test]
#[ignore]
async fn a_forged_launch_moves_no_creator_page_number() {
    let db = TestDb::create("creatorpage").await;
    db.store(&rows_of(fixtures::ALL, 1, 0)).await;
    db.trust_the_real_venues().await;

    let token = id_hex(TOKEN);
    let creator = id_hex(CREATOR);
    db.set(&cookbook_parameters(&token, &creator));

    // The two creator screens, whole rows, before the forgery.
    let screens: Vec<&str> = vec![
        "SELECT * FROM launchpad_creator_v(chain = {chain:UInt64}, \
         creator = {creator:String}, as_of = {now:UInt32}, \
         dead_after = {dead_after:UInt32})",
        "SELECT * FROM launchpad_creator_tokens_v(chain = {chain:UInt64}, \
         creator = {creator:String}, as_of = {now:UInt32}, \
         dead_after = {dead_after:UInt32})",
    ];
    let mut before = Vec::new();
    for screen in &screens {
        let rows = db.snapshot(screen).await;
        assert!(!rows.is_empty(), "nothing to protect: {screen}");
        before.push(rows);
    }

    // The exploration twins, which MUST move.
    let twins: Vec<&str> = vec![
        "SELECT * FROM launchpad_creator_all_v(chain = {chain:UInt64}, \
         creator = {creator:String}, as_of = {now:UInt32}, \
         dead_after = {dead_after:UInt32})",
        "SELECT * FROM launchpad_creator_tokens_all_v(\
         chain = {chain:UInt64}, creator = {creator:String}, \
         as_of = {now:UInt32}, dead_after = {dead_after:UInt32})",
    ];
    let mut twins_before = Vec::new();
    for twin in &twins {
        twins_before.push(db.snapshot(twin).await);
    }

    let header = |view: &str, column: &str| {
        format!(
            "SELECT ifNull(toString({column}), '') FROM {view}(\
             chain = {{chain:UInt64}}, creator = {{creator:String}}, \
             as_of = {{now:UInt32}}, dead_after = {{dead_after:UInt32}})"
        )
    };
    let honest_launches =
        db.text(&header("launchpad_creator_v", "launches")).await;
    let honest_rate =
        db.text(&header("launchpad_creator_v", "graduation_rate")).await;
    let honest_fees = db
        .text(&header("launchpad_creator_v", "realised_creator_fees_raw"))
        .await;
    assert_eq!(honest_launches, "1");
    assert_eq!(honest_rate, "1", "the real launch graduated");

    // ---- the forgery, all of it naming the REAL creator.
    let real_creator = address(CREATOR);
    let forger = Address::repeat_byte(0x55);
    let fake_token = Address::repeat_byte(0x56);
    let fake_curve = Address::repeat_byte(0x57);
    let fake_factory = Address::repeat_byte(0x58);
    let huge = U256::from(10u64).pow(U256::from(30u64));
    let place = |log_index: u32| Place {
        chain: CHAIN,
        block_number: 66_679_570,
        log_index,
        timestamp: 1_789_780_400,
        transaction_hash: B256::repeat_byte(0x55),
    };

    let logs = vec![
        // A launch of a token the forger controls, crediting the REAL
        // creator. It never graduates, so unfiltered it is a second
        // launch, a `died` and a halved graduation_rate on their page.
        fixtures::constructed_launch(
            place(0),
            fake_factory,
            fake_token,
            fake_curve,
            real_creator,
            huge,
        ),
        // A corroborated trade on it: trades and volume_quote_raw.
        fixtures::constructed_transfer(
            place(1),
            fake_token,
            fake_curve,
            forger,
            huge,
        ),
        fixtures::constructed_buy(
            place(2),
            fake_curve,
            forger,
            forger,
            huge,
            huge,
            U256::ZERO,
            U256::ZERO,
        ),
    ];

    let mut forged = decode(CHAIN, &logs);
    assert_eq!(forged.tokens.len(), 1);
    assert_eq!(forged.trades.len(), 1);
    assert_eq!(
        forged.tokens[0].creator, real_creator,
        "the launch names the real creator"
    );
    forged.set_version(2);
    db.store(&forged).await;

    // ... and a fee sweep from the forger's curve paying the real
    // creator, which is what realised_creator_fees_raw sums. No fixture
    // constructor emits one, so it goes in as the row a decoder would
    // have written.
    db.execute(&format!(
        "INSERT INTO launchpad_creator_fees (chain, block_number, timestamp, \
         tx_id, tx_index, ordinal, component, family, emitter, token, pool_id, \
         phase, kind, recipient, recipient_known, quote_token, amount, tx_from, \
         epoch, _version, is_deleted) VALUES ({CHAIN}, 66679570, \
         toDateTime(1789780400), unhex('55'), 0, 3, 0, 'pons_v2', {emitter}, \
         {token_id}, toFixedString('', 32), 'curve', 'creator', {recipient}, 1, \
         toFixedString('', 32), toUInt256('{huge}'), {recipient}, 0, 2, 0)",
        emitter = id_literal(&format!("{fake_curve:?}")),
        token_id = id_literal(&format!("{fake_token:?}")),
        recipient = id_literal(CREATOR),
    ))
    .await;

    // THE ASSERTION: neither screen moved.
    for (screen, expected) in screens.iter().zip(&before) {
        assert_eq!(&db.snapshot(screen).await, expected, "{screen}");
    }

    // Both twins see all of it - that is what they are for.
    let mut moved = 0;
    for (twin, was) in twins.iter().zip(&twins_before) {
        if &db.snapshot(twin).await != was {
            moved += 1;
        }
    }
    assert_eq!(moved, twins.len(), "an _all_v twin hid the forgery");

    // The specific lies, spelled out against the trusted header ...
    assert_eq!(
        db.text(&header("launchpad_creator_v", "launches")).await,
        honest_launches
    );
    assert_eq!(
        db.text(&header("launchpad_creator_v", "graduation_rate")).await,
        honest_rate
    );
    assert_eq!(db.text(&header("launchpad_creator_v", "died")).await, "0");
    assert_eq!(
        db.text(&header(
            "launchpad_creator_v",
            "realised_creator_fees_raw"
        ))
        .await,
        honest_fees,
        "a forged fee sweep reached the creator's realised fees"
    );
    assert_eq!(
        db.count(
            "SELECT count() FROM launchpad_creator_tokens_v(\
             chain = {chain:UInt64}, creator = {creator:String}, \
             as_of = {now:UInt32}, dead_after = {dead_after:UInt32})"
        )
        .await,
        1
    );

    // ... and the untrusted twin showing every one of them instead.
    assert_eq!(
        db.text(&header("launchpad_creator_all_v", "launches")).await,
        "2"
    );
    assert_eq!(
        db.text(&header("launchpad_creator_all_v", "graduation_rate"))
            .await,
        "0.5",
        "the forged launch halved the rate in the twin"
    );
    assert_eq!(
        db.text(&header("launchpad_creator_all_v", "died")).await,
        "1"
    );
    assert_eq!(
        db.text(&header("launchpad_creator_all_v", "trusted_launches"))
            .await,
        "1"
    );
    assert!(
        db.number(
            "SELECT toFloat64(realised_creator_fees_raw) FROM \
             launchpad_creator_all_v(chain = {chain:UInt64}, \
             creator = {creator:String}, as_of = {now:UInt32}, \
             dead_after = {dead_after:UInt32})"
        )
        .await
            > db.number(
                "SELECT toFloat64(realised_creator_fees_raw) FROM \
                 launchpad_creator_v(chain = {chain:UInt64}, \
                 creator = {creator:String}, as_of = {now:UInt32}, \
                 dead_after = {dead_after:UInt32})"
            )
            .await,
        "the twin did not show the forged fee sweep"
    );
    assert_eq!(
        db.count(
            "SELECT count() FROM launchpad_creator_tokens_all_v(\
             chain = {chain:UInt64}, creator = {creator:String}, \
             as_of = {now:UInt32}, dead_after = {dead_after:UInt32}) \
             WHERE trusted = 0"
        )
        .await,
        1
    );

    db.drop_database().await;
}

/// Review round 3, item 3. An id parameter is hex WITHOUT `0x`, and the
/// views pad a 40 character one. An EMPTY string went through the same
/// path: `unhex('')` is the empty string and `toFixedString('', 32)` is 32
/// ZERO BYTES, which in this module is a real, populated bucket - the
/// trades whose token leg stayed unverified. So an empty token parameter,
/// which is exactly what a UI sends when its field is unset, returned that
/// bucket instead of nothing. A truncated 39 or 63 character id padded the
/// same way.
///
/// Every parameterized view now carries `AND length({id}) IN (40, 64)`, so
/// a wrong length matches NOTHING while a valid one is untouched.
#[tokio::test]
#[ignore]
async fn an_empty_or_wrong_length_id_parameter_matches_nothing() {
    let db = TestDb::create("emptyid").await;
    db.store(&rows_of(fixtures::ALL, 1, 0)).await;
    db.write("erc20_transfers", &transfers_of(fixtures::ALL, 1, 0)).await;
    db.trust_the_real_venues().await;

    // A curve trade whose token leg stayed unverified and whose family
    // does not name the token: it lands under the 32 zero bytes (0030),
    // which is what an empty parameter used to return. The real fixtures
    // have none, so one is planted - without it this test proves nothing.
    db.execute(&format!(
        "INSERT INTO launchpad_trades (chain, block_number, timestamp, tx_id, \
         tx_index, ordinal, family, emitter, token, token_verified, \
         quote_token, quote_verified, side, trader, caller, token_amount, \
         quote_amount, fee_amount, tax_amount, progress_wad, graduating, \
         sole_unverified_quote, tx_from, tx_to, tx_value, epoch, _version, \
         is_deleted) VALUES ({CHAIN}, 66679600, toDateTime(1789780500), \
         unhex('aa'), 0, 0, 'flap_portal', {emitter}, toFixedString('', 32), 0, \
         toFixedString('', 32), 0, 'buy', {trader}, toFixedString('', 32), 1, 1, \
         0, 0, 0, 0, 0, {trader}, toFixedString('', 32), 0, 0, 3, 0)",
        emitter = id_literal(FLAP_RH),
        trader = id_literal(BUNDLER),
    ))
    .await;

    // The premise: the 32 zero bytes really are a populated bucket here,
    // so "matches nothing" is a filter doing work, not an empty table.
    assert!(
        db.count(&format!(
            "SELECT count() FROM launchpad_trades_by_token FINAL WHERE \
             chain = {CHAIN} AND token = toFixedString('', 32) \
             AND is_deleted = 0"
        ))
        .await
            > 0,
        "the unverified-token bucket is empty: this test proves nothing"
    );

    let token = id_hex(TOKEN);
    let creator = id_hex(CREATOR);

    // Every parameterized view, with the id parameter it scopes on.
    let token_views: Vec<&str> = vec![
        "SELECT count() FROM launchpad_candles_1m_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT count() FROM launchpad_candles_1h_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT count() FROM launchpad_token_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT count() FROM launchpad_token_all_v(chain = {chain:UInt64}, \
         token = {token:String})",
        "SELECT count() FROM launchpad_token_trades_v(chain = {chain:UInt64}, \
         token = {token:String}, from_block = {from_block:UInt64})",
        "SELECT count() FROM launchpad_token_trades_all_v(\
         chain = {chain:UInt64}, token = {token:String}, \
         from_block = {from_block:UInt64})",
        "SELECT count() FROM launchpad_token_holders_v(chain = {chain:UInt64}, \
         token = {token:String}, as_of_block = {as_of_block:UInt64})",
        "SELECT count() FROM launchpad_token_holders_all_v(\
         chain = {chain:UInt64}, token = {token:String}, \
         as_of_block = {as_of_block:UInt64})",
        "SELECT count() FROM launchpad_snipers_v(chain = {chain:UInt64}, \
         token = {token:String}, blocks = {blocks:UInt64})",
        "SELECT count() FROM launchpad_snipers_all_v(chain = {chain:UInt64}, \
         token = {token:String}, blocks = {blocks:UInt64})",
    ];
    let creator_views: Vec<&str> = vec![
        "SELECT count() FROM launchpad_creator_v(chain = {chain:UInt64}, \
         creator = {creator:String}, as_of = {now:UInt32}, \
         dead_after = {dead_after:UInt32})",
        "SELECT count() FROM launchpad_creator_all_v(chain = {chain:UInt64}, \
         creator = {creator:String}, as_of = {now:UInt32}, \
         dead_after = {dead_after:UInt32})",
        "SELECT count() FROM launchpad_creator_tokens_v(\
         chain = {chain:UInt64}, creator = {creator:String}, \
         as_of = {now:UInt32}, dead_after = {dead_after:UInt32})",
        "SELECT count() FROM launchpad_creator_tokens_all_v(\
         chain = {chain:UInt64}, creator = {creator:String}, \
         as_of = {now:UInt32}, dead_after = {dead_after:UInt32})",
    ];

    // A real id still answers: the guard must not have broken the screens.
    db.set(&cookbook_parameters(&token, &creator));
    for sql in token_views.iter().chain(&creator_views) {
        assert!(
            db.count(sql).await > 0,
            "a valid id returned nothing: {sql}"
        );
    }

    // ... and every wrong length answers with nothing at all.
    for bad in [
        "",           // the empty field of a UI
        &token[..39], // one character short of an address
        &token[..63], // one short of a 32 byte id
        "00",         // a stray byte
    ] {
        db.set(&cookbook_parameters(bad, bad));
        for sql in token_views.iter().chain(&creator_views) {
            assert_eq!(
                db.count(sql).await,
                0,
                "id {bad:?} matched rows: {sql}"
            );
        }
    }

    // The empty case, spelled out: it used to return the zero bucket.
    db.set(&cookbook_parameters("", ""));
    assert_eq!(
        db.count(
            "SELECT count() FROM launchpad_token_trades_all_v(\
             chain = {chain:UInt64}, token = {token:String}, \
             from_block = {from_block:UInt64})"
        )
        .await,
        0,
        "an empty token parameter still returns the unverified bucket"
    );

    db.drop_database().await;
}

#[tokio::test]
#[ignore]
async fn hostile_amounts_do_not_wrap() {
    let db = TestDb::create("hostile").await;
    db.trust_the_real_venues().await;

    let curve = address(CURVE);
    let token = address(TOKEN);
    let max = U256::MAX;
    let place = |log_index: u32| Place {
        chain: CHAIN,
        block_number: LAUNCH_BLOCK,
        log_index,
        timestamp: 1_789_780_286,
        transaction_hash: B256::repeat_byte(0x11),
    };

    // CONSTRUCTED: three buys of 2^256-1 on the real curve.
    let mut logs = vec![fixtures::constructed_launch(
        place(0),
        address(PONS_FACTORY),
        token,
        curve,
        address(CREATOR),
        max,
    )];
    for index in 0..3u32 {
        logs.push(fixtures::constructed_transfer(
            place(1 + index * 2),
            token,
            curve,
            Address::repeat_byte(0x22),
            max,
        ));
        logs.push(fixtures::constructed_buy(
            place(2 + index * 2),
            curve,
            Address::repeat_byte(0x22),
            Address::repeat_byte(0x22),
            max,
            max,
            U256::ZERO,
            U256::ZERO,
        ));
    }

    let mut rows = decode(CHAIN, &logs);
    rows.set_version(1);
    assert_eq!(rows.trades.len(), 3);
    db.store(&rows).await;

    // The exact integer survives.
    assert_eq!(
        db.text("SELECT toString(max(quote_amount)) FROM launchpad_trades FINAL")
            .await,
        max.to_string()
    );
    // And the aggregate sums Float64, so three of them are 3 * 1.157e77 -
    // a raw UInt256 sum would have wrapped to something small.
    let volume = db
        .number(&format!(
            "SELECT sum(volume_quote_raw) FROM launchpad_venue_trades_1d_v \
             WHERE chain = {CHAIN}"
        ))
        .await;
    assert!(volume > 3.4e77, "volume {volume}");
    assert!(volume.is_finite());

    // The price of max/max is 1, not a division blow-up.
    db.set(&cookbook_parameters(&id_hex(TOKEN), &id_hex(CREATOR)));
    let close = db
        .number(
            "SELECT ifNull(close_raw, 0.) FROM launchpad_candles_1m_v(\
             chain = {chain:UInt64}, token = {token:String})",
        )
        .await;
    assert_eq!(close, 1.0);

    db.drop_database().await;
}
