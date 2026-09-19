//! Logs -> prediction market rows. Pure: no I/O, no registry, no panics.
//!
//! Two passes:
//!
//! 1. every log is parsed on its own: `topic0` of a family AND the exact
//!    shape the family emits (topic count, data length, ABI offsets inside
//!    the data, value ranges). Anything else is ignored silently.
//! 2. the parsed events are looked at per TRANSACTION, because three
//!    things only exist in that context:
//!    * a trade is a maker `OrderFilled` told from the point of view of the
//!      taker order, whose own `OrderFilled` (taker = the exchange) closes
//!      the match a few logs later;
//!    * the ERC-1155 contract of a trade is the one that moved the traded
//!      token id in the same transaction (exchange events do not name it);
//!    * why an ERC-1155 transfer happened (split, merge, redemption, trade
//!      or a plain transfer) is told by its sibling events.
//!
//! A transaction is never split across two `decode` calls (batches are
//! whole blocks).

use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

use alloy::primitives::{Address, Bytes, B256, U256};

use crate::{db::models::log::DatabaseLog, utils::format::tx_id};

use super::{
    events::{self, EventDef},
    ids,
    models::{
        first_seen_version, FeeUnit, MatchType, PositionEventKind,
        PredictionMarket, PredictionOutcomeToken, PredictionPositionEvent,
        PredictionQuestion, PredictionResolution, PredictionTrade,
        PredictionTransfer, Protocol, QuestionKind, RowSource, Side,
        TransferReason,
    },
    text, PredictionRows,
};

/// Most entries of a partition / payout vector / index set list (the CTF
/// allows 256 outcome slots).
const MAX_OUTCOMES: usize = 256;
/// Most ids of one `TransferBatch`.
const MAX_BATCH: usize = 4_096;
/// Most bytes of an on chain text payload that are kept.
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    ConditionPreparation,
    ConditionResolution,
    CtfSplit,
    CtfMerge,
    CtfRedemption,
    TransferSingle,
    TransferBatch,
    FillV1,
    FillV2,
    FpmmBuy,
    FpmmSell,
    AdapterSplit,
    AdapterMerge,
    AdapterRedemption,
    Converted,
    MarketPrepared,
    QuestionPrepared,
    UmaInitialized,
    UmaReset,
    UmaFlagged,
}

const TABLE: &[(EventDef, Kind)] = &[
    (events::CTF_CONDITION_PREPARATION, Kind::ConditionPreparation),
    (events::CTF_CONDITION_RESOLUTION, Kind::ConditionResolution),
    (events::CTF_POSITION_SPLIT, Kind::CtfSplit),
    (events::CTF_POSITIONS_MERGE, Kind::CtfMerge),
    (events::CTF_PAYOUT_REDEMPTION, Kind::CtfRedemption),
    (events::ERC1155_TRANSFER_SINGLE, Kind::TransferSingle),
    (events::ERC1155_TRANSFER_BATCH, Kind::TransferBatch),
    (events::EXCHANGE_ORDER_FILLED, Kind::FillV1),
    (events::EXCHANGE_V2_ORDER_FILLED, Kind::FillV2),
    (events::FPMM_BUY, Kind::FpmmBuy),
    (events::FPMM_SELL, Kind::FpmmSell),
    (events::NEG_RISK_POSITION_SPLIT, Kind::AdapterSplit),
    (events::NEG_RISK_POSITIONS_MERGE, Kind::AdapterMerge),
    (events::NEG_RISK_PAYOUT_REDEMPTION, Kind::AdapterRedemption),
    (events::NEG_RISK_POSITIONS_CONVERTED, Kind::Converted),
    (events::NEG_RISK_MARKET_PREPARED, Kind::MarketPrepared),
    (events::NEG_RISK_QUESTION_PREPARED, Kind::QuestionPrepared),
    (events::UMA_QUESTION_INITIALIZED, Kind::UmaInitialized),
    (events::UMA_QUESTION_RESET, Kind::UmaReset),
    (events::UMA_QUESTION_FLAGGED, Kind::UmaFlagged),
];

fn lookup(topic0: &B256) -> Option<&'static (EventDef, Kind)> {
    static INDEX: OnceLock<HashMap<B256, &'static (EventDef, Kind)>> =
        OnceLock::new();

    INDEX
        .get_or_init(|| {
            TABLE.iter().map(|entry| (entry.0.topic0, entry)).collect()
        })
        .get(topic0)
        .copied()
}

/// THE place that reads the topic columns of [`DatabaseLog`]: the topics
/// and how many there are (a hole can not come from a node: count 0).
fn topics_of(log: &DatabaseLog) -> ([B256; 4], usize) {
    let topics = [log.topic0, log.topic1, log.topic2, log.topic3];
    let count = topics.iter().take_while(|topic| topic.is_some()).count();
    let count = if topics[count..].iter().any(Option::is_some) {
        0
    } else {
        count
    };

    (topics.map(Option::unwrap_or_default), count)
}

// ------------------------------------------------------------ word readers

fn word(data: &[u8], index: usize) -> Option<&[u8; 32]> {
    let start = index.checked_mul(32)?;
    let end = start.checked_add(32)?;
    data.get(start..end)?.try_into().ok()
}

fn uint(data: &[u8], index: usize) -> Option<U256> {
    word(data, index).map(|bytes| U256::from_be_bytes(*bytes))
}

/// A word that must fit a `usize` below `limit` (offsets, lengths).
fn small(data: &[u8], index: usize, limit: usize) -> Option<usize> {
    let value = uint(data, index)?;
    let value = usize::try_from(value).ok()?;
    (value <= limit).then_some(value)
}

/// An ABI encoded address: the upper 12 bytes must be zero.
fn address_word(bytes: &[u8; 32]) -> Option<Address> {
    bytes[..12]
        .iter()
        .all(|byte| *byte == 0)
        .then(|| Address::from_slice(&bytes[12..]))
}

fn address(data: &[u8], index: usize) -> Option<Address> {
    word(data, index).and_then(address_word)
}

fn topic_address(topic: &B256) -> Option<Address> {
    address_word(&topic.0)
}

/// Word index the dynamic value of head slot `slot` starts at.
fn tail(data: &[u8], slot: usize) -> Option<usize> {
    let offset = small(data, slot, data.len())?;
    offset.is_multiple_of(32).then_some(offset / 32)
}

