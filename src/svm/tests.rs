//! Decoder tests against the RECORDED LIVE transactions in
//! `fixtures/recorded.json`.
//!
//! Every expected number in here was taken from the transaction's own
//! balance changes, not from the decoder's output, so these tests can fail
//! the decoder rather than merely describing it.

use alloy::primitives::U256;

use crate::svm::{
    decode::{decode_transaction, decode_transaction_with},
    fixtures,
    models::{unpack_ordinal, SOLANA_CHAIN},
    programs::{to_base58, Registry, Venue},
};

const CHAIN: u64 = SOLANA_CHAIN;

fn decode(name: &str) -> crate::svm::decode::DecodeOutcome {
    let fixture = fixtures::get(name);
    decode_transaction(CHAIN, fixture.timestamp(), &fixture.transaction)
}

// --- the generic movement layer -----------------------------------------

/// A plain PumpSwap buy. Every figure below is the transaction's own
/// balance change, read out of `account_activity`.
#[test]
fn a_pumpswap_buy_decodes_to_the_balances_that_actually_moved() {
    let outcome = decode("pumpswap_buy");
    assert_eq!(outcome.swaps.len(), 1, "exactly one swap");
    let swap = &outcome.swaps[0];

    assert_eq!(swap.chain, CHAIN);
    assert_eq!(swap.block_number, 448_310_216);
    assert_eq!(swap.tx_index, 139);
    assert_eq!(swap.protocol, "pumpswap");

    // The pool is the common owner of the two vault token accounts, and it
    // is the account PumpSwap's own event names.
    assert_eq!(
        to_base58(&swap.pool_id),
        "EJTBQyiF4GMwXSjucW1qnBMVa7iJD21yyvCvMDFrkSrR"
    );

    // WSOL in, the pump token out.
    assert_eq!(
        to_base58(&swap.token_in),
        "So11111111111111111111111111111111111111112"
    );
    assert_eq!(
        to_base58(&swap.token_out),
        "wPktHiifvRwpjEQJsbBk9tAx75qJoYnzZAnmDV6pump"
    );

    // Pool quote vault 7YHYVZ: 708_414_491_867 -> 708_537_194_761.
    assert_eq!(swap.amount_in, U256::from(122_702_894u64));
    // User base ATA 9acHeD: 281_959_571_155 -> 286_117_191_269.
    assert_eq!(swap.amount_out, U256::from(4_157_620_114u64));
    assert_eq!(swap.amount_out_gross, U256::from(4_157_620_114u64));

    // Both mints are PROVEN by real movement, never claimed by an event.
    assert_eq!(swap.verified_in, swap.token_in);
    assert_eq!(swap.verified_out, swap.token_out);

    // The trader is the fee payer, not whatever the venue calls the user.
    assert_eq!(
        to_base58(&swap.trader),
        "5e2SDXr1HCNyu47txjXnVd8wraceGhgVTzSh24jwrQHy"
    );

    // Direct trade: no router above it.
    assert_eq!(swap.route_ordinal, 0);

    // The swap instruction is the seventh top-level instruction, [6].
    assert_eq!(unpack_ordinal(swap.ordinal), vec![6]);
}

/// The fee transfers of that buy are classified as fees, not as legs.
///
/// Three transfers of the QUOTE mint leave the user for accounts that are
/// not the pool: two protocol fee recipients (30,615 + 30,614 = 61,229) and
/// the coin creator (673,519).
#[test]
fn fee_transfers_are_not_mistaken_for_swap_legs() {
    let swap = &decode("pumpswap_buy").swaps[0];
    assert_eq!(
        swap.fee_amount,
        U256::from(244_916u64 + 61_229 + 673_519),
        "lp_fee + protocol_fee + coin_creator_fee, from the venue's event"
    );
    // Had a fee leg been counted as the swap leg, amount_in would have
    // grown by exactly those amounts.
    assert_eq!(swap.amount_in, U256::from(122_702_894u64));
}

