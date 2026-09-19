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
        programs::{to_base58, Venue},
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

fn token() -> Option<String> {
    std::env::var("ENVIO_API_TOKEN").ok().filter(|t| !t.trim().is_empty())
}

/// Streams `SLOTS` slots below the head and decodes them.
async fn stream_and_decode(token: &str) -> (svm::SvmRows, u64, u64) {
    let source = SolanaSource::new(None, token).expect("build source");
    let head = source.head().await.expect("head");
    let from = head - HEAD_MARGIN - SLOTS;
    let to = from + SLOTS;

    let mut rows = svm::SvmRows::default();
    let mut cursor = from;

    // Follow the server's truncation cursor to the end of the range, the
    // way the pipeline's own loop will have to.
    while cursor < to {
        let batch = source.fetch(cursor, to).await.expect("fetch");
        assert!(
            batch.next_slot > cursor,
            "the server made no progress at {cursor}: a resume loop must \
             treat this as a stop condition, never spin"
        );
        let mut decoded = svm::decode(SOLANA_CHAIN, &batch.batches);
        rows.append(&mut decoded);
        cursor = batch.next_slot;
    }

    (rows, from, to)
}

/// (i) What the stream contains and how much of it the decoders cover.
#[tokio::test]
#[ignore]
async fn live_coverage_per_venue() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let (rows, from, to) = stream_and_decode(&token).await;

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
    println!("\nper venue (swaps, of which cross-checked by an event):");
    for (venue, (count, decoded)) in &per_venue {
        let share = 100.0 * *count as f64 / total.max(1) as f64;
        println!(
            "  {venue:<14} {count:>6}  {share:>5.1}%   event-decoded \
             {decoded:>6} ({:.1}%)",
            100.0 * *decoded as f64 / (*count).max(1) as f64
        );
    }
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

/// (ii) Decoded swaps must match the public RPC EXACTLY.
///
/// Only transactions that decoded to exactly ONE swap are sampled, because
/// a per-(owner, mint) delta over the whole transaction is only equal to the
/// swap's legs when there is a single swap - which is the very netting
/// problem this module exists to avoid, so it is enforced rather than
/// assumed.
#[tokio::test]
#[ignore]
async fn live_swaps_match_the_public_rpc() {
    let Some(token) = token() else {
        eprintln!("ENVIO_API_TOKEN is not set; skipping");
        return;
    };

    let (rows, _, _) = stream_and_decode(&token).await;

    // Transactions holding exactly one swap.
    let mut once: BTreeMap<(u64, u32), usize> = BTreeMap::new();
    for swap in &rows.swaps {
        *once.entry((swap.block_number, swap.tx_index)).or_insert(0) += 1;
    }

    let mut wanted: BTreeMap<Venue, usize> = BTreeMap::new();
    let target = 20;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();

    let mut checked = 0usize;
    let mut per_venue: BTreeMap<String, usize> = BTreeMap::new();

    for swap in &rows.swaps {
        if once[&(swap.block_number, swap.tx_index)] != 1 {
            continue;
        }
        let venue = match swap.protocol.as_str() {
            "pumpswap" => Venue::PumpSwap,
            "pump_fun" => Venue::PumpFun,
            _ => continue,
        };
        let done = wanted.entry(venue).or_insert(0);
        if *done >= target {
            continue;
        }

        let signature = bs58::encode(&swap.tx_id).into_string();
        let Some(result) = rpc_transaction(&client, &signature).await
        else {
            eprintln!("  RPC had no answer for {signature}, skipping");
            continue;
        };
        // Be polite to a free endpoint.
        tokio::time::sleep(Duration::from_millis(250)).await;

        let meta = result.get("meta").expect("meta");
        let deltas = token_deltas(meta);
        let pool = to_base58(&swap.pool_id);
        let token_in = to_base58(&swap.token_in);
        let token_out = to_base58(&swap.token_out);
        let wsol = to_base58(&crate::svm::programs::registry().wsol);

        // The trader is the fee payer, which the RPC puts first.
        let fee_payer = result["transaction"]["message"]["accountKeys"][0]
            ["pubkey"]
            .as_str()
            .expect("fee payer");
        assert_eq!(
            fee_payer,
            to_base58(&swap.trader),
            "{signature}: trader must be the fee payer"
        );

        // The leg that came INTO the pool.
        let expected_in: i128 = swap
            .amount_in
            .to_string()
            .parse()
            .expect("amount_in fits i128");
        if token_in == wsol && swap.protocol == "pump_fun" {
            // A bonding curve's SOL leg is a bare lamport change.
            let delta =
                lamport_delta(&result, &pool).expect("curve lamports");
            assert_eq!(
                delta, expected_in,
                "{signature}: the curve's lamport gain must equal amount_in"
            );
        } else {
            let delta = *deltas
                .get(&(pool.clone(), token_in.clone()))
                .unwrap_or_else(|| {
                    panic!(
                        "{signature}: no {token_in} delta for pool {pool}"
                    )
                });
            assert_eq!(
                delta, expected_in,
                "{signature}: amount_in disagrees with the RPC"
            );
        }

        // The leg that left the pool.
        let expected_out: i128 = swap
            .amount_out_gross
            .to_string()
            .parse()
            .expect("amount_out_gross fits i128");
        if token_out == wsol && swap.protocol == "pump_fun" {
            let delta =
                lamport_delta(&result, &pool).expect("curve lamports");
            assert_eq!(
                delta, -expected_out,
                "{signature}: the curve's lamport loss must equal \
                 amount_out_gross"
            );
        } else {
            let delta = *deltas
                .get(&(pool.clone(), token_out.clone()))
                .unwrap_or_else(|| {
                    panic!(
                        "{signature}: no {token_out} delta for pool {pool}"
                    )
                });
            assert_eq!(
                delta, -expected_out,
                "{signature}: amount_out_gross disagrees with the RPC"
            );
        }

        // Both mints are proven, and they are the ones that moved.
        assert_eq!(swap.verified_in, swap.token_in);
        assert_eq!(swap.verified_out, swap.token_out);
        assert_ne!(token_in, token_out);

        *done += 1;
        checked += 1;
        *per_venue.entry(swap.protocol.clone()).or_insert(0) += 1;

        if wanted.get(&Venue::PumpSwap).copied().unwrap_or(0) >= target
            && wanted.get(&Venue::PumpFun).copied().unwrap_or(0) >= target
        {
            break;
        }
    }

    println!("\n=== cross-check against {RPC} ===");
    for (venue, count) in &per_venue {
        println!("  {venue:<14} {count} swaps matched exactly");
    }
    println!("  total {checked}");

    assert!(
        per_venue.get("pumpswap").copied().unwrap_or(0) >= target,
        "wanted {target} PumpSwap swaps cross-checked, got {:?}",
        per_venue.get("pumpswap")
    );
    assert!(
        per_venue.get("pump_fun").copied().unwrap_or(0) >= target,
        "wanted {target} pump.fun curve trades cross-checked, got {:?}",
        per_venue.get("pump_fun")
    );
}

/// The three tricky transactions of docs/solana-research.md are recorded
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