/// `uint256[]` whose offset sits in head slot `slot`.
fn uint_array(
    data: &[u8],
    slot: usize,
    limit: usize,
) -> Option<Vec<U256>> {
    let start = tail(data, slot)?;
    let len = small(data, start, limit)?;

    (0..len).map(|item| uint(data, start + 1 + item)).collect()
}

/// `bytes` whose offset sits in head slot `slot` (at most
/// [`MAX_TEXT_BYTES`] are kept).
fn bytes_at(data: &[u8], slot: usize) -> Option<Bytes> {
    let start = tail(data, slot)?;
    let len = small(data, start, data.len())?;
    let from = (start + 1).checked_mul(32)?;
    let to = from.checked_add(len)?;
    let raw = data.get(from..to)?;

    Some(Bytes::copy_from_slice(&raw[..raw.len().min(MAX_TEXT_BYTES)]))
}

// ------------------------------------------------------------ parsed events

/// One order's execution, normalised across the V1 / V2 encodings.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fill {
    protocol: Protocol,
    order_hash: B256,
    maker: Address,
    taker: Address,
    side: Side,
    token: U256,
    shares: U256,
    collateral: U256,
    fee: U256,
    fee_unit: FeeUnit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Flow {
    stakeholder: Address,
    collateral: Address,
    parent: B256,
    condition: B256,
    index_sets: Vec<U256>,
    amount: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Parsed {
    Preparation {
        condition: B256,
        oracle: Address,
        question: B256,
        outcomes: u16,
    },
    Resolution {
        condition: B256,
        oracle: Address,
        question: B256,
        payouts: Vec<U256>,
    },
    CtfFlow(PositionEventKind, Flow),
    Transfer {
        operator: Address,
        from: Address,
        to: Address,
        legs: Vec<(U256, U256)>,
    },
    Fill(Fill),
    Fpmm {
        side: Side,
        trader: Address,
        /// Fees excluded.
        collateral: U256,
        fee: U256,
        shares: U256,
    },
    AdapterFlow {
        kind: PositionEventKind,
        stakeholder: Address,
        condition: B256,
        index_sets: Vec<U256>,
        amount: U256,
    },
    Question(Box<QuestionDraft>),
}

/// A `prediction_questions` row minus the position of its log.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QuestionDraft {
    kind: QuestionKind,
    protocol: Protocol,
    question_id: B256,
    event_id: B256,
    question_index: u32,
    data: Bytes,
    creator: Address,
    oracle: Address,
    reward_token: Address,
    reward: U256,
    proposal_bond: U256,
    fee_bips: u32,
}

impl QuestionDraft {
    fn new(
        kind: QuestionKind,
        protocol: Protocol,
        question_id: B256,
    ) -> Self {
        Self {
            kind,
            protocol,
            question_id,
            event_id: B256::ZERO,
            question_index: 0,
            data: Bytes::new(),
            creator: Address::ZERO,
            oracle: Address::ZERO,
            reward_token: Address::ZERO,
            reward: U256::ZERO,
            proposal_bond: U256::ZERO,
            fee_bips: 0,
        }
    }
}

