//! Per-program decoders for the phase 2 venues: Raydium (AMM v4, CPMM,
//! CLMM), Orca Whirlpools and Meteora (DLMM, DAMM v2).
//!
//! Same contract as `events.rs`, which holds the pump.fun pair: this layer
//! ENRICHES a row the movement layer already created, and CROSS-CHECKS it.
//! A row becomes `decoded` only when the venue's own numbers agree with what
//! the movement layer worked out with no knowledge of the venue at all; a
//! contradiction is counted and the row stays `movement`.
//!
//! # The thing phase 1 could not have known
//!
//! **Half of these venues do not publish their event as an instruction.**
//! Phase 1's two venues both use Anchor's `emit_cpi!`, which makes the event
//! a self-CPI INSTRUCTION, and the streaming query selects no log fields at
//! all. But:
//!
//! | Venue | Mechanism | Where the event is |
//! |---|---|---|
//! | Meteora DLMM, DAMM v2 | `emit_cpi!` | an instruction |
//! | Raydium CPMM, CLMM, Orca | `emit!` | a `Program data:` LOG line |
//! | Raydium AMM v4 | bare `msg!` | a `Program log: ray_log:` LOG line |
//!
//! So the log table had to be selected, and [`SvmLog`] carries it. Logs are
//! weaker evidence than instructions - a validator may truncate them, and
//! `has_dropped_log_messages` says when it did - so a log-sourced event is
//! only ever allowed to CONFIRM and enrich a row the movement layer already
//! proved with real token transfers. It can never create one.
//!
//! # Layout verification
//!
//! Every struct below was taken from the venue's published source or IDL and
//! then checked against real mainnet bytes, because phase 1 was burned by a
//! STALE on-chain IDL that truncated an event by 41 bytes. The check that
//! caught the staleness both times is the payload LENGTH: each decoder
//! states the exact byte count its layout implies, and
//! `venue_event_lengths_match_the_chain` asserts it against the recorded
//! fixtures. Two of these layouts had in fact grown since the last published
//! IDL snapshot - Raydium's CPMM `SwapEvent` gained five creator-fee fields
//! and CLMM's gained two trade-fee fields - and the length check is what
//! proved the NEW layout is the live one.

use alloy::primitives::U256;

use crate::svm::{
    decode::{MovementSwap, SvmInstruction, SvmLog, SvmTransaction},
    events::Enrichment,
    models::{Pubkey, SvmSwap},
    programs::{
        Venue, DISC_DAMM2_SWAP, DISC_DLMM_SWAP, DISC_DLMM_SWAP2,
        DISC_ORCA_TRADED, DISC_RAYDIUM_SWAP_EVENT, EVENT_CPI_PREFIX,
        RAY_DIRECTION_PC_TO_COIN, RAY_LOG_PREFIX, RAY_LOG_SWAP_BASE_IN,
        RAY_LOG_SWAP_BASE_OUT, RAY_LOG_SWAP_LEN,
    },
};

// --- byte readers --------------------------------------------------------

fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn u128_at(data: &[u8], offset: usize) -> Option<u128> {
    Some(u128::from_le_bytes(
        data.get(offset..offset + 16)?.try_into().ok()?,
    ))
}

fn i32_at(data: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
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

/// Where an Anchor `emit!` body starts: the 8-byte event discriminator, and
/// nothing else. A self-CPI event has eight more bytes of `emit_cpi!` marker
/// in front - the difference is the whole reason these two families need
/// separate offset tables.
const LOG_BODY: usize = 8;
/// Where a self-CPI (`emit_cpi!`) event body starts.
const CPI_BODY: usize = 16;

// --- Raydium AMM v4: the `ray_log` line ----------------------------------

/// `SwapBaseInLog` / `SwapBaseOutLog` of `raydium-amm/program/src/log.rs`.
///
/// Encoded with **bincode**, not Borsh, and emitted with `msg!` rather than
/// `emit!`: the wire form is `Program log: ray_log: <base64>`. With bincode's
/// fixed-width integers and no length prefixes the two structs are the same
/// 57 bytes, which is what the parser checks.
///
/// The two variants name their fields differently for the same slots - a
/// base-in swap knows `amount_in` and computes `out_amount`, a base-out swap
/// knows `amount_out` and computes `deduct_in` - so this struct stores the
/// SWAP's view of them and remembers which variant it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RayLogSwap {
    /// `false` for `SwapBaseIn`, `true` for `SwapBaseOut`.
    pub base_out: bool,
    /// What actually went in.
    pub amount_in: u64,
    /// What actually came out.
    pub amount_out: u64,
    /// `1` = pc -> coin, `2` = coin -> pc.
    pub direction: u64,
    /// Pool reserves BEFORE the swap.
    pub pool_coin: u64,
    pub pool_pc: u64,
}

