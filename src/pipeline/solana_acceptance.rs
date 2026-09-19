//! THE PROOF for Solana: the real loop (`solana::run_with`, the same
//! function `indexer run --chain solana` calls) against an in-memory chain
//! and a REAL ClickHouse.
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@127.0.0.1:8123/solana_test \
//!   cargo test --lib pipeline::solana_acceptance -- --ignored
//! ```
//!
//! The database in the url is only a PREFIX: every scenario owns (drops and
//! migrates) its own `<prefix>_<scenario>_test`, so they run in parallel
//! without sharing a table. The prefix must end in `_test`.
//!
//! The configuration of every scenario comes out of the REAL command line
//! parser, so "`--chain solana`" means the flag, not a constant.
//!
//! | | What it proves |
//! |---|---|
//! | (a) | a canned range WITH SKIPPED SLOTS produces swaps, `sol_slots`, checkpoints and candles, and `verify` calls it consistent |
//! | (b) | a flush killed after its children and before `sol_slots` heals on restart and ends up equal to a clean index, aggregates included |
//! | (c) | a retried flush counts ONCE, in the candles as well as in the base table |
//! | (d) | a continuity break trips the tripwire and nothing further is written |
//! | (e) | a second process on the same chain is refused |
//! | (f) | `verify` reports consistent and inconsistent correctly |

use super::*;
use crate::{
    configs::Command,
    db::{migrate, next_version, DatabaseParams, FlushKey},
    pipeline::{
        solana::{
            run_with, SlotPage, SlotSource, SolanaRuntime, Tripwire,
            FIRST_SERVED_SLOT,
        },
        solana_store::{SolanaReorgStore, COMMIT_MARKER},
        solana_verify,
        solana_writer::{store_children, SvmBatch},
    },
    reorg::ReorgStore,
    svm::derived::SOL_CANDLE_VIEWS,
    svm::{self, fixtures, models::SOLANA_CHAIN, SvmSlotBatch},
};
use anyhow::Result;
use clickhouse::Client;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

const CHAIN: u64 = SOLANA_CHAIN;
const TOKEN: &str = "00000000-0000-0000-0000-000000000000";

/// 2026-01-01 00:00:00 UTC, a day boundary so the candle buckets of the
/// scenarios are whole.
const BASE_TIMESTAMP: u32 = 1_767_225_600;

/// Slots are two hours apart in these scenarios. Nothing in the pipeline
/// reads the slot RATE - it follows the server's cursor - and stretching
/// the timestamps is what lets a 40-slot chain span whole UTC days, which
/// is what the candle and `verify` checks need.
const SECONDS_PER_SLOT: u32 = 7_200;

/// The first slot of the test chain. Above `FIRST_SERVED_SLOT`, because a
/// start below it is refused (and there is a test for that).
const FIRST_SLOT: u64 = FIRST_SERVED_SLOT + 1_000;

/// Slots the fake server serves per metered query, so the scenarios
/// exercise the cursor-following loop rather than one big answer.
const SERVED_PER_QUERY: u64 = 7;

/// The launchpad aggregate views a heal has to reproduce exactly.
///
/// The CANDLES are compared through their `_all_v` form on purpose: the
/// plain `_v` ones additionally restrict to emitters an operator listed in
/// `launchpad_trusted_emitters`, and these scenarios seed none, so `_v`
/// would be empty on both sides and prove nothing. `_all_v` still applies
/// the reorg validity rule, which is the thing under test.
const LAUNCHPAD_VIEWS: &[&str] = &[
    "launchpad_candles_1m_all_v",
    "launchpad_candles_1h_all_v",
    "launchpad_venue_trades_1d_v",
    "launchpad_launches_1d_v",
    "launchpad_graduations_1d_v",
    "launchpad_creator_fees_1d_v",
];

// ------------------------------------------------------------- the chain

/// An in-memory Solana: a list of PRODUCED slots (the others are skipped)
/// whose `block_height` and parent chain close, exactly as the real chain's
/// do.
#[derive(Clone)]
struct TestChain {
    /// Produced slots, ascending. Skipped slots are simply absent.
    slots: Arc<Vec<SvmSlotBatch>>,
    /// Exclusive: the head the fake `/height` reports.
    head: u64,
    /// `(head, calls)`: the first `calls` reads of `/height` report this
    /// lower head instead, so ONE process makes two passes with the
    /// boundary exactly there - which is what a head follower does every
    /// few seconds, and the only situation the carried anchor is about.
    staged_head: Option<(u64, u64)>,
    head_calls: Arc<AtomicU64>,
    /// Metered queries served, so a scenario can assert the loop is not
    /// spinning.
    queries: Arc<AtomicU64>,
}

/// `SvmSlotBatch` is not `Clone` in the module, and teaching it to be would
/// be an edit to `src/svm/mod.rs` that this work has no other reason to
/// make. Copying it here costs nothing and keeps the change list honest.
fn copy(batch: &SvmSlotBatch) -> SvmSlotBatch {
    SvmSlotBatch {
        slot: batch.slot,
        blockhash: batch.blockhash,
        parent_slot: batch.parent_slot,
        parent_blockhash: batch.parent_blockhash,
        block_height: batch.block_height,
        timestamp: batch.timestamp,
        transactions: batch.transactions.clone(),
    }
}

fn copy_all(batches: &[SvmSlotBatch]) -> Vec<SvmSlotBatch> {
    batches.iter().map(copy).collect()
}

fn hash(slot: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&slot.to_be_bytes());
    out[8] = 0xab;
    out
}

/// A chain of `count` produced slots starting at [`FIRST_SLOT`], with a
/// realistic skip pattern: every fifth slot integer produces no block.
///
/// `block_height` counts PRODUCED BLOCKS, so it increments by exactly one
/// per produced slot however many integers were skipped. Getting that wrong
/// in the fake would make the tripwire fire on a healthy chain, which is
/// precisely the bug the witness exists to avoid.
fn chain(count: u64) -> TestChain {
    /// `block_height` of the first produced slot. Any value does; what
    /// matters is that it increments by one per PRODUCED block.
    const FIRST_HEIGHT: u64 = 900_000;

    let recorded = fixtures::all();
    let mut slots = Vec::new();
    let mut slot = FIRST_SLOT;
    let mut previous = hash(0);

    for (height, index) in (FIRST_HEIGHT..).zip(0..count) {
        // Skip a slot integer now and then: no block, no row, no gap.
        if index % 5 == 4 {
            slot += 1;
        }

        let transactions =
            match recorded.get(index as usize % recorded.len()) {
                // One recorded mainnet transaction per produced slot, so the
                // decoder really runs and the candles really have prices.
                Some(fixture) if index % 2 == 0 => {
                    let mut tx = fixture.transaction.clone();
                    tx.slot = slot;
                    tx.tx_index = 0;
                    vec![tx]
                }
                // The other half are slots that matched nothing: they still get
                // a commit marker, and that is the point.
                _ => Vec::new(),
            };

        slots.push(SvmSlotBatch {
            slot,
            blockhash: hash(slot),
            parent_slot: slots
                .last()
                .map_or(slot - 1, |s: &SvmSlotBatch| s.slot),
            parent_blockhash: previous,
            block_height: height,
            timestamp: BASE_TIMESTAMP + (index as u32) * SECONDS_PER_SLOT,
            transactions,
        });

        previous = hash(slot);
        slot += 1;
    }

    let head = slots.last().map_or(FIRST_SLOT, |s| s.slot + 1);

    TestChain {
        slots: Arc::new(slots),
        head,
        staged_head: None,
        head_calls: Arc::new(AtomicU64::new(0)),
        queries: Arc::new(AtomicU64::new(0)),
    }
}

impl TestChain {
    /// The same chain with the `block_height` of the slot at `index`
    /// jumped: a produced block is missing. This is what the tripwire has
    /// to catch, and it is invisible to any `slot + 1` rule.
    fn with_height_break(&self, index: usize) -> Self {
        let mut slots = copy_all(&self.slots);
        for slot in slots.iter_mut().skip(index) {
            slot.block_height += 3;
        }
        Self { slots: Arc::new(slots), ..self.clone() }
    }

    /// The same chain with the parent hash of the slot at `index` replaced:
    /// the heights still chain, but it is a DIFFERENT block.
    fn with_parent_hash_break(&self, index: usize) -> Self {
        let mut slots = copy_all(&self.slots);
        slots[index].parent_blockhash = [0x99; 32];
        Self { slots: Arc::new(slots), ..self.clone() }
    }

    /// The same chain whose `/height` reports `staged` for the first
    /// `calls` reads: the process then indexes up to `staged`, asks again,
    /// and makes a SECOND pass that starts exactly where the first
    /// stopped.
    fn with_staged_head(&self, staged: u64, calls: u64) -> Self {
        Self {
            staged_head: Some((staged, calls)),
            head_calls: Arc::new(AtomicU64::new(0)),
            ..self.clone()
        }
    }

    fn slot_at(&self, index: usize) -> u64 {
        self.slots[index].slot
    }

    fn produced_slots(&self) -> u64 {
        self.slots.len() as u64
    }
}

impl SlotSource for TestChain {
    async fn head(&self) -> Result<u64> {
        if let Some((staged, calls)) = self.staged_head {
            if self.head_calls.fetch_add(1, Ordering::Relaxed) < calls {
                return Ok(staged);
            }
        }

        Ok(self.head)
    }

    async fn fetch(&self, from: u64, to: u64) -> Result<SlotPage> {
        self.queries.fetch_add(1, Ordering::Relaxed);

        // The real server truncates at its own execution budget and says
        // where it stopped; the fake truncates at a fixed width so the
        // scenarios exercise the cursor-following loop.
        let next_slot = to.min(from + SERVED_PER_QUERY);

        let batches: Vec<SvmSlotBatch> = self
            .slots
            .iter()
            .filter(|slot| slot.slot >= from && slot.slot < next_slot)
            .map(copy)
            .collect();

        Ok(SlotPage { next_slot, batches, budget: Default::default() })
    }
}