/// A pump.fun bonding curve sell.
///
/// The SOL leg of this trade has NO INSTRUCTION AT ALL: the curve program
/// decrements its own account's lamports. The only trace is the curve
/// account's native balance delta, so a decoder that looks solely at
/// transfer instructions sees one mint move and reports nothing.
#[test]
fn a_bonding_curve_sell_recovers_its_sol_leg_from_lamports() {
    let outcome = decode("pumpfun_sell");
    assert_eq!(outcome.swaps.len(), 1);
    let swap = &outcome.swaps[0];

    assert_eq!(swap.protocol, "pump_fun");
    // The bonding curve account is the pool.
    assert_eq!(
        to_base58(&swap.pool_id),
        "HaJwBJYmFyBRxuVQe4Yr53wkkDQbWsDH8uC7S362y1Ew"
    );
    // Token in, SOL out.
    assert_eq!(
        to_base58(&swap.token_in),
        "GeZNqzY8pDDhhsxkWruUhd6tVb4jd3gG8a72g4Gkpump"
    );
    assert_eq!(
        to_base58(&swap.token_out),
        "So11111111111111111111111111111111111111112"
    );
    // The Token-2022 transferChecked amount, which is also both token
    // accounts' balance delta.
    assert_eq!(swap.amount_in, U256::from(4_328_848_585_204u64));
    // The curve's lamport delta: 27_601_921 -> 25_970_622.
    assert_eq!(swap.amount_out, U256::from(1_631_299u64));
    // The instruction is [2, 0]: an inner instruction.
    assert_eq!(unpack_ordinal(swap.ordinal), vec![2, 0]);
}

/// THE reason this decoder works per instruction subtree.
///
/// docs/solana-research.md section 2.2: one transaction, two PumpSwap swaps
/// on the SAME pool in OPPOSITE directions, signed by two different signers.
/// About 6.2 SOL moves each way and the transaction's NET vault delta is
/// about 0.03 SOL. Anything that reads transaction-level balances reports
/// one 0.03 SOL trade instead of two 6.2 SOL trades.
#[test]
fn two_opposite_swaps_in_one_transaction_stay_two_swaps() {
    let outcome = decode("two_opposite_swaps");
    assert_eq!(
        outcome.swaps.len(),
        2,
        "the netting case must produce TWO swaps"
    );

    let (first, second) = (&outcome.swaps[0], &outcome.swaps[1]);

    // Same pool, opposite directions.
    assert_eq!(first.pool_id, second.pool_id);
    assert_eq!(first.token_in, second.token_out);
    assert_eq!(first.token_out, second.token_in);

    // Each leg is far larger than the transaction's net movement, which is
    // the whole point.
    let net = first.amount_in.abs_diff(second.amount_out);
    assert!(
        net < first.amount_in / U256::from(10u8),
        "the two legs should very nearly cancel: {} vs {}",
        first.amount_in,
        second.amount_out
    );
    assert!(first.amount_in > U256::from(1_000_000_000u64));
    assert!(second.amount_in > U256::from(1_000_000_000u64));

    // Ordinals are distinct and ordered, so the two rows cannot collapse
    // into one in a ReplacingMergeTree.
    assert!(first.ordinal < second.ordinal);
    assert_ne!(first.ordinal, second.ordinal);

    // Neither of them borrowed a native lamport delta: the pool is used
    // twice, so that shortcut is refused by construction.
    assert_eq!(outcome.diagnostics.ambiguous_native, 0);
}

/// A prop-AMM quote update must decode to NOTHING.
///
/// A direct call to BisonFi with 379 compute units and zero token movement.
/// Counting "instructions of program P" as trades would overstate prop AMM
/// activity about fivefold (research section 2.2); requiring real movement
/// filters these out by construction rather than by a rule.
#[test]
fn a_prop_amm_quote_update_decodes_to_nothing() {
    let fixture = fixtures::get("bisonfi_quote_update");
    // Register BisonFi as a venue so the test proves the MOVEMENT rule
    // rejects it, not merely that the program is unregistered.
    let registry = Registry::with_venues(&Venue::ALL);
    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &registry,
    );

    assert!(
        outcome.swaps.is_empty(),
        "a quote update is not a trade: {:?}",
        outcome.swaps
    );
    assert!(
        outcome.diagnostics.no_movement >= 1,
        "it should be counted as an instruction with no movement"
    );
}