impl RayLogSwap {
    /// Parses a `ray_log:` log line. `message` is the line with
    /// `"Program log: "` already stripped, which is how HyperSync stores it.
    pub fn parse_log(message: &str) -> Option<Self> {
        let payload = message.trim().strip_prefix(RAY_LOG_PREFIX)?;
        Self::parse(&crate::svm::decode::base64_decode(payload.trim())?)
    }

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() != RAY_LOG_SWAP_LEN {
            return None;
        }
        let log_type = *data.first()?;
        let base_out = match log_type {
            RAY_LOG_SWAP_BASE_IN => false,
            RAY_LOG_SWAP_BASE_OUT => true,
            _ => return None,
        };

        // Common slots: [1] amount, [2] bound, [3] direction,
        // [4] user_source, [5] pool_coin, [6] pool_pc, [7] result.
        let field = |index: usize| u64_at(data, 1 + 8 * (index - 1));
        let first = field(1)?;
        let last = field(7)?;

        // SwapBaseIn  : first = amount_in,  last = out_amount
        // SwapBaseOut : first = max_in,     last = deduct_in
        //               and field(2) = amount_out
        let (amount_in, amount_out) =
            if base_out { (last, field(2)?) } else { (first, last) };

        Some(Self {
            base_out,
            amount_in,
            amount_out,
            direction: field(3)?,
            pool_coin: field(5)?,
            pool_pc: field(6)?,
        })
    }
}

// --- Raydium CPMM: `SwapEvent` -------------------------------------------

/// `raydium-cp-swap/programs/cp-swap/src/states/events.rs`.
///
/// ```text
/// pool_id            Pubkey   32
/// input_vault_before u64       8
/// output_vault_before u64      8
/// input_amount       u64       8
/// output_amount      u64       8
/// input_transfer_fee u64       8
/// output_transfer_fee u64      8
/// base_input         bool      1
/// input_mint         Pubkey   32
/// output_mint        Pubkey   32
/// trade_fee          u64       8
/// creator_fee        u64       8
/// creator_fee_on_input bool    1
/// ```
///
/// 8 (discriminator) + 162 = **170 bytes**, which is exactly what mainnet
/// serves. The published IDL snapshot from before the "support creator fee"
/// change stops after `base_input` at 89 bytes; a decoder written against it
/// would read the mints as fee amounts. The length check below is what tells
/// the two apart, and it is why `input_mint` is read at all - it gives the
/// decoder an independent statement of the direction to check the movement
/// layer against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumCpmmSwap {
    pub pool_id: Pubkey,
    pub input_vault_before: u64,
    pub output_vault_before: u64,
    pub input_amount: u64,
    pub output_amount: u64,
    pub input_transfer_fee: u64,
    pub output_transfer_fee: u64,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub trade_fee: u64,
    pub creator_fee: u64,
}

impl RaydiumCpmmSwap {
    /// Total length of the `Program data:` payload, discriminator included.
    pub const LEN: usize = LOG_BODY + 162;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != DISC_RAYDIUM_SWAP_EVENT
            || data.len() < Self::LEN
        {
            return None;
        }
        Some(Self {
            pool_id: pubkey_at(data, LOG_BODY)?,
            input_vault_before: u64_at(data, LOG_BODY + 32)?,
            output_vault_before: u64_at(data, LOG_BODY + 40)?,
            input_amount: u64_at(data, LOG_BODY + 48)?,
            output_amount: u64_at(data, LOG_BODY + 56)?,
            input_transfer_fee: u64_at(data, LOG_BODY + 64)?,
            output_transfer_fee: u64_at(data, LOG_BODY + 72)?,
            // LOG_BODY + 80 is `base_input`, which is the caller's INTENT
            // (exact-in or exact-out) and not a fill. Deliberately not
            // stored, for the same reason phase 1 drops PumpSwap's slippage
            // bound.
            input_mint: pubkey_at(data, LOG_BODY + 81)?,
            output_mint: pubkey_at(data, LOG_BODY + 113)?,
            trade_fee: u64_at(data, LOG_BODY + 145)?,
            creator_fee: u64_at(data, LOG_BODY + 153)?,
        })
    }

    pub fn total_fee(&self) -> u64 {
        self.trade_fee.saturating_add(self.creator_fee)
    }
}

// --- Raydium CLMM: `SwapEvent` -------------------------------------------

