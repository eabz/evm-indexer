//! Decode-speed measurements. Ignored by default:
//!
//! ```sh
//! cargo test --release svm::profile -- --ignored --nocapture
//! ```
//!
//! Phase 1 measured ~4.5 minutes to decode 150 slots, against a chain that
//! produces ~324,000 slots a day. That is ~65x too slow to keep up, so where
//! the time goes is a number this module has to produce rather than guess.
//!
//! The corpus is the recorded fixtures, replayed at scale. That is not a
//! synthetic benchmark: the transactions are real mainnet ones with real
//! instruction counts, and replaying them measures exactly the per
//! transaction work the pipeline does. The one thing it cannot measure is
//! the NETWORK, which `live_tests::live_decode_speed` times separately -
//! and the split between the two turned out to matter more than any single
//! hot spot.
//!
//! [`flush_cost_per_table`] measures the OTHER half - what a flush costs in
//! ClickHouse - and needs a database:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@127.0.0.1:8123/anything \
//!   cargo test --release svm::profile::flush_cost -- --ignored --nocapture
//! ```

use std::time::Instant;

use crate::svm::{
    decode::decode_transaction, fixtures, models::SOLANA_CHAIN,
};

/// How many times the fixture corpus is replayed. Sized so a run takes a few
/// seconds in release mode.
const REPLAYS: usize = 200;

/// Per-transaction decode cost, broken down by fixture.
///
/// Run it before and after a change and compare the totals.
#[test]
#[ignore]
fn decode_cost_per_transaction() {
    let corpus = fixtures::all();
    println!("\n=== svm decode cost ({REPLAYS} replays of {} real transactions) ===", corpus.len());

    let mut grand_total = 0.0;
    for fixture in corpus {
        let instructions = fixture.transaction.instructions.len();
        let activity = fixture.transaction.activity.len();

        // Warm up, so the first fixture is not charged for cold caches.
        for _ in 0..5 {
            std::hint::black_box(decode_transaction(
                SOLANA_CHAIN,
                fixture.timestamp(),
                &fixture.transaction,
            ));
        }

        let start = Instant::now();
        let mut swaps = 0;
        for _ in 0..REPLAYS {
            let outcome = decode_transaction(
                SOLANA_CHAIN,
                fixture.timestamp(),
                &fixture.transaction,
            );
            swaps += outcome.swaps.len();
        }
        let elapsed = start.elapsed();
        let per_tx = elapsed.as_secs_f64() * 1e6 / REPLAYS as f64;
        grand_total += per_tx;

        println!(
            "  {:<22} {instructions:>3} ix {activity:>3} accounts  \
             {per_tx:>9.1} us/tx  ({} swaps)",
            fixture.name,
            swaps / REPLAYS
        );
    }
    println!("  {:<22} {grand_total:>32.1} us total", "SUM");

    // A rough projection, stated as what it is: the fixtures are a handful
    // of transactions, not a slot's worth, so this is an order of magnitude
    // and not a throughput figure.
    println!(
        "\n  at this cost one core decodes ~{:.0} transactions/second",
        corpus.len() as f64 * 1e6 / grand_total
    );
}

// ---------------------------------------------------------------- flush

/// How many flushes the benchmark issues in a row, `FLUSH_BENCH_FLUSHES`
/// to override. The cost of a flush is not a constant: every insert makes
/// a part and the engine merges what is already there, so what matters is
/// the cost of the LAST flush as much as the first, and a long run is the
/// only way to see a table that gets slower as it fills.
const FLUSHES: usize = 12;

/// How many times the fixture corpus is replicated into one flush,
/// `FLUSH_BENCH_COPIES` to override. The corpus decodes to ~100 rows, so
/// 40 copies is a batch of a few thousand rows across the nine tables -
/// the size a few seconds of Solana with the launchpads on produces.
const COPIES_PER_FLUSH: u64 = 40;

