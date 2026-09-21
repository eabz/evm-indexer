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
/// the Solana venue research section 2.2: one transaction, two PumpSwap swaps
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

// --- review round 4 ------------------------------------------------------
//
// Everything below pins a finding of the ADDENDUM of review round 4.
// Where the report names a real transaction, that transaction is the
// fixture (`fixtures/round4.json`); where it names a shape no recording of
// which could be found, the transaction is BUILT here and the test says so.

/// The five vault authorities the addendum measured: one account per
/// PROGRAM, shared by every pool that program runs.
const GLOBAL_VAULT_AUTHORITIES: &[(&str, &str)] = &[
    ("raydium_amm_v4", "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"),
    ("raydium_cpmm", "GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL"),
    ("meteora_damm_v2", "HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC"),
    ("meteora_dbc", "FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM"),
    ("raydium_launchlab", "WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh"),
];

/// B3. Half the streamed venues own EVERY pool's vaults with one
/// program-wide account, and the movement layer's idea of a pool is exactly
/// that owner. Storing it keys the whole venue into ONE candle series:
/// open/high/low/close would mix USDC/SOL with arbitrary memecoin prices
/// and the volumes would sum amounts of unrelated mints.
///
/// The transaction is the one the report names,
/// `3ZZw4CfNzTMgPnnJRhKk28bteiip6zDimArpQSNURYfLn94J4UTPmYfBqTU2SfJ3pbwodZV3rGsUz7Qfyh8MVPJP`,
/// and it is decoded with its LOGS REMOVED - which is the condition the
/// finding is about: Raydium v4's only event is a log line, so any
/// transaction whose logs the validator truncated, or whose `ray_log` says
/// something the movement layer contradicts, reaches the table on the
/// movement layer alone.
#[test]
fn a_program_wide_vault_authority_is_never_stored_as_a_pool() {
    let authorities: Vec<crate::svm::models::Pubkey> =
        GLOBAL_VAULT_AUTHORITIES
            .iter()
            .map(|(_, id)| crate::svm::programs::pubkey(id))
            .collect();

    for fixture in fixtures::all() {
        // Both readings of every recording: the event confirming the row,
        // and the event absent.
        for logs in [true, false] {
            let mut tx = fixture.transaction.clone();
            if !logs {
                tx.logs.clear();
                tx.dropped_logs = true;
            }
            let outcome = decode_transaction_with(
                CHAIN,
                fixture.timestamp(),
                &tx,
                &Registry::with_venues(&Venue::ALL),
            );
            for swap in &outcome.swaps {
                assert!(
                    !authorities.contains(&swap.pool_id),
                    "{}: {} stored the program-wide vault authority {} as \
                     its pool, which collapses every pair of the venue \
                     into one candle series",
                    fixture.name,
                    swap.protocol,
                    to_base58(&swap.pool_id),
                );
            }
        }
    }
}

/// And the row is not merely un-mis-keyed: the pool is RECOVERED from the
/// instruction's own account metas, so a Raydium v4 fill whose log is gone
/// still lands on the right series.
#[test]
fn the_raydium_v4_pool_survives_the_loss_of_its_log() {
    let fixture = fixtures::get("raydium_v4_and_pumpswap");
    let mut tx = fixture.transaction.clone();
    tx.logs.clear();
    tx.dropped_logs = true;

    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &tx,
        &Registry::with_venues(&Venue::ALL),
    );
    let v4 = outcome
        .swaps
        .iter()
        .find(|swap| swap.protocol == Venue::RaydiumAmmV4.as_str())
        .expect("the Raydium v4 hop");

    // The real pool, which the same transaction's `ray_log` path also
    // names when the log is there: Raydium's SOL/USDC v4 market.
    assert_eq!(
        to_base58(&v4.pool_id),
        "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"
    );
    // ... and it is honestly labelled, because no event confirmed it.
    assert_eq!(v4.confidence, "movement");
}

