//! LIVE proof against mainnet. Ignored by default, and network bound:
//!
//! ```sh
//! ENVIO_API_TOKEN=... cargo test svm::live -- --ignored --nocapture
//! ```
//!
//! Two things are proven here that no fixture can prove:
//!
//! 1. the source really streams, and the decoder's coverage on arbitrary
//!    recent slots is MEASURED rather than assumed, per venue;
//! 2. the decoded amounts, mints and trader agree EXACTLY with a completely
//!    independent source - the public Solana RPC's `getTransaction`, which
//!    is a different server, a different wire format and a different code
//!    path all the way down.
//!
//! The cross-check does not compare HyperSync against HyperSync. It
//! recomputes each swap from the RPC's own `meta.preTokenBalances` /
//! `postTokenBalances` (and `preBalances` / `postBalances` for a native SOL
//! leg), which are validator metadata, and requires the numbers to be
//! identical.
//!
//! Probes are kept small and polite: a few hundred slots and a few dozen RPC
//! calls, spaced out. The token is read from the environment and is never
//! printed, logged or written anywhere.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;

use crate::{
    source::solana::SolanaSource,
    svm::{
        self,
        models::{Pubkey, SOLANA_CHAIN},
        programs::{to_base58, Venue, VENUES},
    },
};

/// Public, rate limited and best effort - exactly what the README says the
/// Solana RPC default is.
const RPC: &str = "https://api.mainnet-beta.solana.com";

/// Slots per live run. Small on purpose: 150 slots is already ~10k swaps,
/// and the whole point is a proof, not a benchmark.
const SLOTS: u64 = 150;

/// Stay well below the head: the server has no live-tail mode and errors
/// with "no progress" at the tip.
const HEAD_MARGIN: u64 = 400;

/// The API token, from the environment or from the git-ignored `.env`.
///
/// The file fallback exists because a git WORKTREE has no `.env` of its
/// own - the file lives once, at the main checkout - and because putting a
/// secret on a command line publishes it to every process table on the
/// machine. The value is never printed, logged or written anywhere.
fn token() -> Option<String> {
    if let Some(value) = std::env::var("ENVIO_API_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Some(value);
    }
    dotenv_token()
}

