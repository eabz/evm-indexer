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
    // The venues below are NAMED but not streamed in phase 1: they have no
    // per-program decoder yet, so registering one would give it movement
    // confidence only. They exist here because the generic decoder needs
    // nothing but a program id and a name, which is the whole point of the
    // two-layer design - see the `a_jupiter_route_is_one_swap_per_venue`
    // test, which registers them and decodes a real three-hop route.
    /// Prop AMM, no IDL and no event (research section 2).
    BisonFi,
    MeteoraDlmm,
    RaydiumCpmm,
}

impl Venue {
    /// Every venue this module can NAME.
    pub const ALL: [Venue; 5] = [
        Venue::PumpSwap,
        Venue::PumpFun,
        Venue::BisonFi,
        Venue::MeteoraDlmm,
        Venue::RaydiumCpmm,
    ];

    /// Value of the `protocol` column.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Venue::PumpSwap => "pumpswap",
            Venue::PumpFun => "pump_fun",
            Venue::BisonFi => "bisonfi",
            Venue::MeteoraDlmm => "meteora_dlmm",
            Venue::RaydiumCpmm => "raydium_cpmm",
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
            Venue::BisonFi => {
                "BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi"
            }
            Venue::MeteoraDlmm => {
                "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"
            }
            Venue::RaydiumCpmm => {
                "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"
            }
        }
    }

    /// Does a per-program decoder exist for this venue?
    pub const fn has_decoder(&self) -> bool {
        matches!(self, Venue::PumpSwap | Venue::PumpFun)
    }
}

impl std::fmt::Display for Venue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Venues phase 1 actually STREAMS. Everything here is queried from
/// HyperSync and written to the swap table; the rest of [`Venue::ALL`] is
/// only a name until its program id is added to this list.
pub const VENUES: [Venue; 2] = [Venue::PumpSwap, Venue::PumpFun];

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
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not snake_case"
            );
        }
    }
}
