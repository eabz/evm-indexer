//! The generic token-movement decoder: pure, no I/O.
//!
//! It runs PER INSTRUCTION SUBTREE and never on a transaction's net balance
//! change. That is not a style preference: docs/solana-research.md section
//! 2.2 records a real transaction (`Qxpfmre4JbRctxg1...`) holding TWO
//! PumpSwap swaps on the SAME pool in OPPOSITE directions, where ~6.2 SOL
//! moves each way and the transaction's net vault delta is ~0.03 SOL. A
//! decoder reading transaction-level balances reports a 0.03 SOL trade
//! instead of two 6.2 SOL trades.
//!
//! # The rule
//!
//! A swap is an instruction `I` of a registered venue program such that,
//! inside `I`'s own subtree and not inside a nested call to ANOTHER
//! registered venue, exactly two mints move across one common counterparty
//! (the pool authority / PDA): some accounts of mint A pay it, some accounts
//! of mint B are paid by it.
//!
//! This works on Solana in a way it never could on EVM. Only the SPL Token
//! program can change an SPL balance, so the transfer instruction IS the
//! movement - there is no gap between "a program said a swap happened" and
//! "tokens moved". The corroboration step `src/dex/corroborate.rs` performs
//! on EVM is therefore structurally satisfied here, and
//! `verified_in` / `verified_out` are always populated. A Solana "swap" that
//! moves no tokens is not a swap and never enters the table, which is also
//! what filters out prop-AMM QUOTE UPDATES - counting instructions instead
//! would overstate those venues ~5x (research section 2.2).
//!
//! # What is classified as what
//!
//! | Shape inside the subtree | Result |
//! |---|---|
//! | one mint in, a different mint out, one common counterparty | a swap |
//! | a third transfer of a mint that is already a leg, to a non-pool account | that leg's FEE |
//! | two or more mints moving the SAME way across the counterparty | liquidity add / remove, NOT a swap |
//! | no token movement at all | a quote update or an unrelated call, dropped |
//! | more than two mints crossing the counterparty, or no single counterparty | unclassified, counted and dropped, never guessed |
//!
//! # Native SOL
//!
//! Some venues move lamports directly instead of wrapped SOL, and a bonding
//! curve does it with NO instruction at all: the program just decrements its
//! own account's lamports. Measured on a live pump.fun sell, the SOL leg has
//! no System transfer child and exists only as a lamport delta on the curve
//! account. So when exactly one mint moves across the pool authority, the
//! authority's own native lamport delta is used as the other leg, with WSOL
//! as its mint - but ONLY when that is unambiguous (see
//! [`Movements::native_leg`]).

use std::collections::HashMap;

use alloy::primitives::{I256, U256};

use crate::svm::{
    models::{
        pack_ordinal, Confidence, Pubkey, SigBytes, SvmSwap, ZERO_PUBKEY,
    },
    programs::{
        registry, Registry, Venue, EVENT_CPI_PREFIX, IX_SYSTEM_TRANSFER,
        IX_TRANSFER, IX_TRANSFER_CHECKED,
    },
};

// --- input ---------------------------------------------------------------

/// One runtime program invocation, the decoder's own owned copy of
/// HyperSync's `instruction_call` row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SvmInstruction {
    /// Full CPI tree path: `[2]` is the third top level instruction,
    /// `[2, 0]` its first child.
    pub path: Vec<u32>,
    pub program: Pubkey,
    pub accounts: Vec<Pubkey>,
    pub data: Vec<u8>,
}

impl SvmInstruction {
    /// Is `self` a strict ancestor of `other`?
    fn is_ancestor_of(&self, other: &[u32]) -> bool {
        other.len() > self.path.len() && other.starts_with(&self.path)
    }

    fn discriminator(&self) -> Option<[u8; 8]> {
        self.data.get(..8).map(|d| {
            let mut out = [0u8; 8];
            out.copy_from_slice(d);
            out
        })
    }

    /// An Anchor `emit_cpi!` event, not a real instruction.
    fn is_event(&self) -> bool {
        self.discriminator() == Some(EVENT_CPI_PREFIX)
    }

    pub fn account(&self, index: usize) -> Option<Pubkey> {
        self.accounts.get(index).copied()
    }
}

/// One `account_activity` row: the native SOL side, the SPL token side, or
/// both. HyperSync serves EVERY account of a matched transaction here,
/// including unchanged ones, with mint, owner, decimals and pre/post.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SvmAccountActivity {
    pub account: Pubkey,
    pub mint: Option<Pubkey>,
    pub pre_owner: Option<Pubkey>,
    pub post_owner: Option<Pubkey>,
    pub decimals: Option<u8>,
    pub pre_token_balance: Option<u64>,
    pub post_token_balance: Option<u64>,
    pub pre_balance: Option<u64>,
    pub post_balance: Option<u64>,
    /// Whether the account signed the transaction. `None` when the source
    /// could not derive the flag - unknown is NOT the same as false, and the
    /// decoder treats it as unknown.
    ///
    /// This is what tells a pool apart from a user when both sides look
    /// symmetric: a pool authority is a program derived address, and a PDA
    /// can never be a transaction signer.
    pub is_signer: Option<bool>,
    pub is_fee_payer: bool,
    /// Token program owning the account, when it is a token account.
    pub token_program: Option<Pubkey>,
}

impl SvmAccountActivity {
    fn token_delta(&self) -> Option<i128> {
        match (self.pre_token_balance, self.post_token_balance) {
            (Some(pre), Some(post)) => {
                Some(i128::from(post) - i128::from(pre))
            }
            // An account opened in this transaction has no pre balance.
            (None, Some(post)) => Some(i128::from(post)),
            (Some(pre), None) => Some(-i128::from(pre)),
            (None, None) => None,
        }
    }

    fn lamport_delta(&self) -> Option<i128> {
        match (self.pre_balance, self.post_balance) {
            (Some(pre), Some(post)) => {
                Some(i128::from(post) - i128::from(pre))
            }
            _ => None,
        }
    }
}

/// One log row: a `Program log:` or `Program data:` line, attributed to the
/// instruction that wrote it.
///
/// **Phase 1 did not select this table, and that turned out to be the single
/// thing standing between the module and half the chain's volume.** Only
/// pump.fun, PumpSwap and the two Meteora programs publish their swap event
/// as a self-CPI INSTRUCTION; Raydium (all three programs) and Orca use
/// Anchor's plain `emit!` or a bare `msg!`, which writes a LOG LINE and
/// nothing else. The research assumed `Program data:` events "arrive via the
/// log table for free" - true, but only if the log table is asked for.
///
/// `path` is HyperSync's `instruction_address` for the log row, i.e. the
/// instruction that emitted it, which is what makes a log attributable to
/// one subtree in exactly the way an instruction is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SvmLog {
    pub path: Vec<u32>,
    pub program: Pubkey,
    /// `true` for a `Program data: <base64>` line (an Anchor `emit!`),
    /// `false` for a plain `Program log: <text>` line.
    pub is_data: bool,
    /// The line with its `Program data: ` / `Program log: ` prefix already
    /// stripped, which is how HyperSync stores it.
    pub message: String,
}

impl SvmLog {
    /// The base64 body of an Anchor `emit!` event, decoded.
    pub fn event_bytes(&self) -> Option<Vec<u8>> {
        if !self.is_data {
            return None;
        }
        base64_decode(self.message.trim())
    }

    /// The 8-byte event discriminator of an Anchor `emit!` event.
    pub fn event_discriminator(&self) -> Option<[u8; 8]> {
        let bytes = self.event_bytes()?;
        bytes.get(..8).map(|head| {
            let mut out = [0u8; 8];
            out.copy_from_slice(head);
            out
        })
    }
}