// ------------------------------------------------------------ the harness

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

    /// The configuration exactly as the binary would parse it - including
    /// `--chain solana` going through the real value parser.
    fn config(&self, flags: &[&str]) -> Config {
        let mut argv = vec![
            "indexer",
            "--chain",
            "solana",
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

    /// Indexes `[FIRST_SLOT, end)` and stops (`--end-block`).
    async fn index_until(
        &self,
        chain: &TestChain,
        end: u64,
    ) -> Result<()> {
        let start = FIRST_SLOT.to_string();
        let end = end.to_string();
        let config = self.config(&[
            "--start-block",
            &start,
            "--end-block",
            &end,
            "--flush-interval-ms",
            "200",
        ]);

        tokio::time::timeout(
            Duration::from_secs(300),
            run_with(config, runtime(chain.clone())),
        )
        .await
        .expect("the Solana pipeline did not finish in time")
    }

    /// Everything a reader can see, as strings: every BLOCK SCOPED `sol_*`
    /// table (FINAL, without the per-flush stamps) and every candle view.
    ///
    /// `sol_tokens` is deliberately left out, exactly as the EVM snapshot
    /// leaves out `tokens`: it is chain state, not slot data, no purge ever
    /// touches it (a mint's decimals do not change with a fork) and it is
    /// therefore not part of "the index of this slot range". Its `program`
    /// column is additionally best-effort - see the note on
    /// `sol_tokens_program_is_best_effort` below.
    async fn snapshot(&self) -> BTreeMap<String, Vec<String>> {
        let mut names: Vec<(&str, bool)> =
            svm::BASE_TABLES.iter().map(|table| (*table, true)).collect();
        // The SHARED launchpad tables the Solana decoder writes, their
        // side tables, and the Solana-only holder table: all block scoped,
        // so a heal has to bring them back identical too.
        names.extend(
            svm::SHARED_BASE_TABLES.iter().map(|table| (*table, true)),
        );
        names.extend(
            crate::launchpads::SIDE_TABLES.iter().map(|t| (*t, true)),
        );
        // `sol_token_balances` comes in through `svm::BASE_TABLES`: since
        // round 4 it is an append log of observations, purged like every
        // other child, so a healed index must match a clean one here too.

        names.extend(SOL_CANDLE_VIEWS.iter().map(|view| (*view, false)));
        names.extend(LAUNCHPAD_VIEWS.iter().map(|view| (*view, false)));

        let mut snapshot = BTreeMap::new();

        for (name, is_table) in names {
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

    /// Every known mint with its decimals, base58 for a readable failure.
    /// Deliberately WITHOUT `program`: see [`Self::snapshot`].
    async fn mints(&self) -> Vec<(String, u8)> {
        self.db
            .db
            .query(&format!(
                "SELECT base58Encode(mint), decimals FROM sol_tokens \
                 FINAL WHERE chain = {CHAIN} ORDER BY mint"
            ))
            .fetch_all()
            .await
            .unwrap()
    }

    /// `verify` once the report stops saying "consistent".
    ///
    /// ClickHouse gives no read-your-writes guarantee (docs/design.md §2):
    /// a test that hand-writes a broken row and verifies in the next
    /// breath can read the state from just before its own insert. That is
    /// a property of the database, not of the check, so the test waits for
    /// its own write instead of pretending the race does not exist.
    async fn verify_inconsistent(
        &self,
    ) -> solana_verify::SolanaVerifyReport {
        let mut report = self.verify().await;

        for _ in 0..200 {
            if !report.is_consistent() {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            report = self.verify().await;
        }

        panic!("verify still reports a consistent index:\n{report}");
    }

    /// Exactly what `indexer verify --chain solana` does with no flags:
    /// the start comes from the stored coverage floor, not from a constant
    /// this test happens to know.
    async fn verify(&self) -> solana_verify::SolanaVerifyReport {
        solana_verify::verify(&self.db, None, 0).await.unwrap()
    }

    async fn assert_consistent(&self) {
        let report = self.verify().await;
        assert!(report.is_consistent(), "{report}");
    }

    /// Writes the coverage floor a real start would write, for the one
    /// scenario that builds its database by hand instead of running the
    /// pipeline. Every check reads the floor now (docs/design.md section
    /// 16), so a database without one is not a database this indexer ever
    /// produces.
    async fn set_floor(&self, slot: u64) {
        crate::coverage::store::set_if_absent(
            &self.db,
            &crate::pipeline::lease::Fence::open(),
            crate::coverage::store::Floor {
                block: slot,
                timestamp: 0,
                reason: crate::coverage::store::Reason::StartBlock,
            },
        )
        .await
        .unwrap();
    }

    /// Removes `[from, to)` from the live checkpoint tiling WITHOUT
    /// touching a single row of data: every overlapping checkpoint is
    /// tombstoned and the parts of it outside the range are re-inserted.
    ///
    /// This is exactly the state a flush whose `sol_slots` insert landed
    /// and whose checkpoint insert did not leaves behind - and also what a
    /// purge that died between tombstoning a checkpoint and re-inserting
    /// its surviving remainder leaves. The slots are stored, and nothing
    /// claims them.
    async fn unclaim(&self, from: u64, to: u64) {
        let live: Vec<(u64, u64, u32)> = self
            .db
            .db
            .query(&format!(
                "SELECT from_block, to_block, epoch FROM checkpoints FINAL \
                 WHERE chain = {CHAIN} AND to_block > {from} \
                 AND from_block < {to}"
            ))
            .fetch_all()
            .await
            .unwrap();
        assert!(!live.is_empty(), "nothing claims [{from}, {to}) already");

        let version = next_version();
        let mut values = Vec::new();

        for (f, t, epoch) in live {
            values.push(format!(
                "({CHAIN}, {f}, {t}, {epoch}, {version}, 1)"
            ));
            if f < from {
                values.push(format!(
                    "({CHAIN}, {f}, {from}, {epoch}, {version}, 0)"
                ));
            }
            if t > to {
                values.push(format!(
                    "({CHAIN}, {to}, {t}, {epoch}, {version}, 0)"
                ));
            }
        }

        self.db
            .db
            .query(&format!(
                "INSERT INTO checkpoints (chain, from_block, to_block, \
                 epoch, _version, is_deleted) VALUES {}",
                values.join(", ")
            ))
            .execute()
            .await
            .unwrap();

        // No read-your-writes: wait until the hole is really visible,
        // otherwise the run that follows may not see it and the test would
        // pass for the wrong reason.
        let store = SolanaReorgStore::new(self.db.clone());
        for _ in 0..200 {
            let tiling = store
                .checkpoint_tiling(CHAIN, FIRST_SLOT, None)
                .await
                .unwrap();
            if crate::db::ranges::contiguous_until(FIRST_SLOT, tiling)
                <= from
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        panic!("the unclaimed range [{from}, {to}) never became a hole");
    }

    /// Adds ONE swap whose pool the decoder could not name: the newest
    /// stored swap below `before` that is at or after `not_before`,
    /// copied with an empty `pool_id` and a free ordinal.
    ///
    /// Such rows are ordinary on Solana - five of the ten venues publish
    /// no pool key of their own, and the decoder now stores the trade with
    /// an empty key rather than the program-wide vault authority it used
    /// to invent (review round 4, B3). Nothing in the recorded fixtures
    /// produces one, so the scenarios that need one make it here, exactly
    /// as the decoder would: same trade, no pool.
    ///
    /// Returns the slot it copied.
    async fn add_unnamed_pool_swap(
        &self,
        not_before: u32,
        before: u64,
    ) -> u64 {
        let slot: u64 = self
            .count(&format!(
                "SELECT toUInt64(max(block_number)) FROM sol_dex_swaps \
                 FINAL WHERE chain = {CHAIN} AND block_number < {before} \
                 AND timestamp >= toDateTime({not_before})"
            ))
            .await;
        assert!(slot > 0, "no stored swap to copy");

        // The row keeps the flush `_version` it was copied from: it lands
        // on a sorting key of its own (a free `ordinal`), so it replaces
        // nothing. The `REPLACE` sits OUTSIDE the subquery on purpose -
        // its aliases are visible to a `WHERE` next to it, and a
        // `pool_id != ''` there would then filter out the very row it
        // just emptied.
        self.db
            .db
            .query(&format!(
                "INSERT INTO sol_dex_swaps SELECT * REPLACE ( \
                   toFixedString('', 32) AS pool_id, \
                   toUInt64(4095) AS ordinal) \
                 FROM (SELECT * FROM sol_dex_swaps FINAL \
                   WHERE chain = {CHAIN} AND block_number = {slot} \
                   AND pool_id != toFixedString('', 32) LIMIT 1)"
            ))
            .execute()
            .await
            .unwrap();

        // No read-your-writes: the run that follows must see the row, or
        // the test would pass without ever exercising it.
        for _ in 0..200 {
            let live = self
                .count(&format!(
                    "SELECT toUInt64(count()) FROM sol_dex_swaps FINAL \
                     WHERE chain = {CHAIN} AND pool_id = \
                     toFixedString('', 32) AND is_deleted = 0"
                ))
                .await;
            if live == 1 {
                return slot;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        panic!("the unnamed-pool swap never became visible");
    }
}

/// Quick heartbeats, but a ttl that survives a saturated test machine.
fn fast_lease() -> crate::pipeline::lease::LeaseOptions {
    crate::pipeline::lease::LeaseOptions {
        heartbeat: Duration::from_millis(100),
        ttl: Duration::from_secs(10),
    }
}

fn runtime(chain: TestChain) -> SolanaRuntime<TestChain> {
    SolanaRuntime {
        budget: None,
        metrics: None,
        status: StatusSink::off(),
        source: chain,
        lease: fast_lease(),
        shutdown: Box::pin(std::future::pending()),
        // The scenarios must not be paced by a real rate limit.
        max_queries_per_minute: 100_000,
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

/// A clean index of `chain` in its own database.
async fn clean_index(name: &str, chain: &TestChain, end: u64) -> Scenario {
    let clean = Scenario::new(name).await;
    clean.index_until(chain, end).await.unwrap();
    clean
}

// ------------------------------------------------------------------- (a)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_canned_range_with_skipped_slots_is_indexed_end_to_end() {
    let scenario = Scenario::new("a_canned_range").await;
    let chain = chain(40);

    scenario.index_until(&chain, chain.head).await.unwrap();

    // The commit marker holds exactly the PRODUCED slots. The skipped
    // integers have no row and must not: they are not blocks.
    let slots = scenario.rows("sol_slots").await;
    assert_eq!(slots, chain.produced_slots(), "sol_slots");

    let asked = chain.head - FIRST_SLOT;
    assert!(
        asked > slots,
        "the scenario must actually skip slots: asked {asked}, stored \
         {slots}"
    );

    // Children.
    assert!(scenario.rows("sol_dex_swaps").await > 0, "sol_dex_swaps");
    assert!(
        scenario.rows("sol_transactions").await > 0,
        "sol_transactions"
    );
    assert!(scenario.rows("sol_tokens").await > 0, "sol_tokens");

    // The LAUNCHPAD rows, in the SHARED chain-neutral tables, written by
    // the same flush and under the same commit marker. The fake chain
    // carries the recorded pump.fun / Meteora DBC / LaunchLab
    // transactions, so these are real decoded launches and curve trades.
    assert!(
        scenario.rows("launchpad_tokens").await > 0,
        "launchpad_tokens"
    );
    assert!(
        scenario.rows("launchpad_trades").await > 0,
        "launchpad_trades"
    );
    assert!(
        scenario.rows("sol_token_balances").await > 0,
        "sol_token_balances"
    );
    // ... and only Solana rows: the shared tables must not be claimed.
    for table in svm::SHARED_BASE_TABLES {
        let other = scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM `{table}` FINAL \
                 WHERE chain != {CHAIN}"
            ))
            .await;
        assert_eq!(other, 0, "{table} holds rows of another chain");
    }
    // Their side tables were fed by the materialized views.
    assert!(
        scenario.rows("launchpad_trades_by_token").await > 0,
        "launchpad_trades_by_token"
    );

    // Every stored transaction, swap and launchpad row has its slot: the
    // commit marker invariant, checked here and not only by `verify`.
    for table in [
        "sol_transactions",
        "sol_dex_swaps",
        "launchpad_tokens",
        "launchpad_trades",
    ] {
        let orphans = scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM `{table}` FINAL \
                 WHERE chain = {CHAIN} AND block_number NOT IN \
                 (SELECT block_number FROM sol_slots FINAL \
                  WHERE chain = {CHAIN})"
            ))
            .await;
        assert_eq!(orphans, 0, "{table} has rows without their slot");
    }

    // The checkpoints TILE the asked range, with `to_block` = the server's
    // cursor. This is the only thing that can tell a skipped slot from a
    // slot nobody asked for.
    let resume = crate::pipeline::solana_store::SolanaReorgStore::new(
        scenario.db.clone(),
    )
    .checkpoint_tiling(CHAIN, FIRST_SLOT, None)
    .await
    .unwrap();
    assert_eq!(
        crate::db::ranges::contiguous_until(FIRST_SLOT, resume),
        chain.head,
        "the checkpoints do not tile the asked range"
    );

    // The candles exist and agree with the swaps.
    let swaps = scenario.rows("sol_dex_swaps").await;
    for view in SOL_CANDLE_VIEWS {
        let counted = scenario
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM `{view}` \
                 WHERE chain = {CHAIN}"
            ))
            .await;
        assert_eq!(counted, swaps, "{view}");
    }

    // And a candle really carries a price, not just a count.
    let priced = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM sol_dex_candles_1d_v \
             WHERE chain = {CHAIN} AND open IS NOT NULL AND close IS NOT NULL"
        ))
        .await;
    assert!(priced > 0, "no candle has an open and a close");

    scenario.assert_consistent().await;
}