/// The account index each venue's pool sits at is not a guess: wherever a
/// recording carries both the instruction and the venue's own event, the
/// index must select exactly the account the event names.
#[test]
fn the_pool_account_index_is_the_account_the_venue_names() {
    use crate::svm::{
        programs::IxKind,
        venues::{
            DbcSwap2, LaunchlabTrade, MeteoraDamm2Swap, RaydiumCpmmSwap,
        },
    };

    let registry = Registry::with_venues(&Venue::ALL);
    let mut checked = 0;

    for fixture in fixtures::all() {
        let tx = &fixture.transaction;
        for instruction in &tx.instructions {
            let Some(venue) = registry.venue(&instruction.program) else {
                continue;
            };
            if !venue.vault_authority_is_global()
                || venue.instruction_kind(&instruction.data)
                    != IxKind::Swap
            {
                continue;
            }

            // What the venue itself says the pool is, from its own event.
            let mut named = Vec::new();
            for child in &tx.instructions {
                if child.path.len() != instruction.path.len() + 1
                    || !child.path.starts_with(&instruction.path)
                {
                    continue;
                }
                if let Some(event) = MeteoraDamm2Swap::parse(&child.data) {
                    named.push(event.pool);
                }
                if let Some(event) = DbcSwap2::parse(&child.data) {
                    named.push(event.pool);
                }
                if let Some(event) = LaunchlabTrade::parse(&child.data) {
                    named.push(event.pool_state);
                }
            }
            for log in tx
                .logs
                .iter()
                .filter(|log| log.path == instruction.path && log.is_data)
            {
                if let Some(event) = log
                    .event_bytes()
                    .and_then(|bytes| RaydiumCpmmSwap::parse(&bytes))
                {
                    named.push(event.pool_id);
                }
            }

            for pool in named {
                let index =
                    venue.pool_account_index().unwrap_or_else(|| {
                        panic!(
                            "{}: {} names a pool in its event but has no \
                         verified account index, so a movement-only row \
                         of it would have no pool key",
                            fixture.name,
                            venue.as_str()
                        )
                    });
                assert_eq!(
                    instruction.account(index),
                    Some(pool),
                    "{}: {}'s pool account index {index} does not select \
                     the pool its own event names",
                    fixture.name,
                    venue.as_str()
                );
                checked += 1;
            }
        }
    }

    assert!(
        checked >= 2,
        "no recording exercises a global-authority venue's pool index"
    );

    // And the decoder checks the same thing on every row it writes, so a
    // venue that changes an account layout shows up as a counter rather
    // than as a wrong pool key. It must be zero over the whole corpus.
    for fixture in fixtures::all() {
        let outcome = decode_transaction_with(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
            &registry,
        );
        assert_eq!(
            outcome.diagnostics.pool_index_disagreed, 0,
            "{}: the account index and the venue's event name different \
             pools",
            fixture.name
        );
    }
}

/// M4. `amount_out` is what the TAKER received, and on a routed trade the
/// taker's receive account is created and closed inside the transaction -
/// so it appears in neither the pre nor the post balances and has no
/// readable delta. Taking "the next movement with a readable delta"
/// instead lands on a protocol or creator FEE recipient, whose small
/// positive delta passes every guard.
///
/// Both numbers below are the report's, measured on the chain.
#[test]
fn amount_out_is_never_a_fee_recipients_balance_delta() {
    let outcome = decode("raydium_v4_and_pumpswap");
    let sell = outcome
        .swaps
        .iter()
        .find(|swap| unpack_ordinal(swap.ordinal) == vec![4])
        .expect("the PumpSwap sell at [4]");
    // The fee recipient's delta the row used to store.
    assert_ne!(
        sell.amount_out,
        U256::from(32_922_301u64),
        "amount_out is a fee recipient's balance delta"
    );
    // The taker's account is unreadable, so the honest answer is what the
    // pool SENT - never some other account's delta.
    assert_eq!(sell.amount_out, sell.amount_out_gross);
    assert_eq!(sell.amount_out, U256::from(131_425_822_336u64));

    let outcome = decode("launchlab_sell");
    let launchlab = outcome
        .swaps
        .iter()
        .find(|swap| swap.protocol == Venue::RaydiumLaunchlab.as_str())
        .expect("the LaunchLab sell");
    assert_ne!(
        launchlab.amount_out,
        U256::from(29_570u64),
        "amount_out is the fee, not the fill"
    );
    assert_eq!(launchlab.amount_out, U256::from(2_949_621u64));
}

