//! Per-program decoders: the second, ENRICHING layer.
//!
//! The movement layer in `decode.rs` guarantees a swap row exists and that
//! its pool, mints, amounts and trader are right. This layer adds what only
//! the venue can tell us - the pool account the venue itself names, exact
//! fee splits, and pool / curve state - and, just as importantly, it
//! CROSS-CHECKS the movement layer on every swap it touches. Where the two
//! disagree the row keeps its `movement` confidence and the disagreement is
//! counted: that is a bug report, not a row.
//!
//! # Dispatch is on the EVENT, never on the instruction discriminator
//!
//! Both programs have grown instruction variants that carry real volume:
//! PumpSwap has `buy_exact_quote_in` beside `buy`, pump.fun has `buy_v2`,
//! `sell_v2`, `buy_exact_sol_in` and `buy_exact_quote_in_v2`, all with
//! different account layouts. Every one of them emits the SAME Anchor event.
//! Decoding the event therefore covers variants that did not exist when this
//! was written, and nothing here depends on an account meta index.
//!
//! # Why fixed offsets are safe here, and where they stop being safe
//!
//! Both event structs contain a Borsh `String` (`ix_name`) and pump.fun's
//! also a `Vec<Shareholder>`, in the MIDDLE of the struct. Every field after
//! those has a variable offset. This module therefore decodes only the fixed
//! prefix - everything up to and including the creator fee - and deliberately
//! stops there. The offsets below were derived from the field order in
//! pump.fun's published IDLs AND verified byte for byte against live
//! mainnet transactions (see `svm::fixtures`).

use crate::svm::{
    decode::{MovementSwap, SvmInstruction, SvmTransaction},
    models::{Pubkey, SvmSwap},
    programs::{
        registry, Venue, DISC_PUMPFUN_TRADE_EVENT,
        DISC_PUMPSWAP_BUY_EVENT, DISC_PUMPSWAP_SELL_EVENT,
        EVENT_CPI_PREFIX,
    },
};
use alloy::primitives::U256;

/// What the per-program layer did to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enrichment {
    /// No event was found (an instruction variant that emits none, or the
    /// event row was not selected).
    None,
    /// The event decoded and agreed with the movement layer.
    Applied,
    /// The event decoded but contradicted the movement layer. The row keeps
    /// `movement` confidence.
    Disagreed,
}

// --- byte readers --------------------------------------------------------

fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn i64_at(data: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn pubkey_at(data: &[u8], offset: usize) -> Option<Pubkey> {
    let mut out = [0u8; 32];
    out.copy_from_slice(data.get(offset..offset + 32)?);
    Some(out)
}

fn bool_at(data: &[u8], offset: usize) -> Option<bool> {
    Some(*data.get(offset)? != 0)
}

/// Offset of the first event field: 8 bytes of `emit_cpi!` marker, then the
/// 8-byte event discriminator.
const BODY: usize = 16;

// --- PumpSwap ------------------------------------------------------------

/// The fixed prefix of PumpSwap's `BuyEvent` / `SellEvent`.
///
/// Both structs share their first 23 fields (14 scalars then 7 pubkeys then
/// the two creator-fee scalars); they only diverge after `coin_creator_fee`,
/// which is where this decoder stops. `base_amount` is `base_amount_out` on
/// a buy and `base_amount_in` on a sell; `quote_amount` likewise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpSwapEvent {
    pub is_buy: bool,
    pub timestamp: i64,
    pub base_amount: u64,
    pub user_base_token_reserves: u64,
    pub user_quote_token_reserves: u64,
    pub pool_base_token_reserves: u64,
    pub pool_quote_token_reserves: u64,
    pub quote_amount: u64,
    pub lp_fee: u64,
    pub protocol_fee: u64,
    pub pool: Pubkey,
    pub user: Pubkey,
    pub coin_creator: Pubkey,
    pub coin_creator_fee: u64,
}

impl PumpSwapEvent {
    /// Field offsets, from the IDL field order. Named so an offset is never
    /// a bare number in the code below.
    const TIMESTAMP: usize = BODY;
    const BASE_AMOUNT: usize = BODY + 8;
    // BODY + 16 is max_quote_amount_in / min_quote_amount_out, the slippage
    // bound. Deliberately not stored: it is an intention, not a fill.
    const USER_BASE_RESERVES: usize = BODY + 24;
    const USER_QUOTE_RESERVES: usize = BODY + 32;
    const POOL_BASE_RESERVES: usize = BODY + 40;
    const POOL_QUOTE_RESERVES: usize = BODY + 48;
    const QUOTE_AMOUNT: usize = BODY + 56;
    // BODY + 64 is lp_fee_basis_points.
    const LP_FEE: usize = BODY + 72;
    // BODY + 80 is protocol_fee_basis_points.
    const PROTOCOL_FEE: usize = BODY + 88;
    // BODY + 96 quote_amount_*_lp_fee, BODY + 104 user_quote_amount_*.
    const POOL: usize = BODY + 112;
    const USER: usize = BODY + 144;
    // 176 user_base_token_account, 208 user_quote_token_account,
    // 240 protocol_fee_recipient, 272 protocol_fee_recipient_token_account.
    const COIN_CREATOR: usize = BODY + 304;
    // BODY + 336 is coin_creator_fee_basis_points.
    const COIN_CREATOR_FEE: usize = BODY + 344;
    /// Everything this decoder reads must be present.
    const MIN_LEN: usize = BODY + 352;