fn parse(log: &DatabaseLog) -> Option<Parsed> {
    let (topics, count) = topics_of(log);
    if count == 0 {
        return None;
    }

    let (def, kind) = lookup(&topics[0])?;
    let data: &[u8] = &log.data;

    if count != usize::from(def.topics)
        || def.data_len.is_some_and(|len| len != data.len())
        || !data.len().is_multiple_of(32)
    {
        return None;
    }

    match kind {
        Kind::ConditionPreparation => {
            let oracle = topic_address(&topics[2])?;
            let slots = uint(data, 0)?;
            let outcomes = u16::try_from(slots).ok()?;

            // The CTF accepts 2..=256 slots and derives the id itself: an
            // event whose id does not match is not a CTF preparation.
            ((2..=MAX_OUTCOMES).contains(&usize::from(outcomes))
                && ids::condition_id(oracle, topics[3], slots)
                    == topics[1])
                .then_some(Parsed::Preparation {
                    condition: topics[1],
                    oracle,
                    question: topics[3],
                    outcomes,
                })
        }
        Kind::ConditionResolution => {
            let slots = small(data, 0, MAX_OUTCOMES)?;
            let payouts = uint_array(data, 1, MAX_OUTCOMES)?;

            (payouts.len() == slots && slots >= 2).then_some(
                Parsed::Resolution {
                    condition: topics[1],
                    oracle: topic_address(&topics[2])?,
                    question: topics[3],
                    payouts,
                },
            )
        }
        Kind::CtfSplit | Kind::CtfMerge => {
            let flow = Flow {
                stakeholder: topic_address(&topics[1])?,
                collateral: address(data, 0)?,
                parent: topics[2],
                condition: topics[3],
                index_sets: uint_array(data, 1, MAX_OUTCOMES)?,
                amount: uint(data, 2)?,
            };
            let kind = if *kind == Kind::CtfSplit {
                PositionEventKind::Split
            } else {
                PositionEventKind::Merge
            };

            (flow.index_sets.len() >= 2)
                .then_some(Parsed::CtfFlow(kind, flow))
        }
        Kind::CtfRedemption => Some(Parsed::CtfFlow(
            PositionEventKind::Redeem,
            Flow {
                stakeholder: topic_address(&topics[1])?,
                collateral: topic_address(&topics[2])?,
                parent: topics[3],
                condition: B256::from(*word(data, 0)?),
                index_sets: uint_array(data, 1, MAX_OUTCOMES)?,
                amount: uint(data, 2)?,
            },
        )),
        Kind::TransferSingle => Some(Parsed::Transfer {
            operator: topic_address(&topics[1])?,
            from: topic_address(&topics[2])?,
            to: topic_address(&topics[3])?,
            legs: vec![(uint(data, 0)?, uint(data, 1)?)],
        }),
        Kind::TransferBatch => {
            let token_ids = uint_array(data, 0, MAX_BATCH)?;
            let values = uint_array(data, 1, MAX_BATCH)?;

            (token_ids.len() == values.len()).then(|| Parsed::Transfer {
                operator: topic_address(&topics[1]).unwrap_or_default(),
                from: topic_address(&topics[2]).unwrap_or_default(),
                to: topic_address(&topics[3]).unwrap_or_default(),
                legs: token_ids.into_iter().zip(values).collect(),
            })
        }
        Kind::FillV1 => {
            let maker_asset = uint(data, 0)?;
            let taker_asset = uint(data, 1)?;
            let making = uint(data, 2)?;
            let taking = uint(data, 3)?;

            // Exactly one side is the collateral (asset id 0). The fee is
            // charged on what the order RECEIVES: shares for a buy.
            let (side, token, shares, collateral, fee_unit) =
                match (maker_asset.is_zero(), taker_asset.is_zero()) {
                    (true, false) => (
                        Side::Buy,
                        taker_asset,
                        taking,
                        making,
                        FeeUnit::Shares,
                    ),
                    (false, true) => (
                        Side::Sell,
                        maker_asset,
                        making,
                        taking,
                        FeeUnit::Collateral,
                    ),
                    _ => return None,
                };

            Some(Parsed::Fill(Fill {
                protocol: Protocol::CtfExchange,
                order_hash: topics[1],
                maker: topic_address(&topics[2])?,
                taker: topic_address(&topics[3])?,
                side,
                token,
                shares,
                collateral,
                fee: uint(data, 4)?,
                fee_unit,
            }))
        }
        Kind::FillV2 => {
            let making = uint(data, 2)?;
            let taking = uint(data, 3)?;
            let (side, shares, collateral) = match small(data, 0, 1)? {
                0 => (Side::Buy, taking, making),
                _ => (Side::Sell, making, taking),
            };

            Some(Parsed::Fill(Fill {
                protocol: Protocol::CtfExchangeV2,
                order_hash: topics[1],
                maker: topic_address(&topics[2])?,
                taker: topic_address(&topics[3])?,
                side,
                token: uint(data, 1)?,
                shares,
                collateral,
                fee: uint(data, 4)?,
                fee_unit: FeeUnit::Collateral,
            }))
        }
        Kind::FpmmBuy | Kind::FpmmSell => {
            let amount = uint(data, 0)?;
            let fee = uint(data, 1)?;
            // Outcome indices are tiny: anything else is another event.
            u16::try_from(U256::from_be_bytes(topics[2].0)).ok()?;

            // Buy: the investment INCLUDES the fee. Sell: the return
            // EXCLUDES it. Normalised to "collateral without the fee, fee
            // on top (buy) / deducted (sell)" like the exchanges.
            let (side, collateral) = if *kind == Kind::FpmmBuy {
                (Side::Buy, amount.checked_sub(fee)?)
            } else {
                (Side::Sell, amount.checked_add(fee)?)
            };

            Some(Parsed::Fpmm {
                side,
                trader: topic_address(&topics[1])?,
                collateral,
                fee,
                shares: uint(data, 2)?,
            })
        }
        Kind::AdapterSplit | Kind::AdapterMerge => {
            Some(Parsed::AdapterFlow {
                kind: if *kind == Kind::AdapterSplit {
                    PositionEventKind::Split
                } else {
                    PositionEventKind::Merge
                },
                stakeholder: topic_address(&topics[1])?,
                condition: topics[2],
                index_sets: Vec::new(),
                amount: uint(data, 0)?,
            })
        }
        Kind::AdapterRedemption => Some(Parsed::AdapterFlow {
            kind: PositionEventKind::Redeem,
            stakeholder: topic_address(&topics[1])?,
            condition: topics[2],
            index_sets: uint_array(data, 0, MAX_OUTCOMES)?,
            amount: uint(data, 1)?,
        }),
        Kind::Converted => Some(Parsed::AdapterFlow {
            kind: PositionEventKind::Convert,
            stakeholder: topic_address(&topics[1])?,
            condition: topics[2],
            index_sets: vec![U256::from_be_bytes(topics[3].0)],
            amount: uint(data, 0)?,
        }),
        Kind::MarketPrepared => {
            let mut draft = QuestionDraft::new(
                QuestionKind::NegRiskEvent,
                Protocol::NegRisk,
                topics[1],
            );
            draft.event_id = topics[1];
            draft.oracle = topic_address(&topics[2])?;
            draft.fee_bips = u32::try_from(uint(data, 0)?).ok()?;
            draft.data = bytes_at(data, 1)?;
            Some(Parsed::Question(Box::new(draft)))
        }
        Kind::QuestionPrepared => {
            let mut draft = QuestionDraft::new(
                QuestionKind::NegRiskQuestion,
                Protocol::NegRisk,
                topics[2],
            );
            draft.event_id = topics[1];
            draft.question_index = u32::try_from(uint(data, 0)?).ok()?;
            draft.data = bytes_at(data, 1)?;
            Some(Parsed::Question(Box::new(draft)))
        }
        Kind::UmaInitialized => {
            let mut draft = QuestionDraft::new(
                QuestionKind::UmaQuestion,
                Protocol::Uma,
                topics[1],
            );
            draft.creator = topic_address(&topics[3])?;
            draft.data = bytes_at(data, 0)?;
            draft.reward_token = address(data, 1)?;
            draft.reward = uint(data, 2)?;
            draft.proposal_bond = uint(data, 3)?;
            Some(Parsed::Question(Box::new(draft)))
        }
        Kind::UmaReset | Kind::UmaFlagged => {
            let kind = if *kind == Kind::UmaReset {
                QuestionKind::UmaReset
            } else {
                QuestionKind::UmaFlagged
            };
            Some(Parsed::Question(Box::new(QuestionDraft::new(
                kind,
                Protocol::Uma,
                topics[1],
            ))))
        }
    }
}

// ------------------------------------------------------- transaction pass

/// What the sibling events of a transaction say about one token id.
#[derive(Debug, Default)]
struct TokenFacts {
    /// Entries of the partition the token was split / merged with.
    partition_len: usize,
    splitters: Vec<Address>,
    mergers: Vec<Address>,
    redeemers: Vec<Address>,
}

/// Position ids are keccak + a modular square root: computed once per
/// `(collateral, condition, index set)` of a batch.
#[derive(Default)]
struct IdCache {
    tokens: HashMap<(Address, B256, U256), Option<U256>>,
}

impl IdCache {
    fn token(
        &mut self,
        collateral: Address,
        condition: B256,
        index_set: U256,
    ) -> Option<U256> {
        *self
            .tokens
            .entry((collateral, condition, index_set))
            .or_insert_with(|| {
                ids::outcome_token_id(collateral, condition, index_set)
            })
    }
}

struct Context<'a> {
    chain: u64,
    rows: &'a mut PredictionRows,
    ids: &'a mut IdCache,
    /// `(registry, token id)` already mapped by this batch.
    mapped: &'a mut HashSet<(Address, U256)>,
}