/// M6. `trader` was the transaction's fee payer, always. On this recorded
/// curve sell the fee payer is a BOT and the venue's event names the person
/// who traded - and `sol_dex_candles_*.traders` is `uniqState(trader)`, so
/// the unique-trader count was a count of bots.
///
/// The launchpad decoder already stored the event's user for this very
/// trade, so the two tables disagreed about one trade. They must agree.
#[test]
fn the_trader_is_the_venues_user_and_not_the_bot_that_paid_the_fee() {
    let fixture = fixtures::get("pumpfun_sell");
    let outcome = decode("pumpfun_sell");
    let swap = &outcome.swaps[0];

    let user = crate::svm::programs::pubkey(fixtures::PUMPFUN_SELL_USER);
    assert_eq!(swap.trader, user, "the trader is the event's user");
    assert_ne!(
        swap.trader, fixture.transaction.fee_payer,
        "the fee payer of this recording is a bot, not the trader"
    );

    // And the launchpad row of the SAME trade agrees, which it did not
    // before: it has always used the event's user.
    let launchpads = crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &outcome.swaps,
    );
    let trade = launchpads
        .trades
        .first()
        .expect("the curve trade's launchpad row");
    assert_eq!(
        trade.trader, swap.trader,
        "sol_dex_swaps and launchpad_trades disagree about who traded"
    );
}

/// M7. `has_dropped_log_messages` is the validator saying this
/// transaction's log stream is incomplete. For a venue whose event exists
/// ONLY as a log line that is decisive - a line that survived cannot be
/// told from one that did not - and the flag was plumbed end to end and
/// never read.
#[test]
fn a_transaction_with_dropped_logs_is_not_enriched_from_its_logs() {
    let fixture = fixtures::get("launchlab_sell");
    let mut tx = fixture.transaction.clone();
    tx.dropped_logs = true;

    let outcome = decode_transaction_with(
        CHAIN,
        fixture.timestamp(),
        &tx,
        &Registry::with_venues(&Venue::ALL),
    );

    for swap in &outcome.swaps {
        let venue = Venue::ALL
            .iter()
            .find(|venue| venue.as_str() == swap.protocol)
            .copied()
            .expect("a known venue");
        match venue.event_source() {
            crate::svm::programs::EventSource::Log => assert_eq!(
                swap.confidence, "movement",
                "{}: its event is a LOG LINE and the validator dropped \
                 log lines here, so nothing can confirm this row",
                swap.protocol
            ),
            // A self-CPI event is an INSTRUCTION. Validators never drop
            // those, so the LaunchLab row is unaffected - which is the
            // other half of the property: the flag must not make the
            // decoder blind to events that are still all there.
            _ => {
                assert_eq!(swap.confidence, "decoded", "{}", swap.protocol)
            }
        }
    }
    assert!(
        outcome.diagnostics.dropped_logs >= 2,
        "the incomplete rows must be COUNTED, or the agreement rate reads \
         them as 'this venue emits no event': {:?}",
        outcome.diagnostics
    );
    // And the same transaction with the flag clear decodes both log
    // venues, so the test is about the flag and not about the bytes.
    let honest = decode("launchlab_sell");
    assert!(honest.swaps.iter().all(|swap| swap.confidence == "decoded"));
}