/// Standard base64 (RFC 4648, `+/`, optional `=` padding) -> bytes.
///
/// Written out rather than pulled in as a dependency: the crate has no
/// base64 crate today, this module needs exactly one direction of the
/// simplest variant, and a new dependency in `Cargo.toml` for thirty lines
/// of table lookup is a poor trade. Returns `None` on any invalid input
/// rather than decoding partially - an event body that is not valid base64
/// is not an event.
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    /// `byte -> 6-bit value`, 0xff where the byte is not a base64 digit.
    static TABLE: std::sync::OnceLock<[u8; 256]> =
        std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0xffu8; 256];
        let alphabet =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for (value, byte) in alphabet.iter().enumerate() {
            table[*byte as usize] = value as u8;
        }
        table
    });

    let bytes = input.as_bytes();
    let body = bytes
        .strip_suffix(b"==")
        .unwrap_or_else(|| bytes.strip_suffix(b"=").unwrap_or(bytes));
    // 4 base64 digits carry 3 bytes; a final group of 2 or 3 digits carries
    // 1 or 2. A remainder of exactly 1 digit cannot encode anything.
    if body.len() % 4 == 1 {
        return None;
    }

    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in body {
        let value = table[*byte as usize];
        if value == 0xff {
            return None;
        }
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// One matched transaction with everything the decoder needs.
#[derive(Debug, Clone)]
pub struct SvmTransaction {
    pub slot: u64,
    pub tx_index: u32,
    pub signature: SigBytes,
    pub fee_payer: Pubkey,
    pub success: bool,
    pub fee: u64,
    pub compute_units: u64,
    pub dropped_logs: bool,
    /// Every instruction row the query returned for this transaction: the
    /// venue instructions AND the SPL / Token-2022 / System transfers, which
    /// must be selected in the SAME query because a matched instruction does
    /// NOT return its children (research section 4.3).
    pub instructions: Vec<SvmInstruction>,
    pub activity: Vec<SvmAccountActivity>,
    /// Log rows of this transaction. Raydium's and Orca's swap events live
    /// here and nowhere else; see [`SvmLog`].
    pub logs: Vec<SvmLog>,
}

impl Default for SvmTransaction {
    /// Hand written because `[u8; 64]` has no `Default` (serde and std stop
    /// at 32 elements).
    fn default() -> Self {
        Self {
            slot: 0,
            tx_index: 0,
            signature: [0u8; 64],
            fee_payer: ZERO_PUBKEY,
            success: false,
            fee: 0,
            compute_units: 0,
            dropped_logs: false,
            instructions: Vec::new(),
            activity: Vec::new(),
            logs: Vec::new(),
        }
    }
}

// --- output --------------------------------------------------------------

/// Why an instruction with token movement did not become a swap. Counted so
/// coverage is measurable instead of assumed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Venue instructions with no token movement at all: prop-AMM quote
    /// updates, config calls, and the self-CPI event rows.
    pub no_movement: u64,
    /// Both mints moved the same way across the pool: a liquidity add or
    /// remove, deliberately kept OUT of the swap table.
    pub liquidity: u64,
    /// Movement the rule could not reduce to exactly two mints and one
    /// counterparty. Never guessed.
    pub unclassified: u64,
    /// A per-program decoder ran and its amounts contradicted the movement
    /// layer. That is a bug report, not a row.
    pub decoder_disagreed: u64,
    /// A native SOL leg was needed but could not be attributed unambiguously.
    pub ambiguous_native: u64,
    /// Both legs were real token transfers, but every candidate for the
    /// pool survived all three tests of `resolve_pool` and no venue event
    /// settled it - live, a bot VAULT trading against a bonding curve,
    /// where taker and pool are both program derived addresses.
    pub ambiguous_pool: u64,
    /// Swaps per venue, indexed by [`Venue::index`].
    pub swaps_by_venue: [u64; Venue::ALL.len()],
    /// Of those, how many the venue's own event CONFIRMED.
    pub confirmed_by_venue: [u64; Venue::ALL.len()],
    /// And how many it CONTRADICTED. `confirmed / (confirmed + disagreed)`
    /// is the agreement rate between the two layers, per venue - the number
    /// that says whether an event layout is right.
    pub disagreed_by_venue: [u64; Venue::ALL.len()],
    /// A venue instruction whose discriminator says "swap" but whose token
    /// movement says otherwise, or the reverse. The two classifications are
    /// independent, so a divergence is worth counting on its own.
    pub kind_disagreed: u64,
    /// Swaps of a LOG-sourced venue in a transaction whose logs the
    /// validator truncated. Their event stream is incomplete by the
    /// validator's own admission, so no log of theirs is read at all and
    /// the row stays `movement` - counted here rather than silently
    /// indistinguishable from "this venue emits no event".
    pub dropped_logs: u64,
    /// Swaps whose pool the venue could not be made to name, on a venue
    /// whose vault authority is program-wide. They are stored with no pool
    /// key and are excluded from the pool-keyed aggregates.
    pub unnamed_pool: u64,
    /// Swaps where the account meta this module reads the pool from and
    /// the pool the venue's own EVENT names are different accounts. Zero
    /// on every recording; anything else means a venue has changed an
    /// account layout and the index has gone stale.
    pub pool_index_disagreed: u64,
    /// A swap the movement layer proved but whose instruction path could
    /// not be packed into an ordinal. The row is DROPPED: a position key
    /// that silently folded onto another row's would make the two
    /// overwrite each other.
    pub unpackable_ordinal: u64,
    /// Fills of an instruction that executed more than one of them (Orca
    /// `two_hop_swap`, Raydium CLMM `swap_router_base_in`), past the first.
    pub extra_hops: u64,
}

impl Diagnostics {
    pub fn merge(&mut self, other: &Diagnostics) {
        self.no_movement += other.no_movement;
        self.liquidity += other.liquidity;
        self.unclassified += other.unclassified;
        self.decoder_disagreed += other.decoder_disagreed;
        self.ambiguous_native += other.ambiguous_native;
        self.ambiguous_pool += other.ambiguous_pool;
        self.kind_disagreed += other.kind_disagreed;
        self.dropped_logs += other.dropped_logs;
        self.unnamed_pool += other.unnamed_pool;
        self.pool_index_disagreed += other.pool_index_disagreed;
        self.unpackable_ordinal += other.unpackable_ordinal;
        self.extra_hops += other.extra_hops;
        for index in 0..Venue::ALL.len() {
            self.swaps_by_venue[index] += other.swaps_by_venue[index];
            self.confirmed_by_venue[index] +=
                other.confirmed_by_venue[index];
            self.disagreed_by_venue[index] +=
                other.disagreed_by_venue[index];
        }
    }

    /// Agreement between the movement layer and the venue's own event,
    /// over the swaps where an event was found at all.
    pub fn agreement_rate(&self, venue: Venue) -> Option<f64> {
        let confirmed = self.confirmed_by_venue[venue.index()];
        let disagreed = self.disagreed_by_venue[venue.index()];
        let judged = confirmed + disagreed;
        (judged > 0).then(|| confirmed as f64 / judged as f64)
    }
}

/// What one transaction produced.
#[derive(Debug, Clone, Default)]
pub struct DecodeOutcome {
    pub swaps: Vec<SvmSwap>,
    pub diagnostics: Diagnostics,
}

// --- movements -----------------------------------------------------------

/// One token or lamport movement, attributed to the instruction subtree it
/// happened in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movement {
    pub path: Vec<u32>,
    pub mint: Pubkey,
    pub amount: u64,
    pub source: Pubkey,
    pub destination: Pubkey,
    pub source_owner: Option<Pubkey>,
    pub destination_owner: Option<Pubkey>,
}

/// Token movements of one transaction, indexed for subtree lookups.
pub struct Movements<'a> {
    tx: &'a SvmTransaction,
    registry: &'a Registry,
    /// Public so the live diagnostics can print the exact flows an
    /// instruction produced; nothing outside this module mutates it.
    pub movements: Vec<Movement>,
    /// For each movement, the path of its NEAREST registered venue ancestor.
    ///
    /// Computed once here rather than per lookup. It used to be recomputed
    /// inside `owned_by`, which made the whole step
    /// O(venues x movements x instructions) for every transaction; a profile
    /// of the decoder put it among the top costs after the curve test.
    nearest_venue: Vec<Option<&'a [u32]>>,
    /// account -> its activity row.
    by_account: HashMap<Pubkey, &'a SvmAccountActivity>,
}