/// `--no-launchpads` on Solana: the DEX rows still land, the launchpad
/// ones do not, and nothing else changes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn no_launchpads_is_honoured_on_solana() {
    let scenario = Scenario::new("a_no_launchpads").await;
    let chain = chain(40);

    let start = FIRST_SLOT.to_string();
    let end = chain.head.to_string();
    let config = scenario.config(&[
        "--start-block",
        &start,
        "--end-block",
        &end,
        "--flush-interval-ms",
        "200",
        "--no-launchpads",
    ]);

    run_with(config, runtime(chain.clone())).await.unwrap();

    // The DEX side is untouched: the flag must not cost a swap.
    assert_eq!(
        scenario.rows("sol_slots").await,
        chain.produced_slots(),
        "sol_slots"
    );
    assert!(scenario.rows("sol_dex_swaps").await > 0, "sol_dex_swaps");

    // And not one launchpad row was stored, in any of their tables.
    for table in svm::SHARED_BASE_TABLES {
        assert_eq!(scenario.rows(table).await, 0, "{table}");
    }
    for table in crate::launchpads::SIDE_TABLES {
        assert_eq!(scenario.rows(table).await, 0, "{table}");
    }
    assert_eq!(scenario.rows("sol_token_balances").await, 0);

    scenario.assert_consistent().await;
}

// ------------------------------------------------------------------- (b)

/// A flush that died between its children and its commit marker.
///
/// This is the single most likely crash in production, and the whole
/// children-first / marker-last protocol exists for it: the orphans must be
/// purged before the range is streamed again, or every candle over those
/// days counts the swaps twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_flush_killed_before_the_commit_marker_is_healed_on_restart() {
    let chain = chain(40);
    let clean = clean_index("b_clean", &chain, chain.head).await;

    let scenario = Scenario::new("b_healed").await;

    // Index the first part normally.
    let middle = chain.slot_at(20);
    scenario.index_until(&chain, middle).await.unwrap();

    // Now simulate the crash: write the CHILDREN of the next windows and
    // never the marker, exactly as a process killed between the two inserts
    // would leave the database.
    let orphan_batches: Vec<SvmSlotBatch> = chain
        .slots
        .iter()
        .filter(|slot| {
            slot.slot >= middle && slot.slot < chain.slot_at(30)
        })
        .map(copy)
        .collect();

    let mut rows = svm::decode(CHAIN, &orphan_batches);
    rows.set_version(next_version());
    rows.set_epoch(0);
    let orphaned_swaps = rows.swaps.len() as u64;
    assert!(orphaned_swaps > 0, "the crash must leave real rows behind");

    let batch = SvmBatch::new(
        rows,
        vec![BlockRange::new(middle, chain.slot_at(30))],
    );
    store_children(&scenario.db, &batch).await.unwrap();

    // The orphans are really there and really have no slot.
    let before = scenario.rows("sol_dex_swaps").await;
    let marker = scenario.rows("sol_slots").await;
    assert!(before > 0);
    let report = scenario.verify().await;
    assert!(
        !report.orphans.is_empty(),
        "the crash left no orphans to heal:\n{report}"
    );

    // Restart and run to the head: the heal purges them, then the range is
    // streamed again.
    scenario.index_until(&chain, chain.head).await.unwrap();

    assert_eq!(
        scenario.rows("sol_slots").await,
        chain.produced_slots(),
        "sol_slots after the heal (was {marker} before)"
    );
    scenario.assert_consistent().await;

    // Equal to a clean index - base tables AND candles. The candles are
    // the part that a missing heal would get wrong while every base table
    // still read perfectly.
    assert_same(
        "after a heal",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );

    // And the mints are the same SET, decimals included. Only the
    // `program` column can differ, and only when it is unknown - see the
    // test below, which pins that down rather than leaving it implied.
    assert_eq!(
        scenario.mints().await,
        clean.mints().await,
        "the healed index knows different mints"
    );
}

/// `sol_tokens.program` is BEST EFFORT, and this test says so on purpose
/// rather than leaving a surprise for whoever reads the column first.
///
/// Not every `account_activity` row carries `post_program_id` /
/// `pre_program_id`, so a batch can decode a mint's decimals without
/// learning which token program owns it. `svm::decode` takes the first row
/// that names one (this work changed it from "the first row", which could
/// record the System program for a mint whose first row happened to be
/// silent), but if NO row in the batch names one the column stays zero -
/// and because `sol_tokens` is a `ReplacingMergeTree(_version)` that no
/// purge ever touches, a later batch that does not know can overwrite an
/// earlier one that did.
///
/// What that means for a reader: `program` answers "is this mint
/// Token-2022, i.e. can it carry a transfer fee?" only when it is
/// non-zero. `decimals`, which is what valuing a swap actually needs, is
/// always right.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn sol_tokens_program_is_best_effort_but_decimals_are_not() {
    let scenario = Scenario::new("b_mints").await;
    let chain = chain(40);

    scenario.index_until(&chain, chain.head).await.unwrap();

    // Every mint that appears in a swap has a row with its decimals.
    let untyped = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM ( \
               SELECT arrayJoin([token0, token1]) AS mint \
               FROM sol_dex_swaps FINAL WHERE chain = {CHAIN} \
             ) WHERE mint NOT IN \
             (SELECT mint FROM sol_tokens FINAL WHERE chain = {CHAIN})"
        ))
        .await;
    assert_eq!(untyped, 0, "a traded mint has no sol_tokens row");

    // And no mint is recorded as belonging to the System program, which is
    // the bug the decoder change fixed: the zero pubkey now means
    // "unknown", never "the System program owns this mint".
    let systemic = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM sol_tokens FINAL \
             WHERE chain = {CHAIN} AND program != toFixedString('', 32) \
             AND base58Encode(program) NOT IN \
             ('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA', \
              'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb')"
        ))
        .await;
    assert_eq!(
        systemic, 0,
        "a mint is recorded with a program that is neither SPL Token, \
         Token-2022, nor 'unknown'"
    );
}

/// A heal leaves the holder log exactly as a clean index has it.
///
/// `sol_token_balances` is an append log of observations keyed on
/// `(chain, mint, owner, account, block_number, tx_index)`: the heal
/// tombstones the orphaned observations like any other child's rows and
/// the re-stream writes them again, so a healed index must hold the same
/// live observations as a clean one - none lost, none resurrected. (Until
/// round 4 it was a latest-value projection no purge could touch; the
/// snapshot comparison in `Scenario::snapshot` now covers it too.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_holder_projection_is_re_observed_after_a_heal() {
    let chain = chain(40);
    let clean = clean_index("b_bal_clean", &chain, chain.head).await;
    let scenario = Scenario::new("b_bal_healed").await;

    let middle = chain.slot_at(20);
    scenario.index_until(&chain, middle).await.unwrap();

    // The crash: children of the next windows, no commit marker.
    let orphans: Vec<SvmSlotBatch> = chain
        .slots
        .iter()
        .filter(|s| s.slot >= middle && s.slot < chain.slot_at(30))
        .map(copy)
        .collect();
    let mut rows = svm::decode(CHAIN, &orphans);
    rows.set_version(next_version());
    rows.set_epoch(0);
    store_children(
        &scenario.db,
        &SvmBatch::new(
            rows,
            vec![BlockRange::new(middle, chain.slot_at(30))],
        ),
    )
    .await
    .unwrap();

    scenario.index_until(&chain, chain.head).await.unwrap();
    scenario.assert_consistent().await;

    // Every (mint, owner, account) the clean index knows, with the same
    // balance: the heal lost none and resurrected none.
    let balances = |s: &Scenario| {
        let db = s.db.clone();
        async move {
            db.db
                .query(&format!(
                    "SELECT base58Encode(mint), base58Encode(owner), \
                 base58Encode(account), toString(balance) \
                 FROM sol_token_balances FINAL WHERE chain = {CHAIN} \
                 ORDER BY mint, owner, account"
                ))
                .fetch_all::<(String, String, String, String)>()
                .await
                .unwrap()
        }
    };

    assert_eq!(
        balances(&scenario).await,
        balances(&clean).await,
        "the healed index has different holder balances"
    );
}

