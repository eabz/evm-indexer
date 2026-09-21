//! Real-world transactions of every family (`fixtures_data.rs`, generated
//! from `eth_getTransactionReceipt` on public RPC endpoints of Polygon,
//! Gnosis, Base and BNB Chain - every log there is REAL and verbatim), plus
//! builders for the CONSTRUCTED logs the scenario tests need (clearly
//! named `constructed_*`).

use alloy::primitives::{Address, Bytes, B256, U256};

use crate::core::models::log::{test_support::log_with, DatabaseLog};

pub use super::fixtures_data::*;
use super::{events, TxOrigin};

pub struct RawLog {
    pub address: &'static str,
    pub topics: &'static [&'static str],
    pub data: &'static str,
    pub log_index: u32,
}

/// The decoder relevant logs of one real transaction.
pub struct RawTx {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    pub transaction_index: u32,
    pub hash: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    pub logs: &'static [RawLog],
}

/// Every real transaction, for tests that sweep all of them.
pub const ALL: &[&RawTx] = &[
    &V1_NEG_RISK_MATCH,
    &V2_MINT_MATCH,
    &NEG_RISK_QUESTION_PREPARED,
    &NEG_RISK_MARKET_PREPARED,
    &UMA_QUESTION_INITIALIZED,
    &USER_SPLIT,
    &USER_MERGE,
    &NEG_RISK_MERGE,
    &RESOLUTION,
    &USER_REDEMPTION,
    &NEG_RISK_REDEMPTION,
    &NEG_RISK_CONVERSION,
    &FPMM_BUY,
    &FPMM_SELL,
    &BSC_V1_MATCH,
    &BASE_V1_MATCH,
];

pub fn address(hex: &str) -> Address {
    hex.parse().unwrap()
}

pub fn hash(hex: &str) -> B256 {
    hex.parse().unwrap()
}

pub fn unsigned(decimal: &str) -> U256 {
    decimal.parse().unwrap()
}

/// THE place that builds a [`DatabaseLog`] in the prediction tests.
#[allow(clippy::too_many_arguments)]
pub fn build(
    chain: u64,
    emitter: Address,
    topics: &[B256],
    data: Vec<u8>,
    block_number: u64,
    log_index: u32,
    timestamp: u32,
    transaction: (B256, u32),
) -> DatabaseLog {
    let mut log = log_with(topics, data);
    log.chain = chain;
    log.address = emitter;
    log.block_number = block_number;
    log.log_index = log_index;
    log.timestamp = timestamp;
    log.transaction_hash = transaction.0;
    log.transaction_index = transaction.1;
    log
}

impl RawTx {
    pub fn hash(&self) -> B256 {
        hash(self.hash)
    }

    pub fn origin(&self) -> TxOrigin {
        TxOrigin { from: address(self.from), to: Some(address(self.to)) }
    }

    /// The logs as the pipeline hands them to the decoder.
    pub fn logs(&self) -> Vec<DatabaseLog> {
        self.placed(self.block_number, self.timestamp)
    }

    /// The same transaction in another block / at another time.
    pub fn placed(
        &self,
        block_number: u64,
        timestamp: u32,
    ) -> Vec<DatabaseLog> {
        self.logs
            .iter()
            .map(|raw| {
                let topics: Vec<B256> =
                    raw.topics.iter().map(|topic| hash(topic)).collect();
                build(
                    self.chain,
                    address(raw.address),
                    &topics,
                    raw.data.parse::<Bytes>().unwrap().to_vec(),
                    block_number,
                    raw.log_index,
                    timestamp,
                    (self.hash(), self.transaction_index),
                )
            })
            .collect()
    }
}

// ------------------------------------------------------ constructed logs

pub fn word(value: U256) -> [u8; 32] {
    value.to_be_bytes::<32>()
}

fn words(values: &[U256]) -> Vec<u8> {
    values.iter().flat_map(|value| word(*value)).collect()
}

/// Where a constructed log sits.
#[derive(Debug, Clone, Copy)]
pub struct Place {
    pub chain: u64,
    pub block_number: u64,
    pub log_index: u32,
    pub timestamp: u32,
    pub transaction_hash: B256,
}

impl Place {
    fn log(
        &self,
        emitter: Address,
        topics: &[B256],
        data: Vec<u8>,
    ) -> DatabaseLog {
        build(
            self.chain,
            emitter,
            topics,
            data,
            self.block_number,
            self.log_index,
            self.timestamp,
            (self.transaction_hash, 0),
        )
    }
}

/// CONSTRUCTED `ConditionResolution`.
pub fn constructed_resolution(
    place: Place,
    registry: Address,
    condition: B256,
    oracle: Address,
    question: B256,
    payouts: &[u64],
) -> DatabaseLog {
    let mut data = words(&[
        U256::from(payouts.len()),
        U256::from(64u8),
        U256::from(payouts.len()),
    ]);
    for payout in payouts {
        data.extend_from_slice(&word(U256::from(*payout)));
    }

    place.log(
        registry,
        &[
            events::CTF_CONDITION_RESOLUTION.topic0,
            condition,
            oracle.into_word(),
            question,
        ],
        data,
    )
}

/// CONSTRUCTED ERC-1155 `TransferSingle`.
pub fn constructed_transfer(
    place: Place,
    registry: Address,
    operator: Address,
    from: Address,
    to: Address,
    id: U256,
    value: U256,
) -> DatabaseLog {
    place.log(
        registry,
        &[
            events::ERC1155_TRANSFER_SINGLE.topic0,
            operator.into_word(),
            from.into_word(),
            to.into_word(),
        ],
        words(&[id, value]),
    )
}

/// CONSTRUCTED CTF `PayoutRedemption` of both outcomes of a binary
/// condition.
pub fn constructed_redemption(
    place: Place,
    registry: Address,
    redeemer: Address,
    collateral: Address,
    condition: B256,
    payout: U256,
) -> DatabaseLog {
    let mut data = condition.to_vec();
    data.extend(words(&[
        U256::from(96u8),
        payout,
        U256::from(2u8),
        U256::from(1u8),
        U256::from(2u8),
    ]));

    place.log(
        registry,
        &[
            events::CTF_PAYOUT_REDEMPTION.topic0,
            redeemer.into_word(),
            collateral.into_word(),
            B256::ZERO,
        ],
        data,
    )
}

/// CONSTRUCTED V2 `OrderFilled`.
#[allow(clippy::too_many_arguments)]
pub fn constructed_v2_fill(
    place: Place,
    exchange: Address,
    order_hash: B256,
    maker: Address,
    taker: Address,
    sell: bool,
    token: U256,
    maker_amount: U256,
    taker_amount: U256,
    fee: U256,
) -> DatabaseLog {
    place.log(
        exchange,
        &[
            events::EXCHANGE_V2_ORDER_FILLED.topic0,
            order_hash,
            maker.into_word(),
            taker.into_word(),
        ],
        words(&[
            U256::from(u8::from(sell)),
            token,
            maker_amount,
            taker_amount,
            fee,
            U256::ZERO,
            U256::ZERO,
        ]),
    )
}
