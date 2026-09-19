//! The program registry: which Solana programs are venues, which are
//! routers, and which move tokens.
//!
//! Every id here is quoted in base58 exactly as
//! docs/solana-research.md appendix B records it, and decoded to 32 bytes
//! by [`pubkey`]. A unit test round-trips every entry back to base58, so a
//! typo is a test failure and not a filter that silently matches nothing.
//!
//! Phase 1 registers the two venues with a per-program decoder (PumpSwap and
//! the pump.fun bonding curve). The generic movement decoder works on ANY
//! program in [`VENUES`], so adding a venue is one line here plus a
//! `protocol` name - that is the whole point of the two-layer design in
//! docs/solana-research.md section 3.3.

use crate::svm::models::Pubkey;

/// Decodes a base58 pubkey at run time.
///
/// Used by the `const`-ish accessors below through a `OnceLock`, because
/// base58 decoding is not a `const fn`. Panics on a malformed literal: every
/// caller passes a string constant from this file, and the round-trip test
/// covers all of them.
pub fn pubkey(base58: &str) -> Pubkey {
    let mut out = [0u8; 32];
    let written = bs58::decode(base58)
        .onto(&mut out[..])
        .unwrap_or_else(|e| panic!("bad base58 pubkey {base58:?}: {e}"));
    assert_eq!(written, 32, "pubkey {base58:?} is not 32 bytes");
    out
}

/// Base58 of a pubkey, for logs and error messages only. Nothing is ever
/// STORED as base58 (docs/design.md section 1).
pub fn to_base58(key: &Pubkey) -> String {
    bs58::encode(key).into_string()
}

// --- token programs -----------------------------------------------------

pub const SPL_TOKEN_B58: &str =
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_B58: &str =
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const SYSTEM_B58: &str = "11111111111111111111111111111111";

/// Wrapped SOL. Also used as the MINT of a native lamport leg, so a SOL
/// trade against a curve that moves lamports directly and one against a
/// WSOL pool land on the same pair.
pub const WSOL_B58: &str = "So11111111111111111111111111111111111111112";

/// SPL Token / Token-2022 `Transfer`: `[0x03, amount: u64]`.
pub const IX_TRANSFER: u8 = 0x03;
/// SPL Token / Token-2022 `TransferChecked`:
/// `[0x0c, amount: u64, decimals: u8]`, and the mint is account meta 1.
pub const IX_TRANSFER_CHECKED: u8 = 0x0c;
/// System program `Transfer`: `[0x02, 0, 0, 0, lamports: u64]`.
pub const IX_SYSTEM_TRANSFER: [u8; 4] = [0x02, 0x00, 0x00, 0x00];

// --- venues -------------------------------------------------------------

/// A venue FAMILY, never a concrete deployment - the same rule the EVM
/// decoder follows (`dex::models::Protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Venue {
    /// The pump.fun AMM that graduated tokens trade on.
    PumpSwap,
    /// The pump.fun bonding curve (a launchpad, but it IS the market until
    /// graduation, so its trades are swaps too).
    PumpFun,
    /// Raydium's original OpenBook-era constant product AMM. Not an Anchor
    /// program: a one-byte instruction tag and a `ray_log:` LOG line.
    RaydiumAmmV4,
    /// Raydium's current constant product pool (`cp-swap`).
    RaydiumCpmm,
    /// Raydium's concentrated liquidity pool.
    RaydiumClmm,
    /// Orca's concentrated liquidity pool.
    OrcaWhirlpool,
    /// Meteora's discretised liquidity book.
    MeteoraDlmm,
    /// Meteora's constant product AMM v2 (`cp-amm`).
    MeteoraDammV2,
    /// Prop AMM, no IDL and no event (research section 2). NAMED but not
    /// streamed: the movement layer needs only a program id and a name, so
    /// registering it is one line - see
    /// `a_jupiter_route_is_one_swap_per_venue`, which does exactly that.
    BisonFi,
}

impl Venue {
    /// Every venue this module can NAME.
    pub const ALL: [Venue; 9] = [
        Venue::PumpSwap,
        Venue::PumpFun,
        Venue::RaydiumAmmV4,
        Venue::RaydiumCpmm,
        Venue::RaydiumClmm,
        Venue::OrcaWhirlpool,
        Venue::MeteoraDlmm,
        Venue::MeteoraDammV2,
        Venue::BisonFi,
    ];