/// Reads `ENVIO_API_TOKEN` out of the nearest `.env`, searching this crate's
/// directory and its parents (a worktree sits three levels under the main
/// checkout, which is where the file is).
fn dotenv_token() -> Option<String> {
    let mut dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..5 {
        let candidate = dir.join(".env");
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            for line in text.lines() {
                let line = line.trim();
                let Some(value) = line.strip_prefix("ENVIO_API_TOKEN=")
                else {
                    continue;
                };
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_owned();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// What one live run measured, beyond the rows themselves.
#[derive(Debug, Default, Clone, Copy)]
struct RunCost {
    queries: u32,
    slots: u64,
    /// Seconds spent waiting for the server.
    fetch_seconds: f64,
    /// Seconds spent in the pure decoder.
    decode_seconds: f64,
    transactions: usize,
}

/// Streams `SLOTS` slots below the head and decodes them, timing the two
/// halves separately.
///
/// The split matters more than any single hot spot: phase 1 reported ~4.5
/// minutes for 150 slots and treated it as decode cost, but the decoder and
/// the network are not remotely comparable here and only a measurement says
/// which one to fix.
async fn stream_and_decode(
    token: &str,
) -> (svm::SvmRows, u64, u64, RunCost) {
    use std::time::Instant;

    let source = SolanaSource::new(None, token).expect("build source");
    let head = source.head().await.expect("head");
    let from = head - HEAD_MARGIN - SLOTS;
    let to = from + SLOTS;

    let mut rows = svm::SvmRows::default();
    let mut cost = RunCost::default();
    let mut cursor = from;

    // Follow the server's truncation cursor to the end of the range, the
    // way the pipeline's own loop will have to.
    while cursor < to {
        let started = Instant::now();
        let batch = source.fetch(cursor, to).await.expect("fetch");
        cost.fetch_seconds += started.elapsed().as_secs_f64();
        cost.queries += 1;
        cost.slots += batch.batches.len() as u64;
        cost.transactions += batch
            .batches
            .iter()
            .map(|slot| slot.transactions.len())
            .sum::<usize>();

        assert!(
            batch.next_slot > cursor,
            "the server made no progress at {cursor}: a resume loop must \
             treat this as a stop condition, never spin"
        );

        let started = Instant::now();
        let mut decoded = svm::decode(SOLANA_CHAIN, &batch.batches);
        cost.decode_seconds += started.elapsed().as_secs_f64();

        rows.append(&mut decoded);
        cursor = batch.next_slot;
    }

    (rows, from, to, cost)
}

fn report_cost(cost: &RunCost) {
    println!("\n=== cost of this run ===");
    println!(
        "  {} queries for {} slots = {:.1} slots/query",
        cost.queries,
        cost.slots,
        cost.slots as f64 / f64::from(cost.queries.max(1))
    );
    println!(
        "  fetch  {:>8.2} s  ({:.0}%)",
        cost.fetch_seconds,
        100.0 * cost.fetch_seconds
            / (cost.fetch_seconds + cost.decode_seconds).max(f64::EPSILON)
    );
    println!(
        "  decode {:>8.2} s  ({:.0}%)   {:.0} us/transaction",
        cost.decode_seconds,
        100.0 * cost.decode_seconds
            / (cost.fetch_seconds + cost.decode_seconds).max(f64::EPSILON),
        cost.decode_seconds * 1e6 / cost.transactions.max(1) as f64
    );
    // Solana produces ~324,538 slots a day (research section 11.2).
    let per_day = 324_538.0;
    println!(
        "  projected to a full day: {:.0} queries ({:.1}/minute against a \
         free budget of 30) and {:.1} minutes of decode",
        per_day / (cost.slots as f64 / f64::from(cost.queries.max(1))).max(1.0),
        per_day
            / (cost.slots as f64 / f64::from(cost.queries.max(1))).max(1.0)
            / 1440.0,
        per_day / cost.slots.max(1) as f64 * cost.decode_seconds / 60.0
    );
}

/// The fix the plan analyst found: raising ONE response cap raises nothing.
///
/// This is the whole reason phase 1 could not keep up. `max_num_instructions`
/// was set and the other four caps were not, so whichever of them was lowest
/// stopped the response - at ONE slot. It is measured here rather than
/// asserted from a document, and it costs two queries.
#[tokio::test]
#[ignore]
async fn live_response_caps_decide_slots_per_query() {
    use hypersync_client_solana::{config::ClientConfig, Client};

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    let from = head - HEAD_MARGIN - 200;
    let to = from + 200;

    let client = Client::new(ClientConfig {
        url: crate::source::solana::DEFAULT_URL.to_owned(),
        bearer_token: Some(token.clone()),
        ..Default::default()
    })
    .expect("client");

    // What phase 1 sent: one cap raised, the rest left unset.
    let mut crippled = crate::source::solana::build_query(from, to);
    crippled.max_num_blocks = None;
    crippled.max_num_transactions = None;
    crippled.max_num_logs = None;
    crippled.max_num_account_activity = None;
    let before = client.get(&crippled).await.expect("crippled query");

    // What this module sends now.
    let after = client
        .get(&crate::source::solana::build_query(from, to))
        .await
        .expect("full query");

    println!("\n=== response caps, measured ===");
    println!(
        "  only max_num_instructions raised : {:>4} slots, {:>7} \
         instruction rows",
        before.blocks.len(),
        before.instruction_calls.len()
    );
    println!(
        "  every cap raised                 : {:>4} slots, {:>7} \
         instruction rows, {} log rows",
        after.blocks.len(),
        after.instruction_calls.len(),
        after.logs.len()
    );
    println!(
        "  -> {:.0}x more slots per query",
        after.blocks.len() as f64 / before.blocks.len().max(1) as f64
    );

    assert!(
        after.blocks.len() > before.blocks.len(),
        "raising every cap must return more slots than raising one"
    );
    assert!(
        after.blocks.len() >= 10,
        "only {} slots came back with every cap raised; the pipeline needs \
         ~9,300 queries a day at 35 slots each to follow the head",
        after.blocks.len()
    );
    // The log table is the phase 2 prerequisite: Raydium and Orca publish
    // their swap events there and nowhere else.
    assert!(
        !after.logs.is_empty(),
        "no log rows came back, so no Raydium or Orca event can ever decode"
    );
}

/// The four launchpad row kinds, live, for all three Solana launchpads.
///
/// Prints what a UI would actually have, per family, and asserts the three
/// properties that make the rows usable at all:
///
/// 1. every launch's curve was RE-DERIVED as a PDA of the launch's own
///    fields, so no row rests on an account meta index being right;
/// 2. every curve trade's token leg is corroborated by the movement layer;
/// 3. a pump.fun graduation names a PumpSwap pool that the DEX decoder
///    also wrote a swap for - the join that makes a token's chart continue
///    after the curve is gone.
///
/// `LAUNCHPAD_SLOTS` widens the window (default 400). Graduations are rare -
/// a few hundred a day against millions of trades - so property 3 needs a
/// wide one and reports rather than fails when the window holds none.
#[tokio::test]
#[ignore]
async fn live_launchpad_rows() {
    use crate::svm::launchpads::SolFamily;

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };
    let slots: u64 = std::env::var("LAUNCHPAD_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    let mut cursor = head - HEAD_MARGIN - slots;
    let end = cursor + slots;

    let mut rows = svm::SvmRows::default();
    let mut queries = 0u32;
    while cursor < end {
        let batch = source.fetch(cursor, end).await.expect("fetch");
        assert!(batch.next_slot > cursor, "no progress at {cursor}");
        queries += 1;
        let mut decoded = svm::decode(SOLANA_CHAIN, &batch.batches);
        rows.append(&mut decoded);
        cursor = batch.next_slot;
    }

    let pads = &rows.launchpads;
    println!(
        "\n=== Solana launchpads: {slots} slots, {queries} queries ==="
    );
    println!(
        "launches {}  curve trades {}  graduations {}  fee rows {}  \
         configs {}  holder balances {}",
        pads.tokens.len(),
        pads.trades.len(),
        pads.graduations.len(),
        pads.creator_fees.len(),
        pads.configs.len(),
        pads.balances.len(),
    );
    println!("diagnostics {:?}", pads.diagnostics);

    for family in SolFamily::ALL {
        let name = family.as_str();
        let launches =
            pads.tokens.iter().filter(|r| r.family == name).count();
        let trades =
            pads.trades.iter().filter(|r| r.family == name).count();
        let verified = pads
            .trades
            .iter()
            .filter(|r| r.family == name && r.token_verified == 1)
            .count();
        let graduating = pads
            .trades
            .iter()
            .filter(|r| r.family == name && r.graduating == 1)
            .count();
        let graduations =
            pads.graduations.iter().filter(|r| r.family == name).count();
        let fees =
            pads.creator_fees.iter().filter(|r| r.family == name).count();
        let with_progress = pads
            .trades
            .iter()
            .filter(|r| r.family == name && !r.progress_wad.is_zero())
            .count();
        println!(
            "  {name:<18} launches {launches:>4}  trades {trades:>6} \
             ({verified} token-verified, {with_progress} with progress)  \
             curve filled {graduating:>3}  graduations {graduations:>3}  \
             fee rows {fees:>4}"
        );
    }

    // (1) Every launch was proven, not claimed: a row only exists when the
    //     curve re-derived from the launch's own fields.
    assert_eq!(
        pads.diagnostics.curve_not_derived, 0,
        "a launch whose curve could not be re-derived was refused; if this \
         is not zero an account meta index has moved"
    );
    assert_eq!(
        pads.diagnostics.bad_length, 0,
        "an event arrived at a length no layout here expects, i.e. a \
         program appended a field"
    );

    // (2) The movement layer corroborates the curve trades.
    let trades = pads.trades.len();
    assert!(trades > 0, "no curve trades in {slots} slots");
    let verified =
        pads.trades.iter().filter(|r| r.token_verified == 1).count();
    println!(
        "\ntoken leg corroborated by real token movement: {verified} of \
         {trades} ({:.2}%)",
        100.0 * verified as f64 / trades as f64
    );
    assert!(
        verified * 100 >= trades * 95,
        "only {verified} of {trades} curve trades were corroborated"
    );

    // Every trade's emitter must be a curve some launch announced, or the
    // token join and the trusted-curve filter would both miss it.
    let curves: std::collections::HashSet<Pubkey> =
        pads.tokens.iter().map(|row| row.curve).collect();
    let known = pads
        .trades
        .iter()
        .filter(|row| curves.contains(&row.emitter))
        .count();
    println!(
        "trades whose curve was ALSO launched inside this window: {known} \
         (the rest launched earlier, exactly as on EVM)"
    );

    // (3) The graduation join. This is the whole point of the module.
    let pools: std::collections::HashSet<Pubkey> =
        rows.swaps.iter().map(|swap| swap.pool_id).collect();
    let mut joined = 0;
    for row in &pads.graduations {
        if row.pool_id == crate::svm::models::ZERO_PUBKEY {
            continue;
        }
        println!(
            "\n  graduation: {} -> pool {} ({} in the same window: {})",
            to_base58(&row.token),
            to_base58(&row.pool_id),
            row.family,
            if pools.contains(&row.pool_id) {
                "the DEX decoder wrote swaps for that pool too"
            } else {
                "no swap on that pool in this window yet"
            }
        );
        if pools.contains(&row.pool_id) {
            joined += 1;
        }
    }
    println!(
        "\ngraduations naming a destination pool: {} of {} (pump.fun's \
         CompletePumpAmmMigrationEvent names one; DBC and LaunchLab emit \
         nothing at migration)",
        pads.graduations
            .iter()
            .filter(|r| r.pool_id != crate::svm::models::ZERO_PUBKEY)
            .count(),
        pads.graduations.len()
    );
    println!("of those, {joined} already join a sol_dex_swaps pool id");
}

/// Every pump.fun curve instruction in a live window, bucketed by what the
/// decoder did with it and WHY.
///
/// The README reported 98.2% agreement on the pump.fun curve and left the
/// remaining 1.8% unexplained. A rate is a symptom; this prints the
/// diagnosis, per instruction rather than per row, because the interesting
/// cases are the ones that produced NO row at all and therefore never
/// reached the agreement statistic.
///
/// Run with `PUMPFUN_SLOTS` to widen the window (default 120).
#[tokio::test]
#[ignore]
async fn live_explain_pumpfun() {
    use crate::svm::{
        events::PumpFunTrade,
        programs::{registry, to_base58, Venue, EVENT_CPI_PREFIX},
    };

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };
    let slots: u64 = std::env::var("PUMPFUN_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    let curve = crate::svm::programs::pubkey(Venue::PumpFun.program_b58());
    let wsol = registry().wsol;

    let mut buckets: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut examples: BTreeMap<&'static str, Vec<String>> =
        BTreeMap::new();
    let note =
        |bucket: &'static str,
         line: String,
         buckets: &mut BTreeMap<&'static str, u64>,
         examples: &mut BTreeMap<&'static str, Vec<String>>| {
            *buckets.entry(bucket).or_insert(0) += 1;
            let shown = examples.entry(bucket).or_default();
            if shown.len() < 4 {
                shown.push(line);
            }
        };

    let mut cursor = head - HEAD_MARGIN - slots;
    let end = cursor + slots;
    let mut instructions_seen = 0u64;

    while cursor < end {
        let batch = source.fetch(cursor, end).await.expect("fetch");
        assert!(batch.next_slot > cursor, "no progress at {cursor}");

        for slot in &batch.batches {
            for tx in &slot.transactions {
                let has_curve =
                    tx.instructions.iter().any(|ix| ix.program == curve);
                if !has_curve {
                    continue;
                }
                let outcome = crate::svm::decode::decode_transaction(
                    SOLANA_CHAIN,
                    slot.timestamp,
                    tx,
                );
                let signature = bs58::encode(tx.signature).into_string();

                // The curve instructions of this transaction, excluding the
                // self-CPI event rows (which are not instructions).
                let calls: Vec<&crate::svm::decode::SvmInstruction> = tx
                    .instructions
                    .iter()
                    .filter(|ix| {
                        ix.program == curve
                            && !ix.data.starts_with(&EVENT_CPI_PREFIX)
                    })
                    .collect();

                for call in &calls {
                    instructions_seen += 1;
                    let ordinal =
                        crate::svm::models::pack_ordinal(&call.path)
                            .unwrap_or(0);
                    let row = outcome.swaps.iter().find(|swap| {
                        swap.ordinal == ordinal
                            && swap.protocol == Venue::PumpFun.as_str()
                    });

                    // The event the curve emitted for THIS instruction.
                    let event = tx
                        .instructions
                        .iter()
                        .find(|candidate| {
                            candidate.program == curve
                                && candidate.path.len()
                                    == call.path.len() + 1
                                && candidate.path.starts_with(&call.path)
                                && candidate
                                    .data
                                    .starts_with(&EVENT_CPI_PREFIX)
                        })
                        .and_then(|ix| PumpFunTrade::parse(&ix.data));

                    let Some(event) = event else {
                        // No TradeEvent: `create`, `migrate`, `withdraw`,
                        // `collect_creator_fee` and the other non-trade
                        // instructions all land here, so they are named
                        // rather than counted as failures.
                        let kind = match row {
                            Some(_) => "no_event_but_row",
                            None => "not_a_trade_instruction",
                        };
                        note(
                            kind,
                            format!(
                                "{signature} path {:?} disc {}",
                                call.path,
                                hex::encode(
                                    call.data.get(..8).unwrap_or_default()
                                )
                            ),
                            &mut buckets,
                            &mut examples,
                        );
                        continue;
                    };

                    let Some(row) = row else {
                        // A real trade the decoder produced no row for.
                        // Which curves this transaction touches more than
                        // once is the thing to look at: a lamport delta
                        // cannot be split between two trades.
                        let same_curve = calls
                            .iter()
                            .filter(|other| {
                                tx.instructions
                                    .iter()
                                    .find(|ix| {
                                        ix.program == curve
                                            && ix.path.len()
                                                == other.path.len() + 1
                                            && ix
                                                .path
                                                .starts_with(&other.path)
                                            && ix.data.starts_with(
                                                &EVENT_CPI_PREFIX,
                                            )
                                    })
                                    .and_then(|ix| {
                                        PumpFunTrade::parse(&ix.data)
                                    })
                                    .is_some_and(|other_event| {
                                        other_event.mint == event.mint
                                    })
                            })
                            .count();
                        // The exact flows the movement layer saw inside
                        // this subtree, which is what says whether a fee
                        // leg, a refund or a second mint broke the rule.
                        let all = crate::svm::decode::Movements::new(
                            tx,
                            registry(),
                        );
                        let mut flows = String::new();
                        for movement in all.movements.iter().filter(|m| {
                            m.path.len() > call.path.len()
                                && m.path.starts_with(&call.path)
                        }) {
                            flows.push_str(&format!(
                                "\n        {} {} {} -> {}",
                                &to_base58(&movement.mint)[..8],
                                movement.amount,
                                &to_base58(
                                    &movement
                                        .source_owner
                                        .unwrap_or_default()
                                )[..8],
                                &to_base58(
                                    &movement
                                        .destination_owner
                                        .unwrap_or_default()
                                )[..8],
                            ));
                        }
                        note(
                            if same_curve > 1 {
                                "dropped_same_curve_twice"
                            } else {
                                "dropped_other"
                            },
                            format!(
                                "{signature} path {:?} mint {} buy {} \
                                 sol {} token {} curve_calls {same_curve} \
                                 unclassified {} ambiguous {} \
                                 liquidity {}{flows}",
                                call.path,
                                to_base58(&event.mint),
                                event.is_buy,
                                event.sol_amount,
                                event.token_amount,
                                outcome.diagnostics.unclassified,
                                outcome.diagnostics.ambiguous_native,
                                outcome.diagnostics.liquidity,
                            ),
                            &mut buckets,
                            &mut examples,
                        );
                        continue;
                    };

                    if row.confidence == "decoded" {
                        note(
                            "agreed",
                            signature.clone(),
                            &mut buckets,
                            &mut examples,
                        );
                        continue;
                    }

                    // A row that stayed `movement`: run the three checks of
                    // `events::enrich_pumpfun` by hand and say which failed.
                    let (expected_in, expected_out) = if event.is_buy {
                        (wsol, event.mint)
                    } else {
                        (event.mint, wsol)
                    };
                    let mints_ok = row.token_in == expected_in
                        && row.token_out == expected_out;
                    let movement_sol: u128 = if event.is_buy {
                        row.amount_in.to::<u128>()
                    } else {
                        row.amount_out_gross.to::<u128>()
                    };
                    let movement_token: u128 = if event.is_buy {
                        row.amount_out_gross.to::<u128>()
                    } else {
                        row.amount_in.to::<u128>()
                    };
                    let token_ok =
                        movement_token == u128::from(event.token_amount);
                    let sol_gap = movement_sol
                        .abs_diff(u128::from(event.sol_amount));
                    let sol_ok = sol_gap <= u128::from(event.total_fee());

                    let bucket = match (mints_ok, token_ok, sol_ok) {
                        (false, _, _) => "disagreed_mints",
                        (_, false, _) => "disagreed_token_leg",
                        (_, _, false) => "disagreed_sol_leg",
                        _ => "movement_but_all_checks_pass",
                    };
                    note(
                        bucket,
                        format!(
                            "{signature} path {:?} buy {} | movement sol \
                             {movement_sol} token {movement_token} | event \
                             sol {} token {} fee {} creator_fee {} | \
                             sol_gap {sol_gap} (tolerance {}) | in {} out {}",
                            call.path,
                            event.is_buy,
                            event.sol_amount,
                            event.token_amount,
                            event.fee,
                            event.creator_fee,
                            event.total_fee(),
                            to_base58(&row.token_in),
                            to_base58(&row.token_out),
                        ),
                        &mut buckets,
                        &mut examples,
                    );
                }
            }
        }
        cursor = batch.next_slot;
    }

    println!(
        "\n=== pump.fun curve: {instructions_seen} instructions over \
         {slots} slots ==="
    );
    let total: u64 = buckets.values().sum();
    for (bucket, count) in &buckets {
        println!(
            "  {bucket:<28} {count:>6}  ({:>5.2}%)",
            100.0 * *count as f64 / total.max(1) as f64
        );
    }
    for (bucket, lines) in &examples {
        if *bucket == "agreed" {
            continue;
        }
        println!("\n  --- {bucket} ---");
        for line in lines {
            println!("    {line}");
        }
    }
}

/// Why a venue's event contradicted the movement layer, in its own numbers.
///
/// A disagreement rate is a symptom; this prints the diagnosis. It re-reads
/// the event of every swap that stayed `movement` and puts the venue's
/// figures next to the ones the SPL transfers prove, which is how the
/// Raydium CPMM creator-fee case was found.
#[tokio::test]
#[ignore]
async fn live_explain_disagreements() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };
    let venue_filter = std::env::var("EXPLAIN_VENUE")
        .unwrap_or_else(|_| "raydium_cpmm".to_owned());

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    let from = head - HEAD_MARGIN - 60;
    let batch = source.fetch(from, from + 60).await.expect("fetch");
    let rows = svm::decode(SOLANA_CHAIN, &batch.batches);

    // (slot, tx_index) -> the transaction, so a row can be taken back to
    // the bytes it came from.
    let mut by_key = BTreeMap::new();
    for slot in &batch.batches {
        for tx in &slot.transactions {
            by_key.insert((slot.slot, tx.tx_index), tx);
        }
    }

    println!("\n=== {venue_filter}: rows the event did not confirm ===");
    let mut shown = 0;
    for swap in &rows.swaps {
        if swap.protocol != venue_filter || swap.confidence == "decoded" {
            continue;
        }
        let Some(tx) = by_key.get(&(swap.block_number, swap.tx_index))
        else {
            continue;
        };
        let path = crate::svm::models::unpack_ordinal(swap.ordinal);
        let Some(instruction) =
            tx.instructions.iter().find(|ix| ix.path == path)
        else {
            continue;
        };

        // A self-CPI venue keeps its event in an INSTRUCTION, not a log.
        if venue_filter == "raydium_launchlab"
            || venue_filter == "meteora_dbc"
        {
            let cpi = tx.instructions.iter().find(|candidate| {
                candidate.program == instruction.program
                    && candidate.path.len() == path.len() + 1
                    && candidate.path.starts_with(&path)
                    && candidate.data.starts_with(
                        &crate::svm::programs::EVENT_CPI_PREFIX,
                    )
            });
            println!(
                "\n  {} ordinal {:?}\n    movement : in {:>20} out(gross) \
                 {:>20} pool {}",
                bs58::encode(&swap.tx_id).into_string(),
                path,
                swap.amount_in,
                swap.amount_out_gross,
                to_base58(&swap.pool_id),
            );
            match cpi.map(|ix| (ix.data.len(), ix)) {
                Some((len, ix)) => {
                    match crate::svm::venues::LaunchlabTrade::parse(
                        &ix.data,
                    ) {
                        Some(event) => println!(
                            "    event({len}) : in {:>20} out {:>20}\n    \
                             fees     : protocol {} platform {} creator {} \
                             share {}\n    reserves : base {} -> {} quote \
                             {} -> {}\n    flags    : direction {} status \
                             {} exact_in {} pool {}",
                            event.amount_in,
                            event.amount_out,
                            event.protocol_fee,
                            event.platform_fee,
                            event.creator_fee,
                            event.share_fee,
                            event.real_base_before,
                            event.real_base_after,
                            event.real_quote_before,
                            event.real_quote_after,
                            event.trade_direction,
                            event.pool_status,
                            event.exact_in,
                            to_base58(&event.pool_state),
                        ),
                        None => println!(
                            "    event({len}) : did not parse at this \
                             length (disc {})",
                            hex::encode(
                                ix.data.get(8..16).unwrap_or_default()
                            )
                        ),
                    }
                }
                None => println!("    event    : no self-CPI child"),
            }
            shown += 1;
            if shown >= 12 {
                break;
            }
            continue;
        }

        let event = tx
            .logs
            .iter()
            .filter(|log| {
                log.is_data
                    && log.path == path
                    && log.program == instruction.program
            })
            .find_map(|log| {
                let bytes = log.event_bytes()?;
                crate::svm::venues::RaydiumCpmmSwap::parse(&bytes)
            });

        println!(
            "\n  {} ordinal {:?}",
            bs58::encode(&swap.tx_id).into_string(),
            path
        );
        println!(
            "    movement : in {:>20}  out(gross) {:>20}",
            swap.amount_in, swap.amount_out_gross
        );
        match event {
            Some(event) => println!(
                "    event    : in {:>20}  out         {:>20}\n    \
                 fees     : trade {} creator {} in_xfer {} out_xfer {}\n    \
                 delta    : in {} out {}",
                event.input_amount,
                event.output_amount,
                event.trade_fee,
                event.creator_fee,
                event.input_transfer_fee,
                event.output_transfer_fee,
                i128::from(event.input_amount)
                    - swap.amount_in.to_string().parse::<i128>().unwrap(),
                i128::from(event.output_amount)
                    - swap
                        .amount_out_gross
                        .to_string()
                        .parse::<i128>()
                        .unwrap(),
            ),
            None => println!("    event    : NOT FOUND or wrong length"),
        }

        shown += 1;
        if shown >= 12 {
            break;
        }
    }
    println!("\n  {shown} shown");
}

