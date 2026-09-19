//! Envio EVM HyperSync: the only place that talks to the EVM source.
//!
//! One generic query (every block, transaction and log) streamed over a
//! bounded block range. No chain specific logic: the endpoint is derived
//! from the chain id unless a url is given.

use crate::{
    core::convert::{hash_to_b256, quantity_to_u32},
    db::ranges::BlockRange,
    pipeline::{transform::ResponseRows, BlockSource, SourceResponse},
    reorg::{BlockHeader, CanonicalChain},
};
use anyhow::{bail, Context, Result};
use futures::future::BoxFuture;
use hypersync_client::{
    net_types::{
        BlockField, LogField, LogFilter, Query, TransactionField,
        TransactionFilter,
    },
    Client, StreamConfig,
};
use log::{info, warn};
use tokio::sync::mpsc::{self, Receiver};

// Field selection (docs/design.md, section 8): HyperSync is asked for
// exactly what a column stores, nothing else. Dropping a column drops the
// field from the query (`logs_bloom` is gone from both). The
// `selection_is_exactly_what_the_rows_need` test enforces both directions.

/// Block header fields needed by `DatabaseBlock` and the withdrawals.
const BLOCK_FIELDS: [BlockField; 21] = [
    BlockField::Number,
    BlockField::Hash,
    BlockField::ParentHash,
    BlockField::Nonce,
    BlockField::Sha3Uncles,
    BlockField::TransactionsRoot,
    BlockField::StateRoot,
    BlockField::ReceiptsRoot,
    BlockField::Miner,
    BlockField::Difficulty,
    BlockField::TotalDifficulty,
    BlockField::ExtraData,
    BlockField::Size,
    BlockField::GasLimit,
    BlockField::GasUsed,
    BlockField::Timestamp,
    BlockField::Uncles,
    BlockField::BaseFeePerGas,
    BlockField::WithdrawalsRoot,
    BlockField::Withdrawals,
    BlockField::MixHash,
];

/// Transaction + receipt fields needed by `DatabaseTransaction`.
const TRANSACTION_FIELDS: [TransactionField; 20] = [
    TransactionField::BlockHash,
    TransactionField::BlockNumber,
    TransactionField::From,
    TransactionField::Gas,
    TransactionField::GasPrice,
    TransactionField::Hash,
    TransactionField::Input,
    TransactionField::Nonce,
    TransactionField::To,
    TransactionField::TransactionIndex,
    TransactionField::Value,
    TransactionField::MaxPriorityFeePerGas,
    TransactionField::MaxFeePerGas,
    TransactionField::AccessList,
    TransactionField::CumulativeGasUsed,
    TransactionField::EffectiveGasPrice,
    TransactionField::GasUsed,
    TransactionField::ContractAddress,
    TransactionField::Type,
    TransactionField::Status,
];

const LOG_FIELDS: [LogField; 10] = [
    LogField::LogIndex,
    LogField::TransactionIndex,
    LogField::TransactionHash,
    LogField::BlockNumber,
    LogField::Address,
    LogField::Data,
    LogField::Topic0,
    LogField::Topic1,
    LogField::Topic2,
    LogField::Topic3,
];

/// Builds the query for `[range.from, range.to)`.
pub fn build_query(range: BlockRange) -> Query {
    // No traces, by design (docs/design.md, section 9).
    Query::new()
        .from_block(range.from)
        .to_block_excl(range.to)
        // Blocks without transactions must be returned too: the block row
        // is what marks a block as indexed.
        .include_all_blocks()
        .where_transactions(TransactionFilter::all())
        .where_logs(LogFilter::all())
        .select_block_fields(BLOCK_FIELDS)
        .select_transaction_fields(TRANSACTION_FIELDS)
        .select_log_fields(LOG_FIELDS)
}

/// What the fork-point search compares with the stored blocks.
const HEADER_FIELDS: [BlockField; 4] = [
    BlockField::Number,
    BlockField::Hash,
    BlockField::ParentHash,
    BlockField::Timestamp,
];

/// Headers only: no transactions, no logs.
pub fn build_header_query(range: BlockRange) -> Query {
    Query::new()
        .from_block(range.from)
        .to_block_excl(range.to)
        .include_all_blocks()
        .select_block_fields(HEADER_FIELDS)
}

/// Requests per [`CanonicalChain::headers`] call. One is the norm (the
/// ranges are a few hundred headers at most).
const MAX_HEADER_REQUESTS: usize = 16;

#[derive(Clone)]
pub struct Source {
    client: Client,
}