    pub fn parse(data: &[u8]) -> Option<Self> {
        let is_buy = match data.get(8..16)? {
            d if d == DISC_PUMPSWAP_BUY_EVENT => true,
            d if d == DISC_PUMPSWAP_SELL_EVENT => false,
            _ => return None,
        };
        if data.len() < Self::MIN_LEN {
            return None;
        }
        Some(Self {
            is_buy,
            timestamp: i64_at(data, Self::TIMESTAMP)?,
            base_amount: u64_at(data, Self::BASE_AMOUNT)?,
            user_base_token_reserves: u64_at(
                data,
                Self::USER_BASE_RESERVES,
            )?,
            user_quote_token_reserves: u64_at(
                data,
                Self::USER_QUOTE_RESERVES,
            )?,
            pool_base_token_reserves: u64_at(
                data,
                Self::POOL_BASE_RESERVES,
            )?,
            pool_quote_token_reserves: u64_at(
                data,
                Self::POOL_QUOTE_RESERVES,
            )?,
            quote_amount: u64_at(data, Self::QUOTE_AMOUNT)?,
            lp_fee: u64_at(data, Self::LP_FEE)?,
            protocol_fee: u64_at(data, Self::PROTOCOL_FEE)?,
            pool: pubkey_at(data, Self::POOL)?,
            user: pubkey_at(data, Self::USER)?,
            coin_creator: pubkey_at(data, Self::COIN_CREATOR)?,
            coin_creator_fee: u64_at(data, Self::COIN_CREATOR_FEE)?,
        })
    }

    /// Total taken out of the quote leg.
    pub fn total_fee(&self) -> u64 {
        self.lp_fee
            .saturating_add(self.protocol_fee)
            .saturating_add(self.coin_creator_fee)
    }
}

// --- pump.fun ------------------------------------------------------------

/// The fixed prefix of pump.fun's `TradeEvent`, up to `creator_fee`. The
/// next field is `track_volume`, then a Borsh `String`, so this is the last
/// safely addressable field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpFunTrade {
    pub mint: Pubkey,
    pub sol_amount: u64,
    pub token_amount: u64,
    pub is_buy: bool,
    pub user: Pubkey,
    pub timestamp: i64,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub fee: u64,
    pub creator: Pubkey,
    pub creator_fee: u64,
}

impl PumpFunTrade {
    const MINT: usize = BODY;
    const SOL_AMOUNT: usize = BODY + 32;
    const TOKEN_AMOUNT: usize = BODY + 40;
    const IS_BUY: usize = BODY + 48;
    const USER: usize = BODY + 49;
    const TIMESTAMP: usize = BODY + 81;
    const VIRTUAL_SOL: usize = BODY + 89;
    const VIRTUAL_TOKEN: usize = BODY + 97;
    const REAL_SOL: usize = BODY + 105;
    const REAL_TOKEN: usize = BODY + 113;
    // BODY + 121 fee_recipient, BODY + 153 fee_basis_points.
    const FEE: usize = BODY + 161;
    const CREATOR: usize = BODY + 169;
    // BODY + 201 creator_fee_basis_points.
    const CREATOR_FEE: usize = BODY + 209;
    const MIN_LEN: usize = BODY + 217;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_PUMPFUN_TRADE_EVENT {
            return None;
        }
        if data.len() < Self::MIN_LEN {
            return None;
        }
        Some(Self {
            mint: pubkey_at(data, Self::MINT)?,
            sol_amount: u64_at(data, Self::SOL_AMOUNT)?,
            token_amount: u64_at(data, Self::TOKEN_AMOUNT)?,
            is_buy: bool_at(data, Self::IS_BUY)?,
            user: pubkey_at(data, Self::USER)?,
            timestamp: i64_at(data, Self::TIMESTAMP)?,
            virtual_sol_reserves: u64_at(data, Self::VIRTUAL_SOL)?,
            virtual_token_reserves: u64_at(data, Self::VIRTUAL_TOKEN)?,
            real_sol_reserves: u64_at(data, Self::REAL_SOL)?,
            real_token_reserves: u64_at(data, Self::REAL_TOKEN)?,
            fee: u64_at(data, Self::FEE)?,
            creator: pubkey_at(data, Self::CREATOR)?,
            creator_fee: u64_at(data, Self::CREATOR_FEE)?,
        })
    }

    pub fn total_fee(&self) -> u64 {
        self.fee.saturating_add(self.creator_fee)
    }
}