/// (i) What the stream contains and how much of it the decoders cover.
#[tokio::test]
#[ignore]
async fn live_coverage_per_venue() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let (rows, from, to, cost) = stream_and_decode(&token).await;

    let mut per_venue: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for swap in &rows.swaps {
        let entry =
            per_venue.entry(swap.protocol.clone()).or_insert((0, 0));
        entry.0 += 1;
        if swap.confidence == "decoded" {
            entry.1 += 1;
        }
    }

    let total = rows.swaps.len() as u64;
    let decoded: u64 = per_venue.values().map(|(_, d)| *d).sum();

    println!("\n=== Solana live proof: slots [{from}, {to}) ===");
    println!("slots returned          {}", rows.slots.len());
    println!("matched transactions    {}", rows.transactions.len());
    println!("swaps decoded           {total}");
    println!("mints with decimals     {}", rows.tokens.len());
    report_cost(&cost);

    println!(
        "\nper venue: swaps, share, how many the venue's own event \
         CONFIRMED, and the AGREEMENT RATE between the two layers over the \
         swaps where an event was found at all"
    );
    for venue in Venue::ALL {
        let swaps = rows.diagnostics.swaps_by_venue[venue.index()];
        if swaps == 0 {
            continue;
        }
        let confirmed = rows.diagnostics.confirmed_by_venue[venue.index()];
        let disagreed = rows.diagnostics.disagreed_by_venue[venue.index()];
        let agreement = rows
            .diagnostics
            .agreement_rate(venue)
            .map(|rate| format!("{:.2}%", 100.0 * rate))
            .unwrap_or_else(|| "n/a".to_owned());
        println!(
            "  {:<16} {swaps:>6} ({:>5.1}%)  confirmed {confirmed:>6} \
             ({:>5.1}%)  disagreed {disagreed:>4}  agreement {agreement:>7} \
             [{}]",
            venue.as_str(),
            100.0 * swaps as f64 / total.max(1) as f64,
            100.0 * confirmed as f64 / swaps as f64,
            match venue.event_source() {
                crate::svm::programs::EventSource::SelfCpi => "self-CPI",
                crate::svm::programs::EventSource::Log => "log line",
                crate::svm::programs::EventSource::None => "no event",
            }
        );
    }
    println!(
        "\ndiscriminator vs movement disagreements: {} (the venue's own \
         instruction name said one thing and the token flow another)",
        rows.diagnostics.kind_disagreed
    );

    // Every venue with a decoder must actually be decoding something. A
    // venue that streams swaps but confirms none of them is a decoder that
    // silently does nothing - which is exactly what a wrong event offset
    // or an unselected log table looks like.
    for venue in VENUES {
        let swaps = rows.diagnostics.swaps_by_venue[venue.index()];
        if swaps < 20 || !venue.has_decoder() {
            continue;
        }
        let confirmed = rows.diagnostics.confirmed_by_venue[venue.index()];
        assert!(
            confirmed > 0,
            "{} produced {swaps} swaps and its decoder confirmed NONE of \
             them",
            venue.as_str()
        );
        if let Some(rate) = rows.diagnostics.agreement_rate(venue) {
            assert!(
                rate > 0.95,
                "{} agreement between the two layers is only {:.1}%",
                venue.as_str(),
                100.0 * rate
            );
        }
    }
    let _ = &per_venue;
    println!(
        "\ngeneric movement layer covered 100% of the {total} rows \
         (it is what creates them); the per-program layer confirmed \
         {decoded} ({:.1}%)",
        100.0 * decoded as f64 / total.max(1) as f64
    );
    println!("diagnostics {:?}", rows.diagnostics);

    assert!(total > 0, "no swaps decoded in {SLOTS} slots");
    assert!(
        rows.slots.len() as u64 >= SLOTS / 2,
        "far fewer slots than requested came back"
    );
    // Every venue phase 1 streams has a decoder, so a row whose event
    // decoded should agree with the movement layer. A disagreement is a bug
    // report rather than a row, and the RATE is what this watches: a small
    // tail of exotic instruction variants is tolerable, a jump is not.
    let disagreement_rate =
        rows.diagnostics.decoder_disagreed as f64 / total.max(1) as f64;
    println!(
        "decoder disagreements   {} ({:.3}%)",
        rows.diagnostics.decoder_disagreed,
        100.0 * disagreement_rate
    );
    assert!(
        disagreement_rate < 0.005,
        "the per-program decoders contradicted the movement layer on \
         {:.2}% of live swaps",
        100.0 * disagreement_rate
    );
    // Sanity: the two layers agree often enough that the event offsets are
    // clearly right and not accidentally matching one transaction.
    assert!(
        decoded * 2 > total,
        "only {decoded} of {total} rows were confirmed by their event"
    );
}