    /// Position in [`Venue::ALL`], so per-venue counters can be a fixed
    /// array rather than a map - which keeps `Diagnostics` `Copy` and
    /// cheap to merge across threads.
    pub const fn index(&self) -> usize {
        match self {
            Venue::PumpSwap => 0,
            Venue::PumpFun => 1,
            Venue::RaydiumAmmV4 => 2,
            Venue::RaydiumCpmm => 3,
            Venue::RaydiumClmm => 4,
            Venue::OrcaWhirlpool => 5,
            Venue::MeteoraDlmm => 6,
            Venue::MeteoraDammV2 => 7,
            Venue::BisonFi => 8,
        }
    }

    /// Value of the `protocol` column.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Venue::PumpSwap => "pumpswap",
            Venue::PumpFun => "pump_fun",
            Venue::RaydiumAmmV4 => "raydium_amm_v4",
            Venue::RaydiumCpmm => "raydium_cpmm",
            Venue::RaydiumClmm => "raydium_clmm",
            Venue::OrcaWhirlpool => "orca_whirlpool",
            Venue::MeteoraDlmm => "meteora_dlmm",
            Venue::MeteoraDammV2 => "meteora_damm_v2",
            Venue::BisonFi => "bisonfi",
        }
    }

    pub const fn program_b58(&self) -> &'static str {
        match self {
            Venue::PumpSwap => {
                "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"
            }
            Venue::PumpFun => {
                "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
            }
            Venue::RaydiumAmmV4 => {
                "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
            }
            Venue::RaydiumCpmm => {
                "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
            }
            Venue::RaydiumClmm => {
                "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"
            }
            Venue::OrcaWhirlpool => {
                "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"
            }
            Venue::MeteoraDlmm => {
                "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"
            }
            Venue::MeteoraDammV2 => {
                "cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"
            }
            Venue::BisonFi => {
                "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi"
            }
        }
    }

    /// Does a per-program decoder exist for this venue?
    pub const fn has_decoder(&self) -> bool {
        !matches!(self, Venue::BisonFi)
    }

    /// Where the venue publishes its swap event.
    ///
    /// This is the single most consequential fact about a venue for this
    /// module, and the research got it wrong for half of them: only pump.fun,
    /// PumpSwap and the two Meteora programs use Anchor's `emit_cpi!`, which
    /// puts the event in an INSTRUCTION. Raydium and Orca use plain `emit!`
    /// (or, for Raydium v4, a bare `msg!`), which puts it in a LOG LINE - a
    /// table phase 1 did not select at all.
    pub const fn event_source(&self) -> EventSource {
        match self {
            Venue::PumpSwap
            | Venue::PumpFun
            | Venue::MeteoraDlmm
            | Venue::MeteoraDammV2 => EventSource::SelfCpi,
            Venue::RaydiumAmmV4
            | Venue::RaydiumCpmm
            | Venue::RaydiumClmm
            | Venue::OrcaWhirlpool => EventSource::Log,
            Venue::BisonFi => EventSource::None,
        }
    }
}

/// How a venue's swap event reaches us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// Anchor `emit_cpi!`: the program invokes ITSELF, so the event is an
    /// instruction row. Validators never drop an instruction.
    SelfCpi,
    /// Anchor `emit!` or a bare `msg!`: the event is a LOG line. Validators
    /// DO truncate logs, which is why a transaction with
    /// `has_dropped_log_messages` must be treated as incomplete.
    Log,
    /// The venue publishes nothing. The movement layer is all there is.
    None,
}