/// A purge can CORRECT a holder balance.
///
/// The heal path re-streams what it purged, so re-observing is enough
/// there. A purge happens for other reasons too: the operator re-indexes
/// the range differently or with `--no-launchpads`, or the reason for the
/// purge is that the SOURCE data was wrong. `sol_token_balances` used to
/// be a latest-value projection whose `_version` was the POSITION, and no
/// statement in the codebase could remove a row of it: the wrong balance
/// stayed live for ever (review round 4, MAJOR 12). It is now an append
/// log of observations, tombstoned by the ordinary purge like every other
/// child table, and the holder view falls back to the newest surviving
/// observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_purge_corrects_a_holder_balance() {
    use crate::{
        metrics::Metrics,
        pipeline::backfill::EpochOnly,
        reorg::{NoHooks, PurgeReason, Purger},
    };

    let scenario = Scenario::new("b_bal_purged").await;
    let chain = chain(40);
    scenario.index_until(&chain, chain.head).await.unwrap();

    // One observed holder of one launchpad mint.
    let (mint, owner, account, newest): (String, String, String, u64) =
        scenario
            .db
            .db
            .query(&format!(
                "SELECT hex(mint), hex(owner), hex(account), \
                 toUInt64(block_number) FROM sol_token_balances \
                 WHERE chain = {CHAIN} AND is_deleted = 0 \
                 AND balance > 0 ORDER BY block_number DESC LIMIT 1"
            ))
            .fetch_one()
            .await
            .expect("a stored holder balance");
    assert!(newest > FIRST_SLOT);

    // An EARLIER observation of the same token account, as an earlier
    // flush would have written it. Two observations of one account is
    // what the old projection could not hold at all.
    const EARLIER: f64 = 4_242.0;
    let unhex32 = |text: &str| -> [u8; 32] {
        let mut out = [0u8; 32];
        out.copy_from_slice(&hex::decode(text).expect("32 hex bytes"));
        out
    };
    scenario
        .db
        .insert_rows(
            "sol_token_balances",
            &[crate::svm::launchpads::SolTokenBalance {
                chain: CHAIN,
                mint: unhex32(&mint),
                owner: unhex32(&owner),
                account: unhex32(&account),
                balance: alloy::primitives::U256::from(EARLIER as u64),
                block_number: FIRST_SLOT,
                tx_index: 0,
                timestamp: BASE_TIMESTAMP,
                epoch: scenario.db.epoch(),
                _version: next_version(),
                is_deleted: 0,
            }],
        )
        .await
        .expect("insert the earlier observation");
    let before = FIRST_SLOT;

    let holding = |as_of: u64| {
        let db = scenario.db.db.clone();
        let (mint, owner) = (mint.clone(), owner.clone());
        async move {
            // `max()` over no rows is 0.0, which is a balance: -1 is how
            // "this wallet is not in the list at all" comes back.
            db.query(&format!(
                "SELECT if(count() = 0, -1., toFloat64(max(balance_raw))) \
                 FROM sol_launchpad_token_holders_v(chain = {CHAIN}, \
                 token = '{mint}', as_of_block = {as_of}) \
                 WHERE account = unhex('{owner}')"
            ))
            .fetch_one::<f64>()
            .await
            .unwrap()
        }
    };

    let at_newest = holding(u64::MAX).await;
    assert_ne!(at_newest, EARLIER, "the two observations must differ");
    assert_eq!(
        holding(before).await,
        EARLIER,
        "the holder list of a past block does not read that block"
    );

    // The purge of everything from the newest observation up, WITHOUT a
    // re-stream: the operator's "this range was wrong" case.
    let purger = Purger::new(
        Arc::new(SolanaReorgStore::new(scenario.db.clone())),
        Arc::new(EpochOnly::new(scenario.db.clone())),
        Arc::new(NoHooks),
        Arc::new(Metrics::disabled()),
    );
    purger
        .purge_range(CHAIN, newest, Some(chain.head), PurgeReason::GapHeal)
        .await
        .expect("the purge must be able to remove balance observations");

    // The purged observation is gone from the table ...
    assert_eq!(
        scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM sol_token_balances FINAL \
                 WHERE chain = {CHAIN} AND block_number >= {newest}"
            ))
            .await,
        0,
        "a purge still cannot remove a balance observation"
    );
    // ... and the holder list reads the one before it, not the purged
    // one and not nothing.
    let after = holding(u64::MAX).await;
    assert_ne!(after, -1.0, "the wallet fell out of the holder list");
    assert_eq!(
        after, EARLIER,
        "the holder list did not fall back to the surviving observation \
         (it read {after}, the purged one was {at_newest})"
    );
}

// ------------------------------------------------------------------- (c)

/// The same flush applied twice - a timed-out insert that WAS applied and
/// is retried - must count once, in the candles as well as in the base
/// table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_retried_flush_counts_once() {
    let scenario = Scenario::new("c_retried").await;
    let chain = chain(20);

    // This scenario writes its rows by hand rather than running the loop,
    // so the floor a real start would have written has to be written here
    // too: `verify` starts at the floor, and a Solana database without one
    // reads as "every slot since 0 was never asked for".
    scenario.set_floor(FIRST_SLOT).await;

    // No `index_until` here: a retry means the SAME flush sent twice, and
    // "the same flush" is the same `_version`. So the whole scenario is
    // one flush, applied and then applied again - which is exactly what a
    // timed-out insert that had in fact been applied looks like on the
    // wire. (Re-sending the same slots under a NEW `_version` is not a
    // retry, it is what re-streaming after a purge does, and it MUST be
    // written; the end of this test checks that too.)
    let window = BlockRange::new(FIRST_SLOT, chain.head);
    let batches = copy_all(&chain.slots);

    let mut rows = svm::decode(CHAIN, &batches);
    let version = next_version();
    rows.set_version(version);
    rows.set_epoch(0);

    let swaps = rows.swaps.len() as u64;
    let slots = rows.slots.len() as u64;
    assert!(swaps > 0 && slots > 0);

    let batch = SvmBatch::new(rows, vec![window]);

    // The whole commit protocol, twice.
    crate::pipeline::solana_writer::store_batch(&scenario.db, &batch)
        .await
        .unwrap();
    crate::pipeline::solana_writer::store_batch(&scenario.db, &batch)
        .await
        .unwrap();

    // And table by table, twice more, the way `insert_retrying` retries an
    // individual insert.
    let key =
        FlushKey { chain: CHAIN, span: (window.from, window.to), version };

    for attempt in 0..2 {
        for (table, result) in [
            (
                "sol_dex_swaps",
                scenario
                    .db
                    .insert_flush("sol_dex_swaps", &batch.rows.swaps, &key)
                    .await,
            ),
            (
                "sol_slots",
                scenario
                    .db
                    .insert_flush("sol_slots", &batch.rows.slots, &key)
                    .await,
            ),
        ] {
            result.unwrap_or_else(|e| {
                panic!("attempt {attempt} of '{table}': {e}")
            });
        }
    }

    // WITHOUT `FINAL`: the base tables are ReplacingMergeTrees, so `FINAL`
    // would hide a duplicate whether or not the deduplication worked. The
    // real question is whether a second physical row was written at all.
    for (table, expected) in
        [("sol_dex_swaps", swaps), ("sol_slots", slots)]
    {
        assert_eq!(
            scenario
                .count(&format!("SELECT toUInt64(count()) FROM `{table}`"))
                .await,
            expected,
            "{table} holds a second physical copy of a retried flush"
        );
    }

    // The number that actually matters: a materialized view only ever
    // ADDS, so without the deduplication token every candle over these
    // days would count every swap four times over.
    for view in SOL_CANDLE_VIEWS {
        let counted = scenario
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM `{view}` \
                 WHERE chain = {CHAIN}"
            ))
            .await;
        assert_eq!(counted, swaps, "{view} counted a retried flush twice");
    }

    scenario.assert_consistent().await;

    // A LATER flush of the same slots (another `_version`) is NOT a retry:
    // it is written, because that is what re-streaming after a purge does.
    let mut again = svm::decode(CHAIN, &copy_all(&chain.slots));
    again.set_version(next_version());
    again.set_epoch(0);
    crate::pipeline::solana_writer::store_batch(
        &scenario.db,
        &SvmBatch::new(again, vec![window]),
    )
    .await
    .unwrap();

    assert_eq!(
        scenario
            .count("SELECT toUInt64(count()) FROM sol_dex_swaps")
            .await,
        swaps * 2,
        "a flush with a new version must be written"
    );
    assert_eq!(scenario.rows("sol_dex_swaps").await, swaps);
}

// ------------------------------------------------------------------- (d)

/// A break in the `block_height` chain stops the indexer and writes nothing
/// further.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_height_break_trips_the_tripwire_and_stops() {
    let scenario = Scenario::new("d_height_break").await;
    let healthy = chain(40);
    let broken = healthy.with_height_break(25);
    let break_slot = broken.slot_at(25);

    let error = scenario
        .index_until(&broken, broken.head)
        .await
        .expect_err("a continuity break must be fatal");

    assert!(
        Tripwire::is_cause_of(&error),
        "the failure is not the tripwire: {error:#}"
    );

    let text = format!("{error:#}");
    assert!(text.contains("SOLANA CONTINUITY TRIPWIRE"), "{text}");
    assert!(text.contains("block_height"), "{text}");
    // The message must tell an operator what to do, not just what happened.
    assert!(text.contains("indexer verify --chain solana"), "{text}");

    // NOTHING at or above the break was written, in any table.
    for table in ["sol_slots", "sol_transactions", "sol_dex_swaps"] {
        let above = scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM `{table}` FINAL \
                 WHERE chain = {CHAIN} AND block_number >= {break_slot}"
            ))
            .await;
        assert_eq!(
            above, 0,
            "{table} holds rows at or above the tripped slot {break_slot}"
        );
    }
}