// --- the independent cross-check -----------------------------------------

async fn rpc_transaction(
    client: &reqwest::Client,
    signature: &str,
) -> Option<Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [signature, {
            "encoding": "jsonParsed",
            // Version 1 transactions exist now and a lower value is
            // rejected outright on a meaningful share of traffic.
            "maxSupportedTransactionVersion": 1
        }]
    });
    for attempt in 0..4 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(800 * attempt)).await;
        }
        let Ok(response) = client.post(RPC).json(&body).send().await
        else {
            continue;
        };
        let Ok(value) = response.json::<Value>().await else {
            continue;
        };
        if let Some(result) = value.get("result") {
            if !result.is_null() {
                return Some(result.clone());
            }
        }
    }
    None
}

/// Token balance deltas per (owner, mint), straight from validator metadata.
fn token_deltas(meta: &Value) -> BTreeMap<(String, String), i128> {
    let mut deltas: BTreeMap<(String, String), i128> = BTreeMap::new();
    let read =
        |key: &str,
         sign: i128,
         out: &mut BTreeMap<(String, String), i128>| {
            for entry in meta
                .get(key)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let (Some(owner), Some(mint), Some(amount)) = (
                    entry.get("owner").and_then(|v| v.as_str()),
                    entry.get("mint").and_then(|v| v.as_str()),
                    entry
                        .get("uiTokenAmount")
                        .and_then(|v| v.get("amount"))
                        .and_then(|v| v.as_str())
                        .and_then(|v| v.parse::<i128>().ok()),
                ) else {
                    continue;
                };
                *out.entry((owner.to_owned(), mint.to_owned()))
                    .or_insert(0) += sign * amount;
            }
        };
    read("preTokenBalances", -1, &mut deltas);
    read("postTokenBalances", 1, &mut deltas);
    deltas
}

/// Every SPL `transfer` / `transferChecked` amount in the transaction, as
/// the RPC's own `jsonParsed` decoder read it.
///
/// This is the second independent RPC-side witness, and it exists because
/// per-(owner, mint) balance deltas are NOT enough on their own: they are
/// net over the whole transaction, so a route whose other hop runs on a
/// venue this module does not register nets the taker's two legs together
/// and the input debit stops being exactly `amount_in`. The transfer
/// instructions do not net - each one states its own amount - which is
/// exactly what the movement layer reads, recomputed here by somebody
/// else's parser.
fn transfer_amounts(result: &Value) -> Vec<i128> {
    fn collect(instructions: &Value, out: &mut Vec<i128>) {
        for instruction in instructions.as_array().into_iter().flatten() {
            let Some(parsed) = instruction.get("parsed") else {
                continue;
            };
            let kind = parsed.get("type").and_then(|v| v.as_str());
            if !matches!(kind, Some("transfer") | Some("transferChecked"))
            {
                continue;
            }
            let Some(info) = parsed.get("info") else { continue };
            // `transfer` carries `amount`, `transferChecked` carries
            // `tokenAmount.amount`; both are decimal strings.
            let amount = info
                .get("amount")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    info.get("tokenAmount")?.get("amount")?.as_str()
                })
                .and_then(|v| v.parse::<i128>().ok());
            if let Some(amount) = amount {
                out.push(amount);
            }
        }
    }

    let mut out = Vec::new();
    collect(&result["transaction"]["message"]["instructions"], &mut out);
    for group in result["meta"]["innerInstructions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        collect(&group["instructions"], &mut out);
    }
    out
}

/// Native lamport delta of `account`, from the RPC's own balance arrays.
fn lamport_delta(result: &Value, account: &str) -> Option<i128> {
    let keys = result
        .get("transaction")?
        .get("message")?
        .get("accountKeys")?
        .as_array()?;
    let index = keys.iter().position(|key| {
        key.get("pubkey").and_then(|v| v.as_str()) == Some(account)
            || key.as_str() == Some(account)
    })?;
    let meta = result.get("meta")?;
    let pre = meta.get("preBalances")?.as_array()?.get(index)?.as_i64()?;
    let post =
        meta.get("postBalances")?.as_array()?.get(index)?.as_i64()?;
    Some(i128::from(post) - i128::from(pre))
}

