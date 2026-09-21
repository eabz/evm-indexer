//! `decode(chain, logs) -> LaunchpadRows`: pure, no I/O, never panics.
//!
//! Decoding is by event FAMILY (`topic0` + the exact shape of the log),
//! never by an address registry, so a new deployment of a known ABI - on
//! any chain - is indexed on day one. The flip side is that ANY contract
//! can emit these events: what makes a row trustworthy is decided at read
//! time (`launchpad_trusted_emitters`, README §3) and, for the amounts, by
//! the corroboration below.
//!
//! # Corroboration (the forgery rule, mirrored from `src/dex`)
//!
//! A curve event is a CLAIM of whoever emitted it. An ERC-20 `Transfer` is
//! a claim of the TOKEN. So a leg of a curve trade is **verified** when, in
//! the same transaction, some token contract reports a movement of exactly
//! the leg's amount to the emitter (in leg) or from the emitter (out leg):
//!
//! * candidates = ERC-20 `Transfer` logs (3 topics, 32 data bytes) of the
//!   trade's transaction, `value` EQUAL to the leg amount (no tolerance),
//!   the right direction against the emitter, emitted by a contract other
//!   than the emitter, and not yet consumed by an earlier leg;
//! * a per-token curve contract (`pons_v2`) moves the tokens BEFORE it
//!   emits, so only transfers with a smaller log index count. The Flap
//!   portal is a singleton that settles some legs after its event, so
//!   there any position counts;
//! * candidates of more than one token => the leg stays UNVERIFIED.
//!   Ambiguity is never resolved by guessing;
//! * otherwise the last candidate is consumed and its emitter is the
//!   proven asset. One transfer verifies one leg.
//!
//! What this proves: that asset moved, in that amount, to / from the
//! emitter. What it does NOT prove: that the movement was a trade at a
//! market price (wash trading stays possible, as on any real venue).
//!
//! Native-coin legs have no log and can NEVER be verified this way. See
//! [`LaunchpadTrade::sole_unverified_quote`] and README §3 for exactly
//! what `transactions.value` adds and what it does not.
//!
//! # The text these events carry is HOSTILE
//!
//! `name`, `symbol` and `metadata_uri` are bytes chosen by whoever emitted
//! the log, and they come back out of every feed onto a screen.
//! [`sanitize`] strips the control AND the Unicode format characters (the
//! bidi overrides, the zero width and tag blocks) and caps the length. It
//! does not escape for any output format: **the UI escapes them**.

use std::collections::HashMap;

use alloy::primitives::{Address, Bytes, B256, U256};

use crate::{
    core::models::log::DatabaseLog,
    db::format::{address_of_id32, id32, tx_id},
};

use super::{
    events::{self, EventDef},
    models::{
        Family, FeeKind, FeePhase, LaunchpadCreatorFee,
        LaunchpadGraduation, LaunchpadToken, LaunchpadTrade, PoolKind,
        Side,
    },
    LaunchpadRows,
};

/// Longest on-chain text kept (names, symbols, metadata URIs).
const MAX_TEXT: usize = 256;

/// `1e18`: a curve progress of one whole token sold out (`flap_portal`).
const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

// -------------------------------------------------------------- reading

/// Topics of a log, absent ones distinguished from zero ones.
#[derive(Clone, Copy)]
struct Topics {
    values: [B256; 4],
    count: usize,
}

impl Topics {
    fn of(log: &DatabaseLog) -> Self {
        let topics = [log.topic0, log.topic1, log.topic2, log.topic3];
        let count = topics.iter().take_while(|t| t.is_some()).count();
        // A hole (None followed by Some) can not come from a node.
        let count = if topics[count..].iter().any(Option::is_some) {
            0
        } else {
            count
        };

        Self { values: topics.map(Option::unwrap_or_default), count }
    }

    fn at(&self, index: usize) -> B256 {
        if index < self.count {
            self.values[index]
        } else {
            B256::ZERO
        }
    }

    /// Topic `index` as an address, zero when it is not an EVM address
    /// (the 12 leading bytes must be zero).
    fn address(&self, index: usize) -> Address {
        clean_address(self.at(index))
    }
}

/// The EVM address inside a 32 byte word, zero when the padding is not
/// zero (so a forged word can never turn into someone else's address).
fn clean_address(word: B256) -> Address {
    address_of_id32(word).unwrap_or(Address::ZERO)
}

/// Bounds-checked view over the data section of a log.
#[derive(Clone, Copy)]
struct Data<'a>(&'a [u8]);