/// M9. A fill routed through an aggregator other than Jupiter v6 used to
/// read as a DIRECT trade, because Jupiter v6 was the only registered
/// router. Volume was never wrong - the fill is attributed to the venue
/// either way - but the attribution was missing for most of the 40% of
/// the chain that is routed.
#[test]
fn a_fill_under_any_registered_router_carries_it_as_attribution() {
    let registry = Registry::with_venues(&Venue::ALL);
    let mut seen = 0;
    for (name, id) in crate::svm::programs::ROUTERS_B58 {
        let router = crate::svm::programs::pubkey(id);
        assert_eq!(registry.router(&router), Some(*name));

        let tx = build::routed_pumpswap_like(router);
        let outcome = decode_transaction_with(CHAIN, 1, &tx, &registry);
        let swap = outcome
            .swaps
            .first()
            .unwrap_or_else(|| panic!("{name}: the fill went missing"));
        assert_eq!(
            swap.route_program, router,
            "{name}: a fill under it is not attributed to it"
        );
        assert_ne!(swap.route_ordinal, 0);
        // The venue keeps the volume. A router is never a venue.
        assert_eq!(swap.protocol, Venue::MeteoraDammV2.as_str());
        seen += 1;
    }
    assert!(seen >= 10, "the router list did not grow");
}

/// The counter-example, on real bytes: the recorded pump.fun sell sits
/// under `MAyhSmzX...`, which the research listed next to the routers.
/// It is pump.fun's OWN "Mayhem Mode" program - a launchpad program - and
/// registering it would have attributed this curve trade to an aggregator
/// that does not exist.
#[test]
fn the_pumpfun_mayhem_wrapper_is_not_a_route() {
    let fixture = fixtures::get("pumpfun_sell");
    let mayhem = crate::svm::programs::pubkey(
        "MAyhSmzXzV1pTf7LsNkrNwkWKTo4ougAJ1PPg47MD4e",
    );
    assert!(
        fixture
            .transaction
            .instructions
            .iter()
            .any(|instruction| instruction.program == mayhem
                && instruction.path.len() == 1),
        "the recording no longer has the Mayhem wrapper"
    );

    let swap = &decode("pumpfun_sell").swaps[0];
    assert_eq!(swap.route_program, crate::svm::models::ZERO_PUBKEY);
    assert_eq!(swap.route_ordinal, 0);
}

/// B3, the other half: when the pool cannot be named at all the row is
/// stored with NO pool key and stays out of the pool-keyed aggregates,
/// rather than being keyed on the vault authority.
///
/// Meteora DAMM v2 is the venue with no verified pool account index, so a
/// DAMM v2 fill whose event did not turn up is exactly that case.
#[test]
fn a_pool_that_cannot_be_named_is_left_out_of_the_pool_keyed_series() {
    let registry = Registry::with_venues(&Venue::ALL);
    let router = crate::svm::programs::pubkey(
        crate::svm::programs::ROUTERS_B58[0].1,
    );
    let tx = build::routed_pumpswap_like(router);
    let outcome = decode_transaction_with(CHAIN, 1, &tx, &registry);

    let swap = &outcome.swaps[0];
    assert_eq!(swap.protocol, Venue::MeteoraDammV2.as_str());
    // The trade is still counted - the amounts, mints and price are exact.
    assert_eq!(swap.amount_in, U256::from(1_000u64));
    // It simply has no pool key, which is what the candle views filter on.
    assert_eq!(swap.pool_id, crate::svm::models::ZERO_PUBKEY);
    assert_eq!(swap.confidence, "movement");
    assert_eq!(outcome.diagnostics.unnamed_pool, 1);
}

