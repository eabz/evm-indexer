//! Real transactions of every family (`fixtures_data.rs`, generated from
//! `eth_getTransactionReceipt` / `eth_getTransactionByHash` on public RPC
//! endpoints of Robinhood Chain, BNB Chain and Base - every log there is
//! REAL and verbatim, and ALL logs of each transaction are kept), plus
//! builders for the CONSTRUCTED logs the adversarial tests need (always
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

/// One real transaction, with every one of its logs.
pub struct RawTx {
    pub chain: u64,
    pub block_number: u64,
    pub timestamp: u32,
    pub transaction_index: u32,
    pub hash: &'static str,
    pub from: &'static str,
    pub to: &'static str,
    /// Native coin sent with the transaction, as a decimal string.
    pub value: &'static str,
    pub logs: &'static [RawLog],
}

pub fn address(hex: &str) -> Address {
    hex.parse().unwrap()
}

pub fn hash(hex: &str) -> B256 {
    hex.parse().unwrap()
}

pub fn unsigned(decimal: &str) -> U256 {
    decimal.parse().unwrap()
}

/// THE place that builds a [`DatabaseLog`] in the launchpad tests.
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
        TxOrigin {
            from: address(self.from),
            to: Some(address(self.to)),
            value: unsigned(self.value),
        }
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

/// CONSTRUCTED `pons_v2` `TokenLaunched` - a launch anybody can emit.
pub fn constructed_launch(
    place: Place,
    factory: Address,
    token: Address,
    curve: Address,
    creator: Address,
    threshold: U256,
) -> DatabaseLog {
    place.log(
        factory,
        &[
            events::PONS_V2_TOKEN_LAUNCHED.topic0,
            token.into_word(),
            curve.into_word(),
            creator.into_word(),
        ],
        words(&[U256::ZERO, U256::ZERO, threshold]),
    )
}

/// CONSTRUCTED `pons_v2` `CurveBuy`.
#[allow(clippy::too_many_arguments)]
pub fn constructed_buy(
    place: Place,
    curve: Address,
    buyer: Address,
    recipient: Address,
    quote_in: U256,
    tokens_out: U256,
    fee: U256,
    tax: U256,
) -> DatabaseLog {
    place.log(
        curve,
        &[
            events::PONS_V2_CURVE_BUY.topic0,
            buyer.into_word(),
            recipient.into_word(),
        ],
        words(&[quote_in, tokens_out, fee, tax]),
    )
}

/// CONSTRUCTED ERC-20 `Transfer`, the only evidence a curve leg can have.
pub fn constructed_transfer(
    place: Place,
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
) -> DatabaseLog {
    place.log(
        token,
        &[events::ERC20_TRANSFER.topic0, from.into_word(), to.into_word()],
        word(amount).to_vec(),
    )
}