impl<'a> Data<'a> {
    fn word(&self, index: usize) -> B256 {
        let start = index.saturating_mul(32);
        let end = start.saturating_add(32);

        match self.0.get(start..end) {
            Some(bytes) => B256::from_slice(bytes),
            None => B256::ZERO,
        }
    }

    fn u256(&self, index: usize) -> U256 {
        U256::from_be_bytes(self.word(index).0)
    }

    fn address(&self, index: usize) -> Address {
        clean_address(self.word(index))
    }

    /// An `int24` stored in a 32 byte word, as `i32`.
    fn int24(&self, index: usize) -> i32 {
        let word = self.word(index);
        let raw =
            u32::from_be_bytes([0, word.0[29], word.0[30], word.0[31]]);
        if raw & 0x0080_0000 != 0 {
            (raw | 0xff00_0000) as i32
        } else {
            raw as i32
        }
    }

    /// A dynamic `string` whose head word is at `index`, [`sanitize`]d.
    /// Empty on any malformed offset / length - never a panic, never an
    /// allocation bigger than [`MAX_TEXT`].
    fn text(&self, index: usize) -> String {
        let offset: usize = match self.u256(index).try_into() {
            Ok(offset) => offset,
            Err(_) => return String::new(),
        };
        if !offset.is_multiple_of(32) || offset >= self.0.len() {
            return String::new();
        }

        let length: usize = match self.u256(offset / 32).try_into() {
            Ok(length) => length,
            Err(_) => return String::new(),
        };
        let start = offset + 32;
        let end = match start.checked_add(length) {
            Some(end) if end <= self.0.len() => end,
            _ => return String::new(),
        };

        sanitize(&String::from_utf8_lossy(
            &self.0[start..end.min(start + MAX_TEXT)],
        ))
    }
}

/// Unicode FORMAT characters (general category `Cf`) as of Unicode 16,
/// plus the two separators `Zl` / `Zp`. `char::is_control()` is `Cc`
/// ONLY, so on its own it lets `U+202E RIGHT-TO-LEFT OVERRIDE` and the
/// rest of this list through.
const HIDDEN: &[(u32, u32)] = &[
    (0x00ad, 0x00ad),
    (0x0600, 0x0605),
    (0x061c, 0x061c),
    (0x06dd, 0x06dd),
    (0x070f, 0x070f),
    (0x0890, 0x0891),
    (0x08e2, 0x08e2),
    (0x180e, 0x180e),
    // Zero width space / non-joiner / joiner, LRM, RLM.
    (0x200b, 0x200f),
    // Line and paragraph separator (Zl / Zp).
    (0x2028, 0x2029),
    // The bidi overrides and embeddings - "Trojan Source".
    (0x202a, 0x202e),
    // Word joiner, the invisible operators, and the bidi isolates.
    (0x2060, 0x206f),
    (0xfeff, 0xfeff),
    // Interlinear annotation.
    (0xfff9, 0xfffb),
    (0x110bd, 0x110bd),
    (0x110cd, 0x110cd),
    (0x13430, 0x1343f),
    (0x1bca0, 0x1bca3),
    (0x1d173, 0x1d17a),
    // The tag characters: a whole second string, invisible.
    (0xe0001, 0xe0001),
    (0xe0020, 0xe007f),
];

/// Longest text kept, in CHARACTERS (the byte cap of [`MAX_TEXT`] runs
/// first, so this only ever shortens further).
const MAX_CHARS: usize = 128;

/// Strips the characters that let on-chain text lie about what it is, and
/// collapses the whitespace runs a removed character leaves behind.
///
/// `name`, `symbol` and `metadata_uri` are chosen by whoever emitted the
/// log - the decoder has no registry - and they come back out of every
/// feed onto a screen. Removed: the `Cc` controls (a newline forges a
/// second row in a log line or a CSV export), the `Cf` format characters
/// including the bidi overrides and the invisible tag block, and the two
/// Unicode separators. A symbol is also one line, so every remaining
/// whitespace character becomes a plain space and runs of them collapse.
///
/// This does NOT escape for any output format: the strings are stored as
/// TEXT and **the UI escapes them** for whatever it renders into (the
/// README says so next to the cookbook). A `<script>` in a symbol is data
/// here and must stay data there. Same rule, and very nearly the same
/// code, as `crate::predictions::text::sanitize`; if a change ever has to
/// touch both, one of the two modules owns it and the other calls it.
///
/// Public so the Solana launchpad decoder (`svm::launchpads`) applies the
/// SAME rule to the same columns: a pump.fun symbol carrying a bidi
/// override is no less hostile than an EVM one, and two copies of this
/// would drift.
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_CHARS * 4));
    let mut kept = 0usize;
    let mut pending_space = false;

    for c in text.chars() {
        // Whitespace FIRST, so a newline or a tab becomes the one space
        // that keeps two words apart instead of vanishing as a control.
        if c.is_whitespace() {
            pending_space = true;
            continue;
        }
        // A hidden character is dropped without a space: it was put
        // INSIDE a word precisely so a reader would not see the join.
        let code = u32::from(c);
        if c.is_control()
            || HIDDEN.iter().any(|(lo, hi)| (*lo..=*hi).contains(&code))
        {
            continue;
        }

        let width = usize::from(pending_space && kept > 0) + 1;
        if kept + width > MAX_CHARS {
            break;
        }
        if pending_space && kept > 0 {
            out.push(' ');
        }
        out.push(c);
        kept += width;
        pending_space = false;
    }

    out
}

