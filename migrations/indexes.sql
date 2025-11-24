-- ============================================================================
-- ClickHouse Database Optimizations for EVM Indexer API
-- ============================================================================
-- This file contains recommended indexes and materialized views to optimize
-- read queries while minimizing impact on write performance.
--
-- IMPORTANT: Test these optimizations in a staging environment before
-- applying to production. Monitor write performance after applying changes.
-- ============================================================================

-- ============================================================================
-- SECONDARY INDEXES (Data Skipping Indexes)
-- ============================================================================
-- ClickHouse uses data skipping indexes to skip granules that don't match
-- the query conditions. These have minimal impact on write performance.
-- ============================================================================

-- Blocks table indexes
ALTER TABLE indexer.blocks ADD INDEX IF NOT EXISTS idx_blocks_miner miner TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.blocks ADD INDEX IF NOT EXISTS idx_blocks_hash hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.blocks ADD INDEX IF NOT EXISTS idx_blocks_timestamp timestamp TYPE minmax GRANULARITY 4;

-- Transactions table indexes
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_hash hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_from `from` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_to `to` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_method method TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_status status TYPE set(10) GRANULARITY 4;
ALTER TABLE indexer.transactions ADD INDEX IF NOT EXISTS idx_tx_timestamp timestamp TYPE minmax GRANULARITY 4;

-- Logs table indexes
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_address address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_topic0 topic0 TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_topic1 topic1 TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_topic2 topic2 TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_topic3 topic3 TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.logs ADD INDEX IF NOT EXISTS idx_logs_timestamp timestamp TYPE minmax GRANULARITY 4;

-- ERC20 transfers indexes
ALTER TABLE indexer.erc20_transfers ADD INDEX IF NOT EXISTS idx_erc20_token token_address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc20_transfers ADD INDEX IF NOT EXISTS idx_erc20_from `from` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc20_transfers ADD INDEX IF NOT EXISTS idx_erc20_to `to` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc20_transfers ADD INDEX IF NOT EXISTS idx_erc20_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.erc20_transfers ADD INDEX IF NOT EXISTS idx_erc20_timestamp timestamp TYPE minmax GRANULARITY 4;

-- ERC721 transfers indexes
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_token token_address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_from `from` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_to `to` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_id id TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.erc721_transfers ADD INDEX IF NOT EXISTS idx_erc721_timestamp timestamp TYPE minmax GRANULARITY 4;

-- ERC1155 transfers indexes
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_token token_address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_from `from` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_to `to` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_operator operator TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.erc1155_transfers ADD INDEX IF NOT EXISTS idx_erc1155_timestamp timestamp TYPE minmax GRANULARITY 4;

-- Contracts table indexes
ALTER TABLE indexer.contracts ADD INDEX IF NOT EXISTS idx_contracts_address contract_address TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.contracts ADD INDEX IF NOT EXISTS idx_contracts_creator creator TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.contracts ADD INDEX IF NOT EXISTS idx_contracts_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;

-- Traces table indexes
ALTER TABLE indexer.traces ADD INDEX IF NOT EXISTS idx_traces_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.traces ADD INDEX IF NOT EXISTS idx_traces_from `from` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.traces ADD INDEX IF NOT EXISTS idx_traces_to `to` TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.traces ADD INDEX IF NOT EXISTS idx_traces_action_type action_type TYPE set(10) GRANULARITY 4;
ALTER TABLE indexer.traces ADD INDEX IF NOT EXISTS idx_traces_call_type call_type TYPE set(10) GRANULARITY 4;

-- Withdrawals table indexes
ALTER TABLE indexer.withdrawals ADD INDEX IF NOT EXISTS idx_withdrawals_address address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.withdrawals ADD INDEX IF NOT EXISTS idx_withdrawals_validator validator_index TYPE minmax GRANULARITY 4;
ALTER TABLE indexer.withdrawals ADD INDEX IF NOT EXISTS idx_withdrawals_timestamp timestamp TYPE minmax GRANULARITY 4;

-- DEX trades table indexes
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_pool pool_address TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_sender sender TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_recipient recipient TYPE bloom_filter GRANULARITY 4;
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_tx_hash transaction_hash TYPE bloom_filter GRANULARITY 1;
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_name dex_name TYPE set(20) GRANULARITY 4;
ALTER TABLE indexer.dex_trades ADD INDEX IF NOT EXISTS idx_dex_timestamp timestamp TYPE minmax GRANULARITY 4;

-- Tokens table indexes
ALTER TABLE indexer.tokens ADD INDEX IF NOT EXISTS idx_tokens_name name TYPE tokenbf_v1(10240, 3, 0) GRANULARITY 4;
ALTER TABLE indexer.tokens ADD INDEX IF NOT EXISTS idx_tokens_symbol symbol TYPE tokenbf_v1(10240, 3, 0) GRANULARITY 4;
ALTER TABLE indexer.tokens ADD INDEX IF NOT EXISTS idx_tokens_type type TYPE set(10) GRANULARITY 4;


-- ============================================================================
-- MATERIALIZED VIEWS FOR COMMON AGGREGATIONS
-- ============================================================================
-- These views pre-compute aggregations that would be expensive to calculate
-- on-the-fly. They update automatically as data is inserted.
-- ============================================================================

