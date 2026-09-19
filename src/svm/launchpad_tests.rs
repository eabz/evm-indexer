//! Launchpad decoder tests against the RECORDED LIVE transactions in
//! `fixtures/launchpads.json`.
//!
//! Same discipline as `tests.rs`: every expected value is either the
//! transaction's own data or something re-derived INDEPENDENTLY of the
//! decoder - a bonding curve recomputed from its mint, a pool recomputed
//! from its pair - so these tests can fail the decoder rather than merely
//! describe it.

use alloy::primitives::U256;

use crate::svm::{
    decode::decode_transaction,
    fixtures,
    launchpads::{
        balance_version, config_as_u256, u256_as_config, SolLaunchpadRows,
    },
    models::{SOLANA_CHAIN, ZERO_PUBKEY},
    pda::{find_program_address, is_on_curve},
    programs::{pubkey, registry, Venue},
};

const CHAIN: u64 = SOLANA_CHAIN;

/// Decodes one recorded transaction's launchpad rows, corroborated by the
/// swap rows the movement layer produced for that same transaction.
fn launchpads(name: &str) -> SolLaunchpadRows {
    let fixture = fixtures::get(name);
    let swaps = decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
    )
    .swaps;
    crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &swaps,
    )
}

/// The bonding curve pump.fun derives for a mint. Recomputed here so a
/// test never checks the decoder against itself.
fn pumpfun_curve(mint: &crate::svm::models::Pubkey) -> [u8; 32] {
    find_program_address(
        &[b"bonding-curve", mint],
        &pubkey(Venue::PumpFun.program_b58()),
    )
    .expect("a bump exists")
    .0
}

/// A pump.fun LAUNCH, off the chain.
///
/// Three Borsh `String`s sit at the FRONT of `CreateEvent`, so nothing in
/// it is at a fixed offset and this proves the walk. And the curve the
/// event names must be the program's own PDA of the mint - the check that
/// turns a launch from a claim into a proof, which EVM has no equivalent
/// of.
#[test]
fn a_pumpfun_launch_names_a_curve_that_derives_from_its_own_mint() {
    let rows = launchpads("pumpfun_create");

    assert_eq!(rows.tokens.len(), 1, "one launch");
    assert_eq!(rows.diagnostics.curve_not_derived, 0);
    assert_eq!(rows.diagnostics.bad_length, 0);

    let launch = &rows.tokens[0];
    assert_eq!(launch.family, "pumpfun");
    assert_eq!(
        launch.emitter,
        pubkey(Venue::PumpFun.program_b58()),
        "the emitter is the PROGRAM, which cannot be forged"
    );
    assert_ne!(launch.token, ZERO_PUBKEY);
    assert_ne!(launch.creator, ZERO_PUBKEY);
    assert!(!launch.symbol.is_empty(), "a launch names its symbol");
    assert!(launch.initial_supply > U256::ZERO);
    assert_eq!(launch.curve, pumpfun_curve(&launch.token));

    // The launch's own position, not an invented one.
    assert_eq!(launch.block_number, fixtures::get("pumpfun_create").slot);
    assert_eq!(launch.tx_id.len(), 64, "a Solana signature is 64 bytes");
}

/// A sniper's bundle: several curve trades in ONE transaction.
///
/// Each stays its own row with its own ordinal. This is the launchpad-table
/// form of the netting failure `two_opposite_swaps` pins for the swap
/// table.
#[test]
fn a_multi_trade_bundle_stays_one_row_per_trade() {
    let rows = launchpads("pumpfun_multi_buy");

    assert!(
        rows.trades.len() > 1,
        "the fixture was recorded because it holds several trades"
    );
    let mut ordinals: Vec<u64> =
        rows.trades.iter().map(|row| row.ordinal).collect();
    let before = ordinals.len();
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(before, ordinals.len(), "two trades share an ordinal");

    for trade in &rows.trades {
        assert_eq!(trade.family, "pumpfun");
        assert!(trade.token_amount > U256::ZERO);
        assert!(["buy", "sell"].contains(&trade.side.as_str()));
        assert_ne!(trade.token, ZERO_PUBKEY);
        // The emitter is the CURVE, so a trade joins its launch row and
        // passes the trusted-curve filter exactly as it does on EVM.
        assert_eq!(trade.emitter, pumpfun_curve(&trade.token));
    }
}