fn decode_transaction(
    context: &mut Context<'_>,
    logs: &[(&DatabaseLog, Parsed)],
) {
    // Who traded here: transfers to / from them are trades.
    let exchanges: HashSet<Address> = logs
        .iter()
        .filter(|(_, parsed)| {
            matches!(parsed, Parsed::Fill(_) | Parsed::Fpmm { .. })
        })
        .map(|(log, _)| log.address)
        .collect();

    // Token ids the registry events of this transaction talk about.
    let mut facts: HashMap<(Address, U256), TokenFacts> = HashMap::new();
    // Collateral of a condition, for adapter events that do not carry it.
    let mut collaterals: HashMap<B256, Address> = HashMap::new();

    for (log, parsed) in logs {
        let Parsed::CtfFlow(kind, flow) = parsed else { continue };
        collaterals.entry(flow.condition).or_insert(flow.collateral);

        if !flow.parent.is_zero() {
            continue;
        }

        for index_set in &flow.index_sets {
            let Some(token) = context.ids.token(
                flow.collateral,
                flow.condition,
                *index_set,
            ) else {
                continue;
            };

            let entry = facts.entry((log.address, token)).or_default();
            match kind {
                PositionEventKind::Split => {
                    entry.partition_len = flow.index_sets.len();
                    entry.splitters.push(flow.stakeholder);
                }
                PositionEventKind::Merge => {
                    entry.partition_len = flow.index_sets.len();
                    entry.mergers.push(flow.stakeholder);
                }
                _ => entry.redeemers.push(flow.stakeholder),
            }

            if let Some(outcome_index) = ids::single_outcome(*index_set) {
                if context.mapped.insert((log.address, token)) {
                    context.rows.outcome_tokens.push(
                        PredictionOutcomeToken {
                            chain: context.chain,
                            registry: log.address,
                            outcome_token_id: token,
                            market_id: flow.condition,
                            outcome_index,
                            collateral_token: flow.collateral,
                            first_seen_block: log.block_number,
                            first_seen_timestamp: log.timestamp,
                            _version: first_seen_version(
                                log.block_number,
                                log.transaction_index,
                                u64::from(log.log_index),
                            ),
                        },
                    );
                }
            }
        }
    }

    // The ERC-1155 contract that moved a token id in this transaction.
    let mut movers: HashMap<U256, Address> = HashMap::new();
    for (log, parsed) in logs {
        if let Parsed::Transfer { legs, .. } = parsed {
            for (token, _) in legs {
                movers.entry(*token).or_insert(log.address);
            }
        }
    }

    // Maker fills waiting for the taker order that closes their match,
    // per exchange.
    let mut open: HashMap<Address, Vec<(&DatabaseLog, &Fill)>> =
        HashMap::new();

    for (index, (log, parsed)) in logs.iter().enumerate() {
        match parsed {
            Parsed::Preparation {
                condition,
                oracle,
                question,
                outcomes,
            } => {
                context.rows.markets.push(PredictionMarket {
                    chain: context.chain,
                    market_id: *condition,
                    registry: log.address,
                    protocol: Protocol::Ctf,
                    oracle: *oracle,
                    question_id: *question,
                    outcome_count: *outcomes,
                    block_number: log.block_number,
                    timestamp: log.timestamp,
                    tx_id: tx_id(log.transaction_hash),
                    tx_index: log.transaction_index,
                    ordinal: u64::from(log.log_index),
                    tx_from: Address::ZERO,
                    source: RowSource::Event,
                    epoch: 0,
                    _version: 0,
                });
            }
            Parsed::Resolution {
                condition,
                oracle,
                question,
                payouts,
            } => {
                let denominator =
                    payouts.iter().fold(U256::ZERO, |sum, payout| {
                        sum.saturating_add(*payout)
                    });

                context.rows.resolutions.push(PredictionResolution {
                    chain: context.chain,
                    market_id: *condition,
                    registry: log.address,
                    oracle: *oracle,
                    question_id: *question,
                    outcome_count: payouts.len() as u16,
                    payout_numerators: payouts.clone(),
                    payout_denominator: denominator,
                    block_number: log.block_number,
                    timestamp: log.timestamp,
                    tx_id: tx_id(log.transaction_hash),
                    tx_index: log.transaction_index,
                    ordinal: u64::from(log.log_index),
                    epoch: 0,
                    _version: 0,
                });
            }
            Parsed::CtfFlow(kind, flow) => {
                context.rows.position_events.push(position_event(
                    context.chain,
                    log,
                    Protocol::Ctf,
                    *kind,
                    flow.stakeholder,
                    flow.condition,
                    flow.collateral,
                    flow.parent,
                    flow.index_sets.clone(),
                    flow.amount,
                ));
            }
            Parsed::AdapterFlow {
                kind,
                stakeholder,
                condition,
                index_sets,
                amount,
            } => {
                let protocol = if *kind == PositionEventKind::Convert {
                    Protocol::NegRisk
                } else {
                    Protocol::CtfAdapter
                };

                context.rows.position_events.push(position_event(
                    context.chain,
                    log,
                    protocol,
                    *kind,
                    *stakeholder,
                    *condition,
                    collaterals
                        .get(condition)
                        .copied()
                        .unwrap_or_default(),
                    B256::ZERO,
                    index_sets.clone(),
                    *amount,
                ));
            }
            Parsed::Question(draft) => {
                context.rows.questions.push(question(
                    context.chain,
                    log,
                    draft,
                ));
            }
            Parsed::Transfer { operator, from, to, legs } => {
                for (batch_index, (token, amount)) in
                    legs.iter().enumerate()
                {
                    context.rows.transfers.push(transfer(
                        context.chain,
                        log,
                        batch_index as u32,
                        (*operator, *from, *to),
                        (*token, *amount),
                        facts.get(&(log.address, *token)),
                        &exchanges,
                    ));
                }
            }
            Parsed::Fill(fill) => {
                // The taker order's own fill names the exchange as taker
                // and closes the match.
                if fill.taker == log.address {
                    let makers =
                        open.remove(&log.address).unwrap_or_default();
                    close_match(context, &movers, &makers, Some(fill));
                } else {
                    open.entry(log.address).or_default().push((log, fill));
                }
            }
            Parsed::Fpmm { side, trader, collateral, fee, shares } => {
                // The pool moves the shares right before it reports the
                // trade: that transfer names the token (and the registry).
                let token =
                    logs[..index].iter().rev().find_map(|(_, other)| {
                        let Parsed::Transfer { from, to, legs, .. } =
                            other
                        else {
                            return None;
                        };
                        let expected = match side {
                            Side::Buy => (log.address, *trader),
                            Side::Sell => (*trader, log.address),
                        };

                        ((*from, *to) == expected
                            && legs.len() == 1
                            && legs[0].1 == *shares)
                            .then_some(legs[0].0)
                    });

                let Some(token) = token else { continue };

                context.rows.trades.push(PredictionTrade {
                    chain: context.chain,
                    block_number: log.block_number,
                    timestamp: log.timestamp,
                    tx_id: tx_id(log.transaction_hash),
                    tx_index: log.transaction_index,
                    ordinal: u64::from(log.log_index),
                    protocol: Protocol::Fpmm,
                    exchange: log.address,
                    registry: movers
                        .get(&token)
                        .copied()
                        .unwrap_or_default(),
                    order_hash: B256::ZERO,
                    maker: log.address,
                    taker: *trader,
                    tx_from: Address::ZERO,
                    tx_to: Address::ZERO,
                    outcome_token_id: token,
                    side: *side,
                    share_amount: *shares,
                    collateral_amount: *collateral,
                    match_type: MatchType::Amm,
                    maker_outcome_token_id: token,
                    maker_side: side.opposite(),
                    maker_collateral_amount: *collateral,
                    maker_fee_amount: U256::ZERO,
                    maker_fee_unit: FeeUnit::Collateral,
                    taker_fee_amount: *fee,
                    taker_fee_unit: FeeUnit::Collateral,
                    epoch: 0,
                    _version: 0,
                });
            }
        }
    }

    // Orders filled directly by the operator: no taker order exists.
    let mut leftovers: Vec<_> = open.into_values().collect();
    leftovers
        .sort_by_key(|fills| fills.first().map(|(log, _)| log.log_index));
    for makers in leftovers {
        close_match(context, &movers, &makers, None);
    }
}