/// Does the log have exactly the shape the definition declares?
fn matches(log: &DatabaseLog, topics: &Topics, def: &EventDef) -> bool {
    if topics.count != usize::from(def.topics) {
        return false;
    }

    match def.data_len {
        Some(length) => log.data.len() == length,
        // Dynamic payloads still have one head word per parameter.
        None => {
            let head = def.signature.matches(',').count() + 1
                - (usize::from(def.topics) - 1);
            log.data.len() >= head * 32
                && log.data.len().is_multiple_of(32)
        }
    }
}

// ------------------------------------------------------------- evidence

struct TransferSeen {
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
    ordinal: u64,
    used: bool,
}

struct CreditSeen {
    recipient: Address,
    source: Address,
    amount: U256,
    used: bool,
}

/// What the OTHER logs of a transaction say. Collected in one pass before
/// anything is decoded, so a decoder never looks forward by itself.
#[derive(Default)]
struct TxEvidence {
    transfers: Vec<TransferSeen>,
    credits: Vec<CreditSeen>,
    /// `flap_portal` `TokenQuoteSet`: token -> quote asset.
    quote_of: HashMap<Address, Address>,
    /// `flap_portal` `FlapTokenProgressChanged`: (token, wad, ordinal),
    /// in log order. A transaction can hold several per token.
    progress: Vec<(Address, U256, u64)>,
    /// `pons_v2` `PoolRegistered`: token -> (pool id, quote, creator).
    registered: HashMap<Address, (B256, Address, Address)>,
    /// `pons_v2` curves that emitted `CurveCompleted`.
    completed: Vec<Address>,
}

impl TxEvidence {
    /// The asset that moved exactly `amount` to (`inbound`) or from the
    /// `emitter`, consuming the transfer that proves it.
    fn verify_leg(
        &mut self,
        emitter: Address,
        amount: U256,
        inbound: bool,
        before: Option<u64>,
    ) -> Option<Address> {
        let mut found: Option<usize> = None;
        let mut token: Option<Address> = None;

        for (index, transfer) in self.transfers.iter().enumerate() {
            let side = if inbound { transfer.to } else { transfer.from };
            let positioned =
                before.is_none_or(|ordinal| transfer.ordinal < ordinal);

            if transfer.used
                || transfer.amount != amount
                || side != emitter
                || transfer.token == emitter
                || !positioned
            {
                continue;
            }

            match token {
                // Two different tokens could prove this leg: ambiguous.
                Some(seen) if seen != transfer.token => return None,
                _ => {}
            }
            token = Some(transfer.token);
            found = Some(index);
        }

        let index = found?;
        self.transfers[index].used = true;
        token
    }

    /// Who an escrow credited `amount` on behalf of `source`.
    fn credited(
        &mut self,
        source: Address,
        amount: U256,
    ) -> Option<Address> {
        let index = self.credits.iter().position(|credit| {
            !credit.used
                && credit.source == source
                && credit.amount == amount
        })?;
        self.credits[index].used = true;
        Some(self.credits[index].recipient)
    }

    /// Supply minted by `token` itself in this transaction (the largest
    /// `Transfer` from the zero address emitted BY the token).
    fn minted(&self, token: Address) -> U256 {
        self.transfers
            .iter()
            .filter(|t| t.token == token && t.from == Address::ZERO)
            .map(|t| t.amount)
            .max()
            .unwrap_or(U256::ZERO)
    }

    /// Curve progress reported for `token` right AFTER `ordinal`.
    fn progress_after(&self, token: Address, ordinal: u64) -> U256 {
        self.progress
            .iter()
            .filter(|(seen, _, at)| *seen == token && *at > ordinal)
            .min_by_key(|(_, _, at)| *at)
            .map(|(_, wad, _)| *wad)
            .unwrap_or(U256::ZERO)
    }

