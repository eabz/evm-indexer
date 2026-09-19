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
    /// The venue publishes its event as a LOG LINE and the validator
    /// truncated this transaction's logs, so its event stream is
    /// incomplete by the validator's own admission. No log of it is read
    /// at all: a surviving line may belong to another hop, and Orca's
    /// `nth` indexing into the two `Traded` lines of a `two_hop_swap` is
    /// meaningless once one of them can be missing. The row keeps
    /// `movement` confidence and says so (review round 4, M7).
    Incomplete,
}

// --- byte readers --------------------------------------------------------

/// A Borsh length prefix: `u32` little endian.
fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

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

/// The quote leg at the TAIL of pump.fun's `TradeEvent`, past the two
/// variable-length fields.
///
/// pump.fun now runs curves quoted in USDC, BONK and other pump tokens, not
/// only in SOL. On such a curve `sol_amount` is **0** and the real quote leg
/// is here - which is why reading only the fixed prefix made every
/// non-SOL-quote trade look like a contradiction rather than a trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFunQuote {
    /// The quote mint. All-zero on a SOL curve: the program writes
    /// `Pubkey::default()` when the quote is omitted or is wrapped SOL.
    pub quote_mint: Pubkey,
    pub quote_amount: u64,
    pub virtual_quote_reserves: u64,
    pub real_quote_reserves: u64,
}