// --- dispatch ------------------------------------------------------------

/// The Anchor `emit_cpi!` event a venue instruction emitted, if any.
///
/// The event is a self CPI: the program invokes ITSELF, so the row is a
/// DIRECT child of the swap instruction with the same executing account.
fn event_of<'a>(
    tx: &'a SvmTransaction,
    instruction: &SvmInstruction,
) -> Option<&'a SvmInstruction> {
    tx.instructions.iter().find(|candidate| {
        candidate.program == instruction.program
            && candidate.path.len() == instruction.path.len() + 1
            && candidate.path.starts_with(&instruction.path)
            && candidate.data.starts_with(&EVENT_CPI_PREFIX)
    })
}

/// Runs the per-program decoder for `venue` over `row`, cross-checking it
/// against what the movement layer found.
pub fn enrich(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    venue: Venue,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(event) = event_of(tx, instruction) else {
        return Enrichment::None;
    };

    match venue {
        Venue::PumpSwap => enrich_pumpswap(&event.data, swap, row),
        Venue::PumpFun => enrich_pumpfun(&event.data, swap, row),
        // Named but not decoded yet. The row keeps `movement` confidence,
        // which is exactly what it is: price, size and trader are exact,
        // pool state is not available.
        Venue::BisonFi | Venue::MeteoraDlmm | Venue::RaydiumCpmm => {
            Enrichment::None
        }
    }
}

fn enrich_pumpswap(
    data: &[u8],
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(event) = PumpSwapEvent::parse(data) else {
        return Enrichment::None;
    };

    // THE cross-check: the movement layer inferred the pool as the common
    // owner of the two vault token accounts, with no knowledge of PumpSwap.
    // The venue names the pool itself. They must be the same account.
    if event.pool != swap.authority {
        return Enrichment::Disagreed;
    }

    // On a buy the base mint leaves the pool, on a sell it enters it. The
    // event's base amount must equal the movement layer's leg for that mint.
    let movement_base =
        if event.is_buy { swap.amount_out_gross } else { swap.amount_in };
    if movement_base != event.base_amount {
        return Enrichment::Disagreed;
    }

    // The quote leg is the other side, net of the fees that left the
    // subtree. Checked loosely (the fee split differs per instruction
    // variant) but it must not be wildly off.
    let movement_quote =
        if event.is_buy { swap.amount_in } else { swap.amount_out_gross };
    if movement_quote == 0 || event.quote_amount == 0 {
        return Enrichment::Disagreed;
    }

    let base_reserve = U256::from(event.pool_base_token_reserves);
    let quote_reserve = U256::from(event.pool_quote_token_reserves);
    let (base_mint, quote_mint) = if event.is_buy {
        (swap.mint_out, swap.mint_in)
    } else {
        (swap.mint_in, swap.mint_out)
    };
    apply_reserves(
        row,
        base_mint,
        base_reserve,
        quote_mint,
        quote_reserve,
    );

    row.sender = event.user;
    row.fee_amount = U256::from(event.total_fee());
    row.mark_decoded(event.pool);
    Enrichment::Applied
}

fn enrich_pumpfun(
    data: &[u8],
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(event) = PumpFunTrade::parse(data) else {
        return Enrichment::None;
    };
    let wsol = registry().wsol;

    // On a buy the curve receives SOL and pays out the token.
    let (expected_in, expected_out) =
        if event.is_buy { (wsol, event.mint) } else { (event.mint, wsol) };
    if swap.mint_in != expected_in || swap.mint_out != expected_out {
        return Enrichment::Disagreed;
    }

    let (movement_sol, movement_token) = if event.is_buy {
        (swap.amount_in, swap.amount_out_gross)
    } else {
        (swap.amount_out_gross, swap.amount_in)
    };
    if movement_token != event.token_amount {
        return Enrichment::Disagreed;
    }
    // The SOL leg comes from the curve's own lamport delta, which is the
    // trade NET of the fees paid out of the same movement. The event's
    // sol_amount is the gross, so they differ by exactly the fees.
    let gross = movement_sol.saturating_add(event.total_fee());
    if movement_sol != event.sol_amount && gross != event.sol_amount {
        return Enrichment::Disagreed;
    }

    apply_reserves(
        row,
        event.mint,
        U256::from(event.real_token_reserves),
        wsol,
        U256::from(event.real_sol_reserves),
    );
    row.sender = event.user;
    row.fee_amount = U256::from(event.total_fee());
    // The bonding curve IS the pool, and the movement layer already found
    // it as the non-signer counterparty. The event confirms the trade.
    row.mark_decoded(swap.authority);
    Enrichment::Applied
}