    /// The asset that moved exactly `amount` INTO `pool` (graduations).
    fn moved_into(&self, pool: Address, amount: U256) -> Address {
        let mut token = Address::ZERO;

        for transfer in &self.transfers {
            if transfer.to != pool || transfer.amount != amount {
                continue;
            }
            if token != Address::ZERO && token != transfer.token {
                return Address::ZERO;
            }
            token = transfer.token;
        }

        token
    }
}

fn collect(logs: &[DatabaseLog]) -> HashMap<B256, TxEvidence> {
    let mut by_transaction: HashMap<B256, TxEvidence> = HashMap::new();

    for log in logs {
        let topics = Topics::of(log);
        if topics.count == 0 {
            continue;
        }
        let data = Data(log.data.as_ref());
        let emitter = log.address;
        let topic0 = topics.at(0);

        if topic0 == events::ERC20_TRANSFER.topic0
            && matches(log, &topics, &events::ERC20_TRANSFER)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .transfers
                .push(TransferSeen {
                    token: emitter,
                    from: topics.address(1),
                    to: topics.address(2),
                    amount: data.u256(0),
                    ordinal: u64::from(log.log_index),
                    used: false,
                });
            continue;
        }

        if topic0 == events::PONS_V2_CREDITED.topic0
            && matches(log, &topics, &events::PONS_V2_CREDITED)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .credits
                .push(CreditSeen {
                    recipient: topics.address(1),
                    source: topics.address(2),
                    amount: data.u256(0),
                    used: false,
                });
            continue;
        }

        if topic0 == events::FLAP_TOKEN_QUOTE_SET.topic0
            && matches(log, &topics, &events::FLAP_TOKEN_QUOTE_SET)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .quote_of
                .insert(data.address(0), data.address(1));
            continue;
        }

        if topic0 == events::FLAP_PROGRESS_CHANGED.topic0
            && matches(log, &topics, &events::FLAP_PROGRESS_CHANGED)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .progress
                .push((
                    data.address(0),
                    data.u256(1),
                    u64::from(log.log_index),
                ));
            continue;
        }

        if topic0 == events::PONS_V2_POOL_REGISTERED.topic0
            && matches(log, &topics, &events::PONS_V2_POOL_REGISTERED)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .registered
                .insert(
                    data.address(0),
                    (topics.at(1), data.address(1), data.address(2)),
                );
            continue;
        }

        if topic0 == events::PONS_V2_CURVE_COMPLETED.topic0
            && matches(log, &topics, &events::PONS_V2_CURVE_COMPLETED)
        {
            by_transaction
                .entry(log.transaction_hash)
                .or_default()
                .completed
                .push(emitter);
        }
    }

    by_transaction
}

// --------------------------------------------------------------- decode

/// Where a row sits, shared by every decoder.
#[derive(Clone, Copy)]
struct Place {
    chain: u64,
    block_number: u64,
    timestamp: u32,
    transaction_hash: B256,
    tx_index: u32,
    ordinal: u64,
}

impl Place {
    fn of(chain: u64, log: &DatabaseLog) -> Self {
        Self {
            chain,
            block_number: log.block_number,
            timestamp: log.timestamp,
            transaction_hash: log.transaction_hash,
            tx_index: log.transaction_index,
            ordinal: u64::from(log.log_index),
        }
    }
}