/// `raydium-clmm/programs/amm/src/states/pool.rs`.
///
/// ```text
/// pool_state      Pubkey  32     zero_for_one   bool   1
/// sender          Pubkey  32     sqrt_price_x64 u128  16
/// token_account_0 Pubkey  32     liquidity      u128  16
/// token_account_1 Pubkey  32     tick           i32    4
/// amount_0        u64      8     trade_fee_0    u64    8
/// transfer_fee_0  u64      8     trade_fee_1    u64    8
/// amount_1        u64      8
/// transfer_fee_1  u64      8
/// ```
///
/// 8 + 213 = **221 bytes** on the wire. The 2025 snapshot of this struct
/// ended at `tick` (205 bytes); mainnet serves 221, so the trade-fee fields
/// are live and are read here.
///
/// Note that it shares its discriminator with the CPMM event above - both
/// programs called their event `SwapEvent` - so the decoder MUST key on the
/// log row's `program_id`. The lengths differ too (221 vs 170), which the
/// parser uses as a second, independent guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumClmmSwap {
    pub pool_state: Pubkey,
    pub sender: Pubkey,
    pub amount_0: u64,
    pub transfer_fee_0: u64,
    pub amount_1: u64,
    pub transfer_fee_1: u64,
    /// `true` when token 0 went INTO the pool.
    pub zero_for_one: bool,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick: i32,
    pub trade_fee_0: u64,
    pub trade_fee_1: u64,
}

impl RaydiumClmmSwap {
    pub const LEN: usize = LOG_BODY + 213;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != DISC_RAYDIUM_SWAP_EVENT
            || data.len() < Self::LEN
        {
            return None;
        }
        Some(Self {
            pool_state: pubkey_at(data, LOG_BODY)?,
            sender: pubkey_at(data, LOG_BODY + 32)?,
            // 64 and 96 are token_account_0 / token_account_1: the VAULTS,
            // which the movement layer has already found for itself.
            amount_0: u64_at(data, LOG_BODY + 128)?,
            transfer_fee_0: u64_at(data, LOG_BODY + 136)?,
            amount_1: u64_at(data, LOG_BODY + 144)?,
            transfer_fee_1: u64_at(data, LOG_BODY + 152)?,
            zero_for_one: bool_at(data, LOG_BODY + 160)?,
            sqrt_price_x64: u128_at(data, LOG_BODY + 161)?,
            liquidity: u128_at(data, LOG_BODY + 177)?,
            tick: i32_at(data, LOG_BODY + 193)?,
            trade_fee_0: u64_at(data, LOG_BODY + 197)?,
            trade_fee_1: u64_at(data, LOG_BODY + 205)?,
        })
    }

    /// Amounts as (into the pool, out of the pool).
    pub fn legs(&self) -> (u64, u64) {
        if self.zero_for_one {
            (self.amount_0, self.amount_1)
        } else {
            (self.amount_1, self.amount_0)
        }
    }

    /// Fee taken, in the units of the leg it was taken from.
    pub fn total_fee(&self) -> u64 {
        self.trade_fee_0.saturating_add(self.trade_fee_1)
    }
}

// --- Orca Whirlpools: `Traded` -------------------------------------------

/// `whirlpools/programs/whirlpool/src/events.rs`.
///
/// ```text
/// whirlpool           Pubkey  32
/// a_to_b              bool     1
/// pre_sqrt_price      u128    16
/// post_sqrt_price     u128    16
/// input_amount        u64      8
/// output_amount       u64      8
/// input_transfer_fee  u64      8
/// output_transfer_fee u64      8
/// lp_fee              u64      8
/// protocol_fee        u64      8
/// ```
///
/// 8 + 113 = **121 bytes**, matching mainnet. The Whirlpool program has no
/// `SwapEvent` and no self-CPI event of any kind: `Traded` is emitted with
/// `emit!` and reaches us only as a log line. A `two_hop_swap` emits `Traded`
/// TWICE, once per hop, which the subtree rule handles the same way it
/// handles a Jupiter route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrcaTraded {
    pub whirlpool: Pubkey,
    /// `true` when token A went INTO the pool.
    pub a_to_b: bool,
    pub pre_sqrt_price: u128,
    pub post_sqrt_price: u128,
    pub input_amount: u64,
    pub output_amount: u64,
    pub input_transfer_fee: u64,
    pub output_transfer_fee: u64,
    pub lp_fee: u64,
    pub protocol_fee: u64,
}