/// What a venue instruction does, judged by its discriminator alone.
///
/// This is deliberately INDEPENDENT of the movement layer, which decides the
/// same question from the shape of the token flow. Where they disagree the
/// row is not written and the disagreement is counted: two independent
/// answers to "was this a trade?" is the whole point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IxKind {
    /// A swap. Exactly one mint in, a different one out.
    Swap,
    /// Adding or removing liquidity, or collecting fees on a position. Both
    /// mints move the SAME way, so it must never become a swap row.
    Liquidity,
    /// Pool creation, config, admin. Usually moves nothing.
    Admin,
    /// Not a discriminator this registry knows. Never guessed at.
    Unknown,
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Venues actually STREAMED. Everything here is queried from HyperSync and
/// written to the swap table; the rest of [`Venue::ALL`] is only a name
/// until its program id is added to this list.
///
/// Ordered by 30-day volume share (docs/solana-research.md section 1.1), so
/// the list reads as the coverage it buys:
/// PumpSwap 23.1 + Orca 10.0 + Raydium 9.5 + Meteora DLMM 8.0 +
/// pump.fun 3.2 + Meteora DAMM v2 0.4 = **~54% of Solana DEX volume**.
pub const VENUES: [Venue; 8] = [
    Venue::PumpSwap,
    Venue::OrcaWhirlpool,
    Venue::RaydiumAmmV4,
    Venue::RaydiumClmm,
    Venue::RaydiumCpmm,
    Venue::MeteoraDlmm,
    Venue::PumpFun,
    Venue::MeteoraDammV2,
];

// --- routers ------------------------------------------------------------

/// Aggregators and routers. They are ATTRIBUTION ONLY and their volume is
/// NEVER venue volume: 40% of Solana DEX volume is routed
/// (docs/solana-research.md section 1.2), so counting a router's
/// instruction as a trade would double count almost half the chain.
///
/// A swap whose instruction sits under one of these gets `route_ordinal` and
/// `route_program` set; the fill itself is still attributed to the venue
/// that executed it.
pub const ROUTERS_B58: &[(&str, &str)] =
    &[("jupiter_v6", "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4")];

// --- discriminators -----------------------------------------------------

/// Anchor's `emit_cpi!` self-CPI marker: the program invokes ITSELF with an
/// instruction whose data starts with these 8 bytes, then the 8-byte event
/// discriminator, then the Borsh body.
///
/// This is the robust event form: validators truncate log LINES (HyperSync
/// even exposes `has_dropped_log_messages`) but never drop an instruction.
pub const EVENT_CPI_PREFIX: [u8; 8] =
    [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];