impl<'a> Movements<'a> {
    pub fn new(tx: &'a SvmTransaction, registry: &'a Registry) -> Self {
        let mut by_account = HashMap::with_capacity(tx.activity.len());
        for row in &tx.activity {
            by_account.insert(row.account, row);
        }

        let mut movements = Vec::new();
        for instruction in &tx.instructions {
            if let Some(movement) =
                parse_transfer(instruction, registry, &by_account)
            {
                movements.push(movement);
            }
        }

        // The venue instructions, once: `venue()` is a linear scan of the
        // registry and this used to run per (movement, instruction) pair.
        let venue_instructions: Vec<&SvmInstruction> = tx
            .instructions
            .iter()
            .filter(|candidate| {
                !candidate.is_event()
                    && registry.venue(&candidate.program).is_some()
            })
            .collect();

        let nearest_venue = movements
            .iter()
            .map(|movement| {
                venue_instructions
                    .iter()
                    .filter(|candidate| {
                        candidate.is_ancestor_of(&movement.path)
                    })
                    .max_by_key(|candidate| candidate.path.len())
                    .map(|candidate| candidate.path.as_slice())
            })
            .collect();

        Self { tx, registry, movements, nearest_venue, by_account }
    }

    /// Movements inside `instruction`'s subtree whose NEAREST registered
    /// venue ancestor is `instruction` itself.
    ///
    /// The "nearest venue ancestor" part is what keeps a router's or an
    /// outer venue's subtree from swallowing an inner venue's transfers: in
    /// a Jupiter 3-hop every hop keeps its own two legs, which is why the
    /// route decodes to three swaps rather than one.
    fn owned_by(&self, instruction: &SvmInstruction) -> Vec<&Movement> {
        self.movements
            .iter()
            .zip(&self.nearest_venue)
            .filter(|(movement, nearest)| {
                instruction.is_ancestor_of(&movement.path)
                    && **nearest == Some(instruction.path.as_slice())
            })
            .map(|(movement, _)| movement)
            .collect()
    }

    /// Nearest ancestor that is a registered ROUTER, for attribution.
    fn nearest_router(
        &self,
        path: &[u32],
    ) -> Option<(&'static str, Vec<u32>, Pubkey)> {
        self.tx
            .instructions
            .iter()
            .filter(|candidate| candidate.is_ancestor_of(path))
            .filter_map(|candidate| {
                self.registry.router(&candidate.program).map(|name| {
                    (name, candidate.path.clone(), candidate.program)
                })
            })
            .max_by_key(|(_, path, _)| path.len())
    }

    /// The pool authority's own native lamport delta, used as a SOL leg when
    /// only one mint moved as a token.
    ///
    /// Only returned when it is UNAMBIGUOUS: at most one venue instruction
    /// in the whole transaction may resolve to this authority, otherwise two
    /// trades on one pool would share (and net out) a single lamport delta -
    /// the exact failure this module exists to avoid. The fee payer's own
    /// row is never used, because it also pays the transaction fee.
    fn native_leg(&self, authority: &Pubkey) -> Option<i128> {
        let row = self.by_account.get(authority)?;
        if row.is_fee_payer {
            return None;
        }
        let delta = row.lamport_delta()?;
        if delta == 0 {
            return None;
        }
        Some(delta)
    }

    fn received_by(&self, account: &Pubkey) -> Option<i128> {
        self.by_account.get(account).and_then(|row| row.token_delta())
    }

    /// What the pool's vault was actually CREDITED on the input leg.
    ///
    /// The mirror of `SvmSwap::with_received`. A Token-2022 transfer fee
    /// comes out of the receiver's credit on BOTH legs, and until this was
    /// measured the only way to check an event that reports the credited
    /// figure - Raydium LaunchLab does - was to invent a tolerance.
    ///
    /// Returns the amount sent when the vault's delta is not readable or
    /// not smaller, so a caller always gets a usable number and the gap it
    /// implies is never negative.
    ///
    /// The delta is read off the ONE account the principal input leg was
    /// paid into, never off "the first account in the subtree with a
    /// readable delta": see [`SvmSwap::with_received`] for what that cost
    /// on the output leg.
    fn input_received(&self, swap: &MovementSwap) -> u64 {
        match self.received_by(&swap.in_account) {
            Some(delta)
                if delta > 0
                    && (delta as u128) <= u128::from(swap.amount_in) =>
            {
                delta as u64
            }
            _ => swap.amount_in,
        }
    }
}