impl OrcaTraded {
    pub const LEN: usize = LOG_BODY + 113;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != DISC_ORCA_TRADED || data.len() < Self::LEN {
            return None;
        }
        Some(Self {
            whirlpool: pubkey_at(data, LOG_BODY)?,
            a_to_b: bool_at(data, LOG_BODY + 32)?,
            pre_sqrt_price: u128_at(data, LOG_BODY + 33)?,
            post_sqrt_price: u128_at(data, LOG_BODY + 49)?,
            input_amount: u64_at(data, LOG_BODY + 65)?,
            output_amount: u64_at(data, LOG_BODY + 73)?,
            input_transfer_fee: u64_at(data, LOG_BODY + 81)?,
            output_transfer_fee: u64_at(data, LOG_BODY + 89)?,
            lp_fee: u64_at(data, LOG_BODY + 97)?,
            protocol_fee: u64_at(data, LOG_BODY + 105)?,
        })
    }

    pub fn total_fee(&self) -> u64 {
        self.lp_fee.saturating_add(self.protocol_fee)
    }
}

// --- Meteora DLMM: `Swap` and `Swap2Evt` ---------------------------------

/// The FIRST of the two self-CPI events a DLMM swap emits
/// (`MeteoraAg/dlmm-sdk`, `idls/dlmm.json`, `lb_clmm` 0.12.0).
///
/// ```text
/// lb_pair      Pubkey  32     swap_for_y   bool   1
/// from         Pubkey  32     fee          u64    8
/// start_bin_id i32      4     protocol_fee u64    8
/// end_bin_id   i32      4     fee_bps      u128  16
/// amount_in    u64      8     host_fee     u64    8
/// amount_out   u64      8
/// ```
///
/// 16 (self-CPI marker + discriminator) + 129 = **145 bytes**.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeteoraDlmmSwap {
    pub lb_pair: Pubkey,
    pub from: Pubkey,
    pub start_bin_id: i32,
    pub end_bin_id: i32,
    pub amount_in: u64,
    pub amount_out: u64,
    /// `true` when X went in and Y came out.
    pub swap_for_y: bool,
    /// Total fee, protocol share included.
    pub fee: u64,
    pub protocol_fee: u64,
    pub host_fee: u64,
}

impl MeteoraDlmmSwap {
    pub const LEN: usize = CPI_BODY + 129;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != EVENT_CPI_PREFIX
            || data.get(8..16)? != DISC_DLMM_SWAP
            || data.len() < Self::LEN
        {
            return None;
        }
        Some(Self {
            lb_pair: pubkey_at(data, CPI_BODY)?,
            from: pubkey_at(data, CPI_BODY + 32)?,
            start_bin_id: i32_at(data, CPI_BODY + 64)?,
            end_bin_id: i32_at(data, CPI_BODY + 68)?,
            amount_in: u64_at(data, CPI_BODY + 72)?,
            amount_out: u64_at(data, CPI_BODY + 80)?,
            swap_for_y: bool_at(data, CPI_BODY + 88)?,
            fee: u64_at(data, CPI_BODY + 89)?,
            protocol_fee: u64_at(data, CPI_BODY + 97)?,
            // CPI_BODY + 105 is fee_bps, a u128 rate rather than an amount.
            host_fee: u64_at(data, CPI_BODY + 121)?,
        })
    }
}

/// The SECOND DLMM event. It is NOT `Swap` with fields appended: it
/// reorders them, putting `swap_for_y` and `fee_bps` BEFORE the amounts.
/// Decoding it with `Swap`'s offsets yields plausible-looking nonsense,
/// which is exactly the failure mode this module is built to avoid.
///
/// ```text
/// lb_pair    32   swap_for_y      1    amount_out        8
/// from       32   fee_bps        16    mm_fee            8
/// start_bin   4   amount_in       8    protocol_fee      8
/// end_bin     4   amount_left     8    limit_order_fee   8
///                                      host_fee          8
///                                      fees_on_input     1
///                                      fees_on_token_x   1
/// ```
///
/// 16 + 147 = **163 bytes**. It carries what `Swap` does not: whether the
/// fee came off the INPUT leg or the output one, which is the difference
/// between a fee that is inside `amount_in` and one that is not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeteoraDlmmSwap2 {
    pub lb_pair: Pubkey,
    pub from: Pubkey,
    pub swap_for_y: bool,
    pub amount_in: u64,
    pub amount_left: u64,
    pub amount_out: u64,
    pub mm_fee: u64,
    pub protocol_fee: u64,
    pub limit_order_fee: u64,
    pub host_fee: u64,
    pub fees_on_input: bool,
}