/// THE row the whole module exists for: a graduation naming the PumpSwap
/// pool the token carries on trading in.
#[test]
fn a_graduation_names_the_destination_pool() {
    let rows = launchpads("pumpfun_graduation");

    assert_eq!(rows.graduations.len(), 1);
    let graduation = &rows.graduations[0];

    assert_eq!(graduation.family, "pumpfun");
    assert_ne!(graduation.token, ZERO_PUBKEY);
    assert_ne!(
        graduation.pool_id, ZERO_PUBKEY,
        "without a destination pool a graduation cannot be joined to the \
         DEX candles, which is the only reason the row exists"
    );
    // A pubkey is a NATIVE 32 byte id: a reader prints all 32 bytes and
    // must never strip a 12 byte pad off one.
    assert_eq!(graduation.pool_kind, "pool_id");
    assert!(graduation.token_amount > U256::ZERO);
    assert_eq!(graduation.emitter, pumpfun_curve(&graduation.token));
}

/// A curve quoted in something other than SOL.
///
/// `sol_amount` is 0 on these and the real leg is in the `TradeEvent`
/// TAIL, behind a Borsh `String` and a `Vec`. Reading only the fixed
/// prefix made every one of them contradict the movement layer - measured
/// live at 4.3% to 11.5% of all pump.fun curve instructions.
#[test]
fn a_non_sol_quote_curve_trade_is_decoded_from_the_event_tail() {
    let rows = launchpads("pumpfun_quote_curve");

    let quoted: Vec<_> = rows
        .trades
        .iter()
        .filter(|trade| trade.quote_token != registry().wsol)
        .collect();
    assert!(
        !quoted.is_empty(),
        "the fixture was recorded because it holds a non-SOL quote curve"
    );

    for trade in quoted {
        assert!(
            trade.quote_amount > U256::ZERO,
            "the quote leg came back zero, i.e. the tail was not read"
        );
        assert_ne!(trade.token, trade.quote_token);
        assert_eq!(
            trade.token_verified, 1,
            "the movement layer must corroborate the token leg"
        );
    }

    // And the swap table agrees: the venue's own event confirmed it.
    let fixture = fixtures::get("pumpfun_quote_curve");
    let swaps = decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
    )
    .swaps;
    let curve: Vec<_> = swaps
        .iter()
        .filter(|swap| swap.protocol == Venue::PumpFun.as_str())
        .collect();
    assert!(!curve.is_empty());
    assert!(
        curve.iter().all(|swap| swap.confidence == "decoded"),
        "a quote-curve trade the event did not confirm"
    );
}

/// A Meteora DBC launch names the partner CONFIG, which is the only thing
/// on chain that identifies bags.fm and the other front ends.
#[test]
fn a_dbc_launch_carries_the_config_that_attributes_its_front_end() {
    let rows = launchpads("dbc_launch");
    assert_eq!(rows.tokens.len(), 1);
    assert_eq!(rows.diagnostics.bad_length, 0);

    let launch = &rows.tokens[0];
    assert_eq!(launch.family, "meteora_dbc");
    assert_eq!(launch.emitter, pubkey(Venue::MeteoraDbc.program_b58()));
    assert_ne!(launch.token, ZERO_PUBKEY);
    assert_ne!(launch.curve, ZERO_PUBKEY);

    // The config survives the numeric column it shares with EVM.
    assert_ne!(launch.launch_config_id, U256::ZERO);
    let config = u256_as_config(launch.launch_config_id);
    assert_eq!(config_as_u256(&config), launch.launch_config_id);

    // And it is ON the ed25519 curve, which is worth pinning because it
    // contradicts the obvious guess. DBC's `create_config` takes the
    // config as a SIGNER KEYPAIR rather than deriving it, so a partner
    // holds its private key. Nothing may therefore use the off-curve test
    // to decide whether an account is "one of the venue's": that rule
    // holds for pools and curves, not for configs.
    assert!(
        is_on_curve(&config),
        "a DBC config is a signer keypair, not a PDA"
    );
}