-- Daily block statistics per chain
CREATE MATERIALIZED VIEW IF NOT EXISTS indexer.mv_daily_block_stats
ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(date)
ORDER BY (chain, date)
AS SELECT
    chain,
    toDate(timestamp) AS date,
    count() AS block_count,
    sum(transactions) AS transaction_count,
    sum(gas_used) AS total_gas_used,
    max(number) AS max_block_number,
    min(number) AS min_block_number
FROM indexer.blocks
GROUP BY chain, toDate(timestamp);

-- Daily transaction statistics per chain
CREATE MATERIALIZED VIEW IF NOT EXISTS indexer.mv_daily_tx_stats
ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(date)
ORDER BY (chain, date)
AS SELECT
    chain,
    toDate(timestamp) AS date,
    count() AS tx_count,
    countIf(status = '0x1') AS successful_tx_count,
    countIf(status = '0x0') AS failed_tx_count,
    sum(gas_used) AS total_gas_used
FROM indexer.transactions
GROUP BY chain, toDate(timestamp);

-- Daily ERC20 transfer volume per token
CREATE MATERIALIZED VIEW IF NOT EXISTS indexer.mv_daily_erc20_volume
ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(date)
ORDER BY (chain, token_address, date)
AS SELECT
    chain,
    token_address,
    toDate(timestamp) AS date,
    count() AS transfer_count,
    uniqExact(`from`) AS unique_senders,
    uniqExact(`to`) AS unique_receivers
FROM indexer.erc20_transfers
GROUP BY chain, token_address, toDate(timestamp);

-- Daily DEX trade statistics per pool
CREATE MATERIALIZED VIEW IF NOT EXISTS indexer.mv_daily_dex_stats
ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(date)
ORDER BY (chain, dex_name, pool_address, date)
AS SELECT
    chain,
    dex_name,
    pool_address,
    toDate(timestamp) AS date,
    count() AS trade_count,
    uniqExact(sender) AS unique_traders
FROM indexer.dex_trades
GROUP BY chain, dex_name, pool_address, toDate(timestamp);

-- Contract deployment statistics per chain per day
CREATE MATERIALIZED VIEW IF NOT EXISTS indexer.mv_daily_contract_deployments
ENGINE = SummingMergeTree()
PARTITION BY toYYYYMM(date)
ORDER BY (chain, date)
AS SELECT
    chain,
    toDate(fromUnixTimestamp(block_number)) AS date,
    count() AS contracts_deployed,
    uniqExact(creator) AS unique_deployers
FROM indexer.contracts
GROUP BY chain, toDate(fromUnixTimestamp(block_number));


-- ============================================================================
-- PROJECTION RECOMMENDATIONS
-- ============================================================================
-- Projections store data in different sort orders for faster queries.
-- They have moderate write overhead but significant read benefits.
-- Uncomment if you need these optimizations and can accept the write cost.
-- ============================================================================

-- Projection for transaction lookups by hash (very common query)
-- ALTER TABLE indexer.transactions ADD PROJECTION IF NOT EXISTS proj_tx_by_hash (
--     SELECT * ORDER BY hash
-- );

-- Projection for log lookups by address and topic0 (eth_getLogs pattern)
-- ALTER TABLE indexer.logs ADD PROJECTION IF NOT EXISTS proj_logs_by_address (
--     SELECT * ORDER BY (chain, address, topic0, block_number)
-- );

-- Projection for transfer lookups by token and address
-- ALTER TABLE indexer.erc20_transfers ADD PROJECTION IF NOT EXISTS proj_erc20_by_token (
--     SELECT * ORDER BY (chain, token_address, block_number)
-- );

-- Projection for traces by transaction hash
-- ALTER TABLE indexer.traces ADD PROJECTION IF NOT EXISTS proj_traces_by_tx (
--     SELECT * ORDER BY (chain, transaction_hash, block_number)
-- );


-- ============================================================================
-- PERFORMANCE TUNING SETTINGS
-- ============================================================================
-- These settings can be applied at the table or query level for optimization.
-- ============================================================================

-- Enable lightweight deletes for better update performance (if needed)
-- ALTER TABLE indexer.blocks MODIFY SETTING enable_mixed_granularity_parts = 1;

-- Optimize for read-heavy workloads
-- ALTER TABLE indexer.transactions MODIFY SETTING merge_with_ttl_timeout = 86400;

-- ============================================================================
-- MAINTENANCE QUERIES
-- ============================================================================
-- Run these periodically to maintain performance
-- ============================================================================

-- Force merge of small parts (run during low-traffic periods)
-- OPTIMIZE TABLE indexer.blocks FINAL;
-- OPTIMIZE TABLE indexer.transactions FINAL;
-- OPTIMIZE TABLE indexer.logs FINAL;
-- OPTIMIZE TABLE indexer.erc20_transfers FINAL;
-- OPTIMIZE TABLE indexer.erc721_transfers FINAL;
-- OPTIMIZE TABLE indexer.erc1155_transfers FINAL;
-- OPTIMIZE TABLE indexer.traces FINAL;
-- OPTIMIZE TABLE indexer.contracts FINAL;
-- OPTIMIZE TABLE indexer.withdrawals FINAL;
-- OPTIMIZE TABLE indexer.dex_trades FINAL;
-- OPTIMIZE TABLE indexer.tokens FINAL;

-- Materialize indexes after adding them (if index was added after data)
-- ALTER TABLE indexer.blocks MATERIALIZE INDEX idx_blocks_miner;
-- ALTER TABLE indexer.transactions MATERIALIZE INDEX idx_tx_hash;
-- (repeat for other indexes as needed)