/// A Jupiter v6 three-hop route becomes one swap PER VENUE, each attributed
/// to the router rather than credited to it.
///
/// This is also the extensibility proof: the three venues here have no
/// per-program decoder at all. Registering their program ids is the only
/// thing needed for the generic layer to decode them.
#[test]
fn a_jupiter_route_is_one_swap_per_venue_and_never_router_volume() {
    let fixture = fixtures::get("jupiter_three_hop");
    let registry = Registry::with_venues(&Venue::ALL);
    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &registry,
    );

    assert_eq!(
        outcome.swaps.len(),
        3,
        "three hops must be three separate fills, got {:?}",
        outcome
            .swaps
            .iter()
            .map(|s| s.protocol.as_str())
            .collect::<Vec<_>>()
    );

    let venues: Vec<&str> =
        outcome.swaps.iter().map(|s| s.protocol.as_str()).collect();
    assert!(venues.contains(&"bisonfi"));
    assert!(venues.contains(&"meteora_dlmm"));
    assert!(venues.contains(&"raydium_cpmm"));

    // None of them is credited to Jupiter, and all of them name it.
    for swap in &outcome.swaps {
        assert_ne!(swap.protocol, "jupiter_v6");
        assert_ne!(
            swap.route_ordinal, 0,
            "{} should carry the router as attribution",
            swap.protocol
        );
        assert_eq!(
            to_base58(&swap.route_program),
            "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"
        );
        // Every hop is a real fill with both mints proven.
        assert!(swap.amount_in > U256::ZERO);
        assert!(swap.amount_out > U256::ZERO);
        assert_ne!(swap.verified_in, swap.verified_out);
    }

    // The hops chain: each hop's output mint is the next hop's input.
    let mut sorted = outcome.swaps.clone();
    sorted.sort_by_key(|swap| swap.ordinal);
    for pair in sorted.windows(2) {
        assert_eq!(
            pair[0].token_out, pair[1].token_in,
            "hops should chain through the route"
        );
    }
}

/// A Token-2022 transfer fee makes what the taker RECEIVED smaller than
/// what the pool SENT, and no event mentions it. Both must be stored.
#[test]
fn gross_and_net_output_are_both_kept() {
    for fixture in fixtures::all() {
        let outcome = decode_transaction_with(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
            &Registry::with_venues(&Venue::ALL),
        );
        for swap in &outcome.swaps {
            assert!(
                swap.amount_out <= swap.amount_out_gross,
                "{}: the taker cannot receive more than the pool sent",
                fixture.name
            );
        }
    }
}

// --- the two layers must agree ------------------------------------------

/// The cross-check the whole two-layer design exists for.
///
/// The movement layer infers the pool with no knowledge of the venue - it
/// is simply the common owner of the two vault token accounts. The
/// per-program decoder reads the pool out of the venue's own event. On
/// every swap where both ran, they must name the same account and the same
/// amounts, and `confidence` becomes 'decoded' only when they did.
#[test]
fn the_per_program_decoders_agree_with_the_movement_layer() {
    let mut decoded = 0;
    for name in ["pumpswap_buy", "pumpfun_sell", "two_opposite_swaps"] {
        let outcome = decode(name);
        assert!(!outcome.swaps.is_empty(), "{name} decoded to nothing");
        assert_eq!(
            outcome.diagnostics.decoder_disagreed, 0,
            "{name}: a per-program decoder contradicted the movement layer"
        );
        for swap in &outcome.swaps {
            assert_eq!(
                swap.confidence, "decoded",
                "{name}: both layers ran, so confidence should be 'decoded'"
            );
            decoded += 1;
        }
    }
    assert_eq!(decoded, 4, "1 PumpSwap buy + 1 curve sell + 2 opposite");
}