fn from_env(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

/// Slots between two copies, so a flush covers a realistic slot span
/// instead of stacking every row on one slot.
const SLOT_STRIDE: u64 = 3;

/// What a Solana flush costs in ClickHouse, table by table.
///
/// Why it exists: the flush went from ~50 ms to 0.6-5.9 s when the
/// launchpad tables and their materialized views joined it
/// (docs/CHECKPOINT.md, "Known open items"), and the suspect - one table
/// partitioned by chain, i.e. one partition holding all of Solana - was a
/// guess. This measures it: every `insert_flush` the writer issues, timed
/// separately, over a dozen consecutive flushes, against a database that
/// starts empty.
///
/// It also times `sol_token_balances_legacy`, a copy of that table in its
/// OLD shape (`PARTITION BY chain`, the latest-value key), with the very
/// same rows - so the before and the after are one run on one server.
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn flush_cost_per_table() {
    use crate::{
        db::{migrate, next_version, Database, DatabaseParams, FlushKey},
        svm::{self, SvmRows, SvmSlotBatch},
    };
    use clickhouse::Client;

    let flushes = from_env("FLUSH_BENCH_FLUSHES", FLUSHES as u64);
    let copies = from_env("FLUSH_BENCH_COPIES", COPIES_PER_FLUSH);

    let base = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for the ignored tests");
    let params = DatabaseParams::parse(&base).unwrap();
    let name = format!(
        "svm_flush_profile_{}_test",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let admin = Client::default()
        .with_url(&params.endpoint)
        .with_user(&params.user)
        .with_password(&params.password);
    let url = {
        let (head, _) = base.rsplit_once('/').unwrap_or((&base, ""));
        format!("{head}/{name}")
    };
    migrate::run(&url).await.expect("apply migrations");
    let db = Database::new(&url, SOLANA_CHAIN).await.unwrap();

    // The table as it was before review round 4: one partition for the
    // whole chain, keyed on the account rather than the observation.
    db.db
        .query(
            "CREATE TABLE sol_token_balances_legacy AS \
             sol_token_balances ENGINE = \
             ReplacingMergeTree(_version, is_deleted) PARTITION BY chain \
             ORDER BY (chain, mint, owner, account) \
             SETTINGS do_not_merge_across_partitions_select_final = 1",
        )
        .execute()
        .await
        .expect("create the legacy shape");

    // Copies with NO materialized views on them, so the cost of the views
    // is a subtraction rather than a guess: `launchpad_trades` feeds five
    // (two side tables, two candle families, the venue 1d aggregate) and
    // `sol_dex_swaps` feeds the three Solana candles.
    for bare in ["launchpad_trades", "sol_dex_swaps"] {
        db.db
            .query(&format!("CREATE TABLE {bare}_bare AS {bare}"))
            .execute()
            .await
            .expect("create the view-free copy");
    }

    // One decode of the real corpus, replicated with fresh slots and fresh
    // identities: the row SHAPES and their ratio are the fixtures', the
    // cardinality is a real batch's.
    let corpus: SvmRows = {
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
        svm::decode(SOLANA_CHAIN, &batches)
    };
    let first_slot =
        corpus.slots.iter().map(|s| s.block_number).min().unwrap();

    let mut totals: Vec<(&str, Vec<f64>)> = Vec::new();
    let record = |table: &'static str, ms: f64, totals: &mut Vec<_>| {
        if let Some((_, samples)) = totals
            .iter_mut()
            .find(|(name, _): &&mut (&str, Vec<f64>)| *name == table)
        {
            samples.push(ms);
        } else {
            totals.push((table, vec![ms]));
        }
    };

    println!(
        "\n=== Solana flush cost, {flushes} consecutive flushes of \
         {copies} corpus copies ==="
    );

    for flush in 0..flushes {
        let mut rows = SvmRows::default();
        for copy in 0..copies {
            let index = flush * copies + copy;
            let mut part = shift(&corpus, index, first_slot);
            rows.append(&mut part);
        }
        let version = next_version();
        rows.set_version(version);
        rows.set_epoch(0);

        let span = (
            rows.slots.iter().map(|s| s.block_number).min().unwrap(),
            rows.slots.iter().map(|s| s.block_number).max().unwrap(),
        );
        let key = FlushKey { chain: SOLANA_CHAIN, span, version };

        let mut flush_total = 0.0;
        macro_rules! timed {
            ($table:literal, $rows:expr) => {{
                let started = Instant::now();
                db.insert_flush($table, $rows, &key)
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", $table));
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                flush_total += ms;
                record($table, ms, &mut totals);
            }};
        }

        // Only these nine are the flush. The rows after them go into
        // copies of two of the tables and are reported separately.
        let pads = &rows.launchpads;
        timed!("launchpad_tokens", &pads.tokens);
        timed!("launchpad_trades", &pads.trades);
        timed!("launchpad_graduations", &pads.graduations);
        timed!("launchpad_creator_fees", &pads.creator_fees);
        timed!("sol_token_balances", &pads.balances);
        timed!("sol_dex_swaps", &rows.swaps);
        timed!("sol_transactions", &rows.transactions);
        timed!("sol_slots", &rows.slots);

        let flush_total = flush_total;
        let mut aside = 0.0;
        macro_rules! compared {
            ($table:literal, $rows:expr) => {{
                let started = Instant::now();
                db.insert_flush($table, $rows, &key)
                    .await
                    .unwrap_or_else(|e| panic!("{}: {e}", $table));
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                aside += ms;
                record($table, ms, &mut totals);
            }};
        }
        compared!("sol_token_balances_legacy", &pads.balances);
        compared!("launchpad_trades_bare", &pads.trades);
        compared!("sol_dex_swaps_bare", &rows.swaps);
        let _ = aside;

        println!(
            "  flush {flush:>2}: {:>5} rows, {flush_total:>8.1} ms",
            rows.rows()
        );
    }

    println!(
        "\n  {:<28} {:>9} {:>9} {:>9}",
        "table", "first", "last", "mean"
    );
    for (table, samples) in &totals {
        let mean: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
        println!(
            "  {table:<28} {:>9.1} {:>9.1} {:>9.1}",
            samples[0],
            samples[samples.len() - 1],
            mean
        );
    }

    // What the change to an append log costs the READER. The holder
    // screen now takes each account's newest observation instead of
    // reading one row per account, so its price is worth a number too -
    // measured against the legacy shape holding the very same rows.
    let mint: String = db
        .db
        .query("SELECT hex(mint) FROM sol_token_balances LIMIT 1")
        .fetch_one()
        .await
        .unwrap();
    let rows_held: u64 = db
        .db
        .query("SELECT toUInt64(count()) FROM sol_token_balances")
        .fetch_one()
        .await
        .unwrap();

    let started = Instant::now();
    let holders: u64 = db
        .db
        .query(&format!(
            "SELECT toUInt64(count()) FROM \
             sol_launchpad_token_holders_v(chain = {SOLANA_CHAIN}, \
             token = '{mint}', as_of_block = 18446744073709551615)"
        ))
        .fetch_one()
        .await
        .unwrap();
    let append_log = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    let _: u64 = db
        .db
        .query(&format!(
            "SELECT toUInt64(count()) FROM (SELECT owner, \
             toFloat64(balance) AS balance_raw, max(block_number) \
             FROM sol_token_balances_legacy FINAL WHERE \
             chain = {SOLANA_CHAIN} AND mint = unhex('{mint}') \
             AND is_deleted = 0 GROUP BY owner, balance \
             HAVING balance_raw > 0)"
        ))
        .fetch_one()
        .await
        .unwrap();
    let projection = started.elapsed().as_secs_f64() * 1000.0;

    println!(
        "\n  holder screen over {rows_held} observations ({holders} \
         holders of the sampled mint):\n    append log \
         {append_log:>8.1} ms     old projection {projection:>8.1} ms"
    );

    let _ = admin
        .query(&format!("DROP DATABASE IF EXISTS {name}"))
        .execute()
        .await;
}

/// The corpus with every slot and every identity moved by `index`, so the
/// copies are distinct rows in distinct slots rather than one key written
/// over and over.
#[cfg(test)]
fn shift(
    corpus: &crate::svm::SvmRows,
    index: u64,
    first_slot: u64,
) -> crate::svm::SvmRows {
    use crate::svm::{models::Pubkey, SvmRows};

    let mut rows = SvmRows {
        slots: corpus.slots.clone(),
        transactions: corpus.transactions.clone(),
        swaps: corpus.swaps.clone(),
        tokens: corpus.tokens.clone(),
        launchpads: corpus.launchpads.clone(),
        diagnostics: Default::default(),
    };

    let offset = index * SLOT_STRIDE;
    let slot = |n: u64| n + offset;
    // The identity bytes: a different mint / pool / account per copy, so
    // the sorting keys spread the way a real batch's do.
    let id = |key: &mut Pubkey| {
        key[28..32].copy_from_slice(&(index as u32).to_le_bytes())
    };

    for row in &mut rows.slots {
        row.block_number = slot(row.block_number);
        row.parent_slot = slot(row.parent_slot);
        row.timestamp += (offset * 400 / 1000) as u32;
        id(&mut row.blockhash);
    }
    for row in &mut rows.transactions {
        row.block_number = slot(row.block_number);
        row.signature[0..4].copy_from_slice(&(index as u32).to_le_bytes());
    }
    for row in &mut rows.swaps {
        row.block_number = slot(row.block_number);
        id(&mut row.pool_id);
        id(&mut row.trader);
    }
    for row in &mut rows.tokens {
        id(&mut row.mint);
    }
    let pads = &mut rows.launchpads;
    for row in &mut pads.tokens {
        row.block_number = slot(row.block_number);
        id(&mut row.token);
        id(&mut row.curve);
    }
    for row in &mut pads.trades {
        row.block_number = slot(row.block_number);
        id(&mut row.token);
        id(&mut row.emitter);
        id(&mut row.trader);
    }
    for row in &mut pads.graduations {
        row.block_number = slot(row.block_number);
        id(&mut row.token);
    }
    for row in &mut pads.creator_fees {
        row.block_number = slot(row.block_number);
        id(&mut row.token);
        id(&mut row.recipient);
    }
    for row in &mut pads.balances {
        row.block_number = slot(row.block_number);
        id(&mut row.mint);
        id(&mut row.owner);
        id(&mut row.account);
    }

    let _ = first_slot;
    rows
}

/// The ed25519 curve test in isolation.
///
/// It is the only genuinely expensive primitive in the decoder - a modular
/// exponentiation with a 252-bit exponent - and the movement layer calls it
/// on every candidate owner of every venue instruction. This test is here so
/// its cost, and the effect of the cache in front of it, are both visible.
#[test]
#[ignore]
fn on_curve_cost() {
    let seed = crate::svm::programs::pubkey(
        "HaJwBJYmFyBRxuVQe4Yr53wkkDQbWsDH8uC7S362y1Ew",
    );

    // Distinct keys: every call misses the cache and pays full price.
    let keys: Vec<[u8; 32]> = (0..2_000u32)
        .map(|i| {
            let mut key = seed;
            key[0..4].copy_from_slice(&i.to_le_bytes());
            key
        })
        .collect();

    let start = Instant::now();
    let mut on = 0;
    for key in &keys {
        if crate::svm::pda::is_on_curve(key) {
            on += 1;
        }
    }
    let cold = start.elapsed();

    // The same keys again: in the real stream a pool authority repeats in
    // every transaction that touches the pool, which is what the cache is
    // for.
    let start = Instant::now();
    for key in &keys {
        std::hint::black_box(crate::svm::pda::is_on_curve(key));
    }
    let warm = start.elapsed();

    println!("\n=== ed25519 curve test ===");
    println!(
        "  cold {:>8.2} us/key   ({} of {} keys on the curve)",
        cold.as_secs_f64() * 1e6 / keys.len() as f64,
        on,
        keys.len()
    );
    println!(
        "  warm {:>8.2} us/key   ({:.0}x)",
        warm.as_secs_f64() * 1e6 / keys.len() as f64,
        cold.as_secs_f64() / warm.as_secs_f64().max(f64::EPSILON)
    );
}
