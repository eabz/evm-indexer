//! HyperSync response -> database rows: ORCHESTRATION only.
//!
//! The conversions themselves belong to the datasets: `core::decode`
//! turns a response into the core rows, every other module's `decode`
//! reads the logs of that batch (`pipeline::modules`). This file joins
//! the two and hands back one [`Transformed`]. Pure, no I/O.

use crate::{
    core::{self, RowBatch},
    db::ranges::BlockRange,
    pipeline::modules::{self, DecodeState, EnabledModules},
    tokens::TokenStandard,
};
use alloy::primitives::Address;
use anyhow::Result;
use hypersync_client::simple_types::{Block, Log, Transaction};
use std::collections::HashMap;

/// Rows of one response plus the token contracts seen in its transfers.
#[derive(Debug, Default)]
pub struct Transformed {
    pub rows: RowBatch,
    /// The standard hint is the transfer type the address was seen in.
    pub tokens_seen: HashMap<Address, TokenStandard>,
}

/// The payload of a HyperSync `QueryResponse` (its own data type can not
/// be named outside the client crate): one inner `Vec` per Arrow batch.
#[derive(Debug, Default)]
pub struct ResponseRows {
    pub blocks: Vec<Vec<Block>>,
    pub transactions: Vec<Vec<Transaction>>,
    pub logs: Vec<Vec<Log>>,
}

/// [`transform_with`] without any decoder module: the core rows only.
/// The pipeline itself always goes through [`transform_with`].
pub fn transform(
    chain: u64,
    data: &ResponseRows,
    covered: BlockRange,
) -> Result<Transformed> {
    transform_with(
        chain,
        data,
        covered,
        EnabledModules::none(),
        &mut DecodeState::default(),
    )
}