impl MeteoraDlmmSwap2 {
    pub const LEN: usize = CPI_BODY + 147;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != EVENT_CPI_PREFIX
            || data.get(8..16)? != DISC_DLMM_SWAP2
            || data.len() < Self::LEN
        {
            return None;
        }
        Some(Self {
            lb_pair: pubkey_at(data, CPI_BODY)?,
            from: pubkey_at(data, CPI_BODY + 32)?,
            // 64 start_bin_id, 68 end_bin_id.
            swap_for_y: bool_at(data, CPI_BODY + 72)?,
            // 73 fee_bps (u128).
            amount_in: u64_at(data, CPI_BODY + 89)?,
            amount_left: u64_at(data, CPI_BODY + 97)?,
            amount_out: u64_at(data, CPI_BODY + 105)?,
            mm_fee: u64_at(data, CPI_BODY + 113)?,
            protocol_fee: u64_at(data, CPI_BODY + 121)?,
            limit_order_fee: u64_at(data, CPI_BODY + 129)?,
            host_fee: u64_at(data, CPI_BODY + 137)?,
            fees_on_input: bool_at(data, CPI_BODY + 145)?,
        })
    }

    pub fn total_fee(&self) -> u64 {
        self.mm_fee
            .saturating_add(self.protocol_fee)
            .saturating_add(self.limit_order_fee)
            .saturating_add(self.host_fee)
    }
}

// --- Meteora DAMM v2: `EvtSwap2` -----------------------------------------

/// `MeteoraAg/damm-v2`, `programs/cp-amm/src/event.rs` (0.2.4).
///
/// The two nested structs are plain Borsh, so they are INLINED with no
/// length prefix and no tag:
///
/// ```text
///   0 pool                 Pubkey 32     84 next_sqrt_price     u128 16
///  32 trade_direction      u8      1    100 claiming_fee        u64   8
///  33 collect_fee_mode     u8      1    108 protocol_fee        u64   8
///  34 has_referral         bool    1    116 compounding_fee     u64   8
///  35 params.amount_0      u64     8    124 referral_fee        u64   8
///  43 params.amount_1      u64     8    132 incl_transfer_fee_in  u64 8
///  51 params.swap_mode     u8      1    140 incl_transfer_fee_out u64 8
///  52 included_fee_input   u64     8    148 excl_transfer_fee_out u64 8
///  60 excluded_fee_input   u64     8    156 current_timestamp   u64   8
///  68 amount_left          u64     8    164 reserve_a_amount    u64   8
///  76 output_amount        u64     8    172 reserve_b_amount    u64   8
/// ```
///
/// 16 + 180 = **196 bytes**. `trade_direction` is 0 for A -> B.
///
/// The older `EvtSwap` (122 bytes) was emitted ALONGSIDE this one until
/// release 0.1.8 and is gone now; this decoder reads only `EvtSwap2`, which
/// covers the whole history, because 0.1.x emitted both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeteoraDamm2Swap {
    pub pool: Pubkey,
    /// `0` = token A went into the pool.
    pub trade_direction: u8,
    /// What went in, transfer fee included.
    pub included_transfer_fee_amount_in: u64,
    /// What the pool sent out, transfer fee included.
    pub included_transfer_fee_amount_out: u64,
    /// What the taker actually received.
    pub excluded_transfer_fee_amount_out: u64,
    pub claiming_fee: u64,
    pub protocol_fee: u64,
    pub compounding_fee: u64,
    pub referral_fee: u64,
    pub reserve_a_amount: u64,
    pub reserve_b_amount: u64,
}

impl MeteoraDamm2Swap {
    pub const LEN: usize = CPI_BODY + 180;

    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.get(..8)? != EVENT_CPI_PREFIX
            || data.get(8..16)? != DISC_DAMM2_SWAP
            || data.len() < Self::LEN
        {
            return None;
        }
        Some(Self {
            pool: pubkey_at(data, CPI_BODY)?,
            trade_direction: *data.get(CPI_BODY + 32)?,
            claiming_fee: u64_at(data, CPI_BODY + 100)?,
            protocol_fee: u64_at(data, CPI_BODY + 108)?,
            compounding_fee: u64_at(data, CPI_BODY + 116)?,
            referral_fee: u64_at(data, CPI_BODY + 124)?,
            included_transfer_fee_amount_in: u64_at(data, CPI_BODY + 132)?,
            included_transfer_fee_amount_out: u64_at(data, CPI_BODY + 140)?,
            excluded_transfer_fee_amount_out: u64_at(data, CPI_BODY + 148)?,
            reserve_a_amount: u64_at(data, CPI_BODY + 164)?,
            reserve_b_amount: u64_at(data, CPI_BODY + 172)?,
        })
    }

    pub fn total_fee(&self) -> u64 {
        self.claiming_fee
            .saturating_add(self.protocol_fee)
            .saturating_add(self.compounding_fee)
            .saturating_add(self.referral_fee)
    }
}

// --- finding the event ---------------------------------------------------