/// pump.fun's `TradeEvent`.
///
/// The fields up to `creator_fee` are at fixed offsets. Everything after
/// them is reached by WALKING the two variable-length Borsh fields, because
/// the quote leg sits behind them and there is no other way to it:
///
/// ```text
/// ... creator_fee u64 | track_volume bool | 3 x u64 | last_update i64
///   | ix_name String        <- u32 length + bytes
///   | mayhem_mode bool | 4 x u64
///   | shareholders Vec<Shareholder>   <- u32 count + 34 bytes each
///   | quote_mint Pubkey | quote_amount u64
///   | virtual_quote_reserves u64 | real_quote_reserves u64
///   | holder_rewards_bps u64 | holder_rewards u64      <- newest, optional
/// ```
///
/// The tail is OPTIONAL on purpose. pump.fun appends fields and states that
/// an event emitted before a field existed is simply shorter, so a missing
/// tail means "an older event", never "a broken one": [`PumpFunTrade::quote`]
/// stays `None` and the SOL reading is used, which is exactly what those
/// events meant.
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
    /// `"buy"`, `"sell"`, `"buy_exact_sol_in"` ... - the instruction variant
    /// the program itself names. `None` when the event stops before it.
    pub ix_name: Option<String>,
    /// The quote leg, when the event is new enough to carry one.
    pub quote: Option<PumpFunQuote>,
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
    /// `track_volume` + `total_unclaimed_tokens` + `total_claimed_tokens` +
    /// `current_sol_volume` + `last_update_timestamp`: where `ix_name`
    /// starts.
    const IX_NAME: usize = BODY + 250;
    /// One `Shareholder`: `address: Pubkey` + `share_bps: u16`.
    const SHAREHOLDER: usize = 34;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(8..16)? != DISC_PUMPFUN_TRADE_EVENT {
            return None;
        }
        if data.len() < Self::MIN_LEN {
            return None;
        }
        let (ix_name, quote) = Self::parse_tail(data);
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
            ix_name,
            quote,
        })
    }

    /// Walks the two variable-length fields to reach the quote leg.
    ///
    /// Returns what it could read and nothing it could not: a short event is
    /// an OLDER event, never a corrupt one, so every step is fallible and
    /// the result degrades to `(None, None)`.
    fn parse_tail(data: &[u8]) -> (Option<String>, Option<PumpFunQuote>) {
        let Some(name_len) = u32_at(data, Self::IX_NAME) else {
            return (None, None);
        };
        // A sane instruction name. Anything else means the offsets have
        // moved and the tail must not be guessed at.
        if name_len > 64 {
            return (None, None);
        }
        let name_at = Self::IX_NAME + 4;
        let Some(bytes) = data.get(name_at..name_at + name_len as usize)
        else {
            return (None, None);
        };
        let ix_name = String::from_utf8(bytes.to_vec()).ok();

        // mayhem_mode bool, then cashback_fee_basis_points, cashback,
        // buyback_fee_basis_points, buyback_fee.
        let shareholders_at = name_at + name_len as usize + 1 + 4 * 8;
        let Some(count) = u32_at(data, shareholders_at) else {
            return (ix_name, None);
        };
        if count > 64 {
            return (ix_name, None);
        }
        let quote_at =
            shareholders_at + 4 + count as usize * Self::SHAREHOLDER;

        let quote = (|| {
            Some(PumpFunQuote {
                quote_mint: pubkey_at(data, quote_at)?,
                quote_amount: u64_at(data, quote_at + 32)?,
                virtual_quote_reserves: u64_at(data, quote_at + 40)?,
                real_quote_reserves: u64_at(data, quote_at + 48)?,
            })
        })();
        (ix_name, quote)
    }

    /// The mint and the amount of the QUOTE leg, whatever it is priced in.
    ///
    /// A SOL curve reports its leg in `sol_amount` and leaves `quote_mint`
    /// all-zero; a USDC or BONK curve leaves `sol_amount` at 0 and puts the
    /// leg in the tail. Reading only one of the two is what made every
    /// non-SOL-quote trade look like a contradiction.
    pub fn quote_leg(&self, wsol: Pubkey) -> (Pubkey, u64) {
        match self.quote {
            Some(quote)
                if quote.quote_mint != crate::svm::models::ZERO_PUBKEY
                    && quote.quote_mint != wsol =>
            {
                (quote.quote_mint, quote.quote_amount)
            }
            _ => (wsol, self.sol_amount),
        }
    }

    /// Reserves of the quote leg AFTER the trade, matching [`Self::quote_leg`].
    pub fn quote_reserves(&self, wsol: Pubkey) -> u64 {
        match self.quote {
            Some(quote)
                if quote.quote_mint != crate::svm::models::ZERO_PUBKEY
                    && quote.quote_mint != wsol =>
            {
                quote.real_quote_reserves
            }
            _ => self.real_sol_reserves,
        }
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
///
/// `nth` is the index of this swap among the swaps the SAME instruction
/// produced. It is zero for every venue but Orca, whose `two_hop_swap`
/// emits two `Traded` log lines from one instruction and needs to know
/// which hop is being enriched.
pub fn enrich(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    venue: Venue,
    swap: &MovementSwap,
    row: &mut SvmSwap,
    nth: usize,
) -> Enrichment {
    // `has_dropped_log_messages` is the validator saying the log stream of
    // this transaction is INCOMPLETE. For a venue whose event only ever
    // exists as a log line that is decisive: a line that did survive
    // cannot be told apart from the one that did not, and enriching from
    // it would present a guess with `decoded` confidence. It was plumbed
    // end to end and then never consulted (review round 4, M7).
    if tx.dropped_logs
        && venue.event_source() == crate::svm::programs::EventSource::Log
    {
        return Enrichment::Incomplete;
    }

    match venue {
        Venue::PumpSwap => match event_of(tx, instruction) {
            Some(event) => enrich_pumpswap(&event.data, swap, row),
            None => Enrichment::None,
        },
        Venue::PumpFun => match event_of(tx, instruction) {
            Some(event) => enrich_pumpfun(&event.data, swap, row),
            None => Enrichment::None,
        },
        // Phase 2. Half of these read a LOG line rather than a self-CPI
        // instruction, so they cannot go through `event_of`.
        Venue::RaydiumAmmV4
        | Venue::RaydiumCpmm
        | Venue::RaydiumClmm
        | Venue::OrcaWhirlpool
        | Venue::MeteoraDlmm
        | Venue::MeteoraDammV2
        // The two launchpad curves are markets too, and both publish a
        // self-CPI swap event: Meteora DBC's `EvtSwap2` and Raydium
        // LaunchLab's `TradeEvent`.
        | Venue::MeteoraDbc
        | Venue::RaydiumLaunchlab => crate::svm::venues::enrich(
            tx,
            instruction,
            venue,
            swap,
            row,
            nth,
        ),
        // Publishes nothing. The row keeps `movement` confidence, which is
        // exactly what it is: price, size and trader are exact, pool state
        // is not available.
        Venue::BisonFi => Enrichment::None,
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
    // Every PumpSwap fee - lp, protocol and coin creator - comes out of
    // the QUOTE leg.
    row.set_fee(event.total_fee(), quote_mint);
    row.mark_trader(event.user);
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
    // NOT always SOL: pump.fun runs curves quoted in USDC, BONK and other
    // pump tokens, and on those `sol_amount` is 0.
    let (quote_mint, quote_amount) = event.quote_leg(wsol);

    // On a buy the curve receives the quote asset and pays out the token.
    let (expected_in, expected_out) = if event.is_buy {
        (quote_mint, event.mint)
    } else {
        (event.mint, quote_mint)
    };
    if swap.mint_in != expected_in || swap.mint_out != expected_out {
        return Enrichment::Disagreed;
    }

    let (movement_quote, movement_token) = if event.is_buy {
        (swap.amount_in, swap.amount_out_gross)
    } else {
        (swap.amount_out_gross, swap.amount_in)
    };
    if movement_token != event.token_amount {
        return Enrichment::Disagreed;
    }
    // On a SOL curve the quote leg is the curve account's own lamport delta
    // and the event's `sol_amount` is the trade GROSS. They differ by
    // whichever fees were routed through the curve's lamports in the same
    // instruction rather than paid directly by the user - measured live,
    // that is sometimes the protocol fee, sometimes the creator fee,
    // sometimes both and sometimes neither, so the tolerance is the total
    // fee rather than an exact equality on one particular routing.
    //
    // Anything OUTSIDE that band is a genuine contradiction and the row
    // keeps `movement` confidence.
    let difference = movement_quote.abs_diff(quote_amount);
    if difference > event.total_fee() {
        return Enrichment::Disagreed;
    }

    apply_reserves(
        row,
        event.mint,
        U256::from(event.real_token_reserves),
        quote_mint,
        U256::from(event.quote_reserves(wsol)),
    );
    row.sender = event.user;
    // pump.fun's fee and creator fee are both taken out of the quote leg,
    // whatever the curve is quoted in.
    row.set_fee(event.total_fee(), quote_mint);
    row.mark_trader(event.user);
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

    /// The quote leg lives past two variable-length Borsh fields, and
    /// walking them is the whole fix for the non-SOL-quote curves.
    ///
    /// On the recorded SOL curve the tail must resolve to WSOL and to
    /// `sol_amount`, i.e. reading it changes nothing that was already
    /// right. That is the property worth pinning: a new parser that
    /// silently altered the SOL case would be a regression dressed up as a
    /// feature.
    #[test]
    fn the_quote_leg_of_a_sol_curve_is_wsol_and_the_sol_amount() {
        let data = fixtures::pumpfun_sell_event_bytes();
        let event = PumpFunTrade::parse(&data).expect("parses");
        let wsol = registry().wsol;

        let (mint, amount) = event.quote_leg(wsol);
        assert_eq!(mint, wsol);
        assert_eq!(amount, event.sol_amount);
        assert_eq!(amount, 1_631_299);
        assert_eq!(event.quote_reserves(wsol), event.real_sol_reserves);

        // And the instruction variant the program itself names.
        if let Some(name) = &event.ix_name {
            assert!(
                ["buy", "sell", "buy_exact_sol_in"]
                    .iter()
                    .any(|known| name.starts_with(known)),
                "unexpected ix_name {name:?}"
            );
        }
    }

    /// A curve quoted in something other than SOL reports `sol_amount = 0`
    /// and puts the real leg in the tail. Before this was parsed, EVERY such
    /// trade contradicted the movement layer - measured live at 4.3% to
    /// 11.5% of all pump.fun curve instructions.
    ///
    /// Built from the recorded SOL event by appending a tail, so the fixed
    /// prefix is real bytes off the chain and only the part under test is
    /// synthetic.
    #[test]
    fn a_non_sol_quote_curve_reports_its_leg_in_the_tail() {
        let usdc = crate::svm::programs::pubkey(
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
        );
        let wsol = registry().wsol;
        let mut data = fixtures::pumpfun_sell_event_bytes();

        // Truncate to the fixed prefix and rebuild the tail by hand.
        data.truncate(PumpFunTrade::IX_NAME);
        // sol_amount = 0, as the program writes it on a quote curve.
        data[PumpFunTrade::SOL_AMOUNT..PumpFunTrade::SOL_AMOUNT + 8]
            .copy_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&4u32.to_le_bytes());
        data.extend_from_slice(b"sell");
        data.push(0); // mayhem_mode
        for _ in 0..4 {
            data.extend_from_slice(&0u64.to_le_bytes());
        }
        // Two shareholders, to prove the Vec is WALKED and not assumed
        // empty: with a fixed offset the quote mint would be read 68 bytes
        // early, inside the shareholder list.
        data.extend_from_slice(&2u32.to_le_bytes());
        for _ in 0..2 {
            data.extend_from_slice(&[0x11u8; 32]);
            data.extend_from_slice(&0u16.to_le_bytes());
        }
        data.extend_from_slice(&usdc);
        data.extend_from_slice(&123_456_789u64.to_le_bytes());
        data.extend_from_slice(&11u64.to_le_bytes());
        data.extend_from_slice(&22u64.to_le_bytes());

        let event = PumpFunTrade::parse(&data).expect("parses");
        assert_eq!(event.sol_amount, 0);
        assert_eq!(event.ix_name.as_deref(), Some("sell"));
        assert_eq!(event.quote_leg(wsol), (usdc, 123_456_789));
        assert_eq!(event.quote_reserves(wsol), 22);
    }

    /// An event that stops before the tail is an OLDER event, not a broken
    /// one: pump.fun appends fields and says so. Truncating anywhere must
    /// degrade to the SOL reading rather than invent a quote mint.
    #[test]
    fn a_short_event_degrades_to_the_sol_reading_instead_of_guessing() {
        let full = fixtures::pumpfun_sell_event_bytes();
        let wsol = registry().wsol;

        for length in PumpFunTrade::MIN_LEN..full.len() {
            let event =
                PumpFunTrade::parse(&full[..length]).expect("parses");
            let (mint, amount) = event.quote_leg(wsol);
            assert_eq!(
                mint, wsol,
                "a truncated event invented a quote mint at {length} bytes"
            );
            assert_eq!(amount, event.sol_amount);
        }
    }

    #[test]
    fn a_pumpfun_event_is_not_read_as_a_pumpswap_one() {
        let pumpfun = fixtures::pumpfun_sell_event_bytes();
        assert!(PumpSwapEvent::parse(&pumpfun).is_none());
        let pumpswap = fixtures::pumpswap_buy_event_bytes();
        assert!(PumpFunTrade::parse(&pumpswap).is_none());
    }
}