/// A venue with no per-program decoder still produces rows, marked honestly.
///
/// BisonFi publishes nothing at all, so its fill can only ever be
/// `movement` - and that is not a defect, it is the honest label: the price,
/// the size and the trader are exact and only the pool state is missing.
#[test]
fn a_venue_without_a_decoder_is_marked_movement() {
    let fixture = fixtures::get("jupiter_three_hop");
    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &Registry::with_venues(&Venue::ALL),
    );

    let bisonfi: Vec<_> = outcome
        .swaps
        .iter()
        .filter(|swap| swap.protocol == "bisonfi")
        .collect();
    assert!(!bisonfi.is_empty(), "the route's BisonFi hop went missing");
    for swap in bisonfi {
        assert_eq!(
            swap.confidence, "movement",
            "BisonFi publishes no event, so nothing can confirm it"
        );
        assert_eq!(swap.reserve0, U256::ZERO);
        assert_eq!(swap.reserve1, U256::ZERO);
    }
}

/// The phase 2 payoff, on a transaction recorded before these decoders
/// existed: the Meteora DLMM hop of the recorded Jupiter route now decodes
/// from the venue's OWN self-CPI events, and agrees with the movement layer.
///
/// This is the strongest evidence in the suite that the `Swap` / `Swap2Evt`
/// offsets are right, because nothing about the recording was chosen to suit
/// them - it is a phase 1 fixture, captured for an entirely different
/// purpose, and the decoder either reproduces the amounts the SPL transfers
/// independently prove or it does not.
#[test]
fn the_meteora_hop_of_the_recorded_route_decodes_from_its_own_events() {
    let fixture = fixtures::get("jupiter_three_hop");
    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &Registry::with_venues(&Venue::ALL),
    );

    let dlmm = outcome
        .swaps
        .iter()
        .find(|swap| swap.protocol == "meteora_dlmm")
        .expect("the route's Meteora DLMM hop");

    assert_eq!(
        dlmm.confidence, "decoded",
        "the DLMM self-CPI events must confirm the movement layer"
    );
    // A confirmed row carries the pool the VENUE names, which the movement
    // layer found independently as the common owner of the two vaults.
    assert_ne!(dlmm.pool_id, crate::svm::models::ZERO_PUBKEY);
    // And the fee is the venue's own number, not something inferred.
    assert!(
        dlmm.fee_amount > U256::ZERO,
        "a DLMM swap always pays a bin fee"
    );

    // The route is still one swap per venue, and still credited to the
    // venues rather than to Jupiter.
    let venues: Vec<&str> =
        outcome.swaps.iter().map(|s| s.protocol.as_str()).collect();
    assert!(venues.contains(&"bisonfi"));
    assert!(venues.contains(&"meteora_dlmm"));
}

// --- invariants over every fixture --------------------------------------

/// Structural invariants no swap row may ever break.
#[test]
fn every_decoded_swap_satisfies_the_row_invariants() {
    for fixture in fixtures::all() {
        let outcome = decode_transaction_with(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
            &Registry::with_venues(&Venue::ALL),
        );
        for swap in &outcome.swaps {
            let what = &fixture.name;
            assert_ne!(
                swap.token_in, swap.token_out,
                "{what}: a swap must move two different mints"
            );
            assert!(
                swap.amount_in > U256::ZERO,
                "{what}: a zero-amount leg is not a trade"
            );
            assert!(swap.amount_out_gross > U256::ZERO, "{what}");
            // token0 / token1 are the mints sorted by raw bytes.
            assert!(swap.token0 < swap.token1, "{what}");
            assert!(
                (swap.token0 == swap.token_in
                    && swap.token1 == swap.token_out)
                    || (swap.token0 == swap.token_out
                        && swap.token1 == swap.token_in),
                "{what}: token0/token1 must be the swap's two mints"
            );
            // Pool relative signs: one leg in, one leg out.
            assert!(
                swap.amount0.is_positive() != swap.amount1.is_positive(),
                "{what}: a swap has one leg into the pool and one out"
            );
            // 64 raw signature bytes.
            assert_eq!(swap.tx_id.len(), 64, "{what}");
            assert_eq!(swap.block_number, fixture.slot, "{what}");
            assert_eq!(swap.timestamp, fixture.timestamp(), "{what}");
        }
    }
}