/// A Raydium LaunchLab launch. Its event names NEITHER mint, so both come
/// from account metas - and the pool's own PDA seeds are what prove the
/// indices were read right rather than merely assumed.
#[test]
fn a_launchlab_launch_proves_its_mints_against_the_pool_pda() {
    let rows = launchpads("launchlab_launch");
    assert_eq!(rows.tokens.len(), 1);
    assert_eq!(
        rows.diagnostics.curve_not_derived, 0,
        "the mints did not reproduce the pool the instruction used"
    );

    let launch = &rows.tokens[0];
    assert_eq!(launch.family, "raydium_launchlab");
    assert_ne!(launch.token, ZERO_PUBKEY);
    assert_ne!(launch.quote_token, ZERO_PUBKEY);
    assert_ne!(launch.token, launch.quote_token);
    assert!(!launch.symbol.is_empty());
    // LaunchLab is the one family that states its graduation target.
    assert!(launch.graduation_threshold > U256::ZERO);

    let (derived, _) = find_program_address(
        &[b"pool", &launch.token, &launch.quote_token],
        &pubkey(Venue::RaydiumLaunchlab.program_b58()),
    )
    .expect("a bump exists");
    assert_eq!(launch.curve, derived);

    // The PLATFORM config is what says which front end - StonkFun,
    // BONK.fun - hosted this launch. The event's own `config` field is the
    // global one and is the same for every launch, so using it would
    // attribute the whole venue to a single front end.
    assert_ne!(launch.launch_config_id, U256::ZERO);
}

/// Holder balances are written for LAUNCHPAD tokens only, and read from
/// the post balances validator metadata already carries.
#[test]
fn holder_balances_are_recorded_for_launchpad_tokens_only() {
    let rows = launchpads("pumpfun_multi_buy");

    let mints: std::collections::HashSet<_> = rows
        .trades
        .iter()
        .map(|trade| trade.token)
        .chain(rows.tokens.iter().map(|token| token.token))
        .collect();
    assert!(!rows.balances.is_empty());
    for balance in &rows.balances {
        assert!(
            mints.contains(&balance.mint),
            "a balance was written for a mint no launchpad row named"
        );
        assert_ne!(balance.owner, ZERO_PUBKEY);
        // The version is the POSITION, so replaying an older range can
        // never move a balance backwards.
        assert_eq!(
            balance._version,
            balance_version(balance.block_number, balance.tx_index)
        );
    }
}

/// Every recorded launchpad transaction decodes without tripping either
/// diagnostic. Those two counters are the module's tripwires - "a layout
/// changed" and "an account index moved" - and a fixture is exactly where
/// they should be pinned.
#[test]
fn no_recorded_launchpad_transaction_trips_a_diagnostic() {
    for name in [
        "pumpfun_create",
        "pumpfun_multi_buy",
        "pumpfun_graduation",
        "pumpfun_quote_curve",
        "dbc_launch",
        "launchlab_launch",
    ] {
        let rows = launchpads(name);
        assert_eq!(rows.diagnostics.bad_length, 0, "{name}");
        assert_eq!(rows.diagnostics.curve_not_derived, 0, "{name}");
        assert!(!rows.is_empty(), "{name} produced no rows at all");
    }
}

// --- review round 4, the addendum's MINORs -------------------------------

/// A leg counts as verified only when the movement layer proves that mint
/// moved on THAT SIDE.
///
/// The check used to test membership alone - "is this mint one of the two
/// the swap row proved?" - which marks an INVERTED trade, one whose side
/// the event and the transfers disagree about, as verified on both legs.
/// That is exactly the disagreement the corroboration exists to catch, so
/// the check was blind to the only thing it could see.
#[test]
fn an_inverted_swap_row_does_not_verify_a_trades_legs() {
    let fixture = fixtures::get("pumpfun_sell");
    let swaps = decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
    )
    .swaps;
    assert_eq!(swaps.len(), 1);

    // As it really is: both legs proven, on the right sides.
    let honest = crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &swaps,
    );
    let trade = &honest.trades[0];
    assert_eq!(trade.side, "sell");
    assert_eq!(trade.token_verified, 1);
    assert_eq!(trade.quote_verified, 1);

    // The same trade against a swap row whose two mints are the wrong way
    // round. Nothing may be verified against that.
    let mut inverted = swaps.clone();
    let row = &mut inverted[0];
    std::mem::swap(&mut row.verified_in, &mut row.verified_out);
    let rows = crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &inverted,
    );
    let trade = &rows.trades[0];
    assert_eq!(
        (trade.token_verified, trade.quote_verified),
        (0, 0),
        "an inverted swap row must verify neither leg"
    );
    assert_eq!(rows.diagnostics.trade_unverified, 1);
}