/// (ii) Decoded swaps must match the public RPC EXACTLY, on every venue.
///
/// This is the check that cannot be fooled by a decoder bug, because
/// nothing in it comes from HyperSync: the amounts are recomputed from the
/// RPC's own `meta.preTokenBalances` / `postTokenBalances`, which are
/// validator metadata, served by a different server over a different wire
/// format and a different code path all the way down.
///
/// Only transactions that decoded to exactly ONE swap are sampled, because
/// a per-(owner, mint) delta over the whole transaction equals the swap's
/// legs only when there is a single swap - which is the very netting
/// problem this module exists to avoid, so it is enforced rather than
/// assumed.
///
/// # What is asserted, and why these particular equalities
///
/// Both amount checks are on the SENDING side of a transfer, and that is
/// deliberate: a Token-2022 transfer fee comes out of what the RECEIVER is
/// credited, never out of what the sender is debited, so a sender-side
/// delta is exact on every mint and needs no tolerance. It is the same
/// asymmetry that put Raydium CPMM's agreement rate at 52.8% until it was
/// understood.
///
/// 1. some owner was debited EXACTLY `amount_in` of `token_in` - the taker,
///    or whatever account the router paid from;
/// 2. some owner was debited EXACTLY `amount_out_gross` of `token_out` AND
///    was credited `token_in` - that conjunction is what makes it the pool
///    rather than any other account in the transaction;
/// 3. `trader` is the fee payer, which the RPC puts first in `accountKeys`.
#[tokio::test]
#[ignore]
async fn live_swaps_match_the_public_rpc() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let (rows, _, _, _cost) = stream_and_decode(&token).await;

    // Transactions holding exactly one swap.
    let mut once: BTreeMap<(u64, u32), usize> = BTreeMap::new();
    for swap in &rows.swaps {
        *once.entry((swap.block_number, swap.tx_index)).or_insert(0) += 1;
    }

    /// Swaps to cross-check per venue.
    const TARGET: usize = 15;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let mut checked: BTreeMap<String, usize> = BTreeMap::new();
    let mut unavailable = 0usize;
    let wsol = to_base58(&crate::svm::programs::registry().wsol);

    for swap in &rows.swaps {
        if once[&(swap.block_number, swap.tx_index)] != 1 {
            continue;
        }
        let done = checked.entry(swap.protocol.clone()).or_insert(0);
        if *done >= TARGET {
            continue;
        }

        let signature = bs58::encode(&swap.tx_id).into_string();
        let Some(result) = rpc_transaction(&client, &signature).await
        else {
            unavailable += 1;
            continue;
        };
        // Be polite to a free endpoint.
        tokio::time::sleep(Duration::from_millis(120)).await;

        let meta = result.get("meta").expect("meta");
        let deltas = token_deltas(meta);
        let token_in = to_base58(&swap.token_in);
        let token_out = to_base58(&swap.token_out);
        let pool = to_base58(&swap.pool_id);

        // (3) the trader is the fee payer.
        let fee_payer = result["transaction"]["message"]["accountKeys"][0]
            ["pubkey"]
            .as_str()
            .expect("fee payer");
        assert_eq!(
            fee_payer,
            to_base58(&swap.trader),
            "{signature}: trader must be the fee payer"
        );
        assert_ne!(token_in, token_out, "{signature}: one mint, not two");

        let amount_in: i128 =
            swap.amount_in.to_string().parse().expect("fits i128");
        let amount_out: i128 =
            swap.amount_out_gross.to_string().parse().expect("fits i128");

        // A bonding curve's SOL leg is a bare lamport change with no token
        // account at all, so it is checked against the native arrays.
        let native_in = token_in == wsol && swap.protocol == "pump_fun";
        let native_out = token_out == wsol && swap.protocol == "pump_fun";

        // (1) a real SPL transfer of exactly amount_in happened, as the
        //     RPC's own jsonParsed decoder reads it.
        if native_in {
            let delta =
                lamport_delta(&result, &pool).expect("curve lamports");
            assert_eq!(
                delta, amount_in,
                "{signature}: the curve's lamport gain must equal amount_in"
            );
        } else {
            let transfers = transfer_amounts(&result);
            assert!(
                transfers.contains(&amount_in),
                "{signature} ({}): the RPC shows no transfer of exactly \
                 {amount_in} {token_in}; it saw {transfers:?}",
                swap.protocol
            );
        }

        // (2) the pool sent exactly amount_out_gross of token_out and was
        //     credited token_in.
        if native_out {
            let delta =
                lamport_delta(&result, &pool).expect("curve lamports");
            assert_eq!(
                delta, -amount_out,
                "{signature}: the curve's lamport loss must equal \
                 amount_out_gross"
            );
        } else {
            // The pool: it was debited EXACTLY `amount_out_gross` of the
            // output mint (a transfer fee comes out of the receiver's
            // credit, never the sender's debit, so this is exact on every
            // mint) and was credited the input mint. The conjunction is
            // what identifies it as the pool rather than any other account
            // in the transaction.
            let senders: Vec<String> = deltas
                .iter()
                .filter(|((_, mint), delta)| {
                    mint == &token_out && **delta == -amount_out
                })
                .map(|((owner, _), _)| owner.clone())
                .collect();
            assert!(
                !senders.is_empty(),
                "{signature} ({}): no account sent exactly {amount_out} of \
                 {token_out}",
                swap.protocol
            );
            // On a bonding curve BUY the input leg is native lamports, so
            // the curve has no token credit of the input mint to point at.
            // There the pool is identified by name instead - and its
            // lamport gain was already checked to the unit just above.
            assert!(
                senders.iter().any(|owner| {
                    if native_in {
                        *owner == pool
                    } else {
                        deltas
                            .get(&(owner.clone(), token_in.clone()))
                            .is_some_and(|delta| *delta > 0)
                    }
                }),
                "{signature} ({}): the account that sent {token_out} was \
                 credited no {token_in}, so it is not the pool",
                swap.protocol
            );
        }

        // Both mints are PROVEN by real movement, never merely claimed.
        assert_eq!(swap.verified_in, swap.token_in);
        assert_eq!(swap.verified_out, swap.token_out);

        *done += 1;
        if checked.len() >= VENUES.len()
            && checked.values().all(|done| *done >= TARGET)
        {
            break;
        }
    }

    println!("\n=== cross-check against {RPC} ===");
    let mut total = 0;
    for (venue, count) in &checked {
        println!("  {venue:<16} {count:>3} swaps matched exactly");
        total += count;
    }
    println!("  total {total}, {unavailable} the RPC could not serve");

    // Every venue that produced enough swaps must have been cross-checked.
    // A venue that decodes but cannot be confirmed against an independent
    // source is not proven.
    for venue in VENUES {
        let available = rows.diagnostics.swaps_by_venue[venue.index()];
        if available < TARGET as u64 {
            println!(
                "  note: {} produced only {available} swaps in this window",
                venue.as_str()
            );
            continue;
        }
        let done = checked.get(venue.as_str()).copied().unwrap_or(0);
        assert!(
            done >= TARGET,
            "wanted {TARGET} {} swaps cross-checked against the public \
             RPC, got {done}",
            venue.as_str()
        );
    }
}

/// Every account the RPC lists for a transaction, as base58.
fn account_keys(result: &Value) -> Vec<String> {
    result["transaction"]["message"]["accountKeys"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|key| {
            key.get("pubkey")
                .and_then(|v| v.as_str())
                .or_else(|| key.as_str())
                .map(str::to_owned)
        })
        .collect()
}

/// The raw instruction data of every instruction the RPC did NOT parse,
/// decoded from its base58.
///
/// This is the second independent witness for a value that is an
/// instruction ARGUMENT rather than an account - pump.fun's `create` takes
/// its `creator` that way, so the wallet credited with a launch need never
/// sign or even appear in the account list. The bytes here are the RPC's,
/// not HyperSync's, so finding a pubkey in them proves the two servers
/// agree about what the transaction contained.
fn raw_instruction_data(result: &Value) -> Vec<Vec<u8>> {
    fn collect(instructions: &Value, out: &mut Vec<Vec<u8>>) {
        for instruction in instructions.as_array().into_iter().flatten() {
            let Some(data) =
                instruction.get("data").and_then(|v| v.as_str())
            else {
                continue;
            };
            if let Ok(bytes) = bs58::decode(data).into_vec() {
                out.push(bytes);
            }
        }
    }

    let mut out = Vec::new();
    collect(&result["transaction"]["message"]["instructions"], &mut out);
    for group in result["meta"]["innerInstructions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        collect(&group["instructions"], &mut out);
    }
    out
}

/// Every mint the RPC's own token-balance metadata names.
fn mints_of(meta: &Value) -> std::collections::HashSet<String> {
    ["preTokenBalances", "postTokenBalances"]
        .into_iter()
        .flat_map(|key| {
            meta.get(key).and_then(|v| v.as_array()).into_iter().flatten()
        })
        .filter_map(|entry| {
            entry.get("mint").and_then(|v| v.as_str()).map(str::to_owned)
        })
        .collect()
}

