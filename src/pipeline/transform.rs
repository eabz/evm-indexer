//! HyperSync response -> database rows. Pure, no I/O.

use crate::{
    db::{
        models::{
            block::DatabaseBlock, contract::DatabaseContract,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
            trace::DatabaseTrace, transaction::DatabaseTransaction,
            withdrawal::DatabaseWithdrawal,
        },
        ranges::BlockRange,
        RowBatch,
    },
    tokens::TokenStandard,
    utils::events::{
        ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
        ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE, TRANSFER_EVENT_SIGNATURE,
    },
};
use alloy::primitives::{Address, B256};
use anyhow::{bail, Context, Result};
use hypersync_client::simple_types::{Block, Log, Trace, Transaction};
use std::collections::{HashMap, HashSet};

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
    pub traces: Vec<Vec<Trace>>,
}

/// Per block values joined into the transaction / log / withdrawal rows.
struct BlockContext {
    timestamp: u32,
    base_fee_per_gas: Option<u64>,
}

/// Converts a response covering exactly the blocks of `covered`.
///
/// The response must contain EVERY block of `covered` (the query asks for
/// all blocks). Anything else is an error: storing a partial range would
/// leave silent holes, and rows can not be timestamped without their block.
pub fn transform(
    chain: u64,
    data: &ResponseRows,
    covered: BlockRange,
) -> Result<Transformed> {
    let mut rows = RowBatch::default();

    // Transactions per block: HyperSync blocks carry no transaction list.
    let mut transaction_counts: HashMap<u64, u64> = HashMap::new();
    for transaction in data.transactions.iter().flatten() {
        if let Some(number) = transaction.block_number {
            *transaction_counts.entry(u64::from(number)).or_default() += 1;
        }
    }

    let mut contexts: HashMap<u64, BlockContext> = HashMap::new();

    for block in data.blocks.iter().flatten() {
        let number = block.number.context("block without a number")?;

        if !(covered.from..covered.to).contains(&number) {
            bail!("block {number} is outside the covered range {covered}");
        }

        let row = DatabaseBlock::from_hypersync(
            block,
            chain,
            transaction_counts.get(&number).copied().unwrap_or_default(),
        )?;

        let context = BlockContext {
            timestamp: row.timestamp,
            base_fee_per_gas: row.base_fee_per_gas,
        };

        // A duplicated block inside one response is dropped.
        if contexts.insert(number, context).is_some() {
            continue;
        }

        for withdrawal in block.withdrawals.iter().flatten() {
            rows.withdrawals.push(DatabaseWithdrawal::from_hypersync(
                withdrawal,
                chain,
                row.number,
                row.timestamp,
            ));
        }

        rows.blocks.push(row);
    }

    if contexts.len() as u64 != covered.len() {
        bail!(
            "response for {covered} contains {} of {} blocks",
            contexts.len(),
            covered.len()
        );
    }

    rows.blocks.sort_unstable_by_key(|block| block.number);

    let context_of = |number: u64, what: &str| {
        contexts.get(&number).with_context(|| {
            format!("{what} references block {number} missing in response")
        })
    };

    let mut contracts_seen: HashSet<Address> = HashSet::new();

    for transaction in data.transactions.iter().flatten() {
        let number = transaction
            .block_number
            .map(u64::from)
            .context("transaction without a block number")?;

        let context = context_of(number, "transaction")?;

        let row = DatabaseTransaction::from_hypersync(
            transaction,
            chain,
            context.timestamp,
            context.base_fee_per_gas,
        )?;

        if let Some(contract) = DatabaseContract::from_transaction(&row) {
            if contracts_seen.insert(contract.contract_address) {
                rows.contracts.push(contract);
            }
        }

        rows.transactions.push(row);
    }

    // Transactions that failed: everything they did was rolled back,
    // including contracts created by their inner calls.
    let failed_transactions: HashSet<B256> = rows
        .transactions
        .iter()
        .filter(|transaction| {
            transaction.status.as_deref() == Some("failure")
        })
        .map(|transaction| transaction.hash)
        .collect();

    rows.traces = data
        .traces
        .iter()
        .flatten()
        .map(|trace| DatabaseTrace::from_hypersync(trace, chain))
        .collect::<Result<_>>()?;

    // Calls that reverted, per transaction: a create below one of them was
    // rolled back even though the create trace itself reports no error.
    let mut reverted_calls: HashMap<B256, Vec<&[u16]>> = HashMap::new();
    for trace in &rows.traces {
        if let (Some(hash), Some(_)) =
            (trace.transaction_hash, &trace.error)
        {
            reverted_calls
                .entry(hash)
                .or_default()
                .push(trace.trace_address.as_slice());
        }
    }

    for trace in &rows.traces {
        let Some(contract) = DatabaseContract::from_trace(trace) else {
            continue;
        };

        let rolled_back = failed_transactions
            .contains(&contract.transaction_hash)
            || reverted_calls.get(&contract.transaction_hash).is_some_and(
                |reverted| {
                    reverted.iter().any(|ancestor| {
                        is_proper_prefix(ancestor, &trace.trace_address)
                    })
                },
            );

        if !rolled_back && contracts_seen.insert(contract.contract_address)
        {
            rows.contracts.push(contract);
        }
    }

    let mut tokens_seen: HashMap<Address, TokenStandard> = HashMap::new();

    for log in data.logs.iter().flatten() {
        let number = log
            .block_number
            .map(u64::from)
            .context("log without a block number")?;

        let context = context_of(number, "log")?;

        let row =
            DatabaseLog::from_hypersync(log, chain, context.timestamp)?;

        decode_transfers(&row, &mut rows, &mut tokens_seen);

        rows.logs.push(row);
    }

    Ok(Transformed { rows, tokens_seen })
}