/// The same for the parent chain: heights that chain but a DIFFERENT block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_parent_hash_break_trips_the_tripwire_too() {
    let scenario = Scenario::new("d_parent_break").await;
    let broken = chain(30).with_parent_hash_break(18);
    let break_slot = broken.slot_at(18);

    let error = scenario
        .index_until(&broken, broken.head)
        .await
        .expect_err("a parent hash break must be fatal");

    assert!(Tripwire::is_cause_of(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("parent_blockhash"),
        "{error:#}"
    );

    let above = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM sol_slots FINAL \
             WHERE chain = {CHAIN} AND block_number >= {break_slot}"
        ))
        .await;
    assert_eq!(above, 0);
}

/// The tripwire at a PASS BOUNDARY, with the database read that used to
/// supply the predecessor answering stale (docs/review-round-4.md,
/// MAJOR 9).
///
/// At the head every pass starts exactly where the previous one stopped,
/// and `anchor_for` used to ask `checkpoints FINAL` whether a checkpoint
/// ends there - a read of the row the previous pass had just written,
/// i.e. the one read ClickHouse is allowed to answer stale (~3%
/// measured). When it did, the first block of the new pass was not
/// checked at all and the only fork check this chain has was off, with no
/// log line.
///
/// This forces that stale answer and puts a forked block exactly at the
/// boundary. It passes only because the loop carries the last
/// `Continuity` in memory: delete `SolanaIndexer::last_continuity` and
/// the run finishes happily with a different block stored, which is the
/// whole point - the pure-function test of `carried_anchor` would still
/// pass.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_carried_anchor_checks_a_fork_a_stale_checkpoint_read_misses()
{
    use crate::pipeline::solana_store::stale;

    let scenario = Scenario::new("d_stale_anchor").await;

    // The block at index 25 does not build on the one below it. The head
    // stops just under it for the first two `/height` reads, so the first
    // pass ends exactly at the boundary and the second pass begins with
    // the forked block.
    let broken = chain(40).with_parent_hash_break(25);
    let boundary = broken.slot_at(25);
    let broken = broken.with_staged_head(boundary, 2);

    // Every checkpoint-adjacency read of this run is answered stale, so
    // the database can supply no anchor at all.
    stale::arm_checkpoint_reads(64);

    let error = scenario
        .index_until(&broken, broken.head)
        .await
        .expect_err("the fork at the pass boundary must be fatal");

    assert!(
        Tripwire::is_cause_of(&error),
        "the first block of the second pass was not checked against the \
         last block of the first: {error:#}"
    );
    let text = format!("{error:#}");
    assert!(text.contains("parent_blockhash"), "{text}");
    assert!(text.contains(&boundary.to_string()), "{text}");

    // The forked block reached no table.
    let above = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM `{COMMIT_MARKER}` FINAL \
             WHERE chain = {CHAIN} AND block_number >= {boundary}"
        ))
        .await;
    assert_eq!(above, 0);

    // ... and the run really did go through two passes, with the slots
    // below the boundary stored by the first one.
    let below = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM `{COMMIT_MARKER}` FINAL \
             WHERE chain = {CHAIN} AND block_number < {boundary}"
        ))
        .await;
    assert_eq!(below, 25, "the first pass stored 25 produced slots");

    // The anchor came from memory, not from the database: had the loop
    // asked, it would have been told "no checkpoint ends here" and would
    // have skipped the check.
    assert!(
        stale::checkpoint_reads_taken() > 0,
        "the loop never even asked the database - arm the knob against \
         the read that is actually taken"
    );
}

// ------------------------------------------------------------------- (e)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_second_solana_process_on_the_same_chain_is_refused() {
    let scenario = Scenario::new("e_second_process").await;
    let chain = chain(40);

    // The first process runs, slowly, and never finishes on its own.
    let first = {
        let config = scenario.config(&["--flush-interval-ms", "200"]);
        let chain = chain.clone();
        tokio::spawn(async move { run_with(config, runtime(chain)).await })
    };

    // Give it time to take the lease and write something.
    let store = SolanaReorgStore::new(scenario.db.clone());
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if store.stored_head(CHAIN).await.unwrap().is_some() {
            break;
        }
    }

    // The second must refuse rather than race it on the epoch.
    let config = scenario.config(&["--flush-interval-ms", "200"]);
    let error = tokio::time::timeout(
        Duration::from_secs(120),
        run_with(config, runtime(chain.clone())),
    )
    .await
    .expect("the second process hung instead of refusing")
    .expect_err("a second process on the same chain must be refused");

    let text = format!("{error:#}");
    assert!(
        text.contains("instance") || text.contains("already"),
        "the refusal does not name the reason: {text}"
    );

    first.abort();
    let _ = first.await;
}

// ------------------------------------------------------------------- (f)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn verify_tells_a_consistent_index_from_an_inconsistent_one() {
    let scenario = Scenario::new("f_verify").await;
    let chain = chain(40);

    scenario.index_until(&chain, chain.head).await.unwrap();

    // Consistent, and it says so - including that the skipped slots are
    // skipped rather than missing.
    let report = scenario.verify().await;
    assert!(report.is_consistent(), "{report}");
    assert!(report.skipped_slots > 0, "{report}");
    assert_eq!(report.stored_slots, chain.produced_slots(), "{report}");
    assert!(report.unasked.is_empty(), "{report}");
    assert!(report.breaks.is_empty(), "{report}");
    assert!(report.to_string().contains("Result: CONSISTENT"), "{report}");

    // The candle cross-check really ran: the chain spans whole UTC days.
    assert!(
        report.candles_skipped.is_none(),
        "the candle check was skipped: {:?}",
        report.candles_skipped
    );

    // Now break it the way ONLY a Solana check can see: tombstone one
    // middle checkpoint WITHOUT touching a single row of data, so the
    // tiling has a hole although every slot is still stored.
    //
    // This is the whole point of check 1. The data cannot show it - an
    // unasked slot and a skipped slot look exactly alike, both being
    // simply absent - and an EVM-style "every integer has a `blocks` row"
    // check would have been screaming about the skipped slots all along
    // instead.
    let tiling = SolanaReorgStore::new(scenario.db.clone())
        .checkpoint_tiling(CHAIN, FIRST_SLOT, None)
        .await
        .unwrap();
    assert!(!tiling.is_empty(), "{tiling:?}");

    // The flush merged its windows, so the tiling is one wide checkpoint.
    // Replace it by its two ends and leave the middle unclaimed - the
    // shape a purge that died between "tombstone the checkpoint" and
    // "re-insert the surviving remainder" would leave.
    let (from, to) = *tiling.last().unwrap();
    let hole_from = chain.slot_at(15);
    let hole_to = chain.slot_at(25);
    let version = next_version();

    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO checkpoints (chain, from_block, to_block, epoch, \
             _version, is_deleted) VALUES \
             ({CHAIN}, {from}, {to}, 0, {version}, 1), \
             ({CHAIN}, {from}, {hole_from}, 0, {version}, 0), \
             ({CHAIN}, {hole_to}, {to}, 0, {version}, 0)"
        ))
        .execute()
        .await
        .unwrap();

    let report = scenario.verify_inconsistent().await;
    assert!(!report.unasked.is_empty(), "{report}");
    assert!(
        report.to_string().contains("Result: PROBLEMS FOUND"),
        "{report}"
    );
    assert!(report.to_string().contains("never asked for"), "{report}");
}

/// `--new-blocks-only` promises the head slot and has to INDEX it.
///
/// The twin of `pipeline::acceptance::
/// new_blocks_only_indexes_the_block_its_floor_promises`: the floor is
/// `head - 1` and the cursor was a SECOND head poll, one slot above it, so
/// the floor's own slot was never asked for. No checkpoint then started at
/// or below the floor, `coverage_v`'s fold never left it, and `verify`,
/// the fleet status line and the control panel all said "Coverage: nothing
/// stored yet" for ever about a chain following the head perfectly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn new_blocks_only_indexes_the_slot_its_floor_promises() {
    use crate::coverage::store;

    let scenario = Scenario::new("f_new_only").await;
    let chain = chain(20);

    let end = chain.head.to_string();
    let config = scenario.config(&[
        "--new-blocks-only",
        "--end-block",
        &end,
        "--flush-interval-ms",
        "200",
    ]);

    tokio::time::timeout(
        Duration::from_secs(300),
        run_with(config, runtime(chain.clone())),
    )
    .await
    .expect("the Solana pipeline did not finish in time")
    .unwrap();

    let coverage = store::coverage(&scenario.db).await.unwrap().unwrap();
    assert_eq!(coverage.floor.block, chain.head - 1, "{coverage:?}");
    assert_eq!(coverage.floor.reason, store::Reason::Head);
    assert!(
        !coverage.is_empty(),
        "the floor's own slot was never asked for, so every surface says \
         'nothing stored yet' for ever: {coverage:?}"
    );

    let report =
        solana_verify::verify(&scenario.db, None, 0).await.unwrap();
    assert_eq!(report.range.from, chain.head - 1, "{report}");
    assert!(report.unasked.is_empty(), "{report}");
    assert!(report.is_consistent(), "{report}");
}