/// M5. Orca's `two_hop_swap` and Raydium CLMM's `swap_router_base_in`
/// execute TWO fills from one instruction, on two different pools. The
/// decoder produced a single row for them - the later hops were proposed
/// as competing readings of one trade and then dropped - and even if two
/// rows had been built they would have collided on the position key,
/// because the ordinal is the instruction path and both hops share it.
#[test]
fn a_two_hop_instruction_becomes_one_row_per_hop() {
    let registry = Registry::with_venues(&Venue::ALL);
    let tx = build::orca_two_hop();
    let outcome = decode_transaction_with(CHAIN, 1, &tx, &registry);

    assert_eq!(
        outcome.swaps.len(),
        2,
        "a two-hop swap must be two fills, got {:?}",
        outcome
            .swaps
            .iter()
            .map(|swap| (swap.amount_in, swap.amount_out))
            .collect::<Vec<_>>()
    );
    assert_eq!(outcome.diagnostics.extra_hops, 1);

    let (first, second) = (&outcome.swaps[0], &outcome.swaps[1]);
    // Two different pools, and the hops chain: the first hop's output mint
    // is the second's input.
    assert_ne!(first.pool_id, second.pool_id);
    assert_eq!(first.token_out, second.token_in);
    // Each hop keeps its OWN amounts. Before, one hop's legs landed in the
    // other's fee column.
    assert_eq!(first.amount_in, U256::from(1_000u64));
    assert_eq!(first.amount_out_gross, U256::from(900u64));
    assert_eq!(second.amount_in, U256::from(900u64));
    assert_eq!(second.amount_out_gross, U256::from(800u64));
    assert_eq!(first.fee_amount, U256::ZERO);
    assert_eq!(second.fee_amount, U256::ZERO);

    // Same instruction, distinct positions: the hop sub-index is the four
    // bits the packed path leaves free at the bottom, so the two rows
    // cannot replace each other in a ReplacingMergeTree and they still
    // sort in execution order.
    assert_eq!(unpack_ordinal(first.ordinal), vec![0]);
    assert_eq!(unpack_ordinal(second.ordinal), vec![0]);
    assert_eq!(crate::svm::models::unpack_hop(first.ordinal), 0);
    assert_eq!(crate::svm::models::unpack_hop(second.ordinal), 1);
    assert!(first.ordinal < second.ordinal);
}

/// Review F, NEW-4. M5 named two venues that run two fills from one
/// instruction: Orca's `two_hop_swap` and Raydium CLMM's
/// `swap_router_base_in`. The ROW SPLITTING covered both; the ENRICHMENT
/// covered Orca only - `enrich_raydium_clmm` still read
/// `data_log_of(.., 0)`, so hop 1 was validated against hop 0's
/// `SwapEvent`.
///
/// Usually that just fails the agreement check and costs the row its
/// `decoded` confidence. When the two hops move equal amounts - which is
/// what this fixture does - the check PASSES and hop 0's `pool_state` is
/// written onto hop 1: a wrong pool key on a real row, feeding the candle
/// series of a pool that trade never touched.
#[test]
fn each_hop_of_a_clmm_router_swap_is_enriched_from_its_own_event() {
    let registry = Registry::with_venues(&Venue::ALL);
    let tx = build::raydium_clmm_router_two_hop();
    let outcome = decode_transaction_with(CHAIN, 1, &tx, &registry);

    assert_eq!(
        outcome.swaps.len(),
        2,
        "a router swap over two pools must be two fills"
    );

    let (first, second) = (&outcome.swaps[0], &outcome.swaps[1]);

    // Both hops were enriched - the events agree with the movements - and
    // each carries the pool ITS OWN event names.
    assert_eq!(first.confidence, "decoded", "{first:?}");
    assert_eq!(second.confidence, "decoded", "{second:?}");
    assert_ne!(
        first.pool_id, second.pool_id,
        "hop 1 was enriched from hop 0's event: both rows now name the \
         same pool"
    );
    assert_eq!(first.pool_id, build::off_curve(0xc1));
    assert_eq!(second.pool_id, build::off_curve(0xd1));
}