impl Source {
    pub fn new(
        chain_id: u64,
        url: Option<&str>,
        api_token: &str,
    ) -> Result<Self> {
        let builder = match url {
            Some(url) => Client::builder().url(url),
            None => Client::builder().chain_id(chain_id),
        };

        let client = builder
            .api_token(api_token)
            .build()
            .context("build HyperSync client")?;

        info!("Using HyperSync endpoint {}.", client.url());

        Ok(Self { client })
    }

    /// Guards against pointing `--hypersync-url` at another chain, which
    /// would silently index the wrong data under this chain id.
    pub async fn verify_chain_id(&self, expected: u64) -> Result<()> {
        match self.client.get_chain_id().await {
            Ok(actual) if actual == expected => Ok(()),
            Ok(actual) => bail!(
                "HyperSync endpoint serves chain {actual} but --chain is \
                 {expected}"
            ),
            Err(e) => {
                // Not every deployment exposes the endpoint; not fatal.
                warn!("Could not verify the HyperSync chain id: {e:#}");
                Ok(())
            }
        }
    }
}

impl CanonicalChain for Source {
    /// Headers of `[from, to)` as HyperSync has them NOW. Heights it does
    /// not have (the chain got shorter) are simply absent: the caller
    /// checks completeness and that the headers chain.
    fn headers(
        &self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'_, Result<Vec<BlockHeader>>> {
        Box::pin(async move {
            let mut headers = Vec::new();
            let mut cursor = from;

            for _ in 0..MAX_HEADER_REQUESTS {
                if cursor >= to {
                    break;
                }

                let response = self
                    .client
                    .get(&build_header_query(BlockRange::new(cursor, to)))
                    .await
                    .with_context(|| {
                        format!("get HyperSync headers [{cursor}, {to})")
                    })?;

                for block in response.data.blocks.iter().flatten() {
                    let (Some(number), Some(hash), Some(parent_hash)) =
                        (block.number, &block.hash, &block.parent_hash)
                    else {
                        bail!("HyperSync returned a header without number or hashes");
                    };

                    let timestamp = block
                        .timestamp
                        .as_ref()
                        .map(quantity_to_u32)
                        .unwrap_or_default();

                    headers.push(BlockHeader {
                        number,
                        hash: hash_to_b256(hash),
                        parent_hash: hash_to_b256(parent_hash),
                        timestamp,
                    });
                }

                // No progress: the archive ends here.
                if response.next_block <= cursor {
                    break;
                }
                cursor = response.next_block;
            }

            headers.sort_unstable_by_key(|header| header.number);
            headers.dedup_by_key(|header| header.number);

            Ok(headers)
        })
    }
}

impl BlockSource for Source {
    /// The client itself treats the archive height as the EXCLUSIVE end of
    /// an open ended stream, so the same (conservative) reading is used
    /// here: blocks `[0, height)` are requested, never `height` itself.
    /// Requesting a block the server does not have yet makes the stream
    /// fail with a "no progress" error.
    async fn head(&self) -> Result<u64> {
        self.client.get_height().await.context("get HyperSync height")
    }