/// The `Program data:` log line this instruction emitted, if any.
///
/// Attribution is by `instruction_address`: HyperSync tags every log row
/// with the instruction that wrote it, so a log belongs to exactly one
/// subtree in the same way an instruction does. An Orca `two_hop_swap` emits
/// two `Traded` lines from ONE instruction, so `nth` picks between them by
/// emission order.
fn data_log_of<'a>(
    tx: &'a SvmTransaction,
    instruction: &SvmInstruction,
    discriminator: [u8; 8],
    nth: usize,
) -> Option<&'a SvmLog> {
    tx.logs
        .iter()
        .filter(|log| {
            log.is_data
                && log.program == instruction.program
                && log.path == instruction.path
                && log.event_discriminator() == Some(discriminator)
        })
        .nth(nth)
}

/// The `ray_log:` line of a Raydium AMM v4 swap instruction.
fn ray_log_of(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
) -> Option<RayLogSwap> {
    tx.logs
        .iter()
        .filter(|log| {
            !log.is_data
                && log.program == instruction.program
                && log.path == instruction.path
        })
        .find_map(|log| RayLogSwap::parse_log(&log.message))
}

/// A self-CPI event instruction under `instruction` carrying `discriminator`.
fn cpi_event_of<'a>(
    tx: &'a SvmTransaction,
    instruction: &SvmInstruction,
    discriminator: [u8; 8],
) -> Option<&'a SvmInstruction> {
    tx.instructions.iter().find(|candidate| {
        candidate.program == instruction.program
            && candidate.path.len() == instruction.path.len() + 1
            && candidate.path.starts_with(&instruction.path)
            && candidate.data.starts_with(&EVENT_CPI_PREFIX)
            && candidate.data.get(8..16) == Some(&discriminator[..])
    })
}

// --- the shared agreement rule -------------------------------------------

/// Does the venue's own pair of amounts match the movement layer's?
///
/// The movement layer measured real SPL transfers, so it is the ground
/// truth; the event is the venue's claim about the same trade. The two
/// measure slightly different things, and the difference is a Token-2022
/// **transfer fee** - on BOTH legs.
///
/// This was found by measurement, not by reading: Raydium CPMM's agreement
/// rate sat at 52.8% live, and in every disagreeing case the gap was
/// **exactly** `input_transfer_fee`, to the unit. The movement layer reads
/// the `transferChecked` instruction, which is what the taker SENT; the
/// event reports what the pool actually CREDITED, which on a mint with a
/// transfer fee is less. Neither is wrong. The output leg has the mirror
/// image of the same problem, which phase 1 already found and models as
/// `amount_out_gross` (what the pool sent) against `amount_out` (what the
/// taker received).
///
/// So each leg is allowed to differ by at most the transfer fee the event
/// itself declares for that leg. Anything outside that band is a real
/// contradiction: the row keeps `movement` confidence and the disagreement
/// is counted.
fn amounts_agree(
    swap: &MovementSwap,
    event_in: u64,
    in_transfer_fee: u64,
    event_out: u64,
    out_transfer_fee: u64,
) -> bool {
    if event_in == 0 || event_out == 0 {
        return false;
    }
    swap.amount_in.abs_diff(event_in) <= in_transfer_fee
        && swap.amount_out_gross.abs_diff(event_out) <= out_transfer_fee
}

/// Writes reserves onto `reserve0` / `reserve1` in the row's token order.
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

// --- the decoders --------------------------------------------------------

/// Raydium AMM v4. The `ray_log` names no accounts at all - it is seven bare
/// integers - so the pool comes from the instruction's account metas, where
/// it is index 1 in every one of the four swap variants (tags 9, 11, 16, 17).
/// That is the one place in this module where an account index is used, and
/// it is guarded by the tag.
pub fn enrich_raydium_v4(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(event) = ray_log_of(tx, instruction) else {
        return Enrichment::None;
    };

    // The ray_log carries no fee field, so the output leg must match to the
    // byte: Raydium v4 takes its fee out of the input before the swap maths
    // and the out_amount is what the vault actually sent.
    // Raydium v4 predates Token-2022 and its pools are classic SPL, so
    // there is no transfer fee on either leg and both must match to the
    // byte.
    if !amounts_agree(swap, event.amount_in, 0, event.amount_out, 0) {
        return Enrichment::Disagreed;
    }

    // THE cross-check the ray_log makes possible: `direction` says which
    // side of the pool the input landed on, and the reserves say how big
    // each side was. `pc` and `coin` are the pool's own naming; the movement
    // layer knows only mints, so the two are tied together through the
    // direction flag.
    let pc_to_coin = event.direction == RAY_DIRECTION_PC_TO_COIN;
    let (mint_pc, mint_coin) = if pc_to_coin {
        (swap.mint_in, swap.mint_out)
    } else {
        (swap.mint_out, swap.mint_in)
    };
    apply_reserves(
        row,
        mint_coin,
        U256::from(event.pool_coin),
        mint_pc,
        U256::from(event.pool_pc),
    );

    let Some(pool) = instruction.account(1) else {
        return Enrichment::None;
    };
    row.mark_decoded(pool);
    Enrichment::Applied
}