/// `sole_unverified_quote` is the flag that says `tx_value` may bound the
/// quote leg. It was hard-coded to 1 on every Solana trade - including the
/// trades whose quote leg is NOT verified, where `tx_value` is 0 and the
/// bound it implies is "the buyer paid at most nothing".
#[test]
fn sole_unverified_quote_is_computed_and_not_asserted() {
    // A trade the movement layer corroborated: the quote leg IS verified,
    // so the flag is 0 and no bound is claimed.
    let fixture = fixtures::get("pumpfun_sell");
    let rows = launchpads("pumpfun_sell");
    assert_eq!(rows.trades[0].quote_verified, 1);
    assert_eq!(rows.trades[0].sole_unverified_quote, 0);

    // The same trade with no swap row to corroborate it: one unverified
    // quote leg in the transaction, so the flag is 1.
    let alone = crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &fixture.transaction,
        &[],
    );
    assert_eq!(alone.trades.len(), 1);
    assert_eq!(alone.trades[0].quote_verified, 0);
    assert_eq!(alone.trades[0].sole_unverified_quote, 1);

    // And the sniper's bundle - SEVERAL curve trades in one transaction,
    // none corroborated - is the counter-example the flag exists for: one
    // transaction value cannot bound any single one of them.
    let bundle = fixtures::get("pumpfun_multi_buy");
    let many = crate::svm::launchpads::decode_transaction(
        CHAIN,
        bundle.timestamp(),
        &bundle.transaction,
        &[],
    );
    assert!(
        many.trades.len() > 1,
        "the bundle fixture holds one trade, so it proves nothing"
    );
    for trade in &many.trades {
        assert_eq!(trade.quote_verified, 0);
        assert_eq!(trade.sole_unverified_quote, 0);
    }
}

/// `launchpad_trades` and `sol_dex_swaps` must report the same amounts for
/// the same trade. pump.fun's row took its amounts from the EVENT while
/// the other two families take theirs from the swap row, so on a
/// Token-2022 curve - where the event states what the curve was CREDITED
/// and the transfer states what the trader SENT - the two tables disagreed
/// about one trade.
#[test]
fn a_curve_trade_reports_the_same_amounts_as_its_swap_row() {
    for name in ["pumpfun_sell", "pumpfun_multi_buy", "launchlab_sell"] {
        let fixture = fixtures::get(name);
        let swaps = decode_transaction(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
        )
        .swaps;
        let rows = crate::svm::launchpads::decode_transaction(
            CHAIN,
            fixture.timestamp(),
            &fixture.transaction,
            &swaps,
        );

        for trade in &rows.trades {
            let Some(swap) =
                swaps.iter().find(|swap| swap.ordinal == trade.ordinal)
            else {
                continue;
            };
            let (token_amount, quote_amount) = if trade.side == "buy" {
                (swap.amount_out_gross, swap.amount_in)
            } else {
                (swap.amount_in, swap.amount_out_gross)
            };
            assert_eq!(
                trade.token_amount, token_amount,
                "{name}: the launchpad row and the swap row disagree \
                 about the token leg"
            );
            assert_eq!(
                trade.quote_amount, quote_amount,
                "{name}: ... about the quote leg"
            );
        }
    }
}

/// A failed transaction produces nothing at all: its state changes were
/// rolled back, so a launch or a trade inside it never happened.
#[test]
fn a_failed_transaction_produces_no_launchpad_rows() {
    let fixture = fixtures::get("pumpfun_create");
    let mut tx = fixture.transaction.clone();
    tx.success = false;

    let rows = crate::svm::launchpads::decode_transaction(
        CHAIN,
        fixture.timestamp(),
        &tx,
        &[],
    );
    assert!(rows.is_empty());
}