/// Ordinals are unique inside a transaction, or two swaps of the same
/// transaction would replace each other in a ReplacingMergeTree.
#[test]
fn ordinals_are_unique_within_a_transaction() {
    for fixture in fixtures::all() {
        let outcome = decode_transaction_with(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
            &Registry::with_venues(&Venue::ALL),
        );
        let mut ordinals: Vec<u64> =
            outcome.swaps.iter().map(|swap| swap.ordinal).collect();
        let before = ordinals.len();
        ordinals.sort_unstable();
        ordinals.dedup();
        assert_eq!(before, ordinals.len(), "{}", fixture.name);
    }
}

/// A failed transaction moved nothing: its instructions were rolled back.
#[test]
fn a_failed_transaction_produces_no_rows() {
    let fixture = fixtures::get("pumpswap_buy");
    let mut tx = fixture.transaction.clone();
    tx.success = false;
    let outcome = decode_transaction(CHAIN, fixture.timestamp(), &tx);
    assert!(outcome.swaps.is_empty());
}

/// The decoder never panics, whatever it is handed.
#[test]
fn truncated_input_is_never_a_panic() {
    for fixture in fixtures::all() {
        let full = &fixture.transaction;
        for count in 0..full.instructions.len() {
            let mut tx = full.clone();
            tx.instructions.truncate(count);
            let _ = decode_transaction(CHAIN, fixture.timestamp(), &tx);
        }
        for count in 0..full.activity.len() {
            let mut tx = full.clone();
            tx.activity.truncate(count);
            let _ = decode_transaction(CHAIN, fixture.timestamp(), &tx);
        }
        // Instruction data chopped to every possible length.
        for index in 0..full.instructions.len() {
            for length in 0..full.instructions[index].data.len().min(40) {
                let mut tx = full.clone();
                tx.instructions[index].data.truncate(length);
                let _ =
                    decode_transaction(CHAIN, fixture.timestamp(), &tx);
            }
        }
    }
}

// --- the module level entry point ---------------------------------------

#[test]
fn the_module_decode_emits_slots_transactions_swaps_and_mints() {
    let fixture = fixtures::get("pumpswap_buy");
    let batch = crate::svm::SvmSlotBatch {
        slot: fixture.slot,
        blockhash: fixture.blockhash,
        parent_slot: fixture.parent_slot,
        parent_blockhash: fixture.parent_blockhash,
        block_height: 0,
        timestamp: fixture.timestamp(),
        transactions: vec![fixture.transaction.clone()],
    };
    let rows = crate::svm::decode(CHAIN, &[batch]);

    assert_eq!(rows.slots.len(), 1);
    assert_eq!(rows.slots[0].block_number, fixture.slot);
    assert_eq!(rows.slots[0].parent_slot, fixture.parent_slot);
    assert_eq!(rows.transactions.len(), 1);
    assert_eq!(rows.swaps.len(), 1);

    // Decimals come free with the stream: both traded mints get a row and
    // no RPC call was made.
    assert_eq!(rows.tokens.len(), 2);
    let wsol = rows
        .tokens
        .iter()
        .find(|token| {
            to_base58(&token.mint)
                == "So11111111111111111111111111111111111111112"
        })
        .expect("WSOL row");
    assert_eq!(wsol.decimals, 9);
    let pump_token = rows
        .tokens
        .iter()
        .find(|token| {
            to_base58(&token.mint)
                == "wPktHiifvRwpjEQJsbBk9tAx75qJoYnzZAnmDV6pump"
        })
        .expect("base mint row");
    assert_eq!(pump_token.decimals, 6);
}