/// THE LIVE-RUN BUG, Solana half. `indexer fleet --chain solana` on a
/// fresh database puts the coverage floor at the head slot and indexes
/// forward from there - and `indexer verify` then checked from slot 0,
/// called the 448 million slots below the floor "never asked for" and
/// printed `PROBLEMS FOUND` about a database with nothing wrong with it.
/// The candle cross-check was switched off with it, because a tiling with
/// holes cannot be compared.
///
/// Pinned here, in the same order as the EVM twin
/// (`pipeline::acceptance::verify_starts_at_the_coverage_floor_and_not_at_block_zero`):
///
/// * every check starts at the floor;
/// * the candles ARE checked, over complete days including the floor's own
///   partial one (nothing is stored below the floor, so both sides count
///   the same swaps);
/// * a restart with no start flag keeps the floor and heals from it;
/// * an explicit `--start-block` below the floor is honoured and honest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn verify_starts_at_the_coverage_floor_and_not_at_slot_zero() {
    use crate::coverage::store;

    let scenario = Scenario::new("f_floor").await;
    let chain = chain(40);

    scenario.index_until(&chain, chain.head).await.unwrap();

    let floor = store::stored(&scenario.db).await.unwrap().unwrap();
    assert_eq!(floor.block, FIRST_SLOT);

    // ---- with no flags, every check starts at the floor.
    let report =
        solana_verify::verify(&scenario.db, None, 0).await.unwrap();

    assert_eq!(report.range.from, FIRST_SLOT, "{report}");
    assert!(report.unasked.is_empty(), "{report}");
    assert!(report.below_floor.is_none(), "{report}");
    assert!(report.is_consistent(), "{report}");
    assert!(
        report.fully_checked(),
        "the candle cross-check did not run: {:?}",
        report.candles_skipped
    );
    assert!(
        report.to_string().contains("Candles: they agree"),
        "{report}"
    );

    // ---- a restart with NO start flag keeps the floor and heals from it.
    //
    // One orphan child below the floor: the heal must leave it alone, and
    // `verify` must not see it either.
    let below = FIRST_SLOT - 100;
    let version = next_version();
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO sol_transactions SELECT * REPLACE (\
             toUInt64({below}) AS block_number, \
             toUInt64({version}) AS _version) FROM (\
             SELECT * FROM sol_transactions FINAL WHERE chain = {CHAIN} \
             LIMIT 1)"
        ))
        .execute()
        .await
        .unwrap();

    let end = chain.head.to_string();
    let restarted = scenario.config(&[
        "--end-block",
        &end,
        "--flush-interval-ms",
        "200",
    ]);
    tokio::time::timeout(
        Duration::from_secs(300),
        run_with(restarted, runtime(chain.clone())),
    )
    .await
    .expect("the Solana pipeline did not finish in time")
    .unwrap();

    assert_eq!(
        store::stored(&scenario.db).await.unwrap().unwrap().block,
        FIRST_SLOT,
        "the floor moved on a restart with no flags"
    );
    assert_eq!(
        scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM sol_transactions FINAL \
                 WHERE chain = {CHAIN} AND block_number = {below}"
            ))
            .await,
        1,
        "the gap heal purged a row BELOW the coverage floor"
    );

    let report =
        solana_verify::verify(&scenario.db, None, 0).await.unwrap();
    assert!(report.is_consistent(), "{report}");
    assert!(report.orphans.is_empty(), "{report}");
    assert!(!report.heal_pending, "{report}");

    // ---- and an explicit start below the floor is honoured, and honest.
    let report =
        solana_verify::verify(&scenario.db, Some(0), 0).await.unwrap();

    assert_eq!(report.range.from, 0, "{report}");
    assert_eq!(report.below_floor, Some(FIRST_SLOT), "{report}");
    assert!(!report.unasked.is_empty(), "{report}");
    assert!(!report.is_consistent(), "{report}");
    assert!(
        report.to_string().contains("BELOW this chain's coverage floor"),
        "{report}"
    );
    assert!(
        report.orphans.iter().any(|o| o.table == "sol_transactions"),
        "{report}"
    );
}

/// A break in the STORED data (not in the stream) is what `verify` has to
/// catch after the fact: checks 2 and 3 over `sol_slots`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn verify_catches_a_height_break_in_the_stored_slots() {
    let scenario = Scenario::new("f_stored_break").await;
    let chain = chain(30);

    scenario.index_until(&chain, chain.head).await.unwrap();
    scenario.assert_consistent().await;

    // Rewrite one stored slot's block_height: a produced block went
    // missing at some point and nothing else records it.
    let slot = chain.slot_at(20);
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO sol_slots (chain, block_number, blockhash, \
             parent_slot, parent_blockhash, block_height, timestamp, \
             epoch, _version, is_deleted) \
             SELECT chain, block_number, blockhash, parent_slot, \
             parent_blockhash, block_height + 5, timestamp, epoch, \
             {} AS _version, 0 AS is_deleted FROM sol_slots FINAL \
             WHERE chain = {CHAIN} AND block_number = {slot}",
            next_version()
        ))
        .execute()
        .await
        .unwrap();

    let report = scenario.verify_inconsistent().await;
    assert!(!report.breaks.is_empty(), "{report}");
    assert!(report.breaks.iter().any(|b| b.height_broken), "{report}");
    assert!(
        report.to_string().contains("a produced block is MISSING"),
        "{report}"
    );
}

// ------------------------------------------------- flags and boundaries

/// `--start-block` below the served history must fail clearly at startup
/// instead of looping on a server that answers empty without advancing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_start_slot_below_the_served_history_is_refused() {
    let scenario = Scenario::new("g_start_slot").await;
    let chain = chain(5);

    let below = (FIRST_SERVED_SLOT - 1).to_string();
    let config = scenario.config(&["--start-block", &below]);

    let error = run_with(config, runtime(chain))
        .await
        .expect_err("a start below the served history must be refused");

    let text = format!("{error:#}");
    assert!(text.contains(&FIRST_SERVED_SLOT.to_string()), "{text}");
    assert!(text.contains("--new-blocks-only"), "{text}");

    // And nothing was written.
    assert_eq!(scenario.rows("sol_slots").await, 0);
}

/// `--confirmations` on a source that already serves behind `finalized`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn confirmations_are_refused_on_solana() {
    let scenario = Scenario::new("g_confirmations").await;
    let chain = chain(5);

    let config = scenario.config(&["--confirmations", "32"]);
    let error = run_with(config, runtime(chain))
        .await
        .expect_err("--confirmations must be refused on Solana");

    let text = format!("{error:#}");
    assert!(text.contains("finalized"), "{text}");
    assert!(text.contains("tripwire"), "{text}");
}

/// The chain registry row is what lets a view print a Solana pubkey with
/// `base58Encode` instead of as an EVM address.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_chain_is_registered_at_startup() {
    let scenario = Scenario::new("g_registry").await;
    let chain = chain(5);

    scenario.index_until(&chain, chain.head).await.unwrap();

    let (name, family): (String, String) = scenario
        .db
        .db
        .query(&format!(
            "SELECT name, family FROM chains FINAL WHERE chain = {CHAIN}"
        ))
        .fetch_one()
        .await
        .unwrap();

    assert_eq!((name.as_str(), family.as_str()), ("solana", "svm"));

    // Idempotent: a second run must not add a second row.
    scenario.index_until(&chain, chain.head).await.unwrap();
    let rows = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM chains FINAL \
             WHERE chain = {CHAIN}"
        ))
        .await;
    assert_eq!(rows, 1);
}

/// A restart with nothing to do must not re-stream anything: the checkpoint
/// tiling is the resume cursor, and a skipped slot must never look like a
/// hole in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_restart_over_a_complete_range_asks_for_nothing() {
    let scenario = Scenario::new("g_restart").await;
    let chain = chain(30);

    scenario.index_until(&chain, chain.head).await.unwrap();

    let before = chain.queries.load(Ordering::Relaxed);
    assert!(before > 0);

    scenario.index_until(&chain, chain.head).await.unwrap();

    assert_eq!(
        chain.queries.load(Ordering::Relaxed),
        before,
        "the restart spent metered queries on a range it had already \
         stored - a skipped slot is being read as a gap"
    );

    scenario.assert_consistent().await;
}

// ------------------------------------------------- (h) round 4 findings

/// Review round 4, BLOCKER 1.
///
/// The commit protocol writes `sol_slots` and THEN, as a separate awaited
/// insert, the checkpoint. When the marker lands and the checkpoint does
/// not - its six retries are exhausted, or the writer is aborted between
/// the two awaits - the slots are stored but no checkpoint claims them.
///
/// The resume path reads only the checkpoint tiling, so it calls that range
/// a hole and streams it again. `has_orphan_children` is FALSE for those
/// slots (their marker is alive, which is the whole point), so the gap heal
/// used to skip them: the range was re-streamed on top of live data with a
/// fresh `_version` and therefore a fresh deduplication token, every insert
/// was accepted, and the candle and launchpad materialized views - which
/// only ever ADD into `SimpleAggregateFunction(sum, ..)` columns - counted
/// the whole window twice. The base tables still read perfectly under
/// `FINAL`, so nothing but `indexer verify` could see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_hole_whose_slots_are_still_stored_is_purged_before_the_restream(
) {
    let chain = chain(40);
    let clean = clean_index("h_lost_cp_clean", &chain, chain.head).await;
    let scenario = Scenario::new("h_lost_cp").await;

    scenario.index_until(&chain, chain.head).await.unwrap();
    scenario.assert_consistent().await;

    let swaps = scenario.rows("sol_dex_swaps").await;
    let slots = scenario.rows("sol_slots").await;
    assert!(swaps > 0 && slots > 0);

    // The crash: the checkpoint of a middle window never became live,
    // while every row it covered did. Exactly what an exhausted retry of
    // the checkpoint insert leaves behind.
    let hole_from = chain.slot_at(15);
    let hole_to = chain.slot_at(25);
    scenario.unclaim(hole_from, hole_to).await;

    // The rows are all still there - this is NOT the orphan-children case.
    let stored_in_hole = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM sol_slots FINAL \
             WHERE chain = {CHAIN} AND block_number >= {hole_from} \
             AND block_number < {hole_to}"
        ))
        .await;
    assert!(
        stored_in_hole > 0,
        "the scenario must leave live slots inside the unclaimed range"
    );

    // Restart and run to the head: the heal must purge the hole before the
    // range is streamed again.
    scenario.index_until(&chain, chain.head).await.unwrap();

    // The number the doubling shows up in, and the only one a reader sees.
    let swaps_after = scenario.rows("sol_dex_swaps").await;
    assert_eq!(swaps_after, swaps, "sol_dex_swaps");

    for view in SOL_CANDLE_VIEWS {
        let counted = scenario
            .count(&format!(
                "SELECT toUInt64(sum(swaps)) FROM `{view}` \
                 WHERE chain = {CHAIN}"
            ))
            .await;
        assert_eq!(
            counted, swaps,
            "{view} counted the re-streamed window a second time"
        );
    }

    scenario.assert_consistent().await;

    // ... and the whole index, candles and launchpad aggregates included,
    // is what a clean one would have been.
    assert_same(
        "after a lost checkpoint was healed",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );
}