fn token_row(
    place: Place,
    family: Family,
    emitter: Address,
    token: Address,
) -> LaunchpadToken {
    LaunchpadToken {
        chain: place.chain,
        token,
        family,
        emitter,
        curve: Address::ZERO,
        creator: Address::ZERO,
        name: String::new(),
        symbol: String::new(),
        metadata_uri: String::new(),
        quote_token: Address::ZERO,
        initial_supply: U256::ZERO,
        graduation_threshold: U256::ZERO,
        pool_id: B256::ZERO,
        pool_kind: PoolKind::PoolId,
        launch_config_id: U256::ZERO,
        block_number: place.block_number,
        timestamp: place.timestamp,
        tx_id: tx_id(place.transaction_hash),
        tx_index: place.tx_index,
        ordinal: place.ordinal,
        tx_from: Address::ZERO,
        epoch: 0,
        _version: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn fee_row(
    place: Place,
    component: u32,
    family: Family,
    emitter: Address,
    token: Address,
    pool_id: B256,
    phase: FeePhase,
    kind: FeeKind,
    amount: U256,
) -> LaunchpadCreatorFee {
    LaunchpadCreatorFee {
        chain: place.chain,
        block_number: place.block_number,
        timestamp: place.timestamp,
        tx_id: tx_id(place.transaction_hash),
        tx_index: place.tx_index,
        ordinal: place.ordinal,
        component,
        family,
        emitter,
        token,
        pool_id,
        phase,
        kind,
        recipient: Address::ZERO,
        recipient_known: 0,
        quote_token: Address::ZERO,
        amount,
        tx_from: Address::ZERO,
        epoch: 0,
        _version: 0,
    }
}

/// Decodes every launchpad row of a batch of logs.
///
/// The batch always holds WHOLE transactions (the pipeline flushes whole
/// blocks), which is what makes the corroboration above sound.
pub fn decode(chain: u64, logs: &[DatabaseLog]) -> LaunchpadRows {
    let mut rows = LaunchpadRows::default();
    let mut evidence = collect(logs);

    for log in logs {
        let topics = Topics::of(log);
        if topics.count == 0 {
            continue;
        }

        let topic0 = topics.at(0);
        let data = Data(log.data.as_ref());
        let emitter = log.address;
        let place = Place::of(chain, log);
        let evidence = evidence.entry(log.transaction_hash).or_default();

        // ------------------------------------------------ pons_v2 launch
        if topic0 == events::PONS_V2_TOKEN_LAUNCHED.topic0
            && matches(log, &topics, &events::PONS_V2_TOKEN_LAUNCHED)
        {
            let token = topics.address(1);
            rows.tokens.push(LaunchpadToken {
                curve: topics.address(2),
                creator: topics.address(3),
                quote_token: data.address(0),
                launch_config_id: data.u256(1),
                graduation_threshold: data.u256(2),
                initial_supply: evidence.minted(token),
                ..token_row(place, Family::PonsV2, emitter, token)
            });
            continue;
        }

        // -------------------------------------------- flap_portal launch
        if topic0 == events::FLAP_TOKEN_CREATED.topic0
            && matches(log, &topics, &events::FLAP_TOKEN_CREATED)
        {
            let token = data.address(3);
            rows.tokens.push(LaunchpadToken {
                curve: emitter,
                creator: data.address(1),
                name: data.text(4),
                symbol: data.text(5),
                metadata_uri: data.text(6),
                quote_token: evidence
                    .quote_of
                    .get(&token)
                    .copied()
                    .unwrap_or(Address::ZERO),
                initial_supply: evidence.minted(token),
                launch_config_id: data.u256(2),
                ..token_row(place, Family::FlapPortal, emitter, token)
            });
            continue;
        }

        // ------------------------------------- attribution only launches
        if topic0 == events::PONS_V1_TOKEN_LAUNCHED.topic0
            && matches(log, &topics, &events::PONS_V1_TOKEN_LAUNCHED)
        {
            let token = topics.address(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.address(2),
                quote_token: data.address(0),
                pool_id: id32(data.address(1)),
                pool_kind: PoolKind::PoolAddress,
                launch_config_id: data.u256(3),
                initial_supply: evidence.minted(token),
                ..token_row(place, Family::PonsV1, emitter, token)
            });
            continue;
        }

        if topic0 == events::LETSCASH_TOKEN_LAUNCHED.topic0
            && matches(log, &topics, &events::LETSCASH_TOKEN_LAUNCHED)
        {
            let token = topics.address(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.address(2),
                pool_id: topics.at(3),
                launch_config_id: data.u256(0),
                initial_supply: evidence.minted(token),
                ..token_row(place, Family::LetsCash, emitter, token)
            });
            continue;
        }

        if topic0 == events::BAGS_TOKEN_CREATED.topic0
            && matches(log, &topics, &events::BAGS_TOKEN_CREATED)
        {
            let token = topics.address(1);
            rows.tokens.push(LaunchpadToken {
                curve: topics.address(2),
                creator: topics.address(3),
                pool_id: data.word(2),
                name: data.text(3),
                symbol: data.text(4),
                metadata_uri: data.text(5),
                initial_supply: evidence.minted(token),
                ..token_row(place, Family::Bags, emitter, token)
            });
            continue;
        }

        if topic0 == events::CLANKER_V4_TOKEN_CREATED.topic0
            && matches(log, &topics, &events::CLANKER_V4_TOKEN_CREATED)
        {
            let token = topics.address(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.address(2),
                metadata_uri: data.text(1),
                name: data.text(2),
                symbol: data.text(3),
                pool_id: data.word(8),
                quote_token: data.address(9),
                launch_config_id: U256::from(
                    i64::from(data.int24(6)).unsigned_abs(),
                ),
                initial_supply: evidence.minted(token),
                ..token_row(place, Family::ClankerV4, emitter, token)
            });
            continue;
        }

        // ------------------------------------------------ pons_v2 trades
        let pons_buy = topic0 == events::PONS_V2_CURVE_BUY.topic0
            && matches(log, &topics, &events::PONS_V2_CURVE_BUY);
        let pons_sell = topic0 == events::PONS_V2_CURVE_SELL.topic0
            && matches(log, &topics, &events::PONS_V2_CURVE_SELL);

        if pons_buy || pons_sell {
            // buy:  (quoteIn, tokensOut, fee, tax) - gross quote in
            // sell: (tokensIn, quoteOut, fee, tax) - net quote out
            let (token_amount, quote_amount) = if pons_buy {
                (data.u256(1), data.u256(0))
            } else {
                (data.u256(0), data.u256(1))
            };
            let side = if pons_buy { Side::Buy } else { Side::Sell };

            // A per-token curve moves the assets BEFORE it emits.
            let at = Some(place.ordinal);
            let token = evidence
                .verify_leg(emitter, token_amount, pons_sell, at)
                .unwrap_or(Address::ZERO);
            let quote = evidence
                .verify_leg(emitter, quote_amount, pons_buy, at)
                .unwrap_or(Address::ZERO);

            rows.trades.push(LaunchpadTrade {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                tx_id: tx_id(place.transaction_hash),
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::PonsV2,
                emitter,
                token,
                token_verified: u8::from(token != Address::ZERO),
                quote_token: quote,
                quote_verified: u8::from(quote != Address::ZERO),
                side,
                // buy: the recipient of the tokens; sell: the account the
                // tokens came from. The other one is a router, the launch
                // forwarder or the quote recipient.
                trader: if pons_buy {
                    topics.address(2)
                } else {
                    topics.address(1)
                },
                caller: if pons_buy {
                    topics.address(1)
                } else {
                    topics.address(2)
                },
                token_amount,
                quote_amount,
                fee_amount: data.u256(2),
                tax_amount: data.u256(3),
                progress_wad: U256::ZERO,
                graduating: u8::from(
                    evidence.completed.contains(&emitter),
                ),
                sole_unverified_quote: 0,
                tx_from: Address::ZERO,
                tx_to: Address::ZERO,
                tx_value: U256::ZERO,
                epoch: 0,
                _version: 0,
            });
            continue;
        }

        // -------------------------------------------- flap_portal trades
        let flap_buy = topic0 == events::FLAP_TOKEN_BOUGHT.topic0
            && matches(log, &topics, &events::FLAP_TOKEN_BOUGHT);
        let flap_sell = topic0 == events::FLAP_TOKEN_SOLD.topic0
            && matches(log, &topics, &events::FLAP_TOKEN_SOLD);

        if flap_buy || flap_sell {
            let token = data.address(1);
            let token_amount = data.u256(3);
            let quote_amount = data.u256(4);

            // The portal is a singleton: it settles some legs after the
            // event, so position is not a filter here.
            let progress = evidence.progress_after(token, place.ordinal);
            let proven_token = evidence.verify_leg(
                emitter,
                token_amount,
                flap_sell,
                None,
            );
            let quote = evidence
                .verify_leg(emitter, quote_amount, flap_buy, None)
                .unwrap_or(Address::ZERO);

            rows.trades.push(LaunchpadTrade {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                tx_id: tx_id(place.transaction_hash),
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::FlapPortal,
                emitter,
                token,
                // The event NAMES the token; the flag says whether the
                // token itself confirmed the movement.
                token_verified: u8::from(proven_token == Some(token)),
                quote_token: quote,
                quote_verified: u8::from(quote != Address::ZERO),
                side: if flap_buy { Side::Buy } else { Side::Sell },
                trader: data.address(2),
                caller: data.address(2),
                token_amount,
                quote_amount,
                fee_amount: data.u256(5),
                tax_amount: U256::ZERO,
                progress_wad: progress,
                graduating: u8::from(progress == WAD),
                sole_unverified_quote: 0,
                tx_from: Address::ZERO,
                tx_to: Address::ZERO,
                tx_value: U256::ZERO,
                epoch: 0,
                _version: 0,
            });
            continue;
        }

        // ------------------------------------------------- graduations
        if topic0 == events::PONS_V2_POOL_GRADUATED.topic0
            && matches(log, &topics, &events::PONS_V2_POOL_GRADUATED)
        {
            let token = topics.address(1);
            let (pool_id, quote_token, _) = evidence
                .registered
                .get(&token)
                .copied()
                .unwrap_or((B256::ZERO, Address::ZERO, Address::ZERO));

            rows.graduations.push(LaunchpadGraduation {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                tx_id: tx_id(place.transaction_hash),
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::PonsV2,
                emitter,
                token,
                pool_id,
                pool_kind: PoolKind::PoolId,
                quote_token,
                token_amount: data.u256(1),
                quote_amount: data.u256(2),
                position_id: data.u256(0),
                tx_from: Address::ZERO,
                epoch: 0,
                _version: 0,
            });
            continue;
        }

        if topic0 == events::FLAP_LAUNCHED_TO_DEX.topic0
            && matches(log, &topics, &events::FLAP_LAUNCHED_TO_DEX)
        {
            let pool = data.address(1);
            let quote_amount = data.u256(3);

            rows.graduations.push(LaunchpadGraduation {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                tx_id: tx_id(place.transaction_hash),
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::FlapPortal,
                emitter,
                token: data.address(0),
                pool_id: id32(pool),
                pool_kind: PoolKind::PoolAddress,
                // Proven by the asset that really moved into the pair.
                quote_token: evidence.moved_into(pool, quote_amount),
                token_amount: data.u256(2),
                quote_amount,
                position_id: U256::ZERO,
                tx_from: Address::ZERO,
                epoch: 0,
                _version: 0,
            });
            continue;
        }

        // -------------------------------------------------------- fees
        if topic0 == events::PONS_V2_FEES_SWEPT.topic0
            && matches(log, &topics, &events::PONS_V2_FEES_SWEPT)
        {
            push_sweep(
                &mut rows,
                evidence,
                place,
                Family::PonsV2,
                emitter,
                B256::ZERO,
                FeePhase::Curve,
                &[
                    (FeeKind::Protocol, data.u256(0)),
                    (FeeKind::Buyback, data.u256(1)),
                    (FeeKind::Creator, data.u256(2)),
                ],
            );
            continue;
        }

        if topic0 == events::PONS_V2_POOL_FEES_SWEPT.topic0
            && matches(log, &topics, &events::PONS_V2_POOL_FEES_SWEPT)
        {
            push_sweep(
                &mut rows,
                evidence,
                place,
                Family::PonsV2,
                emitter,
                topics.at(1),
                FeePhase::Dex,
                &[
                    (FeeKind::Protocol, data.u256(0)),
                    (FeeKind::Buyback, data.u256(1)),
                    (FeeKind::Creator, data.u256(2)),
                    (FeeKind::Locked, data.u256(3)),
                ],
            );
            continue;
        }

        let flap_tax = (topic0 == events::FLAP_TAX_V2.topic0
            && matches(log, &topics, &events::FLAP_TAX_V2))
            || (topic0 == events::FLAP_TAX_V1.topic0
                && matches(log, &topics, &events::FLAP_TAX_V1));

        if flap_tax {
            let amount = data.u256(0);
            if amount != U256::ZERO {
                rows.creator_fees.push(fee_row(
                    place,
                    0,
                    Family::FlapPortal,
                    emitter,
                    topics.address(1),
                    B256::ZERO,
                    FeePhase::Curve,
                    FeeKind::Tax,
                    amount,
                ));
            }
        }
    }

    mark_sole_unverified_quotes(&mut rows);
    rows
}