/// M8. Every transfer in the subtree that was not a leg went into
/// `fee_amount`, whatever its mint - and a System transfer of LAMPORTS
/// becomes a WSOL movement, so the column could hold lamports added to
/// token base units. There is one number and one mint column now, and only
/// fees of a LEG's mint are counted.
#[test]
fn a_fee_of_another_mint_is_not_added_to_the_legs_fee() {
    let registry = Registry::with_venues(&Venue::ALL);
    let tx = build::swap_with_two_fee_mints();
    let outcome = decode_transaction_with(CHAIN, 1, &tx, &registry);

    let swap = &outcome.swaps[0];
    // 50 of the OUTPUT mint is a real fee of this trade.
    assert_eq!(swap.fee_amount, U256::from(50u64));
    assert_eq!(swap.fee_mint, swap.token_out);
    // The 7,000,000 lamports that also left the taker in this subtree are
    // not in it: they are a different unit entirely.
    assert_ne!(swap.fee_amount, U256::from(7_000_050u64));
}

/// Transactions BUILT for the shapes no recording of which could be found.
///
/// The addendum itself could not produce an Orca `two_hop_swap` (0 in 118
/// sampled transactions) and none of the recordings carries a Meteora DAMM
/// v2 swap or a cross-mint fee, so those three shapes are constructed here
/// from the report's description. Everything about them that the decoder
/// reads - transfer instructions, owners, mints - is exactly what HyperSync
/// serves for a real one.
mod build {
    use crate::svm::{
        decode::{SvmAccountActivity, SvmInstruction, SvmTransaction},
        models::Pubkey,
        pda::is_on_curve,
        programs::{
            anchor_discriminator, pubkey, Venue, IX_TRANSFER,
            SPL_TOKEN_B58,
        },
    };

    /// Standard base64 (RFC 4648), the encoder for the decoder
    /// `SvmLog::event_bytes` already has: a log line carries its event
    /// body base64 encoded, so a constructed event has to be encoded the
    /// same way a validator encodes a real one.
    fn base64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                      abcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

        for chunk in bytes.chunks(3) {
            let triple = u32::from(chunk[0]) << 16
                | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
                | u32::from(chunk.get(2).copied().unwrap_or(0));
            let at = |shift: u32| {
                ALPHABET[(triple >> shift) as usize & 63] as char
            };

            out.push(at(18));
            out.push(at(12));
            out.push(if chunk.len() > 1 { at(6) } else { '=' });
            out.push(if chunk.len() > 2 { at(0) } else { '=' });
        }

