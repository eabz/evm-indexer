//! The program registry: which Solana programs are venues, which are
//! routers, and which move tokens.
//!
//! Every id here is quoted in base58 exactly as
//! the Solana venue research appendix B records it, and decoded to 32 bytes
//! by [`pubkey`]. A unit test round-trips every entry back to base58, so a
//! typo is a test failure and not a filter that silently matches nothing.
//!
//! Phase 1 registers the two venues with a per-program decoder (PumpSwap and
//! the pump.fun bonding curve). The generic movement decoder works on ANY
//! program in [`VENUES`], so adding a venue is one line here plus a
//! `protocol` name - that is the whole point of the two-layer design in
//! the Solana venue research section 3.3.

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
    /// Meteora's Dynamic Bonding Curve: a LAUNCHPAD whose curve is also the
    /// market until migration, exactly like the pump.fun curve. bags.fm and
    /// the other DBC front ends are configurations of this ONE program and
    /// are never venues of their own (see `launchpads.rs`).
    MeteoraDbc,
    /// Raydium LaunchLab: the launchpad behind StonkFun, BONK.fun / LetsBonk
    /// and Raydium's own launches. Same story - the platforms are
    /// `platform_config` accounts under one program.
    RaydiumLaunchlab,
    /// Prop AMM, no IDL and no event (research section 2). NAMED but not
    /// streamed: the movement layer needs only a program id and a name, so
    /// registering it is one line - see
    /// `a_jupiter_route_is_one_swap_per_venue`, which does exactly that.
    BisonFi,
}

