//! HyperSync data source: the only place that talks to HyperSync.
//!
//! One generic query (every block, transaction and log, plus traces when
//! enabled) streamed over a bounded block range. No chain specific logic:
//! the endpoint is derived from the chain id unless a url is given.

use crate::{
    db::ranges::BlockRange,
    pipeline::{transform::ResponseRows, BlockSource, SourceResponse},
};
use anyhow::{bail, Context, Result};
use hypersync_client::{
    net_types::{
        BlockField, LogField, LogFilter, Query, TraceField, TraceFilter,
        TransactionField, TransactionFilter,
    },
    Client, StreamConfig,
};
use log::{info, warn};
use tokio::sync::mpsc::{self, Receiver};

/// Block header fields needed by `DatabaseBlock` and the withdrawals.
const BLOCK_FIELDS: [BlockField; 22] = [
    BlockField::Number,
    BlockField::Hash,
    BlockField::ParentHash,
    BlockField::Nonce,
    BlockField::Sha3Uncles,
    BlockField::LogsBloom,
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

const TRACE_FIELDS: [TraceField; 24] = [
    TraceField::From,
    TraceField::To,
    TraceField::CallType,
    TraceField::Gas,
    TraceField::Input,
    TraceField::Init,
    TraceField::Value,
    TraceField::Author,
    TraceField::RewardType,
    TraceField::BlockHash,
    TraceField::BlockNumber,
    TraceField::Address,
    TraceField::Code,
    TraceField::GasUsed,
    TraceField::Output,
    TraceField::Subtraces,
    TraceField::TraceAddress,
    TraceField::TransactionHash,
    TraceField::TransactionPosition,
    TraceField::Type,
    TraceField::Error,
    TraceField::ActionAddress,
    TraceField::Balance,
    TraceField::RefundAddress,
];

/// Builds the query for `[range.from, range.to)`.
pub fn build_query(range: BlockRange, traces: bool) -> Query {
    let mut query = Query::new()
        .from_block(range.from)
        .to_block_excl(range.to)
        // Blocks without transactions must be returned too: the block row
        // is what marks a block as indexed.
        .include_all_blocks()
        .where_transactions(TransactionFilter::all())
        .where_logs(LogFilter::all())
        .select_block_fields(BLOCK_FIELDS)
        .select_transaction_fields(TRANSACTION_FIELDS)
        .select_log_fields(LOG_FIELDS);

    if traces {
        query = query
            .where_traces(TraceFilter::all())
            .select_trace_fields(TRACE_FIELDS);
    }

    query
}

#[derive(Clone)]
pub struct Source {
    client: Client,
    traces: bool,
}

impl Source {
    pub fn new(
        chain_id: u64,
        url: Option<&str>,
        api_token: &str,
        traces: bool,
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

        Ok(Self { client, traces })
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
            .stream(
                build_query(range, self.traces),
                StreamConfig::default(),
            )
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
                        traces: response.data.traces,
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

    #[test]
    fn query_covers_everything_in_a_bounded_range() {
        let query = build_query(BlockRange::new(100, 200), false);

        assert_eq!(query.from_block, 100);
        assert_eq!(query.to_block, Some(200));
        assert!(query.include_all_blocks);
        assert_eq!(query.transactions.len(), 1);
        assert_eq!(query.logs.len(), 1);
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
    fn traces_are_only_requested_when_enabled() {
        let query = build_query(BlockRange::new(0, 10), true);

        assert_eq!(query.traces.len(), 1);
        assert_eq!(query.field_selection.trace.len(), TRACE_FIELDS.len());
    }

    #[test]
    fn malformed_token_is_an_error_not_a_panic() {
        assert!(Source::new(1, None, "not-a-uuid", false).is_err());
    }
}