/// Review round 4, BLOCKER 2.
///
/// `checkpoints` gains one row per flush and nothing ever removed them on
/// this path: at the head's 4 s cadence that is ~25k rows a day, so the
/// old `LIMIT 100_000` on the tiling read was reached in four or five days.
/// From then on the tiling truncated, the resume point fell far below the
/// real head, everything above it read as a hole and was re-streamed over
/// live data - BLOCKER 1's doubling, applied to days of candles.
///
/// The EVM loop compacts after every covered pass; this proves the Solana
/// loop now does the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_loop_compacts_the_checkpoints_it_keeps_adding() {
    let scenario = Scenario::new("h_compaction").await;
    let chain = chain(30);

    // A first process stores part of the range, so the second one really
    // has a pass to cover (a run with nothing to do never reaches the
    // housekeeping).
    let middle = chain.slot_at(15);
    scenario.index_until(&chain, middle).await.unwrap();

    // A year of head-following, in one insert: 400 contiguous live
    // checkpoint rows. They sit BELOW the indexed range so they change no
    // answer the loop depends on, and they are contiguous so compaction
    // has something to collapse.
    let base = FIRST_SLOT - 500;
    let version = next_version();
    let values: Vec<String> = (0..400u64)
        .map(|i| {
            format!(
                "({CHAIN}, {}, {}, 0, {version}, 0)",
                base + i,
                base + i + 1
            )
        })
        .collect();
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO checkpoints (chain, from_block, to_block, epoch, \
             _version, is_deleted) VALUES {}",
            values.join(", ")
        ))
        .execute()
        .await
        .unwrap();

    let store = SolanaReorgStore::new(scenario.db.clone());
    let live = |scenario: &Scenario| {
        let db = scenario.db.clone();
        async move {
            db.db
                .query(&format!(
                    "SELECT toUInt64(count()) FROM checkpoints FINAL \
                     WHERE chain = {CHAIN}"
                ))
                .fetch_one::<u64>()
                .await
                .unwrap()
        }
    };

    let before = live(&scenario).await;
    assert!(before > 400, "{before}");

    // The tiling read must not be truncated by anything, so the answer it
    // gives now is the one that has to survive the compaction.
    let covered_before = store.resume_point(CHAIN, base).await.unwrap();
    assert_eq!(
        covered_before,
        base + 400,
        "the 400 rows tile [base, +400)"
    );

    // A second process covers the rest of the range, and compacts.
    scenario.index_until(&chain, chain.head).await.unwrap();

    let after = live(&scenario).await;
    assert!(
        after < before / 4,
        "the checkpoints were not compacted: {before} -> {after}"
    );

    // The ANSWER is unchanged: compaction collapses rows, never coverage.
    assert_eq!(store.resume_point(CHAIN, base).await.unwrap(), base + 400);
    assert_eq!(
        store.resume_point(CHAIN, FIRST_SLOT).await.unwrap(),
        chain.head,
        "the indexed range is still tiled"
    );

    scenario.assert_consistent().await;
}

/// Review round 4, MAJOR 7.
///
/// The gate a purge waits on used to poll `sol_slots` only, and the mark it
/// polled for was set only when the flush had slot rows. A window whose
/// slots were ALL SKIPPED therefore left the mark untouched: the gate
/// returned immediately, having proved nothing, and the purge could
/// tombstone slots whose checkpoint was still invisible - after which the
/// tiling shows no hole, the slots are never streamed again, and `verify`
/// calls the index complete.
///
/// The same flush is also what pinned down the second half: its checkpoint
/// must carry the flush `_version`, although no row carries one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_visibility_gate_covers_a_window_of_only_skipped_slots() {
    use crate::pipeline::{
        lease::Lease,
        solana::wait_until_visible,
        solana_writer::{ClickhouseSvmSink, LastFlush, SvmSink},
    };

    let scenario = Scenario::new("h_visibility").await;

    let (fatal, _fatal_rx) = tokio::sync::watch::channel(None::<String>);
    let lease =
        Lease::acquire(&scenario.db, fast_lease(), fatal).await.unwrap();

    let last_flush = LastFlush::default();
    let sink = ClickhouseSvmSink {
        db: scenario.db.clone(),
        fence: lease.fence(),
        last_flush: last_flush.clone(),
        stale: Arc::new(std::sync::Mutex::new(Vec::new())),
    };

    // A served window in which the chain produced no block at all. Normal
    // on Solana, and it still claims its slots.
    let window = BlockRange::new(FIRST_SLOT, FIRST_SLOT + 40);
    let version = next_version();
    let mut batch = SvmBatch::new(Default::default(), vec![window]);
    batch.stamp_version(version);
    assert!(batch.rows.slots.is_empty());

    sink.store(&batch).await.unwrap();

    // The mark names the CHECKPOINT: there is no marker row to name.
    let mark = last_flush.lock().unwrap().expect(
        "a flush of a skipped-only window left no mark for the purge gate \
         to wait on",
    );
    assert_eq!(mark.slot, None);
    assert_eq!(mark.checkpoint, (window.from, window.to));
    assert_eq!(mark.version, version);

    // ... and that checkpoint really carries the flush version. With 0
    // there it would lose the ReplacingMergeTree merge against the
    // tombstone of any earlier purge of the same range, and the window
    // would stay invisible for ever.
    let rows = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM checkpoints \
             WHERE chain = {CHAIN} AND from_block = {} AND to_block = {} \
             AND _version = {version} AND is_deleted = 0",
            window.from, window.to
        ))
        .await;
    assert_eq!(
        rows, 1,
        "the checkpoint was not written with the flush version"
    );

    // The gate proves it, rather than returning on an empty mark.
    wait_until_visible(scenario.db.clone(), last_flush).await.unwrap();

    lease.release().await;
}

/// Review round 4, MINOR 22: the `sol_dex_programs` overlay is read again
/// while the loop runs, so an operator's registry change does not need a
/// restart. (The loop re-reads it on the same 5 minute interval as the
/// checkpoint compaction; this pins the read itself.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_program_registry_is_read_again_not_only_at_startup() {
    use crate::pipeline::solana::load_program_names;

    let scenario = Scenario::new("h_registry_reload").await;

    let before = load_program_names(&scenario.db).await.unwrap();
    assert_eq!(before.len(), 0, "a fresh database has no operator rows");

    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO sol_dex_programs (program_id, name, kind, \
             confidence, source, _version) VALUES \
             (toFixedString(base58Decode(\
             '675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8'), 32), \
             'raydium-v4-operator', 'venue', 2, 'operator', {})",
            next_version()
        ))
        .execute()
        .await
        .unwrap();

    // No read-your-writes: poll rather than pretend the race is not there.
    for _ in 0..200 {
        let now = load_program_names(&scenario.db).await.unwrap();
        if now.len() == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!("the registry read never saw the operator's new row");
}

/// A flush that landed while ANOTHER process's purge was rebuilding the
/// same days carries an epoch the validity rule now hides, and nothing
/// else will ever ask for those slots again: their rows ARE stored, so no
/// hole appears in the tiling and no gap query reports them. The running
/// indexer queues them in memory; a restart used to lose the queue, and
/// the Solana loop never asked the database the same question the way the
/// EVM one does (docs/review-round-4.md, MAJOR 3).
///
/// Here the queue is EMPTY at the start of the run and everything comes
/// from what is stored: the loop finds the span by itself, purges it
/// before anything else, and streams it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_flush_that_raced_another_purge_is_found_again_after_a_restart()
{
    let chain = chain(40);
    let clean = clean_index("i_clean", &chain, chain.head).await;
    let scenario = Scenario::new("i_raced_purge").await;

    // Index the first part normally, then stop (the process ends).
    scenario.index_until(&chain, chain.slot_at(31)).await.unwrap();

    // Another process purged and rebuilt every bucket of the third UTC
    // day of this chain under epoch 1. `tombstone_version` is what it
    // stamped BEFORE its rebuild read its input.
    let tombstoned = next_version();
    let day = BASE_TIMESTAMP + 2 * 86_400;
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
               fork_block, to_block, old_head, depth, rows_tombstoned, \
               reason, tombstone_version, completed) \
             VALUES ({CHAIN}, 1, {day}, {}, {}, {}, 0, 0, 0, \
               'redecode', {tombstoned}, 1)",
            day + 86_400,
            chain.slot_at(24),
            chain.slot_at(31),
        ))
        .execute()
        .await
        .unwrap();

    // ... and this is the flush that raced it: the very same three slots,
    // written again AFTER that rebuild had read its input and still
    // stamped with the epoch in force when the flush started. Identical
    // content, so only the stamps differ - which is the whole point: no
    // hole, no orphan, nothing else to notice them by.
    let (from, to) = (chain.slot_at(25), chain.slot_at(27) + 1);
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO `{COMMIT_MARKER}` (chain, block_number, \
               blockhash, parent_slot, parent_blockhash, block_height, \
               timestamp, epoch, _version, is_deleted) \
             SELECT chain, block_number, blockhash, parent_slot, \
               parent_blockhash, block_height, timestamp, 0 AS epoch, \
               {} AS `_version`, 0 AS is_deleted \
             FROM `{COMMIT_MARKER}` FINAL WHERE chain = {CHAIN} \
               AND block_number >= {from} AND block_number < {to}",
            next_version()
        ))
        .execute()
        .await
        .unwrap();

    // No read-your-writes: wait until the question really answers itself
    // before restarting, so a pass cannot succeed for the wrong reason.
    let mut found = Vec::new();
    for _ in 0..200 {
        found = scenario
            .db
            .stale_flush_ranges_in(COMMIT_MARKER, "block_number")
            .await
            .unwrap();
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        found,
        vec![BlockRange::new(from, to)],
        "the slots flushed under the superseded epoch have to be purged \
         and indexed again"
    );

    // The restart: a brand new process, an empty in-memory queue.
    scenario.index_until(&chain, chain.head).await.unwrap();

    // It purged exactly that span before streaming anything ...
    let healed: u64 = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM reorgs WHERE chain = {CHAIN} \
             AND reason = 'gap_heal' AND fork_block = {from} \
             AND to_block = {to} AND completed = 1"
        ))
        .await;
    assert_eq!(
        healed, 1,
        "the restart did not purge the span that raced the other \
         process's purge"
    );

    // ... and what is stored is a clean index again: the span came back
    // under an epoch the validity rule counts, and nothing is doubled.
    assert_eq!(scenario.rows(COMMIT_MARKER).await, chain.produced_slots());
    scenario.assert_consistent().await;
    assert_same(
        "after a restart that found the raced flush",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );
}