/// Raydium CPMM. The event names the pool AND both mints, so the direction
/// is checked independently of the movement layer rather than assumed.
pub fn enrich_raydium_cpmm(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(log) = data_log_of(tx, instruction, DISC_RAYDIUM_SWAP_EVENT, 0)
    else {
        return Enrichment::None;
    };
    let Some(bytes) = log.event_bytes() else {
        return Enrichment::None;
    };
    // Length tells the CPMM event apart from the CLMM one, which shares its
    // discriminator. `program` has already done that; this is the second
    // guard.
    if bytes.len() != RaydiumCpmmSwap::LEN {
        return Enrichment::None;
    }
    let Some(event) = RaydiumCpmmSwap::parse(&bytes) else {
        return Enrichment::None;
    };

    if event.input_mint != swap.mint_in
        || event.output_mint != swap.mint_out
    {
        return Enrichment::Disagreed;
    }
    // `output_amount` is what the pool sent; a Token-2022 transfer fee is
    // reported separately and is taken off the taker's side, not the pool's.
    if !amounts_agree(
        swap,
        event.input_amount,
        event.input_transfer_fee,
        event.output_amount,
        event.output_transfer_fee,
    ) {
        return Enrichment::Disagreed;
    }

    apply_reserves(
        row,
        event.input_mint,
        U256::from(event.input_vault_before),
        event.output_mint,
        U256::from(event.output_vault_before),
    );
    row.fee_amount = U256::from(event.total_fee());
    row.mark_decoded(event.pool_id);
    Enrichment::Applied
}

/// Raydium CLMM. A concentrated pool has no reserves to report, so
/// `reserve0` / `reserve1` stay zero and the pool STATE that matters -
/// `sqrt_price`, `liquidity`, the tick - is what the event is read for. Only
/// the fee and the pool account survive into the current row shape; the
/// state fields are decoded and checked here so that adding the columns
/// later is a schema change and not a decoder change.
pub fn enrich_raydium_clmm(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(log) = data_log_of(tx, instruction, DISC_RAYDIUM_SWAP_EVENT, 0)
    else {
        return Enrichment::None;
    };
    let Some(bytes) = log.event_bytes() else {
        return Enrichment::None;
    };
    if bytes.len() != RaydiumClmmSwap::LEN {
        return Enrichment::None;
    }
    let Some(event) = RaydiumClmmSwap::parse(&bytes) else {
        return Enrichment::None;
    };

    let (amount_in, amount_out) = event.legs();
    let (transfer_fee_in, transfer_fee_out) = if event.zero_for_one {
        (event.transfer_fee_0, event.transfer_fee_1)
    } else {
        (event.transfer_fee_1, event.transfer_fee_0)
    };
    if !amounts_agree(
        swap,
        amount_in,
        transfer_fee_in,
        amount_out,
        transfer_fee_out,
    ) {
        return Enrichment::Disagreed;
    }

    row.sender = event.sender;
    row.fee_amount = U256::from(event.total_fee());
    row.mark_decoded(event.pool_state);
    Enrichment::Applied
}

/// Orca Whirlpools. `Traded` names the pool and the two amounts but NOT the
/// mints, so direction is taken from the movement layer and only the
/// magnitudes are cross-checked.
///
/// `nth` selects between the two `Traded` lines a `two_hop_swap` emits from
/// one instruction. The caller passes the hop's index within the
/// instruction, which the subtree rule already knows.
pub fn enrich_orca(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
    nth: usize,
) -> Enrichment {
    let Some(log) = data_log_of(tx, instruction, DISC_ORCA_TRADED, nth)
    else {
        return Enrichment::None;
    };
    let Some(bytes) = log.event_bytes() else {
        return Enrichment::None;
    };
    let Some(event) = OrcaTraded::parse(&bytes) else {
        return Enrichment::None;
    };

    if !amounts_agree(
        swap,
        event.input_amount,
        event.input_transfer_fee,
        event.output_amount,
        event.output_transfer_fee,
    ) {
        return Enrichment::Disagreed;
    }

    row.fee_amount = U256::from(event.total_fee());
    row.mark_decoded(event.whirlpool);
    Enrichment::Applied
}