        out
    }

    /// A key no private key can exist for, like every pool authority.
    pub fn off_curve(seed: u8) -> Pubkey {
        let mut key = [seed; 32];
        while is_on_curve(&key) {
            key[31] = key[31].wrapping_add(1);
        }
        key
    }

    /// A plain wallet: a real ed25519 public key, i.e. ON the curve. That
    /// is what tells a taker from a pool, so a test wallet that happened to
    /// be off the curve would make every trade ambiguous.
    pub fn wallet(seed: u8) -> Pubkey {
        let mut key = [seed; 32];
        while !is_on_curve(&key) {
            key[31] = key[31].wrapping_add(1);
        }
        key
    }

    pub struct Builder {
        pub tx: SvmTransaction,
        next: u8,
    }

    impl Builder {
        pub fn new() -> Self {
            Self {
                tx: SvmTransaction {
                    success: true,
                    fee_payer: wallet(0xfe),
                    ..Default::default()
                },
                next: 0x10,
            }
        }

        pub fn program(
            &mut self,
            path: &[u32],
            program: Pubkey,
            data: Vec<u8>,
            accounts: Vec<Pubkey>,
        ) {
            self.tx.instructions.push(SvmInstruction {
                path: path.to_vec(),
                program,
                accounts,
                data,
            });
        }

        pub fn venue(
            &mut self,
            path: &[u32],
            venue: Venue,
            instruction: &str,
            accounts: Vec<Pubkey>,
        ) {
            self.program(
                path,
                pubkey(venue.program_b58()),
                anchor_discriminator("global", instruction).to_vec(),
                accounts,
            );
        }

        /// One SPL transfer, with the two token accounts and their owners
        /// in `account_activity` exactly as the source serves them.
        pub fn transfer(
            &mut self,
            path: &[u32],
            mint: Pubkey,
            amount: u64,
            from: Pubkey,
            to: Pubkey,
        ) {
            let source = self.token_account(from, mint, amount);
            let destination = self.token_account(to, mint, 0);
            let mut data = vec![IX_TRANSFER];
            data.extend_from_slice(&amount.to_le_bytes());
            self.program(
                path,
                pubkey(SPL_TOKEN_B58),
                data,
                vec![source, destination, from],
            );
        }

        /// One `Program data:` line, i.e. an Anchor `emit!`, attributed to
        /// the instruction at `path` exactly as HyperSync attributes it.
        pub fn data_log(
            &mut self,
            path: &[u32],
            program: Pubkey,
            event: &[u8],
        ) {
            self.tx.logs.push(crate::svm::decode::SvmLog {
                path: path.to_vec(),
                program,
                is_data: true,
                message: base64(event),
            });
        }

        /// A System transfer of lamports. The movement layer records it as
        /// a WSOL movement, which is right - and is also how a fee paid in
        /// lamports used to be added to a fee paid in token base units.
        pub fn system_transfer(
            &mut self,
            path: &[u32],
            lamports: u64,
            from: Pubkey,
            to: Pubkey,
        ) {
            let mut data =
                crate::svm::programs::IX_SYSTEM_TRANSFER.to_vec();
            data.extend_from_slice(&lamports.to_le_bytes());
            self.program(
                path,
                pubkey(crate::svm::programs::SYSTEM_B58),
                data,
                vec![from, to],
            );
        }

        fn token_account(
            &mut self,
            owner: Pubkey,
            mint: Pubkey,
            balance: u64,
        ) -> Pubkey {
            let mut account = [0u8; 32];
            account[0] = self.next;
            account[1..].copy_from_slice(&owner[1..]);
            account[31] = mint[0];
            self.next = self.next.wrapping_add(1);
            self.tx.activity.push(SvmAccountActivity {
                account,
                mint: Some(mint),
                pre_owner: Some(owner),
                post_owner: Some(owner),
                decimals: Some(6),
                pre_token_balance: Some(balance),
                post_token_balance: Some(balance),
                ..Default::default()
            });
            account
        }
    }

    /// A DAMM v2 swap under `router`: one venue instruction, two legs
    /// across one pool authority.
    pub fn routed_pumpswap_like(router: Pubkey) -> SvmTransaction {
        let mut build = Builder::new();
        let authority = off_curve(0x21);
        let taker = wallet(0x31);
        let (mint_a, mint_b) = ([0xa1u8; 32], [0xb1u8; 32]);

        build.program(&[0], router, vec![0x01], Vec::new());
        build.venue(&[0, 0], Venue::MeteoraDammV2, "swap", Vec::new());
        build.transfer(&[0, 0, 0], mint_a, 1_000, taker, authority);
        build.transfer(&[0, 0, 1], mint_b, 900, authority, taker);
        build.tx
    }

    /// Orca's `two_hop_swap`: ONE instruction, two pools, four transfers.
    /// The taker pays mint A to pool 1, is paid mint B, pays that mint B to
    /// pool 2 and is paid mint C - which is why the two pools are disjoint
    /// counterparties and the taker is a counterparty of all four legs.
    pub fn orca_two_hop() -> SvmTransaction {
        let mut build = Builder::new();
        let pool_one = off_curve(0x41);
        let pool_two = off_curve(0x51);
        let taker = wallet(0x61);
        let (mint_a, mint_b, mint_c) =
            ([0xa2u8; 32], [0xb2u8; 32], [0xc2u8; 32]);

        build.venue(
            &[0],
            Venue::OrcaWhirlpool,
            "two_hop_swap",
            vec![pool_one, pool_two],
        );
        build.transfer(&[0, 0], mint_a, 1_000, taker, pool_one);
        build.transfer(&[0, 1], mint_b, 900, pool_one, taker);
        build.transfer(&[0, 2], mint_b, 900, taker, pool_two);
        build.transfer(&[0, 3], mint_c, 800, pool_two, taker);
        build.tx
    }

    /// Raydium CLMM's `swap_router_base_in`: ONE instruction, two pools,
    /// four transfers and TWO `SwapEvent` log lines - the CLMM twin of
    /// [`orca_two_hop`], which is the shape M5 named and only half fixed.
    ///
    /// The two hops move the SAME amounts on purpose. That is the case in
    /// which reading hop 0's event for hop 1 does not fail the agreement
    /// check and writes hop 0's `pool_state` onto hop 1 instead: a wrong
    /// pool key on a real row, with `decoded` confidence (review F,
    /// NEW-4).
    pub fn raydium_clmm_router_two_hop() -> SvmTransaction {
        let mut build = Builder::new();
        let pool_one = off_curve(0xc1);
        let pool_two = off_curve(0xd1);
        let taker = wallet(0xe1);
        let (mint_a, mint_b, mint_c) =
            ([0xa4u8; 32], [0xb4u8; 32], [0xc4u8; 32]);

        build.venue(
            &[0],
            Venue::RaydiumClmm,
            "swap_router_base_in",
            vec![pool_one, pool_two],
        );
        build.transfer(&[0, 0], mint_a, 1_000, taker, pool_one);
        build.transfer(&[0, 1], mint_b, 1_000, pool_one, taker);
        build.transfer(&[0, 2], mint_b, 1_000, taker, pool_two);
        build.transfer(&[0, 3], mint_c, 1_000, pool_two, taker);

        // One `SwapEvent` per hop, in emission order, both attributed to
        // the router instruction - which is exactly what makes `nth`
        // necessary.
        for pool in [pool_one, pool_two] {
            build.data_log(
                &[0],
                pubkey(Venue::RaydiumClmm.program_b58()),
                &clmm_swap_event(pool, taker, 1_000, 1_000),
            );
        }

        build.tx
    }

    /// A Raydium CLMM `SwapEvent` body: the 8 byte discriminator and the
    /// 213 bytes `RaydiumClmmSwap::parse` reads, at the offsets it reads
    /// them at.
    fn clmm_swap_event(
        pool_state: Pubkey,
        sender: Pubkey,
        amount_0: u64,
        amount_1: u64,
    ) -> Vec<u8> {
        let mut body = vec![0u8; 8 + 213];
        body[..8].copy_from_slice(
            &crate::svm::programs::DISC_RAYDIUM_SWAP_EVENT,
        );
        body[8..40].copy_from_slice(&pool_state);
        body[40..72].copy_from_slice(&sender);
        // 72..136 are the two vault token accounts, which the movement
        // layer has already found for itself.
        body[136..144].copy_from_slice(&amount_0.to_le_bytes());
        body[152..160].copy_from_slice(&amount_1.to_le_bytes());
        // `zero_for_one`: token 0 went into the pool.
        body[168] = 1;
        body
    }

    /// A swap that pays two fees in two different units: 50 of the output
    /// mint, and 7,000,000 LAMPORTS through a System transfer - which the
    /// movement layer records as a WSOL movement, WSOL being neither leg.
    pub fn swap_with_two_fee_mints() -> SvmTransaction {
        let mut build = Builder::new();
        let authority = off_curve(0x71);
        let taker = wallet(0x81);
        let treasury = wallet(0x91);
        let (mint_a, mint_b) = ([0xa3u8; 32], [0xb3u8; 32]);

        build.venue(&[0], Venue::MeteoraDammV2, "swap", Vec::new());
        build.transfer(&[0, 0], mint_a, 1_000, taker, authority);
        build.transfer(&[0, 1], mint_b, 900, authority, taker);
        build.transfer(&[0, 2], mint_b, 50, taker, treasury);
        build.system_transfer(&[0, 3], 7_000_000, taker, treasury);
        build.tx
    }
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