/// Versions and epochs are stamped once per flush across every table.
#[test]
fn version_and_epoch_are_stamped_on_every_block_scoped_row() {
    let fixture = fixtures::get("pumpswap_buy");
    let mut rows = crate::svm::decode(
        CHAIN,
        &[crate::svm::SvmSlotBatch {
            slot: fixture.slot,
            blockhash: fixture.blockhash,
            parent_slot: fixture.parent_slot,
            parent_blockhash: fixture.parent_blockhash,
            block_height: 0,
            timestamp: fixture.timestamp(),
            transactions: vec![fixture.transaction.clone()],
        }],
    );
    rows.set_version(42);
    rows.set_epoch(7);

    assert!(rows.slots.iter().all(|r| r._version == 42 && r.epoch == 7));
    assert!(rows
        .transactions
        .iter()
        .all(|r| r._version == 42 && r.epoch == 7));
    assert!(rows.swaps.iter().all(|r| r._version == 42 && r.epoch == 7));
    // sol_tokens is chain state, not block scoped: it carries no epoch.
    assert!(rows.tokens.iter().all(|r| r._version == 42));
}

// --- phase 2: the recorded venue fixtures --------------------------------

/// One transaction, hops on two DIFFERENT phase 2 venues, one swap each.
///
/// This is the property the whole two-layer design rests on: a route is not
/// one trade, it is N venue fills, and the subtree rule separates them
/// without knowing anything about the router.
#[test]
fn a_route_across_two_phase_2_venues_is_one_swap_per_venue() {
    let outcome = decode("multi_hop_route");

    let mut venues: Vec<&str> =
        outcome.swaps.iter().map(|s| s.protocol.as_str()).collect();
    venues.sort_unstable();
    venues.dedup();
    assert!(
        venues.len() >= 2,
        "the recorded route crosses two venues but decoded to {venues:?}"
    );

    // Every hop is a real fill with both legs proven by token movement,
    // and no hop is attributed to a router.
    for swap in &outcome.swaps {
        assert!(
            swap.amount_in > U256::ZERO,
            "{} has no input",
            swap.protocol
        );
        assert!(
            swap.amount_out_gross > U256::ZERO,
            "{} has no output",
            swap.protocol
        );
        assert_eq!(swap.verified_in, swap.token_in);
        assert_eq!(swap.verified_out, swap.token_out);
        assert_ne!(swap.token_in, swap.token_out);
        assert_ne!(
            swap.protocol, "jupiter_v6",
            "a router must never be a venue"
        );
    }

    // Distinct positions, so two hops can never overwrite each other in a
    // ReplacingMergeTree.
    let mut ordinals: Vec<u64> =
        outcome.swaps.iter().map(|s| s.ordinal).collect();
    let before = ordinals.len();
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(before, ordinals.len(), "two hops share one ordinal");
}

/// A liquidity instruction must decode to NO swap at all.
///
/// The movement layer's sign test is what does it: both mints cross the
/// pool the SAME way, which is not a trade. The venue's own discriminator
/// says the same thing independently, and the test asserts BOTH - a row
/// here would be fabricated volume.
#[test]
fn a_liquidity_instruction_decodes_to_no_swap() {
    use crate::svm::programs::{registry, IxKind};

    let fixture = fixtures::get("liquidity_no_swap");
    let outcome = decode("liquidity_no_swap");

    assert!(
        outcome.swaps.is_empty(),
        "a liquidity add/remove produced {} swap rows: {:?}",
        outcome.swaps.len(),
        outcome.swaps.iter().map(|s| &s.protocol).collect::<Vec<_>>()
    );

    // And the venue's own instruction name agrees that it is not a swap.
    let registry = registry();
    let kinds: Vec<IxKind> = fixture
        .transaction
        .instructions
        .iter()
        .filter_map(|ix| {
            registry
                .venue(&ix.program)
                .map(|venue| venue.instruction_kind(&ix.data))
        })
        .collect();
    assert!(
        kinds.contains(&IxKind::Liquidity),
        "the recorded transaction has no liquidity instruction: {kinds:?}"
    );
    assert!(
        !kinds.contains(&IxKind::Swap),
        "the recorded transaction also contains a swap, so it does not \
         isolate the liquidity case"
    );
}