    /// Streams `[range.from, range.to)`. HyperSync delivers the responses in
    /// block order, each one covering whole blocks up to its `next_block`.
    /// The range is always bounded: an open ended stream would follow the
    /// head on its own and hide the commit points from the sync loop.
    async fn stream(
        &self,
        range: BlockRange,
    ) -> Result<Receiver<Result<SourceResponse>>> {
        let mut responses = self
            .client
            .stream(build_query(range), StreamConfig::default())
            .await
            .with_context(|| {
                format!("start HyperSync stream for {range}")
            })?;

        // Capacity 1: the look-ahead buffering is HyperSync's job, this
        // only adapts the response type.
        let (tx, rx) = mpsc::channel(1);

        tokio::spawn(async move {
            while let Some(response) = responses.recv().await {
                let response = response.map(|response| SourceResponse {
                    next_block: response.next_block,
                    data: ResponseRows {
                        blocks: response.data.blocks,
                        transactions: response.data.transactions,
                        logs: response.data.logs,
                    },
                    rollback_guard: response.rollback_guard,
                });

                if tx.send(response).await.is_err() {
                    // Receiver dropped: dropping `responses` stops the
                    // HyperSync stream too.
                    return;
                }
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::transform::transform;
    use hypersync_client::{
        format::{
            AccessList, Address as HsAddress, Data, Hash, LogArgument,
            Nonce, Quantity, TransactionStatus, TransactionType, UInt,
            Withdrawal,
        },
        simple_types::{Block, Log, Transaction},
    };

    #[test]
    fn query_covers_everything_in_a_bounded_range() {
        let query = build_query(BlockRange::new(100, 200));

        assert_eq!(query.from_block, 100);
        assert_eq!(query.to_block, Some(200));
        assert!(query.include_all_blocks);
        assert_eq!(query.transactions.len(), 1);
        assert_eq!(query.logs.len(), 1);
        // Traces are never requested (docs/design.md, section 9).
        assert!(query.traces.is_empty());
        assert!(query.field_selection.trace.is_empty());
        assert_eq!(query.field_selection.block.len(), BLOCK_FIELDS.len());
        assert_eq!(
            query.field_selection.transaction.len(),
            TRANSACTION_FIELDS.len()
        );
        assert_eq!(query.field_selection.log.len(), LOG_FIELDS.len());
    }

    #[test]
    fn header_query_asks_for_headers_only() {
        let query = build_header_query(BlockRange::new(10, 20));

        assert_eq!(query.from_block, 10);
        assert_eq!(query.to_block, Some(20));
        assert!(query.include_all_blocks);
        assert!(query.transactions.is_empty());
        assert!(query.logs.is_empty());
        assert!(query.traces.is_empty());
        assert_eq!(query.field_selection.block.len(), 4);
        assert!(query.field_selection.transaction.is_empty());
        assert!(query.field_selection.log.is_empty());
    }

    #[test]
    fn dropped_columns_are_not_requested() {
        assert!(!BLOCK_FIELDS.contains(&BlockField::LogsBloom));
    }

    #[test]
    fn malformed_token_is_an_error_not_a_panic() {
        assert!(Source::new(1, None, "not-a-uuid").is_err());
    }

    // ---- selection <-> models ------------------------------------------
    //
    // A response with EVERY field populated is reduced to the selected
    // fields (what HyperSync would actually return) and transformed:
    //
    // 1. the rows must equal the rows of the full response, i.e. every
    //    field a model reads is selected
    // 2. dropping any single selected field must change the rows, i.e.
    //    nothing is requested that no column needs

    const BLOCK: u64 = 100;

    fn full_block() -> Block {
        Block {
            number: Some(BLOCK),
            hash: Some(Hash::from([0xb1; 32])),
            parent_hash: Some(Hash::from([0xb0; 32])),
            nonce: Some(Nonce::from([0x42; 8])),
            sha3_uncles: Some(Hash::from([0x01; 32])),
            logs_bloom: Some(Data::from(vec![0xff; 256])),
            transactions_root: Some(Hash::from([0x02; 32])),
            state_root: Some(Hash::from([0x03; 32])),
            receipts_root: Some(Hash::from([0x04; 32])),
            miner: Some(HsAddress::from([0x05; 20])),
            difficulty: Some(Quantity::from(6u64)),
            total_difficulty: Some(Quantity::from(7u64)),
            extra_data: Some(Data::from(vec![8u8])),
            size: Some(Quantity::from(9u64)),
            gas_limit: Some(Quantity::from(10u64)),
            gas_used: Some(Quantity::from(11u64)),
            timestamp: Some(Quantity::from(1_700_000_000u64)),
            uncles: Some(vec![Hash::from([0x0c; 32])]),
            base_fee_per_gas: Some(Quantity::from(13u64)),
            withdrawals_root: Some(Hash::from([0x0e; 32])),
            withdrawals: Some(vec![Withdrawal {
                index: Some(Quantity::from(15u64)),
                validator_index: Some(Quantity::from(16u64)),
                address: Some(HsAddress::from([0x11; 20])),
                amount: Some(Quantity::from(18u64)),
            }]),
            mix_hash: Some(Hash::from([0x13; 32])),
            ..Default::default()
        }
    }

    fn project_block(full: &Block, fields: &[BlockField]) -> Block {
        let mut block = Block::default();
        for field in fields {
            match field {
                BlockField::Number => block.number = full.number,
                BlockField::Hash => block.hash = full.hash.clone(),
                BlockField::ParentHash => {
                    block.parent_hash = full.parent_hash.clone()
                }
                BlockField::Nonce => block.nonce = full.nonce.clone(),
                BlockField::Sha3Uncles => {
                    block.sha3_uncles = full.sha3_uncles.clone()
                }
                BlockField::TransactionsRoot => {
                    block.transactions_root =
                        full.transactions_root.clone()
                }
                BlockField::StateRoot => {
                    block.state_root = full.state_root.clone()
                }
                BlockField::ReceiptsRoot => {
                    block.receipts_root = full.receipts_root.clone()
                }
                BlockField::Miner => block.miner = full.miner.clone(),
                BlockField::Difficulty => {
                    block.difficulty = full.difficulty.clone()
                }
                BlockField::TotalDifficulty => {
                    block.total_difficulty = full.total_difficulty.clone()
                }
                BlockField::ExtraData => {
                    block.extra_data = full.extra_data.clone()
                }
                BlockField::Size => block.size = full.size.clone(),
                BlockField::GasLimit => {
                    block.gas_limit = full.gas_limit.clone()
                }
                BlockField::GasUsed => {
                    block.gas_used = full.gas_used.clone()
                }
                BlockField::Timestamp => {
                    block.timestamp = full.timestamp.clone()
                }
                BlockField::Uncles => block.uncles = full.uncles.clone(),
                BlockField::BaseFeePerGas => {
                    block.base_fee_per_gas = full.base_fee_per_gas.clone()
                }
                BlockField::WithdrawalsRoot => {
                    block.withdrawals_root = full.withdrawals_root.clone()
                }
                BlockField::Withdrawals => {
                    block.withdrawals = full.withdrawals.clone()
                }
                BlockField::MixHash => {
                    block.mix_hash = full.mix_hash.clone()
                }
                other => panic!("add {other:?} to project_block"),
            }
        }
        block
    }

    fn full_transaction() -> Transaction {
        Transaction {
            block_hash: Some(Hash::from([0xb1; 32])),
            block_number: Some(UInt::from(BLOCK)),
            from: Some(HsAddress::from([0x21; 20])),
            gas: Some(Quantity::from(22u64)),
            gas_price: Some(Quantity::from(23u64)),
            hash: Some(Hash::from([0x24; 32])),
            input: Some(Data::from(vec![0xa9, 0x05, 0x9c, 0xbb, 0x25])),
            nonce: Some(Quantity::from(26u64)),
            to: Some(HsAddress::from([0x27; 20])),
            transaction_index: Some(UInt::from(3u64)),
            value: Some(Quantity::from(28u64)),
            max_priority_fee_per_gas: Some(Quantity::from(29u64)),
            max_fee_per_gas: Some(Quantity::from(30u64)),
            access_list: Some(vec![AccessList {
                address: Some(HsAddress::from([0x31; 20])),
                storage_keys: Some(vec![Hash::from([0x32; 32])]),
            }]),
            cumulative_gas_used: Some(Quantity::from(33u64)),
            effective_gas_price: Some(Quantity::from(34u64)),
            gas_used: Some(Quantity::from(35u64)),
            contract_address: Some(HsAddress::from([0x36; 20])),
            type_: Some(TransactionType::from(2u8)),
            status: Some(TransactionStatus::Success),
            ..Default::default()
        }
    }

    fn project_transaction(
        full: &Transaction,
        fields: &[TransactionField],
    ) -> Transaction {
        let mut tx = Transaction::default();
        for field in fields {
            match field {
                TransactionField::BlockHash => {
                    tx.block_hash = full.block_hash.clone()
                }
                TransactionField::BlockNumber => {
                    tx.block_number = full.block_number
                }
                TransactionField::From => tx.from = full.from.clone(),
                TransactionField::Gas => tx.gas = full.gas.clone(),
                TransactionField::GasPrice => {
                    tx.gas_price = full.gas_price.clone()
                }
                TransactionField::Hash => tx.hash = full.hash.clone(),
                TransactionField::Input => tx.input = full.input.clone(),
                TransactionField::Nonce => tx.nonce = full.nonce.clone(),
                TransactionField::To => tx.to = full.to.clone(),
                TransactionField::TransactionIndex => {
                    tx.transaction_index = full.transaction_index
                }
                TransactionField::Value => tx.value = full.value.clone(),
                TransactionField::MaxPriorityFeePerGas => {
                    tx.max_priority_fee_per_gas =
                        full.max_priority_fee_per_gas.clone()
                }
                TransactionField::MaxFeePerGas => {
                    tx.max_fee_per_gas = full.max_fee_per_gas.clone()
                }
                TransactionField::AccessList => {
                    tx.access_list = full.access_list.clone()
                }
                TransactionField::CumulativeGasUsed => {
                    tx.cumulative_gas_used =
                        full.cumulative_gas_used.clone()
                }
                TransactionField::EffectiveGasPrice => {
                    tx.effective_gas_price =
                        full.effective_gas_price.clone()
                }
                TransactionField::GasUsed => {
                    tx.gas_used = full.gas_used.clone()
                }
                TransactionField::ContractAddress => {
                    tx.contract_address = full.contract_address.clone()
                }
                TransactionField::Type => tx.type_ = full.type_,
                TransactionField::Status => tx.status = full.status,
                other => panic!("add {other:?} to project_transaction"),
            }
        }
        tx
    }

    /// An ERC721 transfer: all four topics are read.
    fn full_log() -> Log {
        let mut log = Log {
            log_index: Some(UInt::from(5u64)),
            transaction_index: Some(UInt::from(3u64)),
            transaction_hash: Some(Hash::from([0x24; 32])),
            block_number: Some(UInt::from(BLOCK)),
            address: Some(HsAddress::from([0x41; 20])),
            data: Some(Data::from(vec![0x42; 32])),
            ..Default::default()
        };
        log.topics.push(Some(LogArgument::from(
            crate::core::events::TRANSFER_EVENT_SIGNATURE.0,
        )));
        log.topics.push(Some(LogArgument::from([0x43; 32])));
        log.topics.push(Some(LogArgument::from([0x44; 32])));
        log.topics.push(Some(LogArgument::from([0x45; 32])));
        log
    }

    fn project_log(full: &Log, fields: &[LogField]) -> Log {
        let mut log = Log::default();
        for _ in 0..4 {
            log.topics.push(None);
        }
        for field in fields {
            match field {
                LogField::LogIndex => log.log_index = full.log_index,
                LogField::TransactionIndex => {
                    log.transaction_index = full.transaction_index
                }
                LogField::TransactionHash => {
                    log.transaction_hash = full.transaction_hash.clone()
                }
                LogField::BlockNumber => {
                    log.block_number = full.block_number
                }
                LogField::Address => log.address = full.address.clone(),
                LogField::Data => log.data = full.data.clone(),
                LogField::Topic0 => log.topics[0] = full.topics[0].clone(),
                LogField::Topic1 => log.topics[1] = full.topics[1].clone(),
                LogField::Topic2 => log.topics[2] = full.topics[2].clone(),
                LogField::Topic3 => log.topics[3] = full.topics[3].clone(),
                other => panic!("add {other:?} to project_log"),
            }
        }
        log
    }

    /// Rows produced from a response reduced to the given selection, as a
    /// comparable string (`Err` included: a missing identity field fails).
    fn rows_for(
        blocks: &[BlockField],
        transactions: &[TransactionField],
        logs: &[LogField],
    ) -> String {
        let data = ResponseRows {
            blocks: vec![vec![project_block(&full_block(), blocks)]],
            transactions: vec![vec![project_transaction(
                &full_transaction(),
                transactions,
            )]],
            logs: vec![vec![project_log(&full_log(), logs)]],
        };

        match transform(1, &data, BlockRange::new(BLOCK, BLOCK + 1)) {
            Ok(transformed) => format!("{:?}", transformed.rows),
            Err(error) => format!("error: {error:#}"),
        }
    }

    fn without<T: PartialEq + Copy>(fields: &[T], dropped: T) -> Vec<T> {
        fields.iter().copied().filter(|f| *f != dropped).collect()
    }

    #[test]
    fn selection_is_exactly_what_the_rows_need() {
        let full = ResponseRows {
            blocks: vec![vec![full_block()]],
            transactions: vec![vec![full_transaction()]],
            logs: vec![vec![full_log()]],
        };
        let expected = format!(
            "{:?}",
            transform(1, &full, BlockRange::new(BLOCK, BLOCK + 1))
                .unwrap()
                .rows
        );

        // 1. Everything the models read is selected.
        let selected =
            rows_for(&BLOCK_FIELDS, &TRANSACTION_FIELDS, &LOG_FIELDS);
        assert!(
            selected == expected,
            "a field the models read is missing"
        );

        // 2. Nothing is selected that no row needs.
        for field in BLOCK_FIELDS {
            let rows = rows_for(
                &without(&BLOCK_FIELDS, field),
                &TRANSACTION_FIELDS,
                &LOG_FIELDS,
            );
            assert!(rows != expected, "{field:?} is selected but unused");
        }
        for field in TRANSACTION_FIELDS {
            let rows = rows_for(
                &BLOCK_FIELDS,
                &without(&TRANSACTION_FIELDS, field),
                &LOG_FIELDS,
            );
            assert!(rows != expected, "{field:?} is selected but unused");
        }
        for field in LOG_FIELDS {
            let rows = rows_for(
                &BLOCK_FIELDS,
                &TRANSACTION_FIELDS,
                &without(&LOG_FIELDS, field),
            );
            assert!(rows != expected, "{field:?} is selected but unused");
        }
    }
}