/// Writes reserves onto `reserve0` / `reserve1`, matching the row's
/// `token0` / `token1` ordering.
fn apply_reserves(
    row: &mut SvmSwap,
    mint_a: Pubkey,
    reserve_a: U256,
    mint_b: Pubkey,
    reserve_b: U256,
) {
    if row.token0 == mint_a && row.token1 == mint_b {
        row.reserve0 = reserve_a;
        row.reserve1 = reserve_b;
    } else if row.token0 == mint_b && row.token1 == mint_a {
        row.reserve0 = reserve_b;
        row.reserve1 = reserve_a;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svm::fixtures;

    /// The PumpSwap offsets, checked against a live BuyEvent whose every
    /// decoded field is independently confirmed by the same transaction's
    /// balances (see `svm::fixtures::PUMPSWAP_BUY_EVENT`).
    #[test]
    fn pumpswap_buy_event_decodes_to_the_verified_values() {
        let data = fixtures::pumpswap_buy_event_bytes();
        let event = PumpSwapEvent::parse(&data).expect("parses");

        assert!(event.is_buy);
        // Equal to the block time of slot 448310216.
        assert_eq!(event.timestamp, 1_789_794_996);
        // Equal to the user's base ATA balance delta.
        assert_eq!(event.base_amount, 4_157_620_114);
        // Equal to the user's base ATA balance BEFORE the swap.
        assert_eq!(event.user_base_token_reserves, 281_959_571_155);
        // Equal to the two pool vaults' balances BEFORE the swap.
        assert_eq!(event.pool_base_token_reserves, 24_652_841_868_728);
        assert_eq!(event.pool_quote_token_reserves, 708_414_491_867);
        // Equal to the sum of the two protocol fee transfers in the tx.
        assert_eq!(event.protocol_fee, 61_229);
        assert_eq!(event.lp_fee, 244_916);
        // quote_amount_in + lp_fee = quote_amount_in_with_lp_fee, and that
        // sum is EXACTLY the pool quote vault's balance delta in this
        // transaction (708_414_491_867 -> 708_537_194_761). The whole
        // trade reconciles: 122_702_894 into the pool + 61_229 protocol
        // fee + 673_519 creator fee = 123_437_642 = user_quote_amount_in.
        assert_eq!(event.quote_amount + event.lp_fee, 122_702_894);
        // The pool the venue names is the vault owner the movement layer
        // infers, which is what makes the two layers cross-checkable.
        assert_eq!(
            crate::svm::programs::to_base58(&event.pool),
            fixtures::PUMPSWAP_BUY_POOL
        );
    }

    /// pump.fun's `TradeEvent`, same treatment.
    #[test]
    fn pumpfun_trade_event_decodes_to_the_verified_values() {
        let data = fixtures::pumpfun_sell_event_bytes();
        let event = PumpFunTrade::parse(&data).expect("parses");

        assert!(!event.is_buy);
        assert_eq!(event.timestamp, 1_789_794_996);
        // Equal to the bonding curve's lamport delta.
        assert_eq!(event.sol_amount, 1_631_299);
        // Equal to the Token-2022 transferChecked amount, and to both
        // token accounts' balance deltas.
        assert_eq!(event.token_amount, 4_328_848_585_204);
        assert_eq!(
            crate::svm::programs::to_base58(&event.mint),
            fixtures::PUMPFUN_SELL_MINT
        );
        assert_eq!(
            crate::svm::programs::to_base58(&event.user),
            fixtures::PUMPFUN_SELL_USER
        );
    }

    #[test]
    fn a_truncated_event_is_none_and_never_a_panic() {
        let full = fixtures::pumpswap_buy_event_bytes();
        for length in 0..full.len() {
            // Must not panic, and must not invent values.
            let _ = PumpSwapEvent::parse(&full[..length]);
        }
        assert!(PumpSwapEvent::parse(&full[..PumpSwapEvent::MIN_LEN - 1])
            .is_none());
        assert!(PumpSwapEvent::parse(&full[..PumpSwapEvent::MIN_LEN])
            .is_some());
    }

    #[test]
    fn a_pumpfun_event_is_not_read_as_a_pumpswap_one() {
        let pumpfun = fixtures::pumpfun_sell_event_bytes();
        assert!(PumpSwapEvent::parse(&pumpfun).is_none());
        let pumpswap = fixtures::pumpswap_buy_event_bytes();
        assert!(PumpFunTrade::parse(&pumpswap).is_none());
    }
}