/// A Token-2022 transfer fee: what the pool SENT and what the taker
/// RECEIVED are different numbers, and both are kept.
///
/// Storing one `amount_out` would be silently wrong for every Token-2022
/// pair, which is why the row has two columns for it.
#[test]
fn a_token_2022_transfer_fee_keeps_gross_and_net_apart() {
    let fixture = fixtures::get("token_2022_fee");
    let outcome = decode("token_2022_fee");
    assert!(!outcome.swaps.is_empty(), "no swap decoded");

    // The transfer fee is stated by the venue itself, in its own event.
    // Every log-event venue is asked, because which of them the recorder
    // happened to find is not the point of the test - and because asking
    // only one of Raydium's two would read the other's bytes at the wrong
    // offsets, they sharing a discriminator.
    use crate::svm::venues::{
        OrcaTraded, RaydiumClmmSwap, RaydiumCpmmSwap,
    };
    let declared: u64 = fixture
        .transaction
        .logs
        .iter()
        .filter_map(|log| log.event_bytes())
        .map(|bytes| {
            if let Some(event) = RaydiumCpmmSwap::parse(&bytes) {
                event.input_transfer_fee + event.output_transfer_fee
            } else if let Some(event) = RaydiumClmmSwap::parse(&bytes) {
                event.transfer_fee_0 + event.transfer_fee_1
            } else if let Some(event) = OrcaTraded::parse(&bytes) {
                event.input_transfer_fee + event.output_transfer_fee
            } else {
                0
            }
        })
        .sum();
    assert!(
        declared > 0,
        "the recorded transaction declares no Token-2022 transfer fee, so \
         it does not exercise the case"
    );

    // The Token-2022 program really is in the transaction, and it is not
    // the classic SPL Token program.
    let token_2022 =
        crate::svm::programs::pubkey(crate::svm::programs::TOKEN_2022_B58);
    assert!(
        fixture
            .transaction
            .instructions
            .iter()
            .any(|ix| ix.program == token_2022),
        "no Token-2022 instruction in the recorded transaction"
    );

    for swap in &outcome.swaps {
        assert!(
            swap.amount_out <= swap.amount_out_gross,
            "the taker cannot receive more than the pool sent"
        );
    }
}

/// A concentrated-liquidity swap that moved the price across a tick.
///
/// Orca's `Traded` is the only event among these venues that reports the
/// sqrt price BEFORE as well as after, so it is the only one that can state
/// a tick crossing rather than imply one. One tick is a 1.0001x price step,
/// i.e. ~0.00005 in sqrt price.
#[test]
fn a_clmm_swap_crossing_ticks_decodes_and_reports_its_price_move() {
    let fixture = fixtures::get("clmm_tick_crossing");
    let outcome = decode("clmm_tick_crossing");

    let orca: Vec<_> = outcome
        .swaps
        .iter()
        .filter(|s| s.protocol == Venue::OrcaWhirlpool.as_str())
        .collect();
    assert!(!orca.is_empty(), "the Orca hop did not decode");

    let events: Vec<crate::svm::venues::OrcaTraded> = fixture
        .transaction
        .logs
        .iter()
        .filter_map(|log| log.event_bytes())
        .filter_map(|bytes| crate::svm::venues::OrcaTraded::parse(&bytes))
        .collect();
    assert!(!events.is_empty(), "no Orca `Traded` event in the recording");

    let crossed = events.iter().any(|event| {
        let (low, high) = if event.pre_sqrt_price < event.post_sqrt_price {
            (event.pre_sqrt_price, event.post_sqrt_price)
        } else {
            (event.post_sqrt_price, event.pre_sqrt_price)
        };
        low > 0 && (high.saturating_sub(low) as f64 / low as f64) > 0.00005
    });
    assert!(
        crossed,
        "the recorded swap did not move the price by a whole tick, so it \
         does not exercise a tick crossing"
    );

    // And the event confirmed the movement layer, which is the point: a
    // log-sourced event is only ever allowed to CONFIRM a row that real
    // token transfers already proved.
    assert!(
        orca.iter().any(|swap| swap.confidence == "decoded"),
        "the Orca event did not confirm any hop"
    );
}