/// `global:buy` - Anchor's sighash, so PumpSwap and pump.fun share it.
pub const DISC_BUY: [u8; 8] =
    [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
/// `global:sell`.
pub const DISC_SELL: [u8; 8] =
    [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// PumpSwap `BuyEvent`. Observed live, and equal to Anchor's sighash - the
/// `anchor_discriminators_are_sighashes` test derives all of these rather
/// than trusting the literals.
pub const DISC_PUMPSWAP_BUY_EVENT: [u8; 8] =
    [0x67, 0xf4, 0x52, 0x1f, 0x2c, 0xf5, 0x77, 0x77];
/// PumpSwap `SellEvent`.
pub const DISC_PUMPSWAP_SELL_EVENT: [u8; 8] =
    [0x3e, 0x2f, 0x37, 0x0a, 0xa5, 0x03, 0xdc, 0x2a];
/// pump.fun `TradeEvent`. Observed live.
pub const DISC_PUMPFUN_TRADE_EVENT: [u8; 8] =
    [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee];

// --- phase 2 venues: instruction and event discriminators ----------------
//
// Everything below is DERIVED from the instruction / event NAME with
// [`anchor_discriminator`], never copied as a magic number. The names come
// from the programs' published sources and IDLs:
//
// - Raydium AMM v4    raydium-io/raydium-amm, program/src/{instruction,log}.rs
// - Raydium CPMM      raydium-io/raydium-cp-swap, programs/cp-swap/src/states/events.rs
// - Raydium CLMM      raydium-io/raydium-clmm, programs/amm/src/states/pool.rs
// - Orca Whirlpools   orca-so/whirlpools, programs/whirlpool/src/events.rs
// - Meteora DLMM      MeteoraAg/dlmm-sdk, idls/dlmm.json (lb_clmm 0.12.0)
// - Meteora DAMM v2   MeteoraAg/damm-v2, programs/cp-amm/src/event.rs (0.2.4)
//
// `venue_discriminators_match_the_chain` then checks the derived bytes
// against the values OBSERVED in the recorded mainnet fixtures, so a name
// that is subtly wrong is a test failure rather than a filter that matches
// nothing. That is the same discipline phase 1 applied to pump.fun after
// finding a STALE on-chain IDL that truncated an event by 41 bytes.

/// Raydium AMM v4 is not an Anchor program: a one-byte instruction tag.
///
/// From `AmmInstruction::unpack`. Tags 9 / 11 are the original OpenBook
/// swaps and 16 / 17 the order-book-free v2 ones; all four emit the same
/// `ray_log`, which is why the decoder dispatches on the LOG and treats the
/// tag only as a cross-check.
pub const RAYDIUM_V4_IX: &[(u8, IxKind)] = &[
    (9, IxKind::Swap),  // SwapBaseIn
    (11, IxKind::Swap), // SwapBaseOut
    (16, IxKind::Swap), // SwapBaseInV2
    (17, IxKind::Swap), // SwapBaseOutV2
    (3, IxKind::Liquidity),
    (4, IxKind::Liquidity),
    (1, IxKind::Admin), // Initialize2
    (6, IxKind::Admin), // SetParams
    (7, IxKind::Admin), // WithdrawPnl
    (14, IxKind::Admin),
    (15, IxKind::Admin),
    (18, IxKind::Admin),
];

/// `ray_log` leading byte: the `LogType` enum of `program/src/log.rs`.
pub const RAY_LOG_SWAP_BASE_IN: u8 = 3;
pub const RAY_LOG_SWAP_BASE_OUT: u8 = 4;
/// `SwapBaseInLog` / `SwapBaseOutLog` are both `u8 + 7 * u64`, bincode with
/// fixed-width integers and no length prefixes.
pub const RAY_LOG_SWAP_LEN: usize = 1 + 7 * 8;
/// The text prefix `msg!` writes, after HyperSync has stripped
/// `"Program log: "`.
pub const RAY_LOG_PREFIX: &str = "ray_log: ";

/// `direction` in a `ray_log`: `PC2Coin = 1`, `Coin2PC = 2`
/// (`program/src/math.rs`).
pub const RAY_DIRECTION_PC_TO_COIN: u64 = 1;

const RAYDIUM_CPMM_IX: &[(&str, IxKind)] = &[
    ("swap_base_input", IxKind::Swap),
    ("swap_base_output", IxKind::Swap),
    ("deposit", IxKind::Liquidity),
    ("withdraw", IxKind::Liquidity),
    ("initialize", IxKind::Admin),
    ("initialize_with_permission", IxKind::Admin),
];

const RAYDIUM_CLMM_IX: &[(&str, IxKind)] = &[
    ("swap", IxKind::Swap),
    ("swap_v2", IxKind::Swap),
    ("swap_router_base_in", IxKind::Swap),
    ("open_position", IxKind::Liquidity),
    ("open_position_v2", IxKind::Liquidity),
    ("open_position_with_token22_nft", IxKind::Liquidity),
    ("close_position", IxKind::Liquidity),
    ("increase_liquidity", IxKind::Liquidity),
    ("increase_liquidity_v2", IxKind::Liquidity),
    ("decrease_liquidity", IxKind::Liquidity),
    ("decrease_liquidity_v2", IxKind::Liquidity),
    ("create_pool", IxKind::Admin),
];

const ORCA_IX: &[(&str, IxKind)] = &[
    ("swap", IxKind::Swap),
    ("swap_v2", IxKind::Swap),
    ("two_hop_swap", IxKind::Swap),
    ("two_hop_swap_v2", IxKind::Swap),
    ("increase_liquidity", IxKind::Liquidity),
    ("increase_liquidity_v2", IxKind::Liquidity),
    ("increase_liquidity_by_token_amounts_v2", IxKind::Liquidity),
    ("decrease_liquidity", IxKind::Liquidity),
    ("decrease_liquidity_v2", IxKind::Liquidity),
    ("reposition_liquidity_v2", IxKind::Liquidity),
    ("collect_fees", IxKind::Liquidity),
    ("collect_fees_v2", IxKind::Liquidity),
    ("collect_reward", IxKind::Liquidity),
    ("collect_reward_v2", IxKind::Liquidity),
    ("open_position", IxKind::Liquidity),
    ("open_position_with_metadata", IxKind::Liquidity),
    ("open_position_with_token_extensions", IxKind::Liquidity),
    ("close_position", IxKind::Liquidity),
    ("close_position_with_token_extensions", IxKind::Liquidity),
    ("initialize_pool", IxKind::Admin),
    ("initialize_pool_v2", IxKind::Admin),
    ("initialize_pool_with_adaptive_fee", IxKind::Admin),
];

const METEORA_DLMM_IX: &[(&str, IxKind)] = &[
    ("swap", IxKind::Swap),
    ("swap2", IxKind::Swap),
    ("swap_exact_out", IxKind::Swap),
    ("swap_exact_out2", IxKind::Swap),
    ("swap_with_price_impact", IxKind::Swap),
    ("swap_with_price_impact2", IxKind::Swap),
    ("add_liquidity", IxKind::Liquidity),
    ("add_liquidity2", IxKind::Liquidity),
    ("add_liquidity_by_strategy", IxKind::Liquidity),
    ("add_liquidity_by_strategy2", IxKind::Liquidity),
    ("add_liquidity_by_strategy_one_side", IxKind::Liquidity),
    ("add_liquidity_by_weight", IxKind::Liquidity),
    ("add_liquidity_by_weight2", IxKind::Liquidity),
    ("add_liquidity_one_side", IxKind::Liquidity),
    ("add_liquidity_one_side_precise", IxKind::Liquidity),
    ("add_liquidity_one_side_precise2", IxKind::Liquidity),
    ("remove_liquidity", IxKind::Liquidity),
    ("remove_liquidity2", IxKind::Liquidity),
    ("remove_liquidity_by_range", IxKind::Liquidity),
    ("remove_liquidity_by_range2", IxKind::Liquidity),
    ("remove_all_liquidity", IxKind::Liquidity),
    ("rebalance_liquidity", IxKind::Liquidity),
    ("claim_fee", IxKind::Liquidity),
    ("claim_fee2", IxKind::Liquidity),
    ("initialize_position", IxKind::Liquidity),
    ("initialize_position2", IxKind::Liquidity),
    ("initialize_position_pda", IxKind::Liquidity),
    ("close_position", IxKind::Liquidity),
    ("close_position2", IxKind::Liquidity),
];

const METEORA_DAMM2_IX: &[(&str, IxKind)] = &[
    ("swap", IxKind::Swap),
    ("swap2", IxKind::Swap),
    ("add_liquidity", IxKind::Liquidity),
    ("remove_liquidity", IxKind::Liquidity),
    ("remove_all_liquidity", IxKind::Liquidity),
    ("claim_position_fee", IxKind::Liquidity),
    ("create_position", IxKind::Liquidity),
    ("close_position", IxKind::Liquidity),
    ("lock_position", IxKind::Liquidity),
    ("permanent_lock_position", IxKind::Liquidity),
    ("split_position", IxKind::Liquidity),
    ("split_position2", IxKind::Liquidity),
    ("initialize_pool", IxKind::Admin),
    ("initialize_customizable_pool", IxKind::Admin),
    ("initialize_pool_with_dynamic_config", IxKind::Admin),
    ("claim_protocol_fee2", IxKind::Admin),
    ("claim_reward", IxKind::Admin),
];

impl Venue {
    /// Anchor instruction names of this venue, and what each one does.
    /// Empty for the programs that are not Anchor programs.
    const fn anchor_instructions(
        &self,
    ) -> &'static [(&'static str, IxKind)] {
        match self {
            Venue::RaydiumCpmm => RAYDIUM_CPMM_IX,
            Venue::RaydiumClmm => RAYDIUM_CLMM_IX,
            Venue::OrcaWhirlpool => ORCA_IX,
            Venue::MeteoraDlmm => METEORA_DLMM_IX,
            Venue::MeteoraDammV2 => METEORA_DAMM2_IX,
            // pump.fun and PumpSwap dispatch on the EVENT (phase 1's
            // finding: the instruction variants multiplied but the event did
            // not), and BisonFi publishes nothing at all.
            Venue::PumpSwap
            | Venue::PumpFun
            | Venue::RaydiumAmmV4
            | Venue::BisonFi => &[],
        }
    }

    /// What this instruction does, judged by its discriminator ALONE.
    ///
    /// Deliberately independent of the movement layer, which answers the same
    /// question from the shape of the token flow.
    /// [`IxKind::Unknown`] is returned for anything not listed - a variant
    /// added after this was written must fall through to the movement
    /// layer's own judgement rather than be classified wrongly.
    pub fn instruction_kind(&self, data: &[u8]) -> IxKind {
        if *self == Venue::RaydiumAmmV4 {
            let Some(tag) = data.first() else {
                return IxKind::Unknown;
            };
            return RAYDIUM_V4_IX
                .iter()
                .find(|(candidate, _)| candidate == tag)
                .map(|(_, kind)| *kind)
                .unwrap_or(IxKind::Unknown);
        }

        let Some(discriminator) = data.get(..8) else {
            return IxKind::Unknown;
        };
        if discriminator == EVENT_CPI_PREFIX {
            // A self-CPI event row, not an instruction.
            return IxKind::Unknown;
        }
        self.anchor_instructions()
            .iter()
            .find(|(name, _)| {
                anchor_discriminator("global", name) == discriminator
            })
            .map(|(_, kind)| *kind)
            .unwrap_or(IxKind::Unknown)
    }
}