/// Turns the maker fills of one match into trades, told from the point of
/// view of the `taker` order (`None`: filled directly by the operator).
fn close_match(
    context: &mut Context<'_>,
    movers: &HashMap<U256, Address>,
    makers: &[(&DatabaseLog, &Fill)],
    taker: Option<&Fill>,
) {
    let total_shares = makers.iter().fold(U256::ZERO, |sum, (_, fill)| {
        sum.saturating_add(fill.shares)
    });
    let mut fee_left = taker.map_or(U256::ZERO, |taker| taker.fee);

    for (position, (log, maker)) in makers.iter().enumerate() {
        let (match_type, token, side) = match taker {
            None => {
                (MatchType::Direct, maker.token, maker.side.opposite())
            }
            Some(taker) if taker.token == maker.token => {
                (MatchType::Complementary, taker.token, taker.side)
            }
            Some(taker) if taker.side == Side::Buy => {
                (MatchType::Mint, taker.token, taker.side)
            }
            Some(taker) => (MatchType::Merge, taker.token, taker.side),
        };

        // Complementary: the collateral the maker names is the collateral
        // of the taker. Mint / merge: the two legs add up to one unit of
        // collateral per share.
        let collateral = match match_type {
            MatchType::Mint | MatchType::Merge => {
                maker.shares.saturating_sub(maker.collateral)
            }
            _ => maker.collateral,
        };

        // Pro rata by shares, the last fill takes what is left.
        let taker_fee = match taker {
            None => U256::ZERO,
            Some(_) if position + 1 == makers.len() => fee_left,
            Some(taker) => taker
                .fee
                .checked_mul(maker.shares)
                .and_then(|product| product.checked_div(total_shares))
                .unwrap_or_default()
                .min(fee_left),
        };
        fee_left -= taker_fee;

        let registry = movers
            .get(&token)
            .or_else(|| movers.get(&maker.token))
            .copied()
            .unwrap_or_default();

        context.rows.trades.push(PredictionTrade {
            chain: context.chain,
            block_number: log.block_number,
            timestamp: log.timestamp,
            tx_id: tx_id(log.transaction_hash),
            tx_index: log.transaction_index,
            ordinal: u64::from(log.log_index),
            protocol: maker.protocol,
            exchange: log.address,
            registry,
            order_hash: maker.order_hash,
            maker: maker.maker,
            taker: taker.map_or(maker.taker, |taker| taker.maker),
            tx_from: Address::ZERO,
            tx_to: Address::ZERO,
            outcome_token_id: token,
            side,
            share_amount: maker.shares,
            collateral_amount: collateral,
            match_type,
            maker_outcome_token_id: maker.token,
            maker_side: maker.side,
            maker_collateral_amount: maker.collateral,
            maker_fee_amount: maker.fee,
            maker_fee_unit: maker.fee_unit,
            taker_fee_amount: taker_fee,
            taker_fee_unit: taker
                .map_or(FeeUnit::Collateral, |taker| taker.fee_unit),
            epoch: 0,
            _version: 0,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn position_event(
    chain: u64,
    log: &DatabaseLog,
    protocol: Protocol,
    kind: PositionEventKind,
    stakeholder: Address,
    market_id: B256,
    collateral_token: Address,
    parent_collection_id: B256,
    index_sets: Vec<U256>,
    amount: U256,
) -> PredictionPositionEvent {
    PredictionPositionEvent {
        chain,
        block_number: log.block_number,
        timestamp: log.timestamp,
        tx_id: tx_id(log.transaction_hash),
        tx_index: log.transaction_index,
        ordinal: u64::from(log.log_index),
        protocol,
        emitter: log.address,
        kind,
        stakeholder,
        market_id,
        collateral_token,
        parent_collection_id,
        index_sets,
        amount,
        tx_from: Address::ZERO,
        epoch: 0,
        _version: 0,
    }
}

fn question(
    chain: u64,
    log: &DatabaseLog,
    draft: &QuestionDraft,
) -> PredictionQuestion {
    let parsed = text::parse(&draft.data);

    PredictionQuestion {
        chain,
        question_id: draft.question_id,
        emitter: log.address,
        kind: draft.kind,
        protocol: draft.protocol,
        event_id: draft.event_id,
        question_index: draft.question_index,
        title: parsed.title,
        description: parsed.description,
        outcomes: parsed.outcomes,
        data: draft.data.clone(),
        creator: draft.creator,
        oracle: draft.oracle,
        reward_token: draft.reward_token,
        reward: draft.reward,
        proposal_bond: draft.proposal_bond,
        fee_bips: draft.fee_bips,
        block_number: log.block_number,
        timestamp: log.timestamp,
        tx_id: tx_id(log.transaction_hash),
        tx_index: log.transaction_index,
        ordinal: u64::from(log.log_index),
        epoch: 0,
        _version: 0,
    }
}

fn transfer(
    chain: u64,
    log: &DatabaseLog,
    batch_index: u32,
    (operator, from, to): (Address, Address, Address),
    (token, amount): (U256, U256),
    facts: Option<&TokenFacts>,
    exchanges: &HashSet<Address>,
) -> PredictionTransfer {
    let traded = exchanges.contains(&from) || exchanges.contains(&to);
    let involves = |who: &[Address], counterparty: Address| {
        !who.is_empty()
            && (counterparty.is_zero() || who.contains(&counterparty))
    };

    let to_reason = match facts {
        _ if traded => TransferReason::Trade,
        Some(facts) if involves(&facts.splitters, from) => {
            TransferReason::Split
        }
        _ => TransferReason::Transfer,
    };

    let from_reason = match facts {
        _ if traded => TransferReason::Trade,
        Some(facts) if involves(&facts.mergers, to) => {
            TransferReason::Merge
        }
        Some(facts) if involves(&facts.redeemers, to) => {
            TransferReason::Redeem
        }
        _ => TransferReason::Transfer,
    };

    let priced = to_reason == TransferReason::Split
        || from_reason == TransferReason::Merge;
    let priced_collateral = facts
        .filter(|facts| priced && facts.partition_len > 0)
        .map_or(U256::ZERO, |facts| {
            amount / U256::from(facts.partition_len)
        });

    PredictionTransfer {
        chain,
        block_number: log.block_number,
        timestamp: log.timestamp,
        tx_id: tx_id(log.transaction_hash),
        tx_index: log.transaction_index,
        ordinal: u64::from(log.log_index),
        batch_index,
        registry: log.address,
        operator,
        from,
        to,
        outcome_token_id: token,
        amount,
        from_reason,
        to_reason,
        priced_collateral,
        epoch: 0,
        _version: 0,
    }
}

/// Decodes every prediction market event of `logs` (whole blocks, in
/// chain order). Never panics, never does I/O.
pub fn decode(chain: u64, logs: &[DatabaseLog]) -> PredictionRows {
    let mut rows = PredictionRows::default();
    let mut ids = IdCache::default();
    let mut mapped = HashSet::new();

    // Group by transaction, keeping the order of first appearance.
    let mut order: Vec<(u64, B256)> = Vec::new();
    let mut groups: HashMap<(u64, B256), Vec<(&DatabaseLog, Parsed)>> =
        HashMap::new();

    for log in logs {
        let Some(parsed) = parse(log) else { continue };
        let key = (log.block_number, log.transaction_hash);

        groups
            .entry(key)
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push((log, parsed));
    }

    for key in order {
        let Some(mut group) = groups.remove(&key) else { continue };
        group.sort_by_key(|(log, _)| log.log_index);

        decode_transaction(
            &mut Context {
                chain,
                rows: &mut rows,
                ids: &mut ids,
                mapped: &mut mapped,
            },
            &group,
        );
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predictions::fixtures::{self, address, hash, unsigned};

    fn million(value: u64) -> U256 {
        U256::from(value) * U256::from(1_000_000u64)
    }

    #[test]
    fn a_v1_taker_order_becomes_one_trade_per_maker_order() {
        let tx = &fixtures::V1_NEG_RISK_MATCH;
        let rows = decode(tx.chain, &tx.logs());

        // 7 OrderFilled + 1 OrdersMatched on chain: 6 trades, not 7 or 8.
        assert_eq!(rows.trades.len(), 6);

        let taker = address("0xd218e474776403a330142299f7796e8ba32eb5c9");
        let yes = hash(
            "0xe6f6e528a0a768bd4b5292120b36da79e1724c2bdb89700ac1cd4766024d2f17",
        );
        let no = hash(
            "0xa0b2581228b6ffbe836878158adf5fdf884ed215a246787ded036e90c8ad0624",
        );

        for trade in &rows.trades {
            assert_eq!(trade.taker, taker);
            assert_eq!(trade.side, Side::Buy);
            assert_eq!(trade.outcome_token_id, U256::from_be_bytes(yes.0));
            assert_eq!(trade.protocol, Protocol::CtfExchange);
            assert_eq!(
                trade.exchange,
                address("0xC5d563A36AE78145C45a50134d48A1215220f80a")
            );
            // The CTF moved the token: that is the registry.
            assert_eq!(
                trade.registry,
                address("0x4D97DCd97eC945f40cF65F87097ACe5EA0476045")
            );
        }

        // Maker 1 SELLS the taker's token: shares change hands.
        let first = &rows.trades[0];
        assert_eq!(first.match_type, MatchType::Complementary);
        assert_eq!(first.maker_side, Side::Sell);
        assert_eq!(first.share_amount, unsigned("454900000"));
        assert_eq!(first.collateral_amount, unsigned("428060900"));
        assert_eq!(first.maker_collateral_amount, first.collateral_amount);

        // Maker 2 BUYS the other outcome at 0.059: a full set is minted
        // and the taker pays the rest of every share, 0.941.
        let second = &rows.trades[1];
        assert_eq!(second.match_type, MatchType::Mint);
        assert_eq!(second.maker_side, Side::Buy);
        assert_eq!(
            second.maker_outcome_token_id,
            U256::from_be_bytes(no.0)
        );
        assert_eq!(second.share_amount, million(500));
        assert_eq!(second.maker_collateral_amount, unsigned("29500000"));
        assert_eq!(second.collateral_amount, unsigned("470500000"));

        // The fills add up to the taker order's own OrderFilled exactly
        // (log 389: 1,266.98122 USDC for 1,346.42 shares).
        let shares = rows
            .trades
            .iter()
            .fold(U256::ZERO, |sum, trade| sum + trade.share_amount);
        let collateral = rows
            .trades
            .iter()
            .fold(U256::ZERO, |sum, trade| sum + trade.collateral_amount);
        assert_eq!(shares, unsigned("1346420000"));
        assert_eq!(collateral, unsigned("1266981220"));
    }

    #[test]
    fn a_v2_mint_match_is_told_from_the_taker() {
        let tx = &fixtures::V2_MINT_MATCH;
        let rows = decode(tx.chain, &tx.logs());

        assert_eq!(rows.trades.len(), 1);
        let trade = &rows.trades[0];

        assert_eq!(trade.protocol, Protocol::CtfExchangeV2);
        assert_eq!(trade.match_type, MatchType::Mint);
        assert_eq!(trade.ordinal, 298);
        assert_eq!(
            trade.maker,
            address("0xcd84d7f9262516865553555751609fb6f52abec6")
        );
        assert_eq!(
            trade.taker,
            address("0xdc41c39b95453c943174f369926018f6963bdd7e")
        );
        assert_eq!(trade.side, Side::Buy);
        assert_eq!(trade.share_amount, million(181));
        // Taker: 63.35 for 181 shares (0.35), maker: 117.65 (0.65).
        assert_eq!(trade.collateral_amount, unsigned("63350000"));
        assert_eq!(trade.maker_collateral_amount, unsigned("117650000"));
        assert_ne!(trade.outcome_token_id, trade.maker_outcome_token_id);
        assert_eq!(trade.taker_fee_amount, unsigned("2058870"));
        assert_eq!(trade.taker_fee_unit, FeeUnit::Collateral);
        assert_eq!(trade.maker_fee_amount, U256::ZERO);

        // The split of the match reveals both outcome tokens.
        assert_eq!(rows.outcome_tokens.len(), 2);
        let taker_token = rows
            .outcome_tokens
            .iter()
            .find(|token| token.outcome_token_id == trade.outcome_token_id)
            .unwrap();
        assert_eq!(taker_token.outcome_index, 0);
        assert_eq!(
            taker_token.market_id,
            hash("0x7d9ba25f111d4adc353e0441fc205c3a39b3e7eef9829999663262055b9911c8")
        );
        assert_eq!(
            taker_token.collateral_token,
            address("0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174")
        );
    }

    #[test]
    fn forks_on_other_chains_decode_without_any_configuration() {
        for tx in [&fixtures::BSC_V1_MATCH, &fixtures::BASE_V1_MATCH] {
            let rows = decode(tx.chain, &tx.logs());

            assert!(!rows.trades.is_empty(), "{}", tx.hash);
            for trade in &rows.trades {
                assert!(!trade.registry.is_zero(), "{}", tx.hash);
                assert!(trade.collateral_amount <= trade.share_amount);
                assert_ne!(trade.match_type, MatchType::Direct);
            }
        }
    }

    #[test]
    fn amm_trades_take_their_token_from_the_transfer() {
        let rows = decode(100, &fixtures::FPMM_BUY.logs());
        assert_eq!(rows.trades.len(), 1);
        let buy = &rows.trades[0];
        assert_eq!(buy.protocol, Protocol::Fpmm);
        assert_eq!(buy.match_type, MatchType::Amm);
        assert_eq!(buy.side, Side::Buy);
        assert_eq!(buy.maker, buy.exchange);
        assert_eq!(buy.share_amount, unsigned("3447261385084172831"));
        assert_eq!(buy.collateral_amount, unsigned("1932715430861723648"));
        assert_eq!(
            buy.outcome_token_id,
            U256::from_be_bytes(
                hash("0x5f8d2755ad2bd741d7b21f6c2e51e559f5f33ef0d2c1c7f75dccc3ea2af76501").0
            )
        );
        assert_eq!(
            buy.registry,
            address("0xCeAfDD6bc0bEF976fdCd1112955828E00543c0Ce")
        );

        let rows = decode(8453, &fixtures::FPMM_SELL.logs());
        assert_eq!(rows.trades.len(), 1);
        let sell = &rows.trades[0];
        assert_eq!(sell.side, Side::Sell);
        // 2.976047 returned + 0.011951 fee (= the 2.987998 the pool
        // merged), for 3.004695 shares.
        assert_eq!(sell.collateral_amount, unsigned("2987998"));
        assert_eq!(sell.taker_fee_amount, unsigned("11951"));
        assert_eq!(sell.share_amount, unsigned("3004695"));
    }

    #[test]
    fn transfers_know_why_they_happened() {
        // A split through an adapter: minted to the adapter (it split),
        // handed to the user (who thereby "split" too).
        let rows = decode(137, &fixtures::USER_SPLIT.logs());
        let user = address("0x94342d80a38389c242c5d355456b28782996646e");

        assert_eq!(rows.transfers.len(), 4);
        for leg in rows.transfers.iter().filter(|leg| leg.to == user) {
            assert_eq!(leg.to_reason, TransferReason::Split);
            assert_eq!(leg.from_reason, TransferReason::Transfer);
            assert_eq!(leg.amount, million(50));
            // 50 collateral bought 50 of EACH outcome: 25 per leg.
            assert_eq!(leg.priced_collateral, million(25));
        }

        // Shares moving through an exchange that traded are trades.
        let rows = decode(137, &fixtures::V2_MINT_MATCH.logs());
        let buyer = address("0xdc41c39b95453c943174f369926018f6963bdd7e");
        let leg =
            rows.transfers.iter().find(|leg| leg.to == buyer).unwrap();
        assert_eq!(leg.to_reason, TransferReason::Trade);
        assert_eq!(leg.priced_collateral, U256::ZERO);

        // A merge burns the full set.
        let rows = decode(137, &fixtures::USER_MERGE.logs());
        assert!(!rows.transfers.is_empty());
        for leg in &rows.transfers {
            assert!(leg.to.is_zero());
            assert_eq!(leg.from_reason, TransferReason::Merge);
            assert_eq!(
                leg.priced_collateral,
                leg.amount / U256::from(2u8)
            );
        }
    }

    #[test]
    fn markets_questions_and_resolutions() {
        let rows =
            decode(137, &fixtures::NEG_RISK_QUESTION_PREPARED.logs());
        assert_eq!(rows.markets.len(), 1);
        assert_eq!(rows.questions.len(), 1);

        let market = &rows.markets[0];
        let question = &rows.questions[0];
        assert_eq!(market.outcome_count, 2);
        assert_eq!(market.question_id, question.question_id);
        // The adapter is the oracle AND the emitter of the question.
        assert_eq!(market.oracle, question.emitter);
        assert_eq!(question.kind, QuestionKind::NegRiskQuestion);
        assert_eq!(
            question.title,
            "Will the Golden State Warriors win the 2025–2026 NBA Pacific Division?"
        );
        // questionId = event id + index.
        assert_eq!(
            question.event_id.0[..31],
            question.question_id.0[..31]
        );
        assert_eq!(question.event_id.0[31], 0);

        let rows = decode(137, &fixtures::NEG_RISK_MARKET_PREPARED.logs());
        assert_eq!(rows.questions.len(), 1);
        assert_eq!(rows.questions[0].kind, QuestionKind::NegRiskEvent);
        assert_eq!(rows.questions[0].title, "NBA Pacific Division Winner");

        let rows = decode(137, &fixtures::UMA_QUESTION_INITIALIZED.logs());
        assert_eq!(rows.markets.len(), rows.questions.len());
        let question = &rows.questions[0];
        assert_eq!(question.kind, QuestionKind::UmaQuestion);
        assert_eq!(
            question.title,
            "Kansas City Royals vs. Pittsburgh Pirates: O/U 10.5"
        );
        // p2 (price 1) pays outcome 0, p1 pays outcome 1.
        assert_eq!(question.outcomes, vec!["Over", "Under"]);
        assert!(rows
            .markets
            .iter()
            .any(|market| market.question_id == question.question_id
                && market.oracle == question.emitter));

        let rows = decode(137, &fixtures::RESOLUTION.logs());
        assert_eq!(rows.resolutions.len(), 1);
        assert_eq!(rows.resolutions[0].payout_numerators.len(), 2);
        assert_eq!(
            rows.resolutions[0].payout_denominator,
            rows.resolutions[0]
                .payout_numerators
                .iter()
                .fold(U256::ZERO, |sum, payout| sum + *payout)
        );
    }

    #[test]
    fn position_events_name_the_adapter_and_the_user() {
        let rows = decode(137, &fixtures::NEG_RISK_MERGE.logs());
        let ctf: Vec<_> = rows
            .position_events
            .iter()
            .filter(|event| event.protocol == Protocol::Ctf)
            .collect();
        let adapter: Vec<_> = rows
            .position_events
            .iter()
            .filter(|event| event.protocol == Protocol::CtfAdapter)
            .collect();

        assert_eq!(ctf.len(), 1);
        assert_eq!(adapter.len(), 1);
        assert_eq!(ctf[0].kind, PositionEventKind::Merge);
        assert_eq!(ctf[0].stakeholder, adapter[0].emitter);
        assert_eq!(ctf[0].market_id, adapter[0].market_id);
        assert_eq!(ctf[0].amount, adapter[0].amount);
        // The adapter event has no collateral: taken from its sibling.
        assert_eq!(ctf[0].collateral_token, adapter[0].collateral_token);
        assert!(!adapter[0].collateral_token.is_zero());

        let rows = decode(137, &fixtures::NEG_RISK_CONVERSION.logs());
        assert!(rows
            .position_events
            .iter()
            .any(|event| event.kind == PositionEventKind::Convert
                && event.protocol == Protocol::NegRisk));
    }

    #[test]
    fn every_real_transaction_decodes_and_maps_its_tokens() {
        for tx in fixtures::ALL {
            let logs = tx.logs();
            let rows = decode(tx.chain, &logs);
            assert!(!rows.is_empty(), "{}", tx.hash);

            // Every computed token id is one the chain really minted /
            // burnt / moved in that transaction.
            for token in &rows.outcome_tokens {
                assert!(
                    rows.transfers.iter().any(|leg| leg.outcome_token_id
                        == token.outcome_token_id
                        && leg.registry == token.registry),
                    "{}: {}",
                    tx.hash,
                    token.outcome_token_id
                );
            }
        }
    }

    #[test]
    fn wrong_shapes_are_ignored_and_nothing_panics() {
        for tx in fixtures::ALL {
            for log in tx.logs() {
                // Truncated at every word boundary (and inside a word).
                for len in (0..log.data.len()).step_by(16) {
                    let mut broken = log.clone();
                    broken.data = Bytes::copy_from_slice(&log.data[..len]);
                    let _ = decode(tx.chain, &[broken]);
                }

                // Offsets / lengths pointing into the void.
                let mut hostile = log.clone();
                let mut data = hostile.data.to_vec();
                for byte in data.iter_mut().take(128) {
                    *byte = 0xff;
                }
                hostile.data = Bytes::from(data);
                let _ = decode(tx.chain, &[hostile]);

                // A topic less.
                let mut fewer = log.clone();
                fewer.topic3 = None;
                fewer.topic_count = fewer.topic_count.min(3);
                if log.topic3.is_some() {
                    assert!(decode(tx.chain, &[fewer]).is_empty());
                }
            }
        }
    }

    #[test]
    fn a_forged_condition_id_is_not_a_market() {
        let mut logs = fixtures::NEG_RISK_QUESTION_PREPARED.logs();
        logs[0].topic1 = Some(B256::repeat_byte(0x42));
        assert!(decode(137, &logs).markets.is_empty());
    }

    #[test]
    fn a_hostile_amount_does_not_break_the_fee_split() {
        let place = fixtures::Place {
            chain: 1,
            block_number: 5,
            log_index: 0,
            timestamp: 1_700_000_000,
            transaction_hash: B256::repeat_byte(5),
        };
        let exchange = Address::repeat_byte(0xe1);
        let taker = Address::repeat_byte(0x7a);
        let token = U256::from(77u8);

        let mut logs = Vec::new();
        for (index, maker) in [0xa1u8, 0xa2].into_iter().enumerate() {
            logs.push(fixtures::constructed_v2_fill(
                fixtures::Place { log_index: index as u32, ..place },
                exchange,
                B256::repeat_byte(maker),
                Address::repeat_byte(maker),
                taker,
                true,
                token,
                U256::MAX,
                U256::MAX,
                U256::ZERO,
            ));
        }
        logs.push(fixtures::constructed_v2_fill(
            fixtures::Place { log_index: 2, ..place },
            exchange,
            B256::repeat_byte(0x7a),
            taker,
            exchange,
            false,
            token,
            U256::MAX,
            U256::MAX,
            U256::MAX,
        ));

        let rows = decode(1, &logs);
        assert_eq!(rows.trades.len(), 2);
        let fees = rows.trades.iter().fold(U256::ZERO, |sum, trade| {
            sum.saturating_add(trade.taker_fee_amount)
        });
        assert_eq!(fees, U256::MAX);
    }
}