/// Meteora DLMM. TWO self-CPI events per swap, and this reads both: `Swap`
/// for the bin range and the headline fee, `Swap2Evt` for the fee split and
/// for whether the fee came off the input leg.
///
/// Where both are present they must agree with each other as well as with
/// the movement layer - three independent statements of the same trade.
pub fn enrich_meteora_dlmm(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let first = cpi_event_of(tx, instruction, DISC_DLMM_SWAP)
        .and_then(|event| MeteoraDlmmSwap::parse(&event.data));
    let second = cpi_event_of(tx, instruction, DISC_DLMM_SWAP2)
        .and_then(|event| MeteoraDlmmSwap2::parse(&event.data));

    let (pool, amount_in, amount_out, fee) = match (&first, &second) {
        (Some(one), Some(two)) => {
            // The two events describe one trade. If they contradict each
            // other the program is not doing what its IDL says and nothing
            // here should be trusted.
            if one.lb_pair != two.lb_pair
                || one.amount_out != two.amount_out
                || one.swap_for_y != two.swap_for_y
            {
                return Enrichment::Disagreed;
            }
            (one.lb_pair, one.amount_in, one.amount_out, two.total_fee())
        }
        (Some(one), None) => {
            (one.lb_pair, one.amount_in, one.amount_out, one.fee)
        }
        (None, Some(two)) => {
            (two.lb_pair, two.amount_in, two.amount_out, two.total_fee())
        }
        (None, None) => return Enrichment::None,
    };

    // `amount_in` is what the taker sent; when the fee comes off the input
    // leg the pool's vault receives that much less, so the pool-side leg the
    // movement layer measured is short by the fee.
    let fees_on_input =
        second.as_ref().map(|two| two.fees_on_input).unwrap_or(false);
    let expected_in =
        if fees_on_input { amount_in.saturating_sub(fee) } else { amount_in };
    if expected_in != swap.amount_in && amount_in != swap.amount_in {
        return Enrichment::Disagreed;
    }
    if swap.amount_out_gross.abs_diff(amount_out) > fee {
        return Enrichment::Disagreed;
    }

    row.fee_amount = U256::from(fee);
    row.mark_decoded(pool);
    Enrichment::Applied
}

/// Meteora DAMM v2. The event reports the transfer-fee-included and
/// -excluded output separately, which is the one venue here that states the
/// Token-2022 gap itself instead of leaving it to be measured.
pub fn enrich_meteora_damm2(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    swap: &MovementSwap,
    row: &mut SvmSwap,
) -> Enrichment {
    let Some(event) = cpi_event_of(tx, instruction, DISC_DAMM2_SWAP)
        .and_then(|event| MeteoraDamm2Swap::parse(&event.data))
    else {
        return Enrichment::None;
    };

    // This venue is the one that states the Token-2022 gap itself: the
    // "included" figures are the transfers as sent, which is exactly what
    // the movement layer measures, so no tolerance is needed on either leg.
    if !amounts_agree(
        swap,
        event.included_transfer_fee_amount_in,
        0,
        event.included_transfer_fee_amount_out,
        0,
    ) {
        return Enrichment::Disagreed;
    }

    // `trade_direction` 0 means token A went in, so reserve A is the input
    // side. These are the reserves AFTER the swap, which is what the event
    // reports; the other venues here report them before it.
    let (mint_a, mint_b) = if event.trade_direction == 0 {
        (swap.mint_in, swap.mint_out)
    } else {
        (swap.mint_out, swap.mint_in)
    };
    apply_reserves(
        row,
        mint_a,
        U256::from(event.reserve_a_amount),
        mint_b,
        U256::from(event.reserve_b_amount),
    );
    row.fee_amount = U256::from(event.total_fee());
    row.mark_decoded(event.pool);
    Enrichment::Applied
}

/// Dispatch for the phase 2 venues.
pub fn enrich(
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    venue: Venue,
    swap: &MovementSwap,
    row: &mut SvmSwap,
    nth: usize,
) -> Enrichment {
    match venue {
        Venue::RaydiumAmmV4 => {
            enrich_raydium_v4(tx, instruction, swap, row)
        }
        Venue::RaydiumCpmm => {
            enrich_raydium_cpmm(tx, instruction, swap, row)
        }
        Venue::RaydiumClmm => {
            enrich_raydium_clmm(tx, instruction, swap, row)
        }
        Venue::OrcaWhirlpool => {
            enrich_orca(tx, instruction, swap, row, nth)
        }
        Venue::MeteoraDlmm => {
            enrich_meteora_dlmm(tx, instruction, swap, row)
        }
        Venue::MeteoraDammV2 => {
            enrich_meteora_damm2(tx, instruction, swap, row)
        }
        // Handled in `events.rs`, or not decodable at all.
        Venue::PumpSwap | Venue::PumpFun | Venue::BisonFi => {
            Enrichment::None
        }
    }
}