/// (iii) LAUNCHES and CURVE TRADES must match the public RPC exactly.
///
/// The counterpart of `live_swaps_match_the_public_rpc`, for the launchpad
/// tables. Nothing here comes from HyperSync: every value is recomputed
/// from the RPC's own `getTransaction` - its account list, its
/// `jsonParsed` transfer instructions and its balance metadata - which is
/// a different server, a different wire format and a different parser.
///
/// # What is asserted, and why these particular equalities
///
/// A launch:
/// 1. the mint the launch names is a real account of that transaction AND
///    the RPC's own token metadata names it, so it is a mint and not any
///    other account;
/// 2. the CURVE is an account of the transaction, and re-derives as the
///    program's PDA of that mint - the forgery-proof part;
/// 3. the creator is an account of the transaction;
/// 4. `tx_from` is the fee payer, which the RPC puts first.
///
/// A curve trade:
/// 5. the traded token is one the RPC's balance metadata names;
/// 6. a real SPL transfer of EXACTLY `token_amount` happened, as the RPC's
///    own parser reads it. This is a SENDER-side equality on purpose: a
///    Token-2022 transfer fee comes out of what the receiver is credited,
///    never out of what the sender is debited, so it needs no tolerance;
/// 7. on a SOL curve the quote leg is the curve's own lamport delta, to
///    the unit - the leg that has no instruction at all.
#[tokio::test]
#[ignore]
async fn live_launchpads_match_the_public_rpc() {
    use crate::svm::launchpads::SolFamily;

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };
    /// Launches and curve trades to cross-check.
    const LAUNCHES: usize = 15;
    const TRADES: usize = 30;

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    let mut cursor = head - HEAD_MARGIN - 250;
    let end = cursor + 250;

    let mut rows = svm::SvmRows::default();
    while cursor < end {
        let batch = source.fetch(cursor, end).await.expect("fetch");
        assert!(batch.next_slot > cursor, "no progress at {cursor}");
        let mut decoded = svm::decode(SOLANA_CHAIN, &batch.batches);
        rows.append(&mut decoded);
        cursor = batch.next_slot;
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let wsol = to_base58(&crate::svm::programs::registry().wsol);

    // --- launches ---------------------------------------------------
    let mut checked_launches = 0usize;
    let mut creators_in_data = 0usize;
    let mut per_family: BTreeMap<String, usize> = BTreeMap::new();
    let mut unavailable = 0usize;

    for launch in &rows.launchpads.tokens {
        if checked_launches >= LAUNCHES {
            break;
        }
        let signature = bs58::encode(&launch.tx_id).into_string();
        let Some(result) = rpc_transaction(&client, &signature).await
        else {
            unavailable += 1;
            continue;
        };
        tokio::time::sleep(Duration::from_millis(120)).await;

        let keys = account_keys(&result);
        let meta = result.get("meta").expect("meta");
        let mints = mints_of(meta);
        let token = to_base58(&launch.token);
        let curve = to_base58(&launch.curve);

        // (4) the fee payer.
        assert_eq!(
            keys.first().map(String::as_str),
            Some(to_base58(&launch.tx_from).as_str()),
            "{signature}: tx_from must be the fee payer"
        );
        // (1) the mint is a real account, and really is a mint.
        assert!(
            keys.contains(&token),
            "{signature}: the launched mint {token} is not an account of \
             its own launch transaction"
        );
        assert!(
            mints.contains(&token),
            "{signature}: the RPC's token metadata does not know {token} \
             as a mint"
        );
        // (2) the curve is a real account, and is the program's PDA.
        assert!(
            keys.contains(&curve),
            "{signature}: the curve {curve} is not an account of the launch"
        );
        let seeds: &[&[u8]] = match launch.family.as_str() {
            "pumpfun" => &[b"bonding-curve", &launch.token],
            "raydium_launchlab" => {
                &[b"pool", &launch.token, &launch.quote_token]
            }
            // Meteora DBC sorts its two mints and mixes in the config, so
            // the quote mint - which lives on the config account this
            // pipeline does not read - is not available here.
            _ => &[],
        };
        if !seeds.is_empty() {
            let (derived, _) = crate::svm::pda::find_program_address(
                seeds,
                &launch.emitter,
            )
            .expect("a bump exists");
            assert_eq!(
                to_base58(&derived),
                curve,
                "{signature}: the curve is not the program's PDA of the \
                 launch's own fields"
            );
        }
        // (3) the creator. It is an instruction ARGUMENT, not an account -
        //     pump.fun's `create` takes `creator: Pubkey` - so a launch can
        //     credit a wallet that never signs and never appears in the
        //     account list. The RPC's own raw instruction bytes are
        //     therefore the witness, and finding the 32 bytes there proves
        //     the two servers saw the same transaction.
        if launch.creator != crate::svm::models::ZERO_PUBKEY {
            let named = keys.contains(&to_base58(&launch.creator))
                || raw_instruction_data(&result).iter().any(|data| {
                    data.windows(32).any(|window| window == launch.creator)
                });
            assert!(
                named,
                "{signature}: the creator {} is in neither the account \
                 list nor the RPC's own instruction bytes",
                to_base58(&launch.creator)
            );
            creators_in_data += 1;
        }

        *per_family.entry(launch.family.clone()).or_insert(0) += 1;
        checked_launches += 1;
    }

    // --- curve trades -------------------------------------------------
    // One trade per transaction, so a per-transaction transfer list can be
    // matched against one row without ambiguity.
    let mut once: BTreeMap<(u64, u32), usize> = BTreeMap::new();
    for trade in &rows.launchpads.trades {
        *once.entry((trade.block_number, trade.tx_index)).or_insert(0) +=
            1;
    }

    let mut checked_trades = 0usize;
    let mut native_legs = 0usize;
    let mut trades_per_family: BTreeMap<String, usize> = BTreeMap::new();

    for trade in &rows.launchpads.trades {
        if checked_trades >= TRADES {
            break;
        }
        if once[&(trade.block_number, trade.tx_index)] != 1 {
            continue;
        }
        if trade.token_verified != 1 {
            continue;
        }
        let signature = bs58::encode(&trade.tx_id).into_string();
        let Some(result) = rpc_transaction(&client, &signature).await
        else {
            unavailable += 1;
            continue;
        };
        tokio::time::sleep(Duration::from_millis(120)).await;

        let meta = result.get("meta").expect("meta");
        let mints = mints_of(meta);
        let token = to_base58(&trade.token);

        // (5) the RPC knows the mint.
        assert!(
            mints.contains(&token),
            "{signature}: the RPC's metadata does not name {token}"
        );

        // (6) a transfer of exactly this many token units happened.
        let amount: i128 =
            trade.token_amount.to_string().parse().expect("fits i128");
        let transfers = transfer_amounts(&result);
        assert!(
            transfers.contains(&amount),
            "{signature} ({}): the RPC shows no transfer of exactly \
             {amount} {token}; it saw {transfers:?}",
            trade.family
        );

        // (7) a SOL curve's quote leg is the curve's own lamport delta.
        let quote = to_base58(&trade.quote_token);
        if quote == wsol && trade.family == SolFamily::PumpFun.as_str() {
            let curve = to_base58(&trade.emitter);
            if let Some(delta) = lamport_delta(&result, &curve) {
                let expected: i128 = trade
                    .quote_amount
                    .to_string()
                    .parse()
                    .expect("fits i128");
                let signed =
                    if trade.side == "buy" { expected } else { -expected };
                assert_eq!(
                    delta, signed,
                    "{signature}: the curve's lamport delta is not the \
                     quote leg the event reported"
                );
                native_legs += 1;
            }
        }

        *trades_per_family.entry(trade.family.clone()).or_insert(0) += 1;
        checked_trades += 1;
    }

    println!("\n=== launchpad cross-check against {RPC} ===");
    println!(
        "launches matched exactly: {checked_launches} ({creators_in_data} \
         with their creator confirmed by the RPC's own bytes)"
    );
    for (family, count) in &per_family {
        println!("  {family:<18} {count}");
    }
    println!("curve trades matched exactly: {checked_trades}");
    for (family, count) in &trades_per_family {
        println!("  {family:<18} {count}");
    }
    println!(
        "  of those, {native_legs} had their SOL leg checked against the \
         curve's own lamport delta"
    );
    println!("  {unavailable} the RPC could not serve");

    assert!(
        checked_launches >= LAUNCHES,
        "wanted {LAUNCHES} launches cross-checked, got {checked_launches}"
    );
    assert!(
        checked_trades >= TRADES,
        "wanted {TRADES} curve trades cross-checked, got {checked_trades}"
    );
}

/// The three tricky transactions of the Solana venue research are recorded
/// fixtures and are asserted in `svm::tests`; this checks the RECORDING is
/// still faithful to what the chain says, so a stale fixture cannot quietly
/// keep passing.
#[tokio::test]
#[ignore]
async fn the_recorded_fixtures_still_match_the_chain() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    for name in [
        "two_opposite_swaps",
        "jupiter_three_hop",
        "bisonfi_quote_update",
        "pumpswap_buy",
        "pumpfun_sell",
    ] {
        let fixture = crate::svm::fixtures::get(name);
        let signature =
            bs58::encode(fixture.transaction.signature).into_string();
        let Some(result) = rpc_transaction(&client, &signature).await
        else {
            eprintln!("  RPC had no answer for {name}; skipping");
            continue;
        };
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(
            result["slot"].as_u64(),
            Some(fixture.slot),
            "{name}: recorded slot drifted"
        );
        assert_eq!(
            result["blockTime"].as_i64(),
            Some(fixture.block_time),
            "{name}: recorded block time drifted"
        );
        let fee_payer = result["transaction"]["message"]["accountKeys"][0]
            ["pubkey"]
            .as_str()
            .expect("fee payer");
        assert_eq!(
            fee_payer,
            to_base58(&fixture.transaction.fee_payer),
            "{name}: recorded fee payer drifted"
        );
        println!("  {name}: still matches the chain");
    }
}

/// A sanity check on the source itself, and the only place the rate limit
/// surface is observed.
#[tokio::test]
#[ignore]
async fn live_head_and_history_bounds() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };
    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");
    assert!(head > 400_000_000, "head looks wrong: {head}");

    // Headers carry the parent chain, which is what continuity means here.
    let headers = source
        .headers(head - HEAD_MARGIN - 20, head - HEAD_MARGIN)
        .await
        .expect("headers");
    assert!(!headers.is_empty());
    println!(
        "\nhead {head}, {} headers over a 20 slot window",
        headers.len()
    );

    // Skipped slots are NORMAL: assert the parent chain links up rather
    // than that every integer is present.
    let by_slot: std::collections::HashMap<u64, Pubkey> =
        headers.iter().map(|h| (h.slot, h.blockhash)).collect();
    let mut linked = 0;
    for header in &headers {
        if let Some(parent) = by_slot.get(&header.parent_slot) {
            assert_eq!(
                *parent, header.parent_blockhash,
                "the parent chain is broken at slot {}",
                header.slot
            );
            linked += 1;
        }
    }
    assert!(linked > 0, "no header linked to its parent");
    let skipped = headers
        .last()
        .zip(headers.first())
        .map(|(last, first)| {
            (last.slot - first.slot + 1) as usize - headers.len()
        })
        .unwrap_or(0);
    println!(
        "  {linked} headers linked to their parent, {skipped} slots \
         skipped (normal on Solana)"
    );
}
// Recording the phase 2 fixtures from live mainnet.
//
// `cargo test --release svm::live_tests::record -- --ignored --nocapture`
//
// Writes `src/svm/fixtures/phase2.json` from real transactions, in the same
// shape `fixtures.rs` reads. The rows are the server's own - the only
// transformation is back into JSON from the structs the source decoded them
// into, which is lossless for everything the decoder consumes.
//
// It is an ignored, hand-run tool rather than part of any suite: fixtures
// are recorded once, committed, and then tested against for ever.

