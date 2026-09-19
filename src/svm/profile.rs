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

use std::time::Instant;

use crate::svm::{decode::decode_transaction, fixtures, models::SOLANA_CHAIN};

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