/// True when `ancestor` is a strict prefix of `path`, i.e. the trace at
/// `ancestor` is a (transitive) parent of the trace at `path`.
fn is_proper_prefix(ancestor: &[u16], path: &[u16]) -> bool {
    ancestor.len() < path.len() && path.starts_with(ancestor)
}

/// ERC20 / ERC721 / ERC1155 transfers are decoded from the generic logs.
fn decode_transfers(
    log: &DatabaseLog,
    rows: &mut RowBatch,
    tokens_seen: &mut HashMap<Address, TokenStandard>,
) {
    let Some(topic0) = log.topic0 else { return };

    if topic0 == TRANSFER_EVENT_SIGNATURE {
        // Same signature: ERC721 indexes the token id (4 topics), ERC20
        // keeps the amount in the data (3 topics).
        if log.topic3.is_some() {
            if let Some(row) = DatabaseERC721Transfer::from_log(log) {
                tokens_seen
                    .entry(row.token_address)
                    .or_insert(TokenStandard::Erc721);
                rows.erc721_transfers.push(row);
            }
        } else if let Some(row) = DatabaseERC20Transfer::from_log(log) {
            tokens_seen
                .entry(row.token_address)
                .or_insert(TokenStandard::Erc20);
            rows.erc20_transfers.push(row);
        }
    } else if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
        || topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
    {
        if let Some(row) = DatabaseERC1155Transfer::from_log(log) {
            tokens_seen
                .entry(row.token_address)
                .or_insert(TokenStandard::Erc1155);
            rows.erc1155_transfers.push(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::log::test_support::word;
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
            traces: vec![],
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
        assert!(rows.blocks.iter().all(|b| !b.is_uncle));

        // Timestamp / base fee joined by block number.
        assert_eq!(rows.transactions[0].timestamp, 1_000);
        assert_eq!(rows.transactions[2].timestamp, 2_000);
        assert_eq!(rows.transactions[0].base_fee_per_gas, Some(7));
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
    fn contracts_come_from_transactions_and_create_traces() {
        let mut deployment = transaction(1, 0, 0xd1);
        deployment.contract_address = Some(HsAddress::from([0xc1; 20]));

        let mut failed = transaction(1, 1, 0xd2);
        failed.contract_address = Some(HsAddress::from([0xc2; 20]));
        failed.status = Some(TransactionStatus::Failure);

        let create = |address: u8, error: Option<&str>| Trace {
            block_number: Some(1),
            type_: Some("create".to_string()),
            from: Some(HsAddress::from([0xfa; 20])),
            address: Some(HsAddress::from([address; 20])),
            transaction_hash: Some(Hash::from([0xd3; 32])),
            error: error.map(str::to_string),
            ..Default::default()
        };

        let data = ResponseRows {
            blocks: vec![vec![block(1, 1)]],
            transactions: vec![vec![deployment, failed]],
            traces: vec![vec![
                // Same contract as the deployment transaction: deduped.
                create(0xc1, None),
                // Deployed by a factory.
                create(0xc3, None),
                // Reverted creation.
                create(0xc4, Some("Reverted")),
                Trace {
                    block_number: Some(1),
                    type_: Some("call".to_string()),
                    ..Default::default()
                },
            ]],
            ..Default::default()
        };

        let rows =
            transform(CHAIN, &data, BlockRange::new(1, 2)).unwrap().rows;

        let contracts: Vec<Address> =
            rows.contracts.iter().map(|c| c.contract_address).collect();

        assert_eq!(
            contracts,
            vec![Address::repeat_byte(0xc1), Address::repeat_byte(0xc3)]
        );
        assert_eq!(rows.contracts[0].creator, Address::repeat_byte(0x0f));
        assert_eq!(rows.contracts[1].creator, Address::repeat_byte(0xfa));
        assert_eq!(rows.traces.len(), 4);
    }

    #[test]
    fn proper_prefix() {
        assert!(is_proper_prefix(&[], &[0]));
        assert!(is_proper_prefix(&[0], &[0, 1]));
        assert!(is_proper_prefix(&[0], &[0, 1, 2]));
        // A trace is not its own ancestor.
        assert!(!is_proper_prefix(&[0, 1], &[0, 1]));
        // Siblings / other branches / descendants.
        assert!(!is_proper_prefix(&[1], &[0, 1]));
        assert!(!is_proper_prefix(&[0, 1, 2], &[0, 1]));
        assert!(!is_proper_prefix(&[], &[]));
    }

    fn create_trace(tx: u8, path: &[u64], address: u8) -> Trace {
        Trace {
            block_number: Some(1),
            type_: Some("create".to_string()),
            from: Some(HsAddress::from([0xfa; 20])),
            address: Some(HsAddress::from([address; 20])),
            transaction_hash: Some(Hash::from([tx; 32])),
            trace_address: Some(path.to_vec()),
            ..Default::default()
        }
    }

    fn call_trace(tx: u8, path: &[u64], error: Option<&str>) -> Trace {
        Trace {
            block_number: Some(1),
            type_: Some("call".to_string()),
            transaction_hash: Some(Hash::from([tx; 32])),
            trace_address: Some(path.to_vec()),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    fn contracts_of(data: &ResponseRows) -> Vec<Address> {
        transform(CHAIN, data, BlockRange::new(1, 2))
            .unwrap()
            .rows
            .contracts
            .iter()
            .map(|contract| contract.contract_address)
            .collect()
    }

    #[test]
    fn creations_of_a_failed_transaction_are_not_contracts() {
        let mut failed = transaction(1, 0, 0xe1);
        failed.status = Some(TransactionStatus::Failure);

        let data = ResponseRows {
            blocks: vec![vec![block(1, 1)]],
            transactions: vec![vec![failed, transaction(1, 1, 0xe2)]],
            traces: vec![vec![
                // The inner create "succeeded" but the transaction ran out
                // of gas afterwards: rolled back.
                call_trace(0xe1, &[], Some("out of gas")),
                create_trace(0xe1, &[0], 0xc1),
                // Same shape in a successful transaction: kept.
                call_trace(0xe2, &[], None),
                create_trace(0xe2, &[0], 0xc2),
            ]],
            ..Default::default()
        };

        assert_eq!(contracts_of(&data), vec![Address::repeat_byte(0xc2)]);

        // The traces themselves are all stored.
        let rows =
            transform(CHAIN, &data, BlockRange::new(1, 2)).unwrap().rows;
        assert_eq!(rows.traces.len(), 4);
    }

    #[test]
    fn creations_below_a_reverted_call_are_not_contracts() {
        // The transaction succeeds (the caller catches the revert), but
        // the sub call [0] reverted: everything below it is rolled back.
        let data = ResponseRows {
            blocks: vec![vec![block(1, 1)]],
            transactions: vec![vec![transaction(1, 0, 0xe1)]],
            traces: vec![vec![
                call_trace(0xe1, &[], None),
                call_trace(0xe1, &[0], Some("Reverted")),
                // Direct child and deeper descendant of the reverted call.
                create_trace(0xe1, &[0, 0], 0xc1),
                call_trace(0xe1, &[0, 1], None),
                create_trace(0xe1, &[0, 1, 0], 0xc2),
                // Sibling branch that did not revert: kept.
                call_trace(0xe1, &[1], None),
                create_trace(0xe1, &[1, 0], 0xc3),
                // Same path in ANOTHER transaction is unrelated: kept.
                create_trace(0xe9, &[0, 0], 0xc4),
            ]],
            ..Default::default()
        };

        assert_eq!(
            contracts_of(&data),
            vec![Address::repeat_byte(0xc3), Address::repeat_byte(0xc4)]
        );
    }

    #[test]
    fn a_reverted_call_after_the_create_in_trace_order_still_counts() {
        // Ancestors normally come first, but do not depend on the order.
        let data = ResponseRows {
            blocks: vec![vec![block(1, 1)]],
            transactions: vec![vec![transaction(1, 0, 0xe1)]],
            traces: vec![vec![
                create_trace(0xe1, &[2, 0], 0xc1),
                call_trace(0xe1, &[2], Some("Reverted")),
            ]],
            ..Default::default()
        };

        assert!(contracts_of(&data).is_empty());
    }
}
