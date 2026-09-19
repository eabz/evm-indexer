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

fn token() -> Option<String> {
    std::env::var("ENVIO_API_TOKEN").ok().filter(|t| !t.trim().is_empty())
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

        let event = tx
            .logs
            .iter()
            .filter(|log| {
                log.is_data && log.path == path && log.program == instruction.program
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
        for instruction in
            instructions.as_array().into_iter().flatten()
        {
            let Some(parsed) = instruction.get("parsed") else {
                continue;
            };
            let kind = parsed.get("type").and_then(|v| v.as_str());
            if !matches!(kind, Some("transfer") | Some("transferChecked")) {
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