use serde_json::json;

/// The four shapes the fixture set has to contain, and why each one is
/// worth a recording.
const WANTED: &[(&str, &str)] = &[
    (
        "multi_hop_route",
        "an aggregator route whose hops land on DIFFERENT phase 2 venues: \
         it must become one swap per venue, each credited to the venue and \
         not to the router",
    ),
    (
        "liquidity_no_swap",
        "an add or remove of liquidity on a phase 2 venue. Both mints move \
         the SAME way across the pool, so it must decode to NO swap at all",
    ),
    (
        "token_2022_fee",
        "a swap on a mint with a Token-2022 transfer fee, where what the \
         pool SENT and what the taker RECEIVED differ - and where the venue \
         event's idea of the input leg differs from the movement layer's by \
         exactly that fee",
    ),
    (
        "clmm_tick_crossing",
        "a concentrated-liquidity swap that moves the pool price across at \
         least one tick boundary, i.e. pre_sqrt_price != post_sqrt_price by \
         more than one tick's worth",
    ),
];

fn hexify(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

fn b58(key: &crate::svm::models::Pubkey) -> String {
    to_base58(key)
}

/// One transaction, in the shape `fixtures::RawFixture` reads.
fn fixture_json(
    name: &str,
    why: &str,
    slot: &crate::svm::SvmSlotBatch,
    tx: &crate::svm::decode::SvmTransaction,
) -> serde_json::Value {
    json!({
        "name": name,
        "why": why,
        "slot": slot.slot,
        "block_time": slot.timestamp as i64,
        "blockhash": b58(&slot.blockhash),
        "parent_slot": slot.parent_slot,
        "parent_blockhash": b58(&slot.parent_blockhash),
        "transaction": {
            "transaction_index": tx.tx_index,
            "transaction_id": bs58::encode(tx.signature).into_string(),
            "fee_payer": b58(&tx.fee_payer),
            "success": tx.success,
            "fee": tx.fee,
            "compute_units_consumed": tx.compute_units,
            "has_dropped_log_messages": tx.dropped_logs,
        },
        "instruction_calls": tx.instructions.iter().map(|ix| json!({
            "instruction_address": ix.path,
            "executing_account": b58(&ix.program),
            "account_arguments": ix.accounts.iter().map(b58)
                .collect::<Vec<_>>(),
            "data": hexify(&ix.data),
        })).collect::<Vec<_>>(),
        "account_activity": tx.activity.iter().map(|row| json!({
            "account": b58(&row.account),
            "mint": row.mint.as_ref().map(b58),
            "pre_owner": row.pre_owner.as_ref().map(b58),
            "post_owner": row.post_owner.as_ref().map(b58),
            "token_decimals": row.decimals,
            "pre_token_balance": row.pre_token_balance
                .map(|v| v.to_string()),
            "post_token_balance": row.post_token_balance
                .map(|v| v.to_string()),
            "pre_balance": row.pre_balance,
            "post_balance": row.post_balance,
            "is_signer": row.is_signer,
            "is_fee_payer": row.is_fee_payer,
            "post_program_id": row.token_program.as_ref().map(b58),
        })).collect::<Vec<_>>(),
        "logs": tx.logs.iter().map(|log| json!({
            "instruction_address": log.path,
            "program_id": b58(&log.program),
            "kind": if log.is_data { "data" } else { "log" },
            "message": log.message,
        })).collect::<Vec<_>>(),
    })
}

/// The launchpad shapes, and why each is worth a recording.
const WANTED_LAUNCHPADS: &[(&str, &str)] = &[
    (
        "pumpfun_create",
        "a pump.fun LAUNCH: three Borsh Strings at the front of CreateEvent, \
         so nothing in it is at a fixed offset, and a bonding curve that \
         must re-derive as the program's own PDA of the mint",
    ),
    (
        "pumpfun_multi_buy",
        "ONE transaction holding SEVERAL pump.fun curve trades - the bundle \
         a sniper sends. Each must stay its own row: a decoder reading \
         transaction level balances would report one netted trade",
    ),
    (
        "pumpfun_graduation",
        "the curve moving into PumpSwap. Its \
         CompletePumpAmmMigrationEvent NAMES the destination pool, which is \
         the join key that makes a token's chart continue after the curve \
         is gone - the whole point of the module",
    ),
    (
        "pumpfun_quote_curve",
        "a pump.fun curve quoted in something other than SOL. sol_amount is \
         0 and the real leg is in the TradeEvent tail, behind a Borsh \
         String and a Vec - the case that cost 1.8-11.5% of curve trades",
    ),
    (
        "dbc_launch",
        "a Meteora DBC launch: EvtInitializePool names the partner CONFIG, \
         which is how bags.fm and the other front ends are attributed \
         without ever becoming venues",
    ),
    (
        "launchlab_launch",
        "a Raydium LaunchLab launch. PoolCreateEvent names NEITHER mint, so \
         they come from account metas and are proved against the pool's own \
         PDA seeds",
    ),
];

/// Records the launchpad fixtures from live mainnet.
///
/// `cargo test --release svm::live_tests::record_launchpad -- --ignored
/// --nocapture`
#[tokio::test]
#[ignore]
async fn record_launchpad_fixtures() {
    use crate::svm::{
        launchpads::SolFamily,
        programs::{
            DISC_DBC_INITIALIZE_POOL, DISC_LAUNCHLAB_POOL_CREATE,
            DISC_PUMPFUN_CREATE, DISC_PUMPFUN_MIGRATED,
            DISC_PUMPFUN_TRADE_EVENT, EVENT_CPI_PREFIX,
        },
    };

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");

    let mut found: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    let mut used: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // A graduation is rare - a few hundred a day against millions of
    // trades - so this sweeps several windows rather than one.
    let mut window = 0u64;
    while found.len() < WANTED_LAUNCHPADS.len() && window < 14 {
        let from = head - HEAD_MARGIN - 120 - window * 900;
        let batch = source.fetch(from, from + 120).await.expect("fetch");
        window += 1;

        for slot in &batch.batches {
            for tx in &slot.transactions {
                let signature = bs58::encode(tx.signature).into_string();
                if used.contains(&signature) {
                    continue;
                }
                // Every self-CPI event of a launchpad program, with the
                // family that emitted it.
                let events: Vec<(SolFamily, &[u8])> = tx
                    .instructions
                    .iter()
                    .filter(|ix| ix.data.starts_with(&EVENT_CPI_PREFIX))
                    .filter_map(|ix| {
                        SolFamily::from_program(&ix.program)
                            .map(|family| (family, ix.data.as_slice()))
                    })
                    .collect();
                if events.is_empty() {
                    continue;
                }
                let has = |family: SolFamily, disc: [u8; 8]| {
                    events.iter().any(|(f, data)| {
                        *f == family && data.get(8..16) == Some(&disc[..])
                    })
                };

                let mut take = |name: &'static str, why: &'static str| {
                    if found.contains_key(name)
                        || used.contains(&signature)
                    {
                        return;
                    }
                    found.insert(name, fixture_json(name, why, slot, tx));
                    used.insert(signature.clone());
                };

                if has(SolFamily::PumpFun, DISC_PUMPFUN_MIGRATED) {
                    take("pumpfun_graduation", WANTED_LAUNCHPADS[2].1);
                }
                if has(SolFamily::PumpFun, DISC_PUMPFUN_CREATE) {
                    take("pumpfun_create", WANTED_LAUNCHPADS[0].1);
                }
                if has(SolFamily::MeteoraDbc, DISC_DBC_INITIALIZE_POOL) {
                    take("dbc_launch", WANTED_LAUNCHPADS[4].1);
                }
                if has(
                    SolFamily::RaydiumLaunchlab,
                    DISC_LAUNCHLAB_POOL_CREATE,
                ) {
                    take("launchlab_launch", WANTED_LAUNCHPADS[5].1);
                }

                // A bundle: more than one curve TradeEvent in one
                // transaction.
                let trades = events
                    .iter()
                    .filter(|(family, data)| {
                        *family == SolFamily::PumpFun
                            && data.get(8..16)
                                == Some(&DISC_PUMPFUN_TRADE_EVENT[..])
                    })
                    .count();
                if trades > 1 {
                    take("pumpfun_multi_buy", WANTED_LAUNCHPADS[1].1);
                }

                // A curve quoted in something other than SOL: the event's
                // own tail says so.
                let quote_curve = events.iter().any(|(family, data)| {
                    *family == SolFamily::PumpFun
                        && crate::svm::events::PumpFunTrade::parse(data)
                            .is_some_and(|event| {
                                event.sol_amount == 0
                                    && event.token_amount > 0
                            })
                });
                if quote_curve {
                    take("pumpfun_quote_curve", WANTED_LAUNCHPADS[3].1);
                }
            }
        }
        println!(
            "  window {window}: have {} of {} ({:?})",
            found.len(),
            WANTED_LAUNCHPADS.len(),
            found.keys().collect::<Vec<_>>()
        );
    }

    for (name, why) in WANTED_LAUNCHPADS {
        if !found.contains_key(name) {
            println!("  MISSING {name}: {why}");
        }
    }

    let out: Vec<serde_json::Value> = found.into_values().collect();
    let path = "src/svm/fixtures/launchpads.json";
    std::fs::write(
        path,
        serde_json::to_string_pretty(&out).expect("serialise"),
    )
    .expect("write fixtures");
    println!("\nwrote {} fixtures to {path}", out.len());
}

/// Scans live slots for the four wanted shapes and records the first of
/// each.
#[tokio::test]
#[ignore]
async fn record_phase2_fixtures() {
    use crate::svm::{
        programs::{registry, IxKind, Venue},
        venues::{OrcaTraded, RaydiumClmmSwap, RaydiumCpmmSwap},
    };

    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let source = SolanaSource::new(None, &token).expect("source");
    let head = source.head().await.expect("head");

    let mut found: BTreeMap<&str, serde_json::Value> = BTreeMap::new();
    // One transaction can satisfy two of the shapes at once - a route
    // across two venues that also happens to touch a Token-2022 mint. Each
    // fixture should be a distinct transaction, so the corpus covers more
    // of the chain rather than storing the same bytes twice.
    let mut used: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let registry = registry();

    // A few windows, because the four shapes do not all appear in any one
    // of them - a liquidity add is far rarer than a swap.
    let mut window = 0u64;
    while found.len() < WANTED.len() && window < 6 {
        let from = head - HEAD_MARGIN - 80 - window * 400;
        let batch = source.fetch(from, from + 80).await.expect("fetch");
        window += 1;

        for slot in &batch.batches {
            for tx in &slot.transactions {
                let signature = bs58::encode(tx.signature).into_string();
                if used.contains(&signature) {
                    continue;
                }
                let outcome = crate::svm::decode::decode_transaction(
                    SOLANA_CHAIN,
                    slot.timestamp,
                    tx,
                );

                // (a) a route across two DIFFERENT phase 2 venues.
                //
                // NOT filtered on `route_ordinal`, deliberately. Only
                // Jupiter v6 is in `ROUTERS_B58`, and a large share of
                // Solana routing goes through DFlow, OKX, GMGN and others
                // whose program ids the research records only as prefixes.
                // Requiring a REGISTERED router therefore finds nothing
                // most windows, while the property actually worth pinning
                // is the subtree rule splitting one transaction into one
                // swap per venue - which holds whoever the router is.
                if !found.contains_key("multi_hop_route")
                    && !used.contains(&signature)
                {
                    let mut venues: Vec<&str> = outcome
                        .swaps
                        .iter()
                        .map(|s| s.protocol.as_str())
                        .collect();
                    venues.sort_unstable();
                    venues.dedup();
                    if venues.len() >= 2 {
                        found.insert(
                            "multi_hop_route",
                            fixture_json(
                                "multi_hop_route",
                                WANTED[0].1,
                                slot,
                                tx,
                            ),
                        );
                        used.insert(signature.clone());
                    }
                }

                // (b) a liquidity instruction that produced no swap.
                if !found.contains_key("liquidity_no_swap")
                    && !used.contains(&signature)
                {
                    let liquidity = tx.instructions.iter().any(|ix| {
                        registry
                            .venue(&ix.program)
                            .map(|venue| {
                                venue.instruction_kind(&ix.data)
                                    == IxKind::Liquidity
                            })
                            .unwrap_or(false)
                    });
                    if liquidity && outcome.swaps.is_empty() {
                        found.insert(
                            "liquidity_no_swap",
                            fixture_json(
                                "liquidity_no_swap",
                                WANTED[1].1,
                                slot,
                                tx,
                            ),
                        );
                        used.insert(signature.clone());
                    }
                }

                // (c) a Token-2022 transfer fee, stated by the venue itself.
                if !found.contains_key("token_2022_fee")
                    && !used.contains(&signature)
                {
                    // All three log-event venues report transfer fees, and
                    // the criterion has to ask each of them: the CPMM and
                    // CLMM events share a discriminator and differ only in
                    // length, so asking only one of them reads the other's
                    // bytes at the wrong offsets and "finds" a fee that is
                    // not there.
                    let fee = tx.logs.iter().any(|log| {
                        let Some(bytes) = log.event_bytes() else {
                            return false;
                        };
                        if let Some(event) = RaydiumCpmmSwap::parse(&bytes)
                        {
                            return event.input_transfer_fee > 0
                                || event.output_transfer_fee > 0;
                        }
                        if let Some(event) = RaydiumClmmSwap::parse(&bytes)
                        {
                            return event.transfer_fee_0 > 0
                                || event.transfer_fee_1 > 0;
                        }
                        if let Some(event) = OrcaTraded::parse(&bytes) {
                            return event.input_transfer_fee > 0
                                || event.output_transfer_fee > 0;
                        }
                        false
                    });
                    if fee && !outcome.swaps.is_empty() {
                        found.insert(
                            "token_2022_fee",
                            fixture_json(
                                "token_2022_fee",
                                WANTED[2].1,
                                slot,
                                tx,
                            ),
                        );
                        used.insert(signature.clone());
                    }
                }

                // (d) a CLMM swap that moved the price across a tick.
                //
                // Orca's `Traded` reports the sqrt price BEFORE and AFTER,
                // which is the only event among these venues that states a
                // tick crossing rather than implying one. One tick is a
                // 1.0001x price step, so a sqrt-price ratio above 1.00005
                // means at least one boundary was crossed.
                if !found.contains_key("clmm_tick_crossing")
                    && !used.contains(&signature)
                {
                    let crossed = tx.logs.iter().any(|log| {
                        log.event_bytes()
                            .and_then(|bytes| OrcaTraded::parse(&bytes))
                            .map(|event| {
                                let (low, high) = if event.pre_sqrt_price
                                    < event.post_sqrt_price
                                {
                                    (
                                        event.pre_sqrt_price,
                                        event.post_sqrt_price,
                                    )
                                } else {
                                    (
                                        event.post_sqrt_price,
                                        event.pre_sqrt_price,
                                    )
                                };
                                low > 0
                                    && high.saturating_sub(low) as f64
                                        / low as f64
                                        > 0.00005
                            })
                            .unwrap_or(false)
                    });
                    let orca = outcome.swaps.iter().any(|s| {
                        s.protocol == Venue::OrcaWhirlpool.as_str()
                    });
                    if crossed && orca {
                        found.insert(
                            "clmm_tick_crossing",
                            fixture_json(
                                "clmm_tick_crossing",
                                WANTED[3].1,
                                slot,
                                tx,
                            ),
                        );
                        used.insert(signature.clone());
                    }
                }
            }
        }
        println!(
            "  window {window}: have {} of {}",
            found.len(),
            WANTED.len()
        );
    }

    for (name, why) in WANTED {
        if !found.contains_key(name) {
            println!("  MISSING {name}: {why}");
        }
    }

    let out: Vec<serde_json::Value> = found.into_values().collect();
    let path = "src/svm/fixtures/phase2.json";
    std::fs::write(
        path,
        serde_json::to_string_pretty(&out).expect("serialise"),
    )
    .expect("write fixtures");
    println!("\nwrote {} fixtures to {path}", out.len());
}

// --- review round 4: the two transactions the addendum names --------------

/// The ADDENDUM of review round 4 cites two real mainnet
/// transactions by signature. Recording them by SLOT and signature - rather
/// than scanning for a shape - is what makes those findings reproducible
/// from the same bytes the reviewer read.
const WANTED_ROUND4: &[(&str, u64, &str, &str)] = &[
    (
        "raydium_v4_and_pumpswap",
        448_414_771,
        "3ZZw4CfNzTMgPnnJRhKk28bteiip6zDimArpQSNURYfLn94J4UTPmYfBqTU2SfJ3pbwodZV3rGsUz7Qfyh8MVPJP",
        "findings B3 and M4. Raydium AMM v4's two vaults are owned by the \
         ONE program-wide authority 5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1, \
         so storing that owner as pool_id collapses every Raydium v4 pair \
         into one candle series. The same transaction's PumpSwap sell pays \
         the taker into an account opened and closed inside it, so a \
         decoder that SKIPS an unreadable destination and takes the next \
         one stores a fee recipient's delta as amount_out",
    ),
    (
        "launchlab_sell",
        448_416_320,
        "3J4T72bwybz5h8qjgoj2M1LPyexmKzL4WCYoMZyrck5Wf2nJXL3FV4mcRksTrivR1K52J1bgUkJSt8Y4xh4Yg97c",
        "finding M4, second case, and a real 1% Token-2022 transfer fee. A \
         Raydium LaunchLab sell whose taker receive account is unreadable: \
         amount_out must fall back to what the pool SENT rather than take \
         the next movement's delta. LaunchLab's vault authority is \
         program-wide too, which is finding B3 on a second venue",
    ),
];

/// Records the round 4 fixtures, by slot and signature.
///
/// `cargo test --release svm::live_tests::record_round4 -- --ignored
/// --nocapture`
#[tokio::test]
#[ignore]
async fn record_round4_fixtures() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let source = SolanaSource::new(None, &token).expect("source");
    let mut out: Vec<serde_json::Value> = Vec::new();

    for (name, slot_number, signature, why) in WANTED_ROUND4 {
        let batch = source
            .fetch(*slot_number, slot_number + 1)
            .await
            .expect("fetch");
        let mut hit = false;
        for slot in &batch.batches {
            for tx in &slot.transactions {
                if bs58::encode(tx.signature).into_string() != *signature {
                    continue;
                }
                out.push(fixture_json(name, why, slot, tx));
                hit = true;
            }
        }
        println!(
            "  {name} @ {slot_number}: {}",
            if hit { "found" } else { "MISSING" }
        );
    }

    let path = "src/svm/fixtures/round4.json";
    std::fs::write(
        path,
        serde_json::to_string_pretty(&out).expect("serialise"),
    )
    .expect("write fixtures");
    println!("\nwrote {} fixtures to {path}", out.len());
}
