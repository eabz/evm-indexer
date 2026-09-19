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

use std::collections::HashMap;

use alloy::primitives::{B256, U256};

use crate::db::models::log::DatabaseLog;

use super::{
    events::{self, EventDef},
    models::{
        id_of, Family, FeeKind, FeePhase, Id, LaunchpadCreatorFee,
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

    /// Topic `index` as an identity, zero when it is not an EVM address
    /// (the 12 leading bytes must be zero).
    fn id(&self, index: usize) -> Id {
        clean_id(self.at(index))
    }
}

/// A 32 byte word holding an EVM address, or zero when the padding is not
/// zero (so a forged word can never turn into someone else's address).
fn clean_id(word: B256) -> Id {
    if word.0[..12].iter().all(|byte| *byte == 0) {
        word
    } else {
        B256::ZERO
    }
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

    fn id(&self, index: usize) -> Id {
        clean_id(self.word(index))
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

    /// A dynamic `string` whose head word is at `index`. Empty on any
    /// malformed offset / length - never a panic, never an allocation
    /// bigger than [`MAX_TEXT`].
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

        let bytes = &self.0[start..end.min(start + MAX_TEXT)];
        String::from_utf8_lossy(bytes)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    }
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
    token: Id,
    from: Id,
    to: Id,
    amount: U256,
    ordinal: u64,
    used: bool,
}

struct CreditSeen {
    recipient: Id,
    source: Id,
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
    quote_of: HashMap<Id, Id>,
    /// `flap_portal` `FlapTokenProgressChanged`: (token, wad, ordinal),
    /// in log order. A transaction can hold several per token.
    progress: Vec<(Id, U256, u64)>,
    /// `pons_v2` `PoolRegistered`: token -> (pool id, quote, creator).
    registered: HashMap<Id, (B256, Id, Id)>,
    /// `pons_v2` curves that emitted `CurveCompleted`.
    completed: Vec<Id>,
}

impl TxEvidence {
    /// The asset that moved exactly `amount` to (`inbound`) or from the
    /// `emitter`, consuming the transfer that proves it.
    fn verify_leg(
        &mut self,
        emitter: Id,
        amount: U256,
        inbound: bool,
        before: Option<u64>,
    ) -> Option<Id> {
        let mut found: Option<usize> = None;
        let mut token: Option<Id> = None;

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
    fn credited(&mut self, source: Id, amount: U256) -> Option<Id> {
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
    fn minted(&self, token: Id) -> U256 {
        self.transfers
            .iter()
            .filter(|t| t.token == token && t.from == B256::ZERO)
            .map(|t| t.amount)
            .max()
            .unwrap_or(U256::ZERO)
    }

    /// Curve progress reported for `token` right AFTER `ordinal`.
    fn progress_after(&self, token: Id, ordinal: u64) -> U256 {
        self.progress
            .iter()
            .filter(|(seen, _, at)| *seen == token && *at > ordinal)
            .min_by_key(|(_, _, at)| *at)
            .map(|(_, wad, _)| *wad)
            .unwrap_or(U256::ZERO)
    }

    /// The asset that moved exactly `amount` INTO `pool` (graduations).
    fn moved_into(&self, pool: Id, amount: U256) -> Id {
        let mut token = B256::ZERO;

        for transfer in &self.transfers {
            if transfer.to != pool || transfer.amount != amount {
                continue;
            }
            if token != B256::ZERO && token != transfer.token {
                return B256::ZERO;
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
        let emitter = id_of(log.address);
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
                    from: topics.id(1),
                    to: topics.id(2),
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
                    recipient: topics.id(1),
                    source: topics.id(2),
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
                .insert(data.id(0), data.id(1));
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
                    data.id(0),
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
                    data.id(0),
                    (topics.at(1), data.id(1), data.id(2)),
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
    emitter: Id,
    token: Id,
) -> LaunchpadToken {
    LaunchpadToken {
        chain: place.chain,
        token,
        family,
        emitter,
        curve: B256::ZERO,
        creator: B256::ZERO,
        name: String::new(),
        symbol: String::new(),
        metadata_uri: String::new(),
        quote_token: B256::ZERO,
        initial_supply: U256::ZERO,
        graduation_threshold: U256::ZERO,
        pool_id: B256::ZERO,
        pool_kind: PoolKind::PoolId,
        launch_config_id: U256::ZERO,
        block_number: place.block_number,
        timestamp: place.timestamp,
        transaction_hash: place.transaction_hash,
        tx_index: place.tx_index,
        ordinal: place.ordinal,
        tx_from: B256::ZERO,
        epoch: 0,
        _version: 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn fee_row(
    place: Place,
    component: u32,
    family: Family,
    emitter: Id,
    token: Id,
    pool_id: Id,
    phase: FeePhase,
    kind: FeeKind,
    amount: U256,
) -> LaunchpadCreatorFee {
    LaunchpadCreatorFee {
        chain: place.chain,
        block_number: place.block_number,
        timestamp: place.timestamp,
        transaction_hash: place.transaction_hash,
        tx_index: place.tx_index,
        ordinal: place.ordinal,
        component,
        family,
        emitter,
        token,
        pool_id,
        phase,
        kind,
        recipient: B256::ZERO,
        recipient_known: 0,
        quote_token: B256::ZERO,
        amount,
        tx_from: B256::ZERO,
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
        let emitter = id_of(log.address);
        let place = Place::of(chain, log);
        let evidence = evidence.entry(log.transaction_hash).or_default();

        // ------------------------------------------------ pons_v2 launch
        if topic0 == events::PONS_V2_TOKEN_LAUNCHED.topic0
            && matches(log, &topics, &events::PONS_V2_TOKEN_LAUNCHED)
        {
            let token = topics.id(1);
            rows.tokens.push(LaunchpadToken {
                curve: topics.id(2),
                creator: topics.id(3),
                quote_token: data.id(0),
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
            let token = data.id(3);
            rows.tokens.push(LaunchpadToken {
                curve: emitter,
                creator: data.id(1),
                name: data.text(4),
                symbol: data.text(5),
                metadata_uri: data.text(6),
                quote_token: evidence
                    .quote_of
                    .get(&token)
                    .copied()
                    .unwrap_or(B256::ZERO),
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
            let token = topics.id(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.id(2),
                quote_token: data.id(0),
                pool_id: data.id(1),
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
            let token = topics.id(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.id(2),
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
            let token = topics.id(1);
            rows.tokens.push(LaunchpadToken {
                curve: topics.id(2),
                creator: topics.id(3),
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
            let token = topics.id(1);
            rows.tokens.push(LaunchpadToken {
                creator: topics.id(2),
                metadata_uri: data.text(1),
                name: data.text(2),
                symbol: data.text(3),
                pool_id: data.word(8),
                quote_token: data.id(9),
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
                .unwrap_or(B256::ZERO);
            let quote = evidence
                .verify_leg(emitter, quote_amount, pons_buy, at)
                .unwrap_or(B256::ZERO);

            rows.trades.push(LaunchpadTrade {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                transaction_hash: place.transaction_hash,
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::PonsV2,
                emitter,
                token,
                token_verified: u8::from(token != B256::ZERO),
                quote_token: quote,
                quote_verified: u8::from(quote != B256::ZERO),
                side,
                // buy: the recipient of the tokens; sell: the account the
                // tokens came from. The other one is a router, the launch
                // forwarder or the quote recipient.
                trader: if pons_buy { topics.id(2) } else { topics.id(1) },
                caller: if pons_buy { topics.id(1) } else { topics.id(2) },
                token_amount,
                quote_amount,
                fee_amount: data.u256(2),
                tax_amount: data.u256(3),
                progress_wad: U256::ZERO,
                graduating: u8::from(
                    evidence.completed.contains(&emitter),
                ),
                sole_unverified_quote: 0,
                tx_from: B256::ZERO,
                tx_to: B256::ZERO,
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
            let token = data.id(1);
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
                .unwrap_or(B256::ZERO);

            rows.trades.push(LaunchpadTrade {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                transaction_hash: place.transaction_hash,
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::FlapPortal,
                emitter,
                token,
                // The event NAMES the token; the flag says whether the
                // token itself confirmed the movement.
                token_verified: u8::from(proven_token == Some(token)),
                quote_token: quote,
                quote_verified: u8::from(quote != B256::ZERO),
                side: if flap_buy { Side::Buy } else { Side::Sell },
                trader: data.id(2),
                caller: data.id(2),
                token_amount,
                quote_amount,
                fee_amount: data.u256(5),
                tax_amount: U256::ZERO,
                progress_wad: progress,
                graduating: u8::from(progress == WAD),
                sole_unverified_quote: 0,
                tx_from: B256::ZERO,
                tx_to: B256::ZERO,
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
            let token = topics.id(1);
            let (pool_id, quote_token, _) = evidence
                .registered
                .get(&token)
                .copied()
                .unwrap_or((B256::ZERO, B256::ZERO, B256::ZERO));

            rows.graduations.push(LaunchpadGraduation {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                transaction_hash: place.transaction_hash,
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
                tx_from: B256::ZERO,
                epoch: 0,
                _version: 0,
            });
            continue;
        }

        if topic0 == events::FLAP_LAUNCHED_TO_DEX.topic0
            && matches(log, &topics, &events::FLAP_LAUNCHED_TO_DEX)
        {
            let pool = data.id(1);
            let quote_amount = data.u256(3);

            rows.graduations.push(LaunchpadGraduation {
                chain,
                block_number: place.block_number,
                timestamp: place.timestamp,
                transaction_hash: place.transaction_hash,
                tx_index: place.tx_index,
                ordinal: place.ordinal,
                family: Family::FlapPortal,
                emitter,
                token: data.id(0),
                pool_id: pool,
                pool_kind: PoolKind::PoolAddress,
                // Proven by the asset that really moved into the pair.
                quote_token: evidence.moved_into(pool, quote_amount),
                token_amount: data.u256(2),
                quote_amount,
                position_id: U256::ZERO,
                tx_from: B256::ZERO,
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
                    topics.id(1),
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
    emitter: Id,
    pool_id: Id,
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
            B256::ZERO,
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
    let mut unverified: HashMap<B256, u32> = HashMap::new();

    for trade in &rows.trades {
        if trade.quote_verified == 0 {
            *unverified.entry(trade.transaction_hash).or_default() += 1;
        }
    }

    for trade in &mut rows.trades {
        trade.sole_unverified_quote = u8::from(
            trade.quote_verified == 0
                && unverified.get(&trade.transaction_hash) == Some(&1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::log_with;
    use alloy::primitives::Address;

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
        assert_eq!(clean_id(B256::repeat_byte(0x11)), B256::ZERO);
        assert_eq!(
            clean_id(id_of(Address::repeat_byte(0x11))),
            id_of(Address::repeat_byte(0x11))
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
}