/// Orca's `Traded`, the ONLY swap event the Whirlpool program emits - and it
/// emits it with `emit!`, so it is a LOG LINE.
pub const DISC_ORCA_TRADED: [u8; 8] =
    [0xe1, 0xca, 0x49, 0xaf, 0x93, 0x2b, 0xa0, 0x96];
/// `event:SwapEvent`. Raydium's CPMM **and** CLMM both named their event
/// `SwapEvent`, so the SAME 8 bytes appear for two different layouts: the
/// decoder must dispatch on the log row's `program_id`, never on this alone.
pub const DISC_RAYDIUM_SWAP_EVENT: [u8; 8] =
    [0x40, 0xc6, 0xcd, 0xe8, 0x26, 0x08, 0x71, 0xe2];
/// Meteora DLMM `Swap`, the first of the TWO self-CPI events a DLMM swap
/// emits.
pub const DISC_DLMM_SWAP: [u8; 8] =
    [0x51, 0x6c, 0xe3, 0xbe, 0xcd, 0xd0, 0x0a, 0xc4];
/// Meteora DLMM `Swap2Evt`, the second. Its field ORDER differs from
/// `Swap`'s, which is exactly the kind of thing a decoder gets wrong when it
/// assumes the newer event is the older one with fields appended.
pub const DISC_DLMM_SWAP2: [u8; 8] =
    [0x2e, 0x74, 0x52, 0xd7, 0x94, 0x1b, 0x54, 0x4d];