/// One slot whose `blockTime` the node did not report is stored with
/// `timestamp` 0 (`src/source/solana.rs` maps `None` to the default), and
/// 0 is NOT a block time. Taking it as the start of a repair window arms
/// the validity rule from 1970 on: `epoch_floor_v` raises the floor on
/// ~20,700 days at once and every aggregate of the chain reads as zero
/// until a rebuild that slices fifty years into monthly INSERTs per
/// aggregate finishes (docs/review-round-4.md, MAJOR 6).
///
/// The store therefore reports the oldest REAL timestamp of the range, as
/// the EVM store now does. The shared clamp in `src/reorg` stays as the
/// last line of defence; this is the line that keeps it from firing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_slot_without_a_block_time_does_not_start_the_repair_window() {
    let scenario = Scenario::new("h_missing_block_time").await;
    let store = SolanaReorgStore::new(scenario.db.clone());
    let version = next_version();

    // Three produced slots: the middle one lost its block time.
    let rows = [
        (FIRST_SLOT, BASE_TIMESTAMP),
        (FIRST_SLOT + 1, 0),
        (FIRST_SLOT + 2, BASE_TIMESTAMP + SECONDS_PER_SLOT),
    ]
    .iter()
    .map(|(slot, timestamp)| {
        format!(
            "({CHAIN}, {slot}, toFixedString('', 32), {}, \
             toFixedString('', 32), {}, toDateTime({timestamp}), 0, \
             {version}, 0)",
            slot - 1,
            900_000 + slot - FIRST_SLOT
        )
    })
    .collect::<Vec<_>>()
    .join(", ");

    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO `{COMMIT_MARKER}` (chain, block_number, \
             blockhash, parent_slot, parent_blockhash, block_height, \
             timestamp, epoch, _version, is_deleted) VALUES {rows}"
        ))
        .execute()
        .await
        .unwrap();

    // No read-your-writes: wait for the rows rather than race them.
    for _ in 0..200 {
        let stored = store
            .stored_slots(
                CHAIN,
                BlockRange::new(FIRST_SLOT, FIRST_SLOT + 3),
            )
            .await
            .unwrap();
        if stored == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let span = store
        .timestamp_span(CHAIN, FIRST_SLOT, Some(FIRST_SLOT + 3))
        .await
        .unwrap()
        .expect("three stored slots are a span");

    assert_eq!(
        span,
        (BASE_TIMESTAMP, BASE_TIMESTAMP + SECONDS_PER_SLOT),
        "the repair window starts at the oldest REAL block time, not at \
         the missing one"
    );

    // A range in which NOTHING has a real block time is a different case:
    // day 0 really is the only bucket those rows contributed to, so the
    // span is reported rather than hidden.
    let only_zero = store
        .timestamp_span(CHAIN, FIRST_SLOT + 1, Some(FIRST_SLOT + 2))
        .await
        .unwrap();
    assert_eq!(only_zero, Some((0, 0)));
}

// ---------------------------------------------- (j) re-review F findings

/// Review F, NEW-1.
///
/// Five of the ten Solana venues publish no pool key, so the decoder
/// stores those trades with an EMPTY `pool_id` and the three candle
/// materialized views skip them: one shared key per venue would merge
/// USDC/SOL prices with memecoin prices into a single series.
///
/// A purge does not use the materialized views. It re-runs
/// `DerivedTable::rebuild_statements`, which is supposed to BE the view's
/// own SELECT - and that copy had no pool guard. So every purge (a tip
/// reorg, a gap heal, a module backfill) gave the repaired days one junk
/// series per venue, and a repaired index stopped equalling a clean one,
/// which is the property docs/design.md §2 exists to guarantee.
///
/// The scenario is the lost-checkpoint heal, with one unnamed-pool swap
/// stored in a day the repair rebuilds and OUTSIDE the range it purges -
/// i.e. a row the rebuild reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_purge_and_a_rebuild_equal_a_clean_index_with_an_unnamed_pool() {
    let chain = chain(40);

    // The hole that is purged and streamed again, and the first UTC day
    // `complete_days` considers whole.
    let hole_from = chain.slot_at(15);
    let hole_to = chain.slot_at(25);
    let day_one = BASE_TIMESTAMP + 86_400;

    let clean = clean_index("j_unnamed_clean", &chain, chain.head).await;
    let copied = clean.add_unnamed_pool_swap(day_one, hole_from).await;

    let scenario = Scenario::new("j_unnamed_purged").await;
    scenario.index_until(&chain, chain.head).await.unwrap();
    assert_eq!(
        scenario.add_unnamed_pool_swap(day_one, hole_from).await,
        copied,
        "the two indexes must hold the SAME unnamed-pool row"
    );

    let swaps = scenario.rows("sol_dex_swaps").await;

    // The crash: the checkpoint of a middle window never became live,
    // while every row it covered did. The heal purges that range and
    // streams it again - and rebuilds the candles of the days it touched,
    // which is where the unnamed-pool row is read.
    scenario.unclaim(hole_from, hole_to).await;
    scenario.index_until(&chain, chain.head).await.unwrap();

    assert_eq!(scenario.rows("sol_dex_swaps").await, swaps);

    // No candle may be keyed on the empty pool: that series is the merged
    // one, and a clean index has none.
    for view in SOL_CANDLE_VIEWS {
        let junk = scenario
            .count(&format!(
                "SELECT toUInt64(count()) FROM `{view}` \
                 WHERE chain = {CHAIN} AND pool_id = toFixedString('', 32)"
            ))
            .await;
        assert_eq!(
            junk, 0,
            "{view}: the rebuild re-created the merged price series of \
             the pools it cannot name"
        );
    }

    scenario.assert_consistent().await;
    assert_same(
        "after a heal with an unnamed-pool swap in the rebuilt days",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );
}

/// Review F, NEW-5.
///
/// The queue of flushes that raced another process's purge is drained by
/// `pass()`, and `pass()` only runs when `target > cursor`. On a bounded
/// run whose range is already tiled the cursor starts AT the resume point,
/// so `pass()` was never entered and the queue - which startup had just
/// filled from the database, announcing in capitals that those spans are
/// "purged and indexed again before anything else" - was carried to the
/// exit untouched. Nothing else ever asks for them: their rows are stored,
/// so the tiling shows no hole, and they stay hidden from every aggregate
/// until somebody runs an unbounded `indexer run`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_bounded_run_over_a_tiled_range_still_drains_the_stale_queue() {
    let chain = chain(40);
    let clean = clean_index("j_drain_clean", &chain, chain.head).await;
    let scenario = Scenario::new("j_drain").await;

    // The whole range, indexed and tiled: a second bounded run over it has
    // nothing to stream.
    scenario.index_until(&chain, chain.head).await.unwrap();
    scenario.assert_consistent().await;

    // Another process purged and rebuilt the third UTC day under epoch 1,
    // and this flush of three slots raced it: identical rows, stamped with
    // the epoch that was in force when the flush started. Nothing but the
    // stamps distinguishes them - no hole, no orphan.
    let tombstoned = next_version();
    let day = BASE_TIMESTAMP + 2 * 86_400;
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
               fork_block, to_block, old_head, depth, rows_tombstoned, \
               reason, tombstone_version, completed) \
             VALUES ({CHAIN}, 1, {day}, {}, {}, {}, 0, 0, 0, \
               'redecode', {tombstoned}, 1)",
            day + 86_400,
            chain.slot_at(24),
            chain.slot_at(31),
        ))
        .execute()
        .await
        .unwrap();

    let (from, to) = (chain.slot_at(25), chain.slot_at(27) + 1);
    scenario
        .db
        .db
        .query(&format!(
            "INSERT INTO `{COMMIT_MARKER}` (chain, block_number, \
               blockhash, parent_slot, parent_blockhash, block_height, \
               timestamp, epoch, _version, is_deleted) \
             SELECT chain, block_number, blockhash, parent_slot, \
               parent_blockhash, block_height, timestamp, 0 AS epoch, \
               {} AS `_version`, 0 AS is_deleted \
             FROM `{COMMIT_MARKER}` FINAL WHERE chain = {CHAIN} \
               AND block_number >= {from} AND block_number < {to}",
            next_version()
        ))
        .execute()
        .await
        .unwrap();

    // No read-your-writes: the run must be able to SEE the span at
    // startup, or the test would pass for the wrong reason.
    for _ in 0..200 {
        let found = scenario
            .db
            .stale_flush_ranges_in(COMMIT_MARKER, "block_number")
            .await
            .unwrap();
        if found == vec![BlockRange::new(from, to)] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // THE BOUNDED RUN, over the very range that is already complete.
    scenario.index_until(&chain, chain.head).await.unwrap();

    let healed: u64 = scenario
        .count(&format!(
            "SELECT toUInt64(count()) FROM reorgs WHERE chain = {CHAIN} \
             AND reason = 'gap_heal' AND fork_block = {from} \
             AND to_block = {to} AND completed = 1"
        ))
        .await;
    assert_eq!(
        healed, 1,
        "the bounded run exited without purging the span its own startup \
         said it would purge before anything else"
    );

    // And it did not just purge: the slots came back under an epoch the
    // validity rule counts, so the index equals a clean one again.
    assert_eq!(scenario.rows(COMMIT_MARKER).await, chain.produced_slots());
    scenario.assert_consistent().await;
    assert_same(
        "after a bounded run drained the stale queue",
        &scenario.snapshot().await,
        &clean.snapshot().await,
    );
}

/// Review F, NEW-2.
///
/// `indexer verify --chain solana` compares `sum(swaps)` of
/// `sol_dex_candles_1d_v` with `count()` over `sol_dex_swaps`, per UTC
/// day. The view skips the swaps whose pool has no name; the base side
/// counted them, so every day holding one - a perfectly healthy day -
/// printed PROBLEMS FOUND. The operator's own health check is the only
/// thing that can find a doubled aggregate, so crying wolf is expensive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_swap_with_no_pool_name_is_not_an_inconsistency() {
    let chain = chain(40);
    let scenario = Scenario::new("j_unnamed_verify").await;

    scenario.index_until(&chain, chain.head).await.unwrap();
    scenario.assert_consistent().await;

    // One unnamed-pool swap, in a day `verify` checks completely.
    let day_one = BASE_TIMESTAMP + 86_400;
    scenario.add_unnamed_pool_swap(day_one, chain.slot_at(30)).await;

    let report = scenario.verify().await;
    assert!(
        report.candles.is_empty(),
        "a swap whose pool has no name is counted on one side only:\n\
         {report}"
    );
    assert!(report.is_consistent(), "{report}");
}