/// Every phase 2 event layout, checked against the recorded mainnet bytes
/// by PAYLOAD LENGTH.
///
/// This is the check that catches a stale IDL, and it has already earned
/// its place twice: Raydium's CPMM `SwapEvent` is 170 bytes on the wire
/// against 89 in the pre-creator-fee snapshot, and CLMM's is 221 against
/// 205 before the trade-fee fields were added. A decoder written to either
/// older layout parses the newer bytes happily and returns nonsense.
#[test]
fn venue_event_lengths_match_the_chain() {
    use crate::svm::venues::{
        MeteoraDlmmSwap, MeteoraDlmmSwap2, OrcaTraded, RaydiumClmmSwap,
        RaydiumCpmmSwap,
    };

    let mut seen: Vec<(&str, usize)> = Vec::new();

    for fixture in fixtures::all() {
        for log in &fixture.transaction.logs {
            let Some(bytes) = log.event_bytes() else { continue };
            if OrcaTraded::parse(&bytes).is_some() {
                assert_eq!(bytes.len(), OrcaTraded::LEN);
                seen.push(("orca Traded", bytes.len()));
            }
            if RaydiumCpmmSwap::parse(&bytes).is_some() {
                assert_eq!(bytes.len(), RaydiumCpmmSwap::LEN);
                seen.push(("raydium cpmm SwapEvent", bytes.len()));
            }
            if RaydiumClmmSwap::parse(&bytes).is_some() {
                assert_eq!(bytes.len(), RaydiumClmmSwap::LEN);
                seen.push(("raydium clmm SwapEvent", bytes.len()));
            }
        }
        for instruction in &fixture.transaction.instructions {
            if MeteoraDlmmSwap::parse(&instruction.data).is_some() {
                assert_eq!(instruction.data.len(), MeteoraDlmmSwap::LEN);
                seen.push(("dlmm Swap", instruction.data.len()));
            }
            if MeteoraDlmmSwap2::parse(&instruction.data).is_some() {
                assert_eq!(instruction.data.len(), MeteoraDlmmSwap2::LEN);
                seen.push(("dlmm Swap2Evt", instruction.data.len()));
            }
        }
    }

    seen.sort_unstable();
    seen.dedup();
    assert!(
        seen.len() >= 3,
        "too few phase 2 event layouts appear in the fixtures to be a \
         meaningful check: {seen:?}"
    );
}

/// A truncated event is `None`, never a panic and never an invented value.
///
/// A validator can and does cut a log line short, so every one of these
/// parsers is fed every prefix of a real event.
#[test]
fn a_truncated_venue_event_is_never_a_panic() {
    use crate::svm::venues::{
        MeteoraDamm2Swap, MeteoraDlmmSwap, MeteoraDlmmSwap2, OrcaTraded,
        RayLogSwap, RaydiumClmmSwap, RaydiumCpmmSwap,
    };

    let mut bodies: Vec<Vec<u8>> = Vec::new();
    for fixture in fixtures::all() {
        for log in &fixture.transaction.logs {
            if let Some(bytes) = log.event_bytes() {
                bodies.push(bytes);
            }
        }
        for instruction in &fixture.transaction.instructions {
            bodies.push(instruction.data.clone());
        }
    }
    assert!(!bodies.is_empty());

    for body in &bodies {
        for length in 0..body.len() {
            let prefix = &body[..length];
            let _ = OrcaTraded::parse(prefix);
            let _ = RaydiumCpmmSwap::parse(prefix);
            let _ = RaydiumClmmSwap::parse(prefix);
            let _ = MeteoraDlmmSwap::parse(prefix);
            let _ = MeteoraDlmmSwap2::parse(prefix);
            let _ = MeteoraDamm2Swap::parse(prefix);
            let _ = RayLogSwap::parse(prefix);
        }
    }
}
