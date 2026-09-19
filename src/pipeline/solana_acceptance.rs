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
        solana_store::{SolanaReorgStore, SOL_CANDLE_VIEWS},
        solana_verify,
        solana_writer::{store_children, SvmBatch},
    },
    reorg::ReorgStore,
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

    fn slot_at(&self, index: usize) -> u64 {
        self.slots[index].slot
    }

    fn produced_slots(&self) -> u64 {
        self.slots.len() as u64
    }
}

impl SlotSource for TestChain {
    async fn head(&self) -> Result<u64> {
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
        names.extend(SOL_CANDLE_VIEWS.iter().map(|view| (*view, false)));

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

    async fn verify(&self) -> solana_verify::SolanaVerifyReport {
        solana_verify::verify(&self.db, FIRST_SLOT, 0).await.unwrap()
    }

    async fn assert_consistent(&self) {
        let report = self.verify().await;
        assert!(report.is_consistent(), "{report}");
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

    // Every stored transaction and swap has its slot: the commit marker
    // invariant, checked here and not only by `verify`.
    for table in ["sol_transactions", "sol_dex_swaps"] {
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
    .checkpoint_tiling(CHAIN, FIRST_SLOT, 10_000)
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

    let batch = SvmBatch {
        rows,
        windows: vec![BlockRange::new(middle, chain.slot_at(30))],
    };
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

// ------------------------------------------------------------------- (c)

/// The same flush applied twice - a timed-out insert that WAS applied and
/// is retried - must count once, in the candles as well as in the base
/// table.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_retried_flush_counts_once() {
    let scenario = Scenario::new("c_retried").await;
    let chain = chain(20);

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

    let batch = SvmBatch { rows, windows: vec![window] };

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
        &SvmBatch { rows: again, windows: vec![window] },
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
        .checkpoint_tiling(CHAIN, FIRST_SLOT, 10_000)
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

    let report = scenario.verify().await;
    assert!(!report.is_consistent(), "{report}");
    assert!(!report.unasked.is_empty(), "{report}");
    assert!(
        report.to_string().contains("Result: PROBLEMS FOUND"),
        "{report}"
    );
    assert!(report.to_string().contains("never asked for"), "{report}");
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

    let report = scenario.verify().await;
    assert!(!report.is_consistent(), "{report}");
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