/// Converts a response covering exactly the blocks of `covered`, and runs
/// the enabled decoder modules (DEX, ...) over ALL of its logs.
///
/// The response must contain EVERY block of `covered` (the query asks for
/// all blocks). Anything else is an error: storing a partial range would
/// leave silent holes, and rows can not be timestamped without their block.
pub fn transform_with(
    chain: u64,
    data: &ResponseRows,
    covered: BlockRange,
    enabled: EnabledModules,
    state: &mut DecodeState,
) -> Result<Transformed> {
    let Transformed { mut rows, mut tokens_seen } =
        core::decode(chain, data, covered)?;

    // Every log of the response, never a filtered subset.
    rows.modules = modules::decode(enabled, chain, &rows, state);

    for (address, standard) in rows.modules.token_hints() {
        tokens_seen.entry(address).or_insert(standard);
    }

    Ok(Transformed { rows, tokens_seen })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        events::TRANSFER_EVENT_SIGNATURE, models::log::test_support::word,
    };
    use alloy::primitives::U256;
    use hypersync_client::format::{
        Address as HsAddress, Data, Hash, LogArgument, Quantity,
        TransactionStatus, UInt, Withdrawal,
    };

    const CHAIN: u64 = 1;

    fn block(number: u64, timestamp: u64) -> Block {
        Block {
            number: Some(number),
            hash: Some(Hash::from([number as u8; 32])),
            parent_hash: Some(Hash::from(
                [number.wrapping_sub(1) as u8; 32],
            )),
            timestamp: Some(Quantity::from(timestamp)),
            base_fee_per_gas: Some(Quantity::from(7u64)),
            ..Default::default()
        }
    }

    fn transaction(block: u64, index: u64, id: u8) -> Transaction {
        Transaction {
            block_number: Some(UInt::from(block)),
            transaction_index: Some(UInt::from(index)),
            hash: Some(Hash::from([id; 32])),
            from: Some(HsAddress::from([0x0f; 20])),
            status: Some(TransactionStatus::Success),
            ..Default::default()
        }
    }

    fn transfer_log(
        block: u64,
        index: u64,
        token: u8,
        topics: usize,
    ) -> Log {
        let mut log = Log {
            block_number: Some(UInt::from(block)),
            log_index: Some(UInt::from(index)),
            transaction_hash: Some(Hash::from([1u8; 32])),
            address: Some(HsAddress::from([token; 20])),
            data: Some(Data::from(word(99))),
            ..Default::default()
        };
        log.topics
            .push(Some(LogArgument::from(TRANSFER_EVENT_SIGNATURE.0)));
        for i in 1..4 {
            log.topics.push(
                (i < topics).then(|| LogArgument::from([i as u8; 32])),
            );
        }
        log
    }

    #[test]
    fn joins_block_data_into_rows_and_counts_transactions() {
        let mut with_withdrawal = block(11, 2_000);
        with_withdrawal.withdrawals = Some(vec![Withdrawal {
            index: Some(Quantity::from(5u64)),
            validator_index: Some(Quantity::from(6u64)),
            address: Some(HsAddress::from([2u8; 20])),
            amount: Some(Quantity::from(1u64)),
        }]);

        let data = ResponseRows {
            // Two Arrow batches, out of order on purpose.
            blocks: vec![vec![with_withdrawal], vec![block(10, 1_000)]],
            transactions: vec![vec![
                transaction(10, 0, 0xa1),
                transaction(10, 1, 0xa2),
                transaction(11, 0, 0xa3),
            ]],
            logs: vec![vec![
                transfer_log(10, 0, 0x20, 3),
                transfer_log(11, 0, 0x21, 4),
            ]],
        };

        let out =
            transform(CHAIN, &data, BlockRange::new(10, 12)).unwrap();
        let rows = out.rows;

        // Sorted by number, transaction count derived from the response.
        assert_eq!(
            rows.blocks.iter().map(|b| b.number).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert_eq!(rows.blocks[0].transactions, 2);
        assert_eq!(rows.blocks[1].transactions, 1);

        // Timestamp / base fee joined by block number.
        assert_eq!(rows.transactions[0].timestamp, 1_000);
        assert_eq!(rows.transactions[2].timestamp, 2_000);
        assert_eq!(
            rows.transactions[0].base_fee_per_gas,
            Some(U256::from(7u64))
        );
        assert_eq!(rows.logs[0].timestamp, 1_000);
        assert_eq!(rows.logs[1].timestamp, 2_000);

        assert_eq!(rows.withdrawals.len(), 1);
        assert_eq!(rows.withdrawals[0].block_number, 11);
        assert_eq!(rows.withdrawals[0].timestamp, 2_000);
        assert_eq!(rows.withdrawals[0].validator_index, 6);
        assert_eq!(rows.withdrawals[0].withdrawal_index, 5);

        // Transfers decoded from the generic logs + token standard hints.
        assert_eq!(rows.erc20_transfers.len(), 1);
        assert_eq!(rows.erc20_transfers[0].amount, U256::from(99u64));
        assert_eq!(rows.erc721_transfers.len(), 1);
        assert!(rows.erc1155_transfers.is_empty());

        assert_eq!(out.tokens_seen.len(), 2);
        assert_eq!(
            out.tokens_seen[&Address::repeat_byte(0x20)],
            TokenStandard::Erc20
        );
        assert_eq!(
            out.tokens_seen[&Address::repeat_byte(0x21)],
            TokenStandard::Erc721
        );
    }

    #[test]
    fn empty_blocks_still_produce_block_rows() {
        let data = ResponseRows {
            blocks: vec![vec![block(1, 1), block(2, 2)]],
            ..Default::default()
        };

        let out = transform(CHAIN, &data, BlockRange::new(1, 3)).unwrap();

        assert_eq!(out.rows.blocks.len(), 2);
        assert_eq!(out.rows.blocks[0].transactions, 0);
        assert_eq!(out.rows.rows(), 2);
    }

    #[test]
    fn a_missing_block_is_an_error_not_a_silent_hole() {
        let data = ResponseRows {
            blocks: vec![vec![block(1, 1), block(3, 3)]],
            ..Default::default()
        };

        let error =
            transform(CHAIN, &data, BlockRange::new(1, 4)).unwrap_err();
        assert!(format!("{error:#}").contains("2 of 3 blocks"));
    }

    #[test]
    fn blocks_outside_the_covered_range_are_rejected() {
        let data = ResponseRows {
            blocks: vec![vec![block(9, 1)]],
            ..Default::default()
        };

        assert!(transform(CHAIN, &data, BlockRange::new(1, 2)).is_err());
    }

    #[test]
    fn rows_of_an_unknown_block_are_rejected() {
        let data = ResponseRows {
            blocks: vec![vec![block(1, 1)]],
            transactions: vec![vec![transaction(2, 0, 1)]],
            ..Default::default()
        };

        assert!(transform(CHAIN, &data, BlockRange::new(1, 2)).is_err());
    }

    #[test]
    fn deployments_stay_in_the_transaction_row() {
        // `contracts` is a view over transactions: what it needs is the
        // created address, the sender, the status and the timestamp.
        let mut deployment = transaction(1, 0, 0xd1);
        deployment.contract_address = Some(HsAddress::from([0xc1; 20]));

        let mut failed = transaction(1, 1, 0xd2);
        failed.contract_address = Some(HsAddress::from([0xc2; 20]));
        failed.status = Some(TransactionStatus::Failure);

        let data = ResponseRows {
            blocks: vec![vec![block(1, 1_234)]],
            transactions: vec![vec![deployment, failed]],
            ..Default::default()
        };

        let rows =
            transform(CHAIN, &data, BlockRange::new(1, 2)).unwrap().rows;

        assert_eq!(
            rows.transactions[0].created_contract(),
            Some(Address::repeat_byte(0xc1))
        );
        assert_eq!(
            rows.transactions[0].status.as_deref(),
            Some("success")
        );
        assert_eq!(rows.transactions[0].from, Address::repeat_byte(0x0f));
        assert_eq!(rows.transactions[0].timestamp, 1_234);
        assert_eq!(
            rows.transactions[1].status.as_deref(),
            Some("failure")
        );
    }

    #[test]
    fn erc721_transfer_of_token_id_zero_is_not_an_erc20_transfer() {
        let mut log = transfer_log(10, 0, 0x21, 4);
        log.topics[3] = Some(LogArgument::from([0u8; 32]));

        let data = ResponseRows {
            blocks: vec![vec![block(10, 1)]],
            logs: vec![vec![log]],
            ..Default::default()
        };

        let rows =
            transform(CHAIN, &data, BlockRange::new(10, 11)).unwrap().rows;

        assert_eq!(rows.logs[0].topic_count, 4);
        assert_eq!(rows.erc721_transfers.len(), 1);
        assert_eq!(rows.erc721_transfers[0].id, U256::ZERO);
        assert!(rows.erc20_transfers.is_empty());
    }
}