/// Parses an SPL Token / Token-2022 / System transfer instruction.
fn parse_transfer(
    instruction: &SvmInstruction,
    registry: &Registry,
    by_account: &HashMap<Pubkey, &SvmAccountActivity>,
) -> Option<Movement> {
    let resolve_owner_src = |account: &Pubkey| {
        by_account
            .get(account)
            .and_then(|row| row.pre_owner.or(row.post_owner))
    };
    let resolve_owner_dst = |account: &Pubkey| {
        by_account
            .get(account)
            .and_then(|row| row.post_owner.or(row.pre_owner))
    };

    if registry.is_token_program(&instruction.program) {
        let tag = *instruction.data.first()?;
        let (amount, source, destination, mint) = match tag {
            // Transfer: [0x03, amount u64]; accounts [source, destination,
            // authority]. The mint is NOT in the instruction, so it comes
            // from the account_activity row of either side.
            IX_TRANSFER => {
                let amount = read_u64(&instruction.data, 1)?;
                let source = instruction.account(0)?;
                let destination = instruction.account(1)?;
                let mint = by_account
                    .get(&source)
                    .and_then(|row| row.mint)
                    .or_else(|| {
                        by_account.get(&destination).and_then(|r| r.mint)
                    })?;
                (amount, source, destination, mint)
            }
            // TransferChecked: [0x0c, amount u64, decimals u8]; accounts
            // [source, MINT, destination, authority]. The mint is explicit,
            // which is what makes a wrapped-SOL account that is opened and
            // closed inside the transaction (and therefore absent from
            // account_activity) still decodable.
            IX_TRANSFER_CHECKED => {
                let amount = read_u64(&instruction.data, 1)?;
                let source = instruction.account(0)?;
                let mint = instruction.account(1)?;
                let destination = instruction.account(2)?;
                (amount, source, destination, mint)
            }
            _ => return None,
        };

        if amount == 0 {
            return None;
        }

        return Some(Movement {
            path: instruction.path.clone(),
            mint,
            amount,
            source,
            destination,
            source_owner: resolve_owner_src(&source).or(Some(source)),
            destination_owner: resolve_owner_dst(&destination)
                .or(Some(destination)),
        });
    }

    if instruction.program == registry.system {
        // System Transfer: [0x02,0,0,0, lamports u64]; accounts [from, to].
        // A native account IS its own owner, so the wallet and the "token
        // account" coincide.
        if instruction.data.get(..4)? != IX_SYSTEM_TRANSFER {
            return None;
        }
        let amount = read_u64(&instruction.data, 4)?;
        if amount == 0 {
            return None;
        }
        let source = instruction.account(0)?;
        let destination = instruction.account(1)?;
        return Some(Movement {
            path: instruction.path.clone(),
            mint: registry.wsol,
            amount,
            source,
            destination,
            source_owner: Some(source),
            destination_owner: Some(destination),
        });
    }

    None
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

// --- the decoder ---------------------------------------------------------

/// Everything the movement layer established about one swap, before a
/// per-program decoder gets a chance to enrich it.
#[derive(Debug, Clone)]
pub struct MovementSwap {
    pub venue: Venue,
    pub path: Vec<u32>,
    /// Common counterparty of both legs: the pool authority.
    pub authority: Pubkey,
    pub mint_in: Pubkey,
    pub mint_out: Pubkey,
    /// Into the pool, as SENT by the taker.
    pub amount_in: u64,
    /// Into the pool, as CREDITED to the pool's vault.
    ///
    /// The mirror image of [`Self::amount_out`] / [`Self::amount_out_gross`],
    /// and it exists for the same reason: a Token-2022 transfer fee comes
    /// out of what the RECEIVER is credited. Equal to `amount_in` on a
    /// classic SPL mint.
    ///
    /// It is not a stored column - what the taker sent is the trade - but
    /// it is what lets a venue's event be checked without inventing a
    /// tolerance. Raydium LaunchLab states no transfer fee anywhere, and
    /// its agreement with the movement layer was 4.8% until the gap was
    /// measured from the chain instead of guessed at.
    pub amount_in_received: u64,
    /// Out of the pool, as SENT.
    pub amount_out_gross: u64,
    /// Out of the pool, as RECEIVED by the taker. Differs from the gross
    /// amount by a Token-2022 transfer fee, which no event mentions.
    pub amount_out: u64,
    /// Transfers of ONE leg's mint that went somewhere other than the pool:
    /// protocol, creator and router fees.
    pub fee_amount: u64,
    /// Which mint [`Self::fee_amount`] is in. Zero when there was no fee.
    pub fee_mint: Pubkey,
    pub payer: Pubkey,
    pub recipient: Pubkey,
    /// The token ACCOUNT that received the principal input leg, i.e. the
    /// pool's vault. What it was really credited is what a Token-2022
    /// transfer fee makes smaller than [`Self::amount_in`].
    pub in_account: Pubkey,
    /// The token ACCOUNT that received the principal output leg, i.e. the
    /// taker's. Named explicitly because the subtree also contains fee
    /// recipients, and reading "the first destination with a readable
    /// balance delta" silently stored a FEE recipient's delta as the
    /// taker's receipt (review round 4, M4).
    pub out_account: Pubkey,
    /// The SOL leg came from a lamport delta rather than an instruction.
    pub native_leg: bool,
    /// Which fill of the instruction this is: 0 for every venue
    /// instruction that executes one swap, 1 and up for the later hops of
    /// an Orca `two_hop_swap` or a Raydium CLMM `swap_router_base_in`.
    pub hop: u32,
}

/// Decodes every swap in one transaction, using the shared registry.
pub fn decode_transaction(
    chain: u64,
    timestamp: u32,
    tx: &SvmTransaction,
) -> DecodeOutcome {
    decode_transaction_with(chain, timestamp, tx, registry())
}

/// Decodes every swap in one transaction against an explicit registry.
///
/// The registry is a parameter so a test can register the venues of a route
/// that phase 1 does not stream yet (the recorded Jupiter three-hop) and
/// check that the subtree rule really does split it into one swap per
/// venue. Adding a venue in production is one line in
/// [`crate::svm::programs::VENUES`] and nothing else.
pub fn decode_transaction_with(
    chain: u64,
    timestamp: u32,
    tx: &SvmTransaction,
    registry: &Registry,
) -> DecodeOutcome {
    let mut outcome = DecodeOutcome::default();

    if !tx.success {
        // An instruction of a failed transaction had its state changes
        // rolled back; it never moved anything.
        return outcome;
    }

    let movements = Movements::new(tx, registry);

    // How many venue instructions resolve to each authority: a pool touched
    // twice in one transaction cannot borrow a native lamport delta.
    //
    // The candidate sets are computed ONCE here and handed to `classify`.
    // They used to be recomputed there, so every venue instruction paid for
    // `pool_candidates` twice and `native_candidates` - which runs the
    // ed25519 curve test - twice as well.
    let mut authority_uses: HashMap<Pubkey, u32> = HashMap::new();
    let mut candidates = Vec::new();

    for instruction in &tx.instructions {
        let Some(venue) = registry.venue(&instruction.program) else {
            continue;
        };
        if instruction.is_event() {
            continue;
        }
        let owned = movements.owned_by(instruction);
        if owned.is_empty() {
            outcome.diagnostics.no_movement += 1;
            continue;
        }
        let pools = pool_candidates(&owned);
        let natives = native_candidates(
            &owned,
            &movements.movements,
            &instruction.path,
        );
        for authority in pools.iter().chain(natives.iter()) {
            *authority_uses.entry(*authority).or_insert(0) += 1;
        }
        candidates.push((instruction, venue, owned, pools, natives));
    }

    for (instruction, venue, owned, pools, natives) in candidates {
        match classify(
            &movements,
            registry,
            venue,
            instruction,
            &owned,
            &pools,
            &natives,
            &authority_uses,
        ) {
            Classified::Swap(mut swap) => {
                // What the vault was CREDITED, which a Token-2022 transfer
                // fee makes smaller than what the taker sent.
                swap.amount_in_received = movements.input_received(&swap);
                let Some(mut row) = build_row(
                    chain,
                    timestamp,
                    tx,
                    instruction,
                    &movements,
                    &swap,
                ) else {
                    outcome.diagnostics.unpackable_ordinal += 1;
                    continue;
                };
                let enriched = crate::svm::events::enrich(
                    tx,
                    instruction,
                    venue,
                    &swap,
                    &mut row,
                    swap.hop as usize,
                );
                record(
                    &mut outcome.diagnostics,
                    venue,
                    enriched,
                    instruction,
                );
                finish_row(
                    &mut outcome.diagnostics,
                    venue,
                    instruction,
                    &mut row,
                );
                outcome.swaps.push(row);
            }
            // One instruction, several fills. Each keeps its own pair of
            // transfers and its own position, so the later hops are
            // stored instead of being dropped or overwriting the first.
            Classified::Hops(hops) => {
                outcome.diagnostics.extra_hops +=
                    hops.len().saturating_sub(1) as u64;
                for mut swap in hops {
                    swap.amount_in_received =
                        movements.input_received(&swap);
                    let Some(mut row) = build_row(
                        chain,
                        timestamp,
                        tx,
                        instruction,
                        &movements,
                        &swap,
                    ) else {
                        outcome.diagnostics.unpackable_ordinal += 1;
                        continue;
                    };
                    let enriched = crate::svm::events::enrich(
                        tx,
                        instruction,
                        venue,
                        &swap,
                        &mut row,
                        swap.hop as usize,
                    );
                    record(
                        &mut outcome.diagnostics,
                        venue,
                        enriched,
                        instruction,
                    );
                    finish_row(
                        &mut outcome.diagnostics,
                        venue,
                        instruction,
                        &mut row,
                    );
                    outcome.swaps.push(row);
                }
            }
            // The movement layer proposed both sides of a symmetric trade;
            // the venue's own event decides which is the pool. Exactly one
            // reading can validate, because the event names the user and
            // the direction.
            Classified::Candidates(proposals) => {
                // A single unvalidated proposal is accepted on the
                // movement layer alone, but only when the candidate really
                // looks like a pool: `resolve_pool` refused to pick it,
                // and a candidate that is ON the ed25519 curve has a
                // private key, so it is somebody's wallet and not a
                // program's vault owner.
                let only_one = proposals.len() == 1
                    && proposals.first().is_some_and(|swap| {
                        !crate::svm::pda::is_on_curve(&swap.authority)
                    });
                let mut accepted = None;
                let mut verdict = crate::svm::events::Enrichment::None;
                for swap in &proposals {
                    let mut swap = swap.clone();
                    swap.amount_in_received =
                        movements.input_received(&swap);
                    let swap = &swap;
                    let Some(mut row) = build_row(
                        chain,
                        timestamp,
                        tx,
                        instruction,
                        &movements,
                        swap,
                    ) else {
                        outcome.diagnostics.unpackable_ordinal += 1;
                        continue;
                    };
                    match crate::svm::events::enrich(
                        tx,
                        instruction,
                        venue,
                        swap,
                        &mut row,
                        swap.hop as usize,
                    ) {
                        crate::svm::events::Enrichment::Applied => {
                            verdict =
                                crate::svm::events::Enrichment::Applied;
                            accepted = Some(row);
                            break;
                        }
                        // A single unambiguous candidate stands on the
                        // movement layer alone.
                        crate::svm::events::Enrichment::None
                            if only_one =>
                        {
                            accepted = Some(row);
                        }
                        other => verdict = other,
                    }
                }
                match accepted {
                    Some(mut row) => {
                        record(
                            &mut outcome.diagnostics,
                            venue,
                            verdict,
                            instruction,
                        );
                        finish_row(
                            &mut outcome.diagnostics,
                            venue,
                            instruction,
                            &mut row,
                        );
                        outcome.swaps.push(row);
                    }
                    // No reading validated, so the trade is real but its
                    // direction is unknown. Never guessed: which of the two
                    // counters it lands in says whether a SOL leg or a
                    // symmetric pair of PDAs was the cause.
                    None if proposals
                        .first()
                        .is_some_and(|s| s.native_leg) =>
                    {
                        outcome.diagnostics.ambiguous_native += 1
                    }
                    None => outcome.diagnostics.ambiguous_pool += 1,
                }
            }
            Classified::Liquidity => {
                outcome.diagnostics.liquidity += 1;
                // The two classifications are independent. The movement
                // layer says "both mints moved the same way"; the
                // discriminator should say `Liquidity` too.
                if venue.instruction_kind(&instruction.data)
                    == crate::svm::programs::IxKind::Swap
                {
                    outcome.diagnostics.kind_disagreed += 1;
                }
            }
            Classified::Unclassified => {
                outcome.diagnostics.unclassified += 1
            }
            Classified::AmbiguousNative => {
                outcome.diagnostics.ambiguous_native += 1
            }
            Classified::NoMovement => outcome.diagnostics.no_movement += 1,
        }
    }

    outcome.swaps.sort_by_key(|swap| swap.ordinal);
    outcome
}

/// Books one swap against its venue, and cross-checks the venue's own
/// discriminator against the shape of the token movement.
///
/// The discriminator check is genuinely independent evidence: the movement
/// layer concluded "one mint in, a different one out" from validator
/// metadata, and the discriminator is the program's own statement of what
/// the instruction was. A swap by movement that the registry calls
/// `Liquidity` means one of the two is wrong, and it is worth a counter
/// rather than a silent row.
fn record(
    diagnostics: &mut Diagnostics,
    venue: Venue,
    enrichment: crate::svm::events::Enrichment,
    instruction: &SvmInstruction,
) {
    use crate::svm::{events::Enrichment, programs::IxKind};

    diagnostics.swaps_by_venue[venue.index()] += 1;
    match enrichment {
        Enrichment::Applied => {
            diagnostics.confirmed_by_venue[venue.index()] += 1
        }
        Enrichment::Disagreed => {
            diagnostics.disagreed_by_venue[venue.index()] += 1;
            diagnostics.decoder_disagreed += 1;
        }
        // Counted on its own, and deliberately NOT as an agreement or a
        // disagreement: the venue said nothing here because the validator
        // threw its words away, which is a different fact from "this venue
        // emits no event" and must not move `agreement_rate`.
        Enrichment::Incomplete => diagnostics.dropped_logs += 1,
        Enrichment::None => {}
    }

    if venue.instruction_kind(&instruction.data) == IxKind::Liquidity {
        diagnostics.kind_disagreed += 1;
    }
}

/// `Swap` is much larger than the other variants, and boxing it would cost
/// an allocation on the hot path for every swap on the chain to save a few
/// bytes of stack in a value that never leaves this function.
#[allow(clippy::large_enum_variant)]
enum Classified {
    Swap(MovementSwap),
    /// A trade whose pool side the movement layer cannot pick on its own.
    /// Each entry is the same trade read from one candidate's point of
    /// view, so at most ONE can be right; the per-program decoder picks.
    ///
    /// Two shapes reach this. A native-leg trade is symmetric by
    /// construction. A two-sided trade gets here when EVERY candidate
    /// survives all three steps of [`resolve_pool`] - measured live, that
    /// is a bot VAULT buying on a bonding curve: the vault is a program
    /// derived address too, so the off-curve test cannot tell it from the
    /// pool, and before this the row was dropped as unclassified.
    Candidates(Vec<MovementSwap>),
    /// SEVERAL fills executed by one instruction, in execution order. Orca
    /// `two_hop_swap` and Raydium CLMM `swap_router_base_in` do this: two
    /// pools, two trades, one `instruction_address`. Each entry keeps only
    /// its own pair of transfers, so neither hop's amounts or fees leak
    /// into the other's.
    Hops(Vec<MovementSwap>),
    Liquidity,
    Unclassified,
    AmbiguousNative,
    NoMovement,
}

/// Accounts that RECEIVE one mint and SEND a different one inside this
/// subtree, i.e. that sit on the other side of BOTH legs.
///
/// **A swap is locally symmetric.** docs/solana-research.md section 3.1
/// proposes "the counterparty accounts of both legs share one owner (the
/// pool authority)" as the identifying rule, but that is incomplete: the
/// TAKER also receives one mint and sends the other, so this returns two
/// candidates on any trade where the taker uses one owner for both legs.
/// Measured on the recorded Jupiter route, where the router's proxy owner
/// is a counterparty of all three hops. [`resolve_pool`] breaks the tie.
fn pool_candidates(movements: &[&Movement]) -> Vec<Pubkey> {
    let mut received: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
    let mut sent: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();

    for movement in movements {
        if let Some(owner) = movement.destination_owner {
            received.entry(owner).or_default().push(movement.mint);
        }
        if let Some(owner) = movement.source_owner {
            sent.entry(owner).or_default().push(movement.mint);
        }
    }

    let mut candidates: Vec<Pubkey> = received
        .iter()
        .filter(|(owner, in_mints)| {
            sent.get(*owner).is_some_and(|out_mints| {
                in_mints.iter().any(|a| out_mints.iter().any(|b| a != b))
            })
        })
        .map(|(owner, _)| *owner)
        .collect();

    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

/// Picks the pool out of the locally symmetric candidates, in three steps,
/// each with its own reason. Anything still ambiguous is REFUSED rather than
/// guessed, because picking the taker would invert the trade's direction and
/// store the wrong account as the pool.
///
/// 1. **Only one candidate.** Nothing to decide.
/// 2. **The pool is local to its own hop.** A pool takes part in exactly one
///    venue instruction; a taker, and above all a router's proxy account,
///    is a counterparty in every hop of the route and in the wrap / unwrap
///    transfers around it. So a candidate that also moves tokens OUTSIDE
///    this subtree is the taker's side.
/// 3. **A pool authority is program derived.** `find_program_address`
///    searches bump seeds until it lands OFF the ed25519 curve, so no
///    private key can exist for a pool; a user's wallet is a real public key
///    and is on the curve. See [`crate::svm::pda`].
fn resolve_pool(
    candidates: &[Pubkey],
    all: &[Movement],
    subtree: &[u32],
) -> Option<Pubkey> {
    if candidates.len() <= 1 {
        return candidates.first().copied();
    }

    let local: Vec<Pubkey> = candidates
        .iter()
        .copied()
        .filter(|candidate| {
            !all.iter()
                .filter(|movement| !movement.path.starts_with(subtree))
                .any(|movement| {
                    movement.source_owner == Some(*candidate)
                        || movement.destination_owner == Some(*candidate)
                })
        })
        .collect();
    if local.len() == 1 {
        return local.first().copied();
    }

    let narrowed =
        if local.is_empty() { candidates.to_vec() } else { local };
    let derived: Vec<Pubkey> = narrowed
        .into_iter()
        .filter(|candidate| !crate::svm::pda::is_on_curve(candidate))
        .collect();
    if derived.len() == 1 {
        return derived.first().copied();
    }

    None
}

/// The possible pool sides when only ONE mint moved as a token and the other
/// leg is native SOL.
///
/// This case is even more symmetric than an ordinary swap: the user sends a
/// token and gains lamports, the pool gains the token and loses exactly the
/// same lamports. `is_signer` does not help (a recorded live pump.fun sell
/// has a BOT paying the fee, so the user is not a signer either) and neither
/// does the curve test (that same user is itself a PDA).
///
/// So the movement layer PROPOSES both sides and the per-program decoder
/// DISPOSES: the venue's own event names the user, so exactly one candidate
/// can survive validation. When no decoder resolves it, a single candidate
/// is accepted and anything else is counted as ambiguous, never guessed.
fn native_candidates(
    movements: &[&Movement],
    all: &[Movement],
    subtree: &[u32],
) -> Vec<Pubkey> {
    let mut owners: Vec<Pubkey> = Vec::new();
    for movement in movements {
        for owner in [movement.source_owner, movement.destination_owner]
            .into_iter()
            .flatten()
        {
            if !owners.contains(&owner) {
                owners.push(owner);
            }
        }
    }

    // The order of these three filters is a PERFORMANCE decision and not a
    // semantic one: they are conjunctive and `is_on_curve` is pure, so the
    // result is identical whichever way round they run. The curve test costs
    // a modular exponentiation, so it goes last, after the two cheap
    // structural filters have thrown most candidates away.
    let mut candidates: Vec<Pubkey> = owners
        .into_iter()
        // Must be a counterparty of EVERY movement of the single mint.
        .filter(|owner| {
            movements.iter().all(|movement| {
                movement.source_owner == Some(*owner)
                    || movement.destination_owner == Some(*owner)
            })
        })
        // A pool takes part in its own hop only.
        .filter(|owner| {
            !all.iter()
                .filter(|movement| !movement.path.starts_with(subtree))
                .any(|movement| {
                    movement.source_owner == Some(*owner)
                        || movement.destination_owner == Some(*owner)
                })
        })
        // A pool is program derived; a plain wallet never is.
        .filter(|owner| !crate::svm::pda::is_on_curve(owner))
        .collect();

    candidates.sort_unstable();
    candidates.dedup();
    candidates
}

/// Splits ONE venue instruction into the several fills it executed, when
/// that is what the movement really shows.
///
/// The test is DISJOINTNESS. Each candidate counterparty is a swap of its
/// own only if the transfers it takes part in are its alone: a movement
/// with two candidates as counterparties means the two are the pool and the
/// taker of a single trade (which is [`resolve_pool`]'s ambiguity, not a
/// second hop), and every candidate must also account for the movements it
/// claims - so the hops together must cover every transfer of the subtree,
/// or something is being dropped.
///
/// Returns `None` unless at least two disjoint candidates each classify to
/// a genuine swap, which is the only shape that may become several rows.
fn split_hops(
    venue: Venue,
    instruction: &SvmInstruction,
    owned: &[&Movement],
    pools: &[Pubkey],
) -> Option<Vec<MovementSwap>> {
    let touches = |authority: &Pubkey, movement: &Movement| {
        movement.source_owner == Some(*authority)
            || movement.destination_owner == Some(*authority)
    };

    // The TAKER is a counterparty of every hop, so it has to go first or
    // nothing is ever disjoint. `resolve_pool`'s third test is what tells
    // the two apart: `find_program_address` searches until it lands OFF
    // the ed25519 curve, so no private key can exist for a pool, while a
    // user's wallet is a real public key and is on the curve.
    let pools: Vec<Pubkey> = pools
        .iter()
        .copied()
        .filter(|candidate| !crate::svm::pda::is_on_curve(candidate))
        .collect();
    if pools.len() < 2 {
        return None;
    }

    // No movement may be shared between two candidates. One that is means
    // the two are the pool and the taker of a SINGLE trade - the ambiguity
    // `resolve_pool` refused to resolve - and not two hops.
    for movement in owned {
        let sharing = pools
            .iter()
            .filter(|authority| touches(authority, movement))
            .count();
        if sharing > 1 {
            return None;
        }
    }

    // Group the transfers by the hop they belong to, in execution order.
    let mut groups: Vec<(Pubkey, Vec<u32>, Vec<&Movement>)> = Vec::new();
    for authority in &pools {
        let mine: Vec<&Movement> = owned
            .iter()
            .copied()
            .filter(|movement| touches(authority, movement))
            .collect();
        let Some(first) =
            mine.iter().map(|movement| movement.path.clone()).min()
        else {
            continue;
        };
        groups.push((*authority, first, mine));
    }
    groups.sort_by(|left, right| left.1.cmp(&right.1));
    if groups.len() < 2 {
        return None;
    }

    // A transfer that crosses NO pool is a fee, and it belongs to the hop
    // it was executed within: the last one that started before it.
    for movement in owned {
        if pools.iter().any(|authority| touches(authority, movement)) {
            continue;
        }
        let hop = groups
            .iter()
            .rposition(|(_, first, _)| *first <= movement.path)
            .unwrap_or(0);
        groups[hop].2.push(movement);
    }

    let mut hops = Vec::with_capacity(groups.len());
    for (index, (authority, _, mine)) in groups.into_iter().enumerate() {
        let Classified::Swap(mut swap) =
            classify_two_sided(venue, instruction, &mine, authority)
        else {
            // One group is not a trade, so this is not a multi-hop swap
            // and nothing here may be guessed at.
            return None;
        };
        swap.hop = index as u32;
        hops.push(swap);
    }
    Some(hops)
}

#[allow(clippy::too_many_arguments)]
fn classify(
    movements: &Movements<'_>,
    registry: &Registry,
    venue: Venue,
    instruction: &SvmInstruction,
    owned: &[&Movement],
    pools: &[Pubkey],
    natives: &[Pubkey],
    authority_uses: &HashMap<Pubkey, u32>,
) -> Classified {
    if owned.is_empty() {
        return Classified::NoMovement;
    }

    if let Some(authority) =
        resolve_pool(pools, &movements.movements, &instruction.path)
    {
        return classify_two_sided(venue, instruction, owned, authority);
    }

    // Two or more candidates survived every test. There are two entirely
    // different reasons for that and they need opposite treatment.
    if pools.len() > 1 {
        // (a) The instruction executed SEVERAL fills. Orca's
        //     `two_hop_swap` and Raydium CLMM's `swap_router_base_in` run
        //     two pools from one instruction, and each pool is the
        //     counterparty of its OWN pair of transfers. The candidates
        //     are then disjoint: no movement has two of them as a
        //     counterparty. That is what tells this case apart from (b),
        //     where the two candidates - the pool and the taker - sit on
        //     opposite ends of the very same two transfers.
        if let Some(hops) = split_hops(venue, instruction, owned, pools) {
            return Classified::Hops(hops);
        }

        // (b) The movement layer genuinely cannot tell the pool from the
        //     taker. Rather than drop the trade, propose each reading and
        //     let the venue's own event pick - exactly what the native-leg
        //     path below has always done. The readings differ in
        //     DIRECTION, which is precisely what an event states.
        let proposals: Vec<MovementSwap> = pools
            .iter()
            .filter_map(|authority| {
                match classify_two_sided(
                    venue,
                    instruction,
                    owned,
                    *authority,
                ) {
                    Classified::Swap(swap) => Some(swap),
                    _ => None,
                }
            })
            .collect();
        if !proposals.is_empty() {
            return Classified::Candidates(proposals);
        }
    }

    // One mint only: the other leg may be native SOL moved without an
    // instruction (a bonding curve decrements its own lamports).
    let mints: Vec<Pubkey> = {
        let mut mints: Vec<Pubkey> =
            owned.iter().map(|movement| movement.mint).collect();
        mints.sort_unstable();
        mints.dedup();
        mints
    };

    if mints.len() != 1 {
        return Classified::Unclassified;
    }

    if natives.is_empty() {
        return Classified::AmbiguousNative;
    }

    let swaps: Vec<MovementSwap> = natives
        .iter()
        .copied()
        .filter_map(|authority| {
            classify_native(
                movements,
                registry,
                venue,
                instruction,
                owned,
                authority_uses,
                authority,
            )
        })
        .collect();

    match swaps.len() {
        0 => Classified::AmbiguousNative,
        _ => Classified::Candidates(swaps),
    }
}

/// Builds the swap for ONE proposed pool side of a native-leg trade.
#[allow(clippy::too_many_arguments)]
fn classify_native(
    movements: &Movements<'_>,
    registry: &Registry,
    venue: Venue,
    instruction: &SvmInstruction,
    owned: &[&Movement],
    authority_uses: &HashMap<Pubkey, u32>,
    authority: Pubkey,
) -> Option<MovementSwap> {
    // A pool touched twice in one transaction cannot be given a single
    // lamport delta: that is exactly the netting this module refuses.
    if authority_uses.get(&authority).copied().unwrap_or(0) > 1 {
        return None;
    }

    let native_delta = movements.native_leg(&authority)?;

    let token_into_pool: i128 = owned
        .iter()
        .map(|movement| {
            if movement.destination_owner == Some(authority) {
                i128::from(movement.amount)
            } else if movement.source_owner == Some(authority) {
                -i128::from(movement.amount)
            } else {
                0
            }
        })
        .sum();

    if token_into_pool == 0 {
        return None;
    }
    // A swap has one leg in and one leg out. Same sign on both means the
    // token and the lamports moved the same way: a liquidity add or remove,
    // or a deposit - not a trade.
    if token_into_pool.signum() == native_delta.signum() {
        return None;
    }

    let token_mint = owned.first()?.mint;
    let (mint_in, mint_out, amount_in, amount_out) = if token_into_pool > 0
    {
        (
            token_mint,
            registry.wsol,
            token_into_pool as u64,
            native_delta.unsigned_abs() as u64,
        )
    } else {
        (
            registry.wsol,
            token_mint,
            native_delta.unsigned_abs() as u64,
            token_into_pool.unsigned_abs() as u64,
        )
    };

    let payer = owned
        .first()
        .and_then(|movement| {
            if movement.source_owner == Some(authority) {
                movement.destination_owner
            } else {
                movement.source_owner
            }
        })
        .unwrap_or(ZERO_PUBKEY);

    Some(MovementSwap {
        venue,
        path: instruction.path.clone(),
        authority,
        mint_in,
        mint_out,
        amount_in,
        // A native lamport leg has no token account and therefore no
        // transfer fee.
        amount_in_received: amount_in,
        amount_out_gross: amount_out,
        amount_out,
        fee_amount: 0,
        fee_mint: ZERO_PUBKEY,
        payer,
        recipient: payer,
        // A native leg moves no token account, so there is nothing whose
        // balance delta could refine either figure.
        in_account: ZERO_PUBKEY,
        out_account: ZERO_PUBKEY,
        native_leg: true,
        hop: 0,
    })
}

fn classify_two_sided(
    venue: Venue,
    instruction: &SvmInstruction,
    owned: &[&Movement],
    authority: Pubkey,
) -> Classified {
    let mut into_pool: HashMap<Pubkey, u64> = HashMap::new();
    let mut out_of_pool: HashMap<Pubkey, u64> = HashMap::new();
    // Fees PER MINT. A swap can pay one fee in lamports and another in the
    // token, and adding those two numbers together produces a quantity in
    // no unit at all (review round 4, M8), so they are kept apart until
    // the legs are known and only one mint's total is ever stored.
    let mut fees: HashMap<Pubkey, u64> = HashMap::new();
    // The PRINCIPAL leg of each direction: the largest transfer of it. A
    // direction can have several legs - the taker's and a fee recipient's
    // share one mint - and "the last one wins" picked an arbitrary member
    // of that set as the payer, the recipient and, through them, as the
    // account whose balance delta became `amount_out`.
    let mut principal_in: Option<&Movement> = None;
    let mut principal_out: Option<&Movement> = None;

    for movement in owned {
        if movement.destination_owner == Some(authority) {
            *into_pool.entry(movement.mint).or_insert(0) +=
                movement.amount;
            if principal_in
                .is_none_or(|best| best.amount < movement.amount)
            {
                principal_in = Some(movement);
            }
        } else if movement.source_owner == Some(authority) {
            *out_of_pool.entry(movement.mint).or_insert(0) +=
                movement.amount;
            if principal_out
                .is_none_or(|best| best.amount < movement.amount)
            {
                principal_out = Some(movement);
            }
        } else {
            // Neither side is the pool: a protocol / creator / router fee.
            *fees.entry(movement.mint).or_insert(0) += movement.amount;
        }
    }

    // Both mints the same way across the pool = a liquidity add or remove,
    // deliberately kept out of the swap table.
    if into_pool.len() >= 2 && out_of_pool.is_empty() {
        return Classified::Liquidity;
    }
    if out_of_pool.len() >= 2 && into_pool.is_empty() {
        return Classified::Liquidity;
    }
    if into_pool.len() != 1 || out_of_pool.len() != 1 {
        return Classified::Unclassified;
    }

    let (mint_in, amount_in) =
        into_pool.into_iter().next().expect("one entry");
    let (mint_out, amount_out_gross) =
        out_of_pool.into_iter().next().expect("one entry");

    if mint_in == mint_out {
        return Classified::Unclassified;
    }

    // Only a transfer of a LEG's mint can be this swap's fee, and only one
    // mint's total is storable. Where both legs carry fees the larger is
    // kept, with the mint that says what it is.
    let (fee_mint, fee_amount) = [mint_in, mint_out]
        .into_iter()
        .filter_map(|mint| fees.get(&mint).map(|amount| (mint, *amount)))
        .max_by_key(|(_, amount)| *amount)
        .unwrap_or((ZERO_PUBKEY, 0));

    let (payer, in_account) = principal_in
        .map(|movement| {
            (
                movement.source_owner.unwrap_or(movement.source),
                movement.destination,
            )
        })
        .unwrap_or((ZERO_PUBKEY, ZERO_PUBKEY));
    let (recipient, out_account) = principal_out
        .map(|movement| {
            (
                movement.destination_owner.unwrap_or(movement.destination),
                movement.destination,
            )
        })
        .unwrap_or((ZERO_PUBKEY, ZERO_PUBKEY));

    Classified::Swap(MovementSwap {
        venue,
        path: instruction.path.clone(),
        authority,
        mint_in,
        mint_out,
        amount_in,
        // Both "received" figures are filled in by the caller, which can
        // see the destination's real balance delta (a Token-2022 transfer
        // fee makes it smaller than what was sent).
        amount_in_received: amount_in,
        amount_out_gross,
        amount_out: amount_out_gross,
        fee_amount,
        fee_mint,
        payer,
        recipient,
        in_account,
        out_account,
        native_leg: false,
        hop: 0,
    })
}

/// Booked once a row is final: the rules that must hold whatever the
/// per-program layer did or did not manage to do.
fn finish_row(
    diagnostics: &mut Diagnostics,
    venue: Venue,
    instruction: &SvmInstruction,
    row: &mut SvmSwap,
) {
    if !venue.vault_authority_is_global() {
        return;
    }
    // A vault authority must never survive as the pool key. `build_row`
    // never writes one, and a per-program decoder writes the pool the
    // venue names, so this is the belt to that braces: an unnamed pool is
    // 32 zero bytes, which the candle views exclude.
    if row.pool_id == ZERO_PUBKEY {
        diagnostics.unnamed_pool += 1;
        return;
    }

    // The account index is checked against the fixtures, but a venue can
    // add an instruction variant with a different account layout at any
    // time. Where the venue's own event named the pool, the index is
    // checked against it on EVERY row, live: a non-zero counter here means
    // an index has gone stale and rows of that venue are being keyed on
    // some other account.
    let named = venue
        .pool_account_index()
        .filter(|_| {
            venue.instruction_kind(&instruction.data)
                == crate::svm::programs::IxKind::Swap
        })
        .and_then(|index| instruction.account(index));
    if row.confidence == Confidence::Decoded.as_str()
        && named.is_some_and(|pool| pool != row.pool_id)
    {
        diagnostics.pool_index_disagreed += 1;
    }
}

/// Builds the storable row. `amount_out` is adjusted here, where the
/// destination's real balance delta is reachable.
///
/// `None` when the instruction path cannot be packed into an ordinal.
/// `pack_ordinal` fails loudly rather than truncating precisely so that two
/// instructions never share a position key; swallowing that with
/// `unwrap_or(0)` reintroduced the collision it exists to prevent, so the
/// row is dropped and counted instead.
#[allow(clippy::too_many_arguments)]
fn build_row(
    chain: u64,
    timestamp: u32,
    tx: &SvmTransaction,
    instruction: &SvmInstruction,
    movements: &Movements<'_>,
    swap: &MovementSwap,
) -> Option<SvmSwap> {
    let ordinal =
        crate::svm::models::pack_ordinal_hop(&swap.path, swap.hop).ok()?;
    let (route_program, route_ordinal) = movements
        .nearest_router(&swap.path)
        .map(|(_, path, program)| {
            (program, pack_ordinal(&path).unwrap_or(0))
        })
        .unwrap_or((ZERO_PUBKEY, 0));

    // token0 / token1 are the two mints sorted by raw bytes: deterministic,
    // and it needs no pool registry. amount0 / amount1 are POOL RELATIVE and
    // signed, positive = into the pool - the same convention the EVM
    // decoder uses.
    let (token0, token1) = if swap.mint_in <= swap.mint_out {
        (swap.mint_in, swap.mint_out)
    } else {
        (swap.mint_out, swap.mint_in)
    };
    let amount_in_signed =
        I256::try_from(swap.amount_in).unwrap_or(I256::ZERO);
    let amount_out_signed =
        I256::try_from(swap.amount_out_gross).unwrap_or(I256::ZERO);
    let (amount0, amount1) = if token0 == swap.mint_in {
        (amount_in_signed, -amount_out_signed)
    } else {
        (-amount_out_signed, amount_in_signed)
    };

    // THE pool key. From the movement layer alone the "pool" is the common
    // OWNER of the two vaults, and for five of the ten streamed venues
    // that owner is one program-wide PDA shared by every pool the program
    // runs - storing it would key the whole venue into a single candle
    // series (review round 4, B3). For those venues the pool is taken from
    // the instruction's own account metas instead, and when even that is
    // not possible the row carries NO pool key rather than a wrong one: it
    // still counts as venue volume and is excluded from the pool-keyed
    // aggregates. A per-program decoder overwrites this with the account
    // the venue itself names.
    let pool_id = if swap.venue.vault_authority_is_global() {
        swap.venue
            .pool_account_index()
            // Only for an instruction the registry already knows is a
            // swap: a variant added after this was written has an account
            // layout nobody has checked, and the wrong account is worse
            // than none.
            .filter(|_| {
                swap.venue.instruction_kind(&instruction.data)
                    == crate::svm::programs::IxKind::Swap
            })
            .and_then(|index| instruction.account(index))
            .filter(|pool| *pool != swap.authority && *pool != ZERO_PUBKEY)
            .unwrap_or(ZERO_PUBKEY)
    } else {
        swap.authority
    };

    Some(
        SvmSwap {
            chain,
            block_number: tx.slot,
            tx_index: tx.tx_index,
            ordinal,
            timestamp,
            tx_id: tx.signature.to_vec(),
            pool_id,
            protocol: swap.venue.as_str().to_owned(),
            venue_program: crate::svm::programs::pubkey(
                swap.venue.program_b58(),
            ),
            // NEVER the instruction's signer: on Solana that is very often a
            // router PDA or a bot.
            trader: tx.fee_payer,
            sender: swap.payer,
            recipient: swap.recipient,
            token0,
            token1,
            amount0,
            amount1,
            token_in: swap.mint_in,
            token_out: swap.mint_out,
            amount_in: U256::from(swap.amount_in),
            amount_out: U256::from(swap.amount_out),
            amount_out_gross: U256::from(swap.amount_out_gross),
            // Always available on Solana: only the SPL Token program can move an
            // SPL balance, so both mints are PROVEN, not claimed.
            verified_in: swap.mint_in,
            verified_out: swap.mint_out,
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            fee_amount: U256::from(swap.fee_amount),
            fee_mint: swap.fee_mint,
            confidence: Confidence::Movement.as_str().to_owned(),
            route_ordinal,
            route_program,
            epoch: 0,
            _version: 0,
            is_deleted: 0,
        }
        .with_received(movements, swap),
    )
}

impl SvmSwap {
    /// A Token-2022 transfer fee makes what the taker RECEIVED smaller than
    /// what the pool SENT. Storing one `amount_out` would silently be wrong
    /// for every Token-2022 pair, so both are kept.
    ///
    /// The delta is read off the ONE account the principal output leg was
    /// paid into. The previous version matched every out-leg of `mint_out`
    /// in the subtree and took the first whose destination had a readable
    /// balance delta - which SKIPS the taker's own account when it is
    /// opened and closed inside the transaction (it then appears in
    /// neither the pre nor the post balances) and lands on the next
    /// movement, a protocol or creator FEE recipient whose small positive
    /// delta passes the `<= gross` guard. Measured on
    /// `3ZZw4CfNzTMgPnnJRhKk28bteiip6zDimArpQSNURYfLn94J4UTPmYfBqTU2SfJ3pbwodZV3rGsUz7Qfyh8MVPJP`:
    /// the taker received 127,409,301,712 and the row stored 32,922,301
    /// (review round 4, M4). When the taker's account is unreadable the
    /// answer is "what the pool sent", never another account's delta.
    fn with_received(
        mut self,
        movements: &Movements<'_>,
        swap: &MovementSwap,
    ) -> Self {
        if swap.native_leg {
            return self;
        }
        if let Some(delta) = movements.received_by(&swap.out_account) {
            if delta > 0
                && (delta as u128) <= u128::from(swap.amount_out_gross)
            {
                self.amount_out = U256::from(delta as u128);
            }
        }
        self
    }

    /// Sets what the pool authority reported, once a per-program decoder has
    /// confirmed it.
    pub(crate) fn mark_decoded(&mut self, pool: Pubkey) {
        self.pool_id = pool;
        self.confidence = Confidence::Decoded.as_str().to_owned();
    }

    /// The account the venue's own event names as the user of this trade.
    ///
    /// `trader` used to be the transaction's fee payer unconditionally,
    /// which on Solana is very often a relayer or a bot: on the recorded
    /// pump.fun curve sell the fee payer is a bot and the event's user is
    /// the person who traded, and `sol_dex_candles_*.traders` is
    /// `uniqState(trader)` - so the unique-trader count was a count of
    /// bots. `launchpads.rs` already stored the event's user for the very
    /// same trade, so the two tables disagreed about one trade (review
    /// round 4, M6).
    ///
    /// The fee payer stays the fallback, and only the fallback: a venue
    /// that names no user leaves it in place.
    pub(crate) fn mark_trader(&mut self, user: Pubkey) {
        if user != ZERO_PUBKEY {
            self.trader = user;
        }
    }

    /// The fee a venue's own event states, and the mint it is in. Storing
    /// one without the other is what let fees of different mints be added
    /// together (review round 4, M8).
    pub(crate) fn set_fee(&mut self, amount: u64, mint: Pubkey) {
        self.fee_amount = U256::from(amount);
        self.fee_mint = if amount == 0 { ZERO_PUBKEY } else { mint };
    }
}