#[allow(clippy::too_many_arguments)]
fn push_sweep(
    rows: &mut LaunchpadRows,
    evidence: &mut TxEvidence,
    place: Place,
    family: Family,
    emitter: Address,
    pool_id: B256,
    phase: FeePhase,
    components: &[(FeeKind, U256)],
) {
    for (index, (kind, amount)) in components.iter().enumerate() {
        if *amount == U256::ZERO {
            continue;
        }

        let mut row = fee_row(
            place,
            index as u32,
            family,
            emitter,
            Address::ZERO,
            pool_id,
            phase,
            *kind,
            *amount,
        );

        // Only the escrow's `Credited` names the beneficiary.
        if *kind != FeeKind::Locked {
            if let Some(recipient) = evidence.credited(emitter, *amount) {
                row.recipient = recipient;
                row.recipient_known = 1;
            }
        }

        rows.creator_fees.push(row);
    }
}

/// A native-coin quote leg leaves no log. `transactions.value` can bound
/// it, but ONLY when the transaction holds a single such trade: a router
/// that buys for fifteen wallets in one transaction (a real fixture) sends
/// one `value` for all of them.
fn mark_sole_unverified_quotes(rows: &mut LaunchpadRows) {
    let mut unverified: HashMap<Bytes, u32> = HashMap::new();

    for trade in &rows.trades {
        if trade.quote_verified == 0 {
            *unverified.entry(trade.tx_id.clone()).or_default() += 1;
        }
    }

    for trade in &mut rows.trades {
        trade.sole_unverified_quote = u8::from(
            trade.quote_verified == 0
                && unverified.get(&trade.tx_id) == Some(&1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::log::test_support::log_with;

    #[test]
    fn words_and_text_are_bounds_checked() {
        let data = Data(&[]);
        assert_eq!(data.word(0), B256::ZERO);
        assert_eq!(data.u256(1_000_000), U256::ZERO);
        assert_eq!(data.text(3), "");

        // An offset pointing past the end yields "", never a panic.
        let mut bytes = vec![0u8; 64];
        bytes[31] = 0xff;
        assert_eq!(Data(&bytes).text(0), "");

        // A length larger than the payload yields "" too.
        let mut bytes = vec![0u8; 96];
        bytes[31] = 32;
        bytes[63] = 0xff;
        assert_eq!(Data(&bytes).text(0), "");
    }

    #[test]
    fn int24_is_sign_extended() {
        let mut bytes = vec![0u8; 32];
        bytes[29..32].copy_from_slice(&[0xff, 0xff, 0xff]);
        assert_eq!(Data(&bytes).int24(0), -1);
        bytes[29..32].copy_from_slice(&[0xff, 0xfc, 0x7c]);
        assert_eq!(Data(&bytes).int24(0), -900);
        bytes[29..32].copy_from_slice(&[0x00, 0x00, 0xc8]);
        assert_eq!(Data(&bytes).int24(0), 200);
    }

    #[test]
    fn a_padded_word_is_not_an_address() {
        assert_eq!(clean_address(B256::repeat_byte(0x11)), Address::ZERO);
        assert_eq!(
            clean_address(id32(Address::repeat_byte(0x11))),
            Address::repeat_byte(0x11)
        );
    }

    #[test]
    fn a_log_with_the_wrong_shape_is_not_decoded() {
        // Right topic0, one data word short.
        let log = log_with(
            &[events::PONS_V2_CURVE_BUY.topic0, B256::ZERO, B256::ZERO],
            vec![0u8; 96],
        );
        assert!(decode(1, &[log]).is_empty());

        // Right shape, wrong topic count.
        let log = log_with(
            &[events::PONS_V2_CURVE_BUY.topic0, B256::ZERO],
            vec![0u8; 128],
        );
        assert!(decode(1, &[log]).is_empty());
    }

    #[test]
    fn no_logs_no_rows() {
        assert!(decode(1, &[]).is_empty());
    }

    /// A name / symbol is chosen by whoever emitted the log. None of it
    /// may reach a screen as anything but one line of visible characters.
    #[test]
    fn hostile_text_loses_its_controls_and_format_characters() {
        // Trojan Source: the override makes the rendering lie.
        assert_eq!(
            sanitize("Will \u{202e}SEY evloser\u{202c} happen?"),
            "Will SEY evloser happen?"
        );
        // A newline forges a second row in a log line or a CSV export.
        assert_eq!(
            sanitize("Real\nFAKE: verified"),
            "Real FAKE: verified"
        );
        // Zero width characters smuggle a different word past a reader.
        assert_eq!(sanitize("PEP\u{200b}E"), "PEPE");
        // The tag block is a whole second string, invisible.
        assert_eq!(sanitize("OK\u{e0041}\u{e0042}"), "OK");
        // Every listed range is actually removed. A separator is
        // whitespace and keeps the words apart; the rest vanish.
        for (lo, hi) in HIDDEN {
            for code in [*lo, *hi] {
                let c = char::from_u32(code).unwrap();
                let want = if c.is_whitespace() { "a b" } else { "ab" };
                assert_eq!(sanitize(&format!("a{c}b")), want, "{code:#x}");
            }
        }
        // Nothing but hidden characters leaves nothing.
        assert_eq!(sanitize("\u{202e}\u{200b}\n\t "), "");
        // HTML is NOT escaped here - it is data, and the UI escapes it.
        assert_eq!(
            sanitize("<script>alert(1)</script>"),
            "<script>alert(1)</script>"
        );
        // Capped, and a multi byte character survives the cap.
        assert_eq!(sanitize(&"é".repeat(600)).chars().count(), MAX_CHARS);
    }

    /// The decoder's own path, not just the helper.
    #[test]
    fn a_decoded_symbol_carries_no_bidi_override() {
        // One dynamic `string` at head slot 0: offset, length, bytes.
        let text = "PE\u{202e}PE";
        let mut data = vec![0u8; 64];
        data[31] = 32;
        data[32 + 31] = text.len() as u8;
        data.extend_from_slice(text.as_bytes());
        data.resize(64 + 32, 0);

        assert_eq!(Data(&data).text(0), "PEPE");
    }
}
