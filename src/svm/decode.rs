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
}

impl Diagnostics {
    pub fn merge(&mut self, other: &Diagnostics) {
        self.no_movement += other.no_movement;
        self.liquidity += other.liquidity;
        self.unclassified += other.unclassified;
        self.decoder_disagreed += other.decoder_disagreed;
        self.ambiguous_native += other.ambiguous_native;
        self.kind_disagreed += other.kind_disagreed;
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
    /// Into the pool.
    pub amount_in: u64,
    /// Out of the pool, as SENT.
    pub amount_out_gross: u64,
    /// Out of the pool, as RECEIVED by the taker. Differs from the gross
    /// amount by a Token-2022 transfer fee, which no event mentions.
    pub amount_out: u64,
    /// Transfers of a leg's mint that went somewhere other than the pool:
    /// protocol, creator and router fees.
    pub fee_amount: u64,
    pub payer: Pubkey,
    pub recipient: Pubkey,
    /// The SOL leg came from a lamport delta rather than an instruction.
    pub native_leg: bool,
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
            Classified::Swap(swap) => {
                let mut row =
                    build_row(chain, timestamp, tx, &movements, &swap);
                let enriched = crate::svm::events::enrich(
                    tx,
                    instruction,
                    venue,
                    &swap,
                    &mut row,
                    0,
                );
                record(
                    &mut outcome.diagnostics,
                    venue,
                    enriched,
                    instruction,
                );
                outcome.swaps.push(row);
            }
            // The movement layer proposed both sides of a symmetric
            // native-leg trade; the venue's own event decides which is the
            // pool. Exactly one reading can validate, because the event
            // names the user and the direction.
            Classified::NativeCandidates(proposals) => {
                let only_one = proposals.len() == 1;
                let mut accepted = None;
                let mut verdict = crate::svm::events::Enrichment::None;
                for swap in &proposals {
                    let mut row =
                        build_row(chain, timestamp, tx, &movements, swap);
                    match crate::svm::events::enrich(
                        tx,
                        instruction,
                        venue,
                        swap,
                        &mut row,
                        0,
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
                    Some(row) => {
                        record(
                            &mut outcome.diagnostics,
                            venue,
                            verdict,
                            instruction,
                        );
                        outcome.swaps.push(row);
                    }
                    None => outcome.diagnostics.ambiguous_native += 1,
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
        Enrichment::None => {}
    }

    if venue.instruction_kind(&instruction.data) == IxKind::Liquidity {
        diagnostics.kind_disagreed += 1;
    }
}

enum Classified {
    Swap(MovementSwap),
    /// A native-leg trade whose pool side the movement layer cannot pick on
    /// its own. Each entry is the same trade read from one candidate's point
    /// of view, so at most ONE can be right; the per-program decoder picks.
    NativeCandidates(Vec<MovementSwap>),
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
        _ => Classified::NativeCandidates(swaps),
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
        amount_out_gross: amount_out,
        amount_out,
        fee_amount: 0,
        payer,
        recipient: payer,
        native_leg: true,
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
    let mut fees: u64 = 0;
    let mut payer = ZERO_PUBKEY;
    let mut recipient = ZERO_PUBKEY;

    for movement in owned {
        if movement.destination_owner == Some(authority) {
            *into_pool.entry(movement.mint).or_insert(0) +=
                movement.amount;
            if let Some(owner) = movement.source_owner {
                payer = owner;
            }
        } else if movement.source_owner == Some(authority) {
            *out_of_pool.entry(movement.mint).or_insert(0) +=
                movement.amount;
            if let Some(owner) = movement.destination_owner {
                recipient = owner;
            } else {
                recipient = movement.destination;
            }
        } else {
            // Neither side is the pool: a protocol / creator / router fee.
            fees = fees.saturating_add(movement.amount);
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

    Classified::Swap(MovementSwap {
        venue,
        path: instruction.path.clone(),
        authority,
        mint_in,
        mint_out,
        amount_in,
        amount_out_gross,
        // Filled in by the caller, which can see the destination's real
        // balance delta (a Token-2022 transfer fee makes it smaller).
        amount_out: amount_out_gross,
        fee_amount: fees,
        payer,
        recipient,
        native_leg: false,
    })
}

/// Builds the storable row. `amount_out` is adjusted here, where the
/// destination's real balance delta is reachable.
fn build_row(
    chain: u64,
    timestamp: u32,
    tx: &SvmTransaction,
    movements: &Movements<'_>,
    swap: &MovementSwap,
) -> SvmSwap {
    let ordinal = pack_ordinal(&swap.path).unwrap_or(0);
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

    SvmSwap {
        chain,
        block_number: tx.slot,
        tx_index: tx.tx_index,
        ordinal,
        timestamp,
        tx_id: tx.signature.to_vec(),
        // From the movement layer alone the pool IS the authority; a
        // per-program decoder replaces it with the account the venue names.
        pool_id: swap.authority,
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
        confidence: Confidence::Movement.as_str().to_owned(),
        route_ordinal,
        route_program,
        epoch: 0,
        _version: 0,
        is_deleted: 0,
    }
    .with_received(movements, swap)
}

impl SvmSwap {
    /// A Token-2022 transfer fee makes what the taker RECEIVED smaller than
    /// what the pool SENT. Storing one `amount_out` would silently be wrong
    /// for every Token-2022 pair, so both are kept.
    fn with_received(
        mut self,
        movements: &Movements<'_>,
        swap: &MovementSwap,
    ) -> Self {
        if swap.native_leg {
            return self;
        }
        let destination_delta = movements
            .movements
            .iter()
            .filter(|movement| {
                movement.source_owner == Some(swap.authority)
                    && movement.mint == swap.mint_out
                    && swap.path.len() < movement.path.len()
                    && movement.path.starts_with(&swap.path)
            })
            .filter_map(|movement| {
                movements.received_by(&movement.destination)
            })
            .next();

        if let Some(delta) = destination_delta {
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
}