impl Venue {
    /// Every venue this module can NAME.
    pub const ALL: [Venue; 11] = [
        Venue::PumpSwap,
        Venue::PumpFun,
        Venue::RaydiumAmmV4,
        Venue::RaydiumCpmm,
        Venue::RaydiumClmm,
        Venue::OrcaWhirlpool,
        Venue::MeteoraDlmm,
        Venue::MeteoraDammV2,
        Venue::MeteoraDbc,
        Venue::RaydiumLaunchlab,
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
            Venue::MeteoraDbc => 8,
            Venue::RaydiumLaunchlab => 9,
            Venue::BisonFi => 10,
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
            Venue::MeteoraDbc => "meteora_dbc",
            Venue::RaydiumLaunchlab => "raydium_launchlab",
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
            Venue::MeteoraDbc => {
                "dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN"
            }
            Venue::RaydiumLaunchlab => {
                "LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj"
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

    /// Is the common OWNER of this venue's two vault token accounts one
    /// program-wide account rather than a per-pool one?
    ///
    /// This is the single most dangerous fact about a Solana venue for an
    /// indexer, and it is true for half of the streamed ones. The movement
    /// layer identifies a pool as "the common counterparty of both legs",
    /// which is the vault OWNER; for these five venues that owner is one
    /// PDA shared by every pool the program runs:
    ///
    /// | Venue | The one authority |
    /// |---|---|
    /// | Raydium AMM v4 | `5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1` |
    /// | Raydium CPMM | `GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL` |
    /// | Meteora DAMM v2 | `HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC` |
    /// | Meteora DBC | `FhVo3mqL8PW5pH5U2CN4XE33DokiyZnUwuGpH2hmHLuM` |
    /// | Raydium LaunchLab | `WLHv2UAZm6z4KyaaELi5pjdbJh6RESMva1Rnn8pJVVh` |
    ///
    /// Storing that account as `pool_id` would key EVERY pair of the venue
    /// into ONE candle series: open/high/low/close would mix USDC/SOL with
    /// arbitrary memecoin prices and the volumes would sum amounts of
    /// unrelated mints with unrelated decimals. So it is never stored -
    /// see [`Venue::pool_from_accounts`], and the row is left out of the
    /// pool-keyed aggregates when the pool cannot be named.
    ///
    /// Orca, Raydium CLMM, Meteora DLMM, PumpSwap and the pump.fun curve
    /// own their vaults per pool and are fine.
    pub const fn vault_authority_is_global(&self) -> bool {
        matches!(
            self,
            Venue::RaydiumAmmV4
                | Venue::RaydiumCpmm
                | Venue::MeteoraDammV2
                | Venue::MeteoraDbc
                | Venue::RaydiumLaunchlab
        )
    }

    /// Where the pool state account sits in the account metas of a SWAP
    /// instruction of this venue.
    ///
    /// Only needed for the venues of [`Venue::vault_authority_is_global`],
    /// and only ever consulted for an instruction the registry already
    /// knows is a swap, so a variant added after this was written yields
    /// `None` rather than some other account. Every index here was read off
    /// a RECORDED mainnet instruction and is asserted against the pool the
    /// venue's own event names by
    /// `the_pool_account_index_is_the_account_the_venue_names`.
    ///
    /// `None` for Meteora DAMM v2: no recording of one of its swaps exists
    /// yet, and an unverified index would store the WRONG pool, which is
    /// worse than storing none.
    pub const fn pool_account_index(&self) -> Option<usize> {
        match self {
            // tags 9 / 11 / 16 / 17, all four with the same prefix.
            Venue::RaydiumAmmV4 => Some(1),
            // payer, authority, amm_config, POOL_STATE, ...
            Venue::RaydiumCpmm => Some(3),
            // pool_authority, config, POOL, input_token_account, ...
            Venue::MeteoraDbc => Some(2),
            // payer, authority, global_config, platform_config, POOL, ...
            Venue::RaydiumLaunchlab => Some(4),
            _ => None,
        }
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
            | Venue::MeteoraDammV2
            | Venue::MeteoraDbc
            | Venue::RaydiumLaunchlab => EventSource::SelfCpi,
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
/// Ordered by 30-day volume share (the Solana venue research section 1.1), so
/// the list reads as the coverage it buys:
/// PumpSwap 23.1 + Orca 10.0 + Raydium 9.5 + Meteora DLMM 8.0 +
/// pump.fun 3.2 + Meteora DAMM v2 0.4 = **~54% of Solana DEX volume**.
pub const VENUES: [Venue; 10] = [
    Venue::PumpSwap,
    Venue::OrcaWhirlpool,
    Venue::RaydiumAmmV4,
    Venue::RaydiumClmm,
    Venue::RaydiumCpmm,
    Venue::MeteoraDlmm,
    Venue::PumpFun,
    Venue::MeteoraDammV2,
    Venue::MeteoraDbc,
    Venue::RaydiumLaunchlab,
];

// --- routers ------------------------------------------------------------

/// Aggregators and routers. They are ATTRIBUTION ONLY and their volume is
/// NEVER venue volume: 40% of Solana DEX volume is routed
/// (the Solana venue research section 1.2), so counting a router's
/// instruction as a trade would double count almost half the chain.
///
/// A swap whose instruction sits under one of these gets `route_ordinal` and
/// `route_program` set; the fill itself is still attributed to the venue
/// that executed it.
///
/// Registering only Jupiter v6 left the other ~60% of routed flow reading as
/// DIRECT trade (review round 4, M9). Nothing about VOLUME changes when a
/// router is added - the nearest-registered-venue subtree rule attributes
/// the fill to the venue either way, and an unregistered router is simply
/// transparent - so this list is attribution and only attribution.
///
/// Provenance: DefiLlama's aggregator table names the shares (Jupiter
/// $16.6B, DFlow $9.3B, OKX $4.0B, Titan $0.7B over 30 days,
/// the Solana venue research §1.2); every id below was taken from the
/// operator's OWN repository or from Jupiter's official platform list and
/// then checked on mainnet, where each is an executable program. The six
/// ids the Solana venue research records only as a PREFIX are resolved here,
/// and `routers_match_the_prefixes_the_research_recorded` asserts each full
/// id against the recorded prefix so a wrong completion is a test failure.
///
/// Two programs the research listed under "routers seen in real
/// transactions" are deliberately NOT here:
///
/// * `MAyhSmzX...` is pump.fun's own "Mayhem Mode" program, a launchpad
///   program and not an aggregator at all.
/// * a venue is never a router (`a_venue_is_never_also_a_router`).
pub const ROUTERS_B58: &[(&str, &str)] = &[
    // Jupiter, every deployed version. The older programs still carry
    // flow, and each one is a different id: a route under v4 read as a
    // direct trade until this list grew.
    ("jupiter_v1", "JUP6i4ozu5ydDCnLiMogSckDPpbtr7BJ4FtzYWkb5Rk"),
    ("jupiter_v2", "JUP2jxvXaqu7NQY1GmNF4m1vodw12LVXYxbFL2uJvfo"),
    ("jupiter_v3", "JUP3c2Uh3WA4Ng34tw6kPd2G4C5BB21Xo36Je1s32Ph"),
    ("jupiter_v4", "JUP4Fb2cqiRUcaTHdrPC8h2gNsA2ETXiPDD33WcGuJB"),
    ("jupiter_v5", "JUP5pEAZeHdHrLxh5UCwAbpjGwYKKoquCpda2hfP4u8"),
    ("jupiter_v6", "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"),
    ("jupiter_v7", "JUP7pNXFL1G2BESRYMtZ1jepzfDQVffkkkf5JhXWWhC"),
    // The second largest aggregator on the chain.
    ("dflow", "DF1ow4tspfHX9JwWJsAb9epbkA8hmpSEAtxXy1V27QBH"),
    // OKX ships two live router versions; both appear in current traffic.
    ("okx_v2", "6m2CDdhRgxpH4WjvdzxAYbGxwdGUz5MziiL5jek2kBma"),
    ("okx_v6", "proVF4pMXVaYqmy4NjniPh4pqKNfMmsihgd4wdkCX3u"),
    ("titan", "T1TANpTeScyeqVzzgNViGDNrkQ6qHz9KrSBS4aNXvGT"),
    // Front ends and bots. They route into somebody else's venue exactly
    // as an aggregator does, so the same rule applies: attribution, never
    // volume.
    ("photon", "BSfD6SHZigAfDWSjzD5Q41jw8LmKwtmjskPH9XW1mrRW"),
    ("trojan", "troyXT7Ty3s2rjJe4bqWaroUrS4Fjd8rbHHNHxcACF4"),
    ("ave", "AveaiuA1emN71q9mS2QQ9BEWNAAHmp8sHSvwLFHQjufM"),
    // `term9YPb...` in the research. It is Terminal (formerly Padre), a
    // trading terminal - NOT Titan, which is the separate id above.
    ("terminal", "term9YPb9mzAsABaqN71A4xdbxHmpBNZavpBiQKZzN3"),
    // The research recorded this prefix as a router seen live and the id
    // is a real deployed program, but no source attributes it to GMGN the
    // company; public explorers label it only as an arbitrage bot. The
    // NAME is therefore the weakest claim on this list. Nothing depends on
    // it being right: a wrong name on an attribution column cannot move
    // volume anywhere.
    ("gmgn", "GMGNreQcJFufBiCTLDBgKhYEfEe9B454UjpDr5CaSLA1"),
];

/// The router prefixes the Solana venue research §1.2 recorded from real
/// mainnet transactions, and the full id each one resolves to. A completion
/// that does not start with the observed prefix is a wrong program.
pub const ROUTER_PREFIXES: &[(&str, &str)] = &[
    ("DF1ow4ts", "dflow"),
    ("proVF4pM", "okx_v6"),
    ("GMGNreQc", "gmgn"),
    ("AveaiuA1", "ave"),
    ("term9YPb", "terminal"),
    ("JUP6Lkb", "jupiter_v6"),
];

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

/// Meteora Dynamic Bonding Curve, from `MeteoraAg/dynamic-bonding-curve`
/// v0.2.1 `programs/dynamic-bonding-curve/src/lib.rs`.
///
/// The on-chain IDL account of this program is STALE (it still advertises
/// 0.1.10, with two instructions the deployed binary no longer has and none
/// of the 0.2.x transfer-hook work), so the names come from the source at
/// the release commit whose deploy timestamp matches the live ProgramData.
/// This is the second time an on-chain IDL has been wrong in this module.
const METEORA_DBC_IX: &[(&str, IxKind)] = &[
    ("swap", IxKind::Swap),
    ("swap2", IxKind::Swap),
    ("swap2_with_transfer_hook", IxKind::Swap),
    // Both legs leave the pool together, which is the shape
    // `IxKind::Liquidity` names.
    ("claim_trading_fee", IxKind::Liquidity),
    ("claim_trading_fee2", IxKind::Liquidity),
    ("claim_creator_trading_fee", IxKind::Liquidity),
    ("claim_creator_trading_fee2", IxKind::Liquidity),
    ("claim_protocol_fee2", IxKind::Liquidity),
    ("partner_withdraw_surplus", IxKind::Liquidity),
    ("creator_withdraw_surplus", IxKind::Liquidity),
    ("withdraw_leftover", IxKind::Liquidity),
    ("withdraw_migration_fee", IxKind::Liquidity),
    ("migrate_meteora_damm", IxKind::Liquidity),
    ("migration_damm_v2", IxKind::Liquidity),
    ("migrate_meteora_damm_lock_lp_token", IxKind::Liquidity),
    ("migrate_meteora_damm_claim_lp_token", IxKind::Liquidity),
    ("create_locker", IxKind::Liquidity),
    ("initialize_virtual_pool_with_spl_token", IxKind::Admin),
    ("initialize_virtual_pool_with_token2022", IxKind::Admin),
    (
        "initialize_virtual_pool_with_token2022_transfer_hook",
        IxKind::Admin,
    ),
    ("create_config", IxKind::Admin),
    ("create_config_with_transfer_hook", IxKind::Admin),
    ("create_partner_metadata", IxKind::Admin),
    ("create_virtual_pool_metadata", IxKind::Admin),
    ("migration_meteora_damm_create_metadata", IxKind::Admin),
    ("migration_damm_v2_create_metadata", IxKind::Admin),
    ("create_token_badge", IxKind::Admin),
    ("close_token_badge", IxKind::Admin),
    ("create_operator_account", IxKind::Admin),
    ("close_operator_account", IxKind::Admin),
    ("transfer_pool_creator", IxKind::Admin),
    ("claim_partner_pool_creation_fee", IxKind::Admin),
    ("claim_protocol_pool_creation_fee", IxKind::Admin),
    ("close_claim_protocol_fee_operator", IxKind::Admin),
];

/// Raydium LaunchLab, from the DEPLOYED program's own on-chain IDL
/// (`metadata.version` 0.2.0). The program is closed source and the public
/// `raydium-io/raydium-idl` copy is stale - it lacks `claim_creator_fee`,
/// `collect_excess_lamports` and the platform curve rules - so the on-chain
/// IDL is the only current source.
const RAYDIUM_LAUNCHLAB_IX: &[(&str, IxKind)] = &[
    ("buy_exact_in", IxKind::Swap),
    ("buy_exact_out", IxKind::Swap),
    ("sell_exact_in", IxKind::Swap),
    ("sell_exact_out", IxKind::Swap),
    ("migrate_to_amm", IxKind::Liquidity),
    ("migrate_to_cpswap", IxKind::Liquidity),
    ("claim_platform_fee", IxKind::Liquidity),
    ("claim_platform_fee_from_vault", IxKind::Liquidity),
    ("claim_creator_fee", IxKind::Liquidity),
    ("claim_vested_token", IxKind::Liquidity),
    ("collect_fee", IxKind::Liquidity),
    ("collect_migrate_fee", IxKind::Liquidity),
    ("initialize", IxKind::Admin),
    ("initialize_v2", IxKind::Admin),
    ("initialize_with_token_2022", IxKind::Admin),
    ("create_config", IxKind::Admin),
    ("update_config", IxKind::Admin),
    ("create_platform_config", IxKind::Admin),
    ("update_platform_config", IxKind::Admin),
    ("create_vesting_account", IxKind::Admin),
    ("create_platform_vesting_account", IxKind::Admin),
    ("create_platform_curve_rule", IxKind::Admin),
    ("update_platform_curve_rule", IxKind::Admin),
    ("remove_platform_curve_rule", IxKind::Admin),
    ("close_platform_curve_rule", IxKind::Admin),
    ("create_platform_allow_config", IxKind::Admin),
    ("close_platform_allow_config", IxKind::Admin),
    ("collect_excess_lamports", IxKind::Admin),
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
            Venue::MeteoraDbc => METEORA_DBC_IX,
            Venue::RaydiumLaunchlab => RAYDIUM_LAUNCHLAB_IX,
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

// --- launchpad events ----------------------------------------------------
//
// Every one of these is DERIVED from the event NAME by
// `launchpad_discriminators_are_sighashes`, never copied as a magic number,
// and every layout states the exact byte count it implies so a program that
// appends a field shows up as "event not found" rather than as a wrong
// number (the rule the phase 2 venues already follow).
//
// ONE COLLISION MATTERS: pump.fun's `TradeEvent` and Raydium LaunchLab's
// `TradeEvent` have the SAME discriminator, because Anchor hashes only the
// struct name and both programs chose it. Their payloads have nothing in
// common - 139 fixed bytes against 363 + two variable-length fields - so a
// dispatcher keyed on the discriminator alone silently reads one as the
// other. Everything here dispatches on (program, discriminator).

/// pump.fun `CreateEvent`: the launch. Three Borsh `String`s at the FRONT,
/// so every field of it is at a variable offset.
pub const DISC_PUMPFUN_CREATE: [u8; 8] =
    [0x1b, 0x72, 0xa9, 0x4d, 0xde, 0xeb, 0x63, 0x76];
/// pump.fun `CompleteEvent`: the curve FILLED. Names no destination pool -
/// at this point there is none.
pub const DISC_PUMPFUN_COMPLETE: [u8; 8] =
    [0x5f, 0x72, 0x61, 0x9c, 0xd4, 0x2e, 0x98, 0x08];
/// pump.fun `CompletePumpAmmMigrationEvent`: the liquidity actually moved
/// into PumpSwap. This one DOES name the destination `pool`, which is the
/// join key into `sol_dex_swaps`.
pub const DISC_PUMPFUN_MIGRATED: [u8; 8] =
    [0xbd, 0xe9, 0x5d, 0xb9, 0x5c, 0x94, 0xea, 0x94];
/// pump.fun `CollectCreatorFeeEvent`: a creator actually swept their vault.
pub const DISC_PUMPFUN_CREATOR_FEE: [u8; 8] =
    [0x7a, 0x02, 0x7f, 0x01, 0x0e, 0xbf, 0x0c, 0xaf];

/// Meteora DBC `EvtInitializePool`: the launch.
pub const DISC_DBC_INITIALIZE_POOL: [u8; 8] =
    [0xe4, 0x32, 0xf6, 0x55, 0xcb, 0x42, 0x86, 0x25];
/// Meteora DBC `EvtSwap2`, the richer of the two events every DBC swap
/// emits: it is the only one carrying `quote_reserve_amount` and
/// `migration_threshold`, i.e. the curve's PROGRESS.
pub const DISC_DBC_SWAP2: [u8; 8] =
    [0xbd, 0x42, 0x33, 0xa8, 0x26, 0x50, 0x75, 0x99];
/// Meteora DBC `EvtSwap`, the older event, emitted alongside `EvtSwap2`.
pub const DISC_DBC_SWAP: [u8; 8] =
    [0x1b, 0x3c, 0x15, 0xd5, 0x8a, 0xaa, 0xbb, 0x93];
/// Meteora DBC `EvtCurveComplete`: the curve filled and the pool is ready to
/// migrate. The migration instructions themselves emit NOTHING.
pub const DISC_DBC_CURVE_COMPLETE: [u8; 8] =
    [0xe5, 0xe7, 0x56, 0x54, 0x9c, 0x86, 0x4b, 0x18];
/// Meteora DBC `EvtCreateConfigV2`: the PARTNER's configuration, and the
/// only place `fee_claimer` - the front end's wallet - is published.
pub const DISC_DBC_CREATE_CONFIG_V2: [u8; 8] =
    [0xa3, 0x4a, 0x42, 0xbb, 0x77, 0xc3, 0x1a, 0x90];
/// Meteora DBC `EvtCreateConfig`, the deprecated form, still emitted
/// alongside V2.
pub const DISC_DBC_CREATE_CONFIG: [u8; 8] =
    [0x83, 0xcf, 0xb4, 0xae, 0xb4, 0x49, 0xa5, 0x36];
/// Meteora DBC `EvtClaimCreatorTradingFee`.
pub const DISC_DBC_CLAIM_CREATOR_FEE: [u8; 8] =
    [0x9a, 0xe4, 0xd7, 0xca, 0x85, 0x9b, 0xd6, 0x8a];
/// Meteora DBC `EvtClaimTradingFee`: the PARTNER's (front end's) share.
pub const DISC_DBC_CLAIM_TRADING_FEE: [u8; 8] =
    [0x1a, 0x53, 0x75, 0xf0, 0x5c, 0xca, 0x70, 0xfe];
/// Meteora DBC `EvtCreatorWithdrawSurplus`.
pub const DISC_DBC_CREATOR_SURPLUS: [u8; 8] =
    [0x98, 0x49, 0x15, 0x0f, 0x42, 0x57, 0x35, 0x9d];
/// Meteora DBC `EvtPartnerWithdrawSurplus`.
pub const DISC_DBC_PARTNER_SURPLUS: [u8; 8] =
    [0xc3, 0x38, 0x98, 0x09, 0xe8, 0x48, 0x23, 0x16];

/// Raydium LaunchLab `PoolCreateEvent`: the launch. Three Borsh `String`s
/// inside `base_mint_param`, and it does NOT name the mint - that comes from
/// the instruction's own accounts, checked against the pool PDA.
pub const DISC_LAUNCHLAB_POOL_CREATE: [u8; 8] =
    [0x97, 0xd7, 0xe2, 0x09, 0x76, 0xa1, 0x73, 0xae];
/// Raydium LaunchLab `TradeEvent`. 139 fixed bytes - and the SAME
/// discriminator as pump.fun's, see the note above.
pub const DISC_LAUNCHLAB_TRADE: [u8; 8] = DISC_PUMPFUN_TRADE_EVENT;

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

    /// Every launchpad event discriminator is Anchor's sighash of its own
    /// name, derived here rather than trusted as a literal.
    #[test]
    fn launchpad_discriminators_are_sighashes() {
        for (name, expected) in [
            ("CreateEvent", DISC_PUMPFUN_CREATE),
            ("CompleteEvent", DISC_PUMPFUN_COMPLETE),
            ("CompletePumpAmmMigrationEvent", DISC_PUMPFUN_MIGRATED),
            ("CollectCreatorFeeEvent", DISC_PUMPFUN_CREATOR_FEE),
            ("EvtInitializePool", DISC_DBC_INITIALIZE_POOL),
            ("EvtSwap", DISC_DBC_SWAP),
            ("EvtSwap2", DISC_DBC_SWAP2),
            ("EvtCurveComplete", DISC_DBC_CURVE_COMPLETE),
            ("EvtCreateConfig", DISC_DBC_CREATE_CONFIG),
            ("EvtCreateConfigV2", DISC_DBC_CREATE_CONFIG_V2),
            ("EvtClaimCreatorTradingFee", DISC_DBC_CLAIM_CREATOR_FEE),
            ("EvtClaimTradingFee", DISC_DBC_CLAIM_TRADING_FEE),
            ("EvtCreatorWithdrawSurplus", DISC_DBC_CREATOR_SURPLUS),
            ("EvtPartnerWithdrawSurplus", DISC_DBC_PARTNER_SURPLUS),
            ("PoolCreateEvent", DISC_LAUNCHLAB_POOL_CREATE),
            ("TradeEvent", DISC_LAUNCHLAB_TRADE),
        ] {
            assert_eq!(
                anchor_discriminator("event", name),
                expected,
                "event:{name}"
            );
        }
    }

    /// The collision that decides the whole dispatch design: two programs
    /// named their event `TradeEvent`, so the 8 bytes are identical and the
    /// payloads are not. Anything keyed on the discriminator alone reads
    /// one as the other and returns plausible nonsense - the same failure
    /// Raydium's two `SwapEvent`s already caused in this module.
    #[test]
    fn pumpfun_and_launchlab_share_the_trade_event_discriminator() {
        assert_eq!(DISC_PUMPFUN_TRADE_EVENT, DISC_LAUNCHLAB_TRADE);
        assert_ne!(
            Venue::PumpFun.program_b58(),
            Venue::RaydiumLaunchlab.program_b58()
        );
    }

    /// Meteora reused `EvtSwap2` for DAMM v2 and for the DBC, so those two
    /// collide as well - and unlike Raydium's pair they are not even the
    /// same length, which is what makes the program check load bearing.
    #[test]
    fn the_dbc_and_damm_v2_swap_events_collide_too() {
        assert_eq!(DISC_DBC_SWAP2, DISC_DAMM2_SWAP);
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

    /// Every router id resolves the PREFIX the research recorded from a
    /// real transaction. A full id guessed from a prefix is a filter that
    /// matches nothing, or worse, matches the wrong program.
    #[test]
    fn routers_match_the_prefixes_the_research_recorded() {
        for (prefix, name) in ROUTER_PREFIXES {
            let (_, id) = ROUTERS_B58
                .iter()
                .find(|(candidate, _)| candidate == name)
                .unwrap_or_else(|| panic!("no router named {name}"));
            assert!(
                id.starts_with(prefix),
                "{name} is {id}, which does not start with the observed \
                 prefix {prefix}"
            );
        }
    }

    /// Router NAMES are unique: two rows with one name would make the
    /// attribution ambiguous.
    #[test]
    fn router_names_are_unique() {
        let mut names: Vec<&str> =
            ROUTERS_B58.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "a router name is used twice");
    }

    /// pump.fun's "Mayhem Mode" program shows up next to the routers in the
    /// research's list of ids seen live, and it is a LAUNCHPAD program.
    /// Registering it would attribute curve trades to a non-existent
    /// aggregator.
    #[test]
    fn the_pumpfun_mayhem_program_is_not_a_router() {
        assert!(
            !ROUTERS_B58.iter().any(|(_, id)| id.starts_with("MAyhSmzX")),
            "MAyhSmzX... is pump.fun's Mayhem Mode program, not a router"
        );
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