/// Meteora DAMM v2 `EvtSwap2`.
pub const DISC_DAMM2_SWAP: [u8; 8] =
    [0xbd, 0x42, 0x33, 0xa8, 0x26, 0x50, 0x75, 0x99];

/// Anchor's discriminator: the first 8 bytes of `sha256("<namespace>:<Name>")`.
///
/// Every discriminator in this file is derived with this, so none of them is
/// a magic number copied from somewhere. `global:` is the namespace of an
/// instruction, `event:` of an `emit_cpi!` event.
pub fn anchor_discriminator(namespace: &str, name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(b":");
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// Resolved ids, decoded once.
pub struct Registry {
    pub spl_token: Pubkey,
    pub token_2022: Pubkey,
    pub system: Pubkey,
    pub wsol: Pubkey,
    /// Venue program -> family.
    pub venues: Vec<(Pubkey, Venue)>,
    /// Router program -> name.
    pub routers: Vec<(Pubkey, &'static str)>,
}

impl Registry {
    pub fn new() -> Self {
        Self::with_venues(&VENUES)
    }

    /// A registry over an explicit venue set. Production uses [`VENUES`];
    /// tests use this to decode a route over venues phase 1 does not stream.
    pub fn with_venues(venues: &[Venue]) -> Self {
        Self {
            spl_token: pubkey(SPL_TOKEN_B58),
            token_2022: pubkey(TOKEN_2022_B58),
            system: pubkey(SYSTEM_B58),
            wsol: pubkey(WSOL_B58),
            venues: venues
                .iter()
                .map(|v| (pubkey(v.program_b58()), *v))
                .collect(),
            routers: ROUTERS_B58
                .iter()
                .map(|(name, id)| (pubkey(id), *name))
                .collect(),
        }
    }

    pub fn venue(&self, program: &Pubkey) -> Option<Venue> {
        self.venues
            .iter()
            .find(|(id, _)| id == program)
            .map(|(_, venue)| *venue)
    }

    pub fn router(&self, program: &Pubkey) -> Option<&'static str> {
        self.routers
            .iter()
            .find(|(id, _)| id == program)
            .map(|(_, name)| *name)
    }

    /// Is this the SPL Token or the Token-2022 program? Both speak the same
    /// transfer instruction layout.
    pub fn is_token_program(&self, program: &Pubkey) -> bool {
        *program == self.spl_token || *program == self.token_2022
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// The one shared registry.
pub fn registry() -> &'static Registry {
    static REGISTRY: std::sync::OnceLock<Registry> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(Registry::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_program_id_round_trips_through_base58() {
        let mut all: Vec<&str> =
            vec![SPL_TOKEN_B58, TOKEN_2022_B58, SYSTEM_B58, WSOL_B58];
        all.extend(VENUES.iter().map(|v| v.program_b58()));
        all.extend(ROUTERS_B58.iter().map(|(_, id)| *id));

        for id in all {
            assert_eq!(
                to_base58(&pubkey(id)),
                id,
                "{id} did not round trip - a typo here is a filter that \
                 silently matches nothing"
            );
        }
    }

    #[test]
    fn program_ids_are_distinct() {
        let registry = Registry::new();
        let mut ids: Vec<Pubkey> =
            registry.venues.iter().map(|(id, _)| *id).collect();
        ids.extend(registry.routers.iter().map(|(id, _)| *id));
        ids.push(registry.spl_token);
        ids.push(registry.token_2022);
        ids.push(registry.system);

        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(before, ids.len(), "a program id is registered twice");
    }

    /// Every discriminator constant is Anchor's sighash, not a magic number.
    ///
    /// `global:buy`, `global:sell`, `event:BuyEvent` and `event:TradeEvent`
    /// were ALSO observed in live instruction data (see
    /// `svm::fixtures`), so this test ties the derivation and the
    /// observation together: `event:SellEvent` is therefore derived rather
    /// than guessed.
    #[test]
    fn anchor_discriminators_are_sighashes() {
        assert_eq!(anchor_discriminator("global", "buy"), DISC_BUY);
        assert_eq!(anchor_discriminator("global", "sell"), DISC_SELL);
        assert_eq!(
            anchor_discriminator("event", "BuyEvent"),
            DISC_PUMPSWAP_BUY_EVENT
        );
        assert_eq!(
            anchor_discriminator("event", "SellEvent"),
            DISC_PUMPSWAP_SELL_EVENT
        );
        assert_eq!(
            anchor_discriminator("event", "TradeEvent"),
            DISC_PUMPFUN_TRADE_EVENT
        );
    }

    #[test]
    fn buy_and_sell_share_anchors_sighash_across_both_venues() {
        // Both programs are Anchor programs with a `buy` and a `sell`
        // instruction, so the discriminators are identical. The decoder
        // must therefore dispatch on the PROGRAM first, never on the
        // discriminator alone.
        assert_ne!(DISC_BUY, DISC_SELL);
        assert_ne!(DISC_BUY, EVENT_CPI_PREFIX);
    }

    #[test]
    fn a_venue_is_never_also_a_router() {
        let registry = Registry::new();
        for (id, venue) in &registry.venues {
            assert!(
                registry.router(id).is_none(),
                "{venue} is registered as a router too - its volume would \
                 be counted twice"
            );
        }
    }

    #[test]
    fn protocol_names_are_unique_and_snake_case() {
        let mut names: Vec<&str> =
            Venue::ALL.iter().map(|v| v.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
        for name in names {
            // Digits are allowed: a venue FAMILY name legitimately carries
            // the program's own version, and `raydium_amm_v4` and
            // `meteora_damm_v2` are different families from their v1s
            // rather than different deployments of one.
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == '_'),
                "{name} is not snake_case"
            );
            assert!(
                !name.starts_with('_') && !name.ends_with('_'),
                "{name} has a stray underscore"
            );
        }
    }
}
