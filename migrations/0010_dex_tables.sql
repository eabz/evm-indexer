-- DEX base tables and their read-path side tables (docs/design.md §5).
--
-- Binary column types throughout: addresses FixedString(20), hashes and
-- pool ids FixedString(32), amounts UInt256 / Int256. Readers format with
-- concat('0x', lower(hex(x))) and query base tables with FINAL.
--
-- Sign convention of amount0 / amount1 everywhere: pool relative,
-- positive = INTO the pool, negative = out of the pool.

-- One row per pool. NOT partitioned by time: the ReplacingMergeTree must be
-- able to collapse an 'rpc' row and an 'event' row of the same pool.
-- _version is NOT the flush time here: event rows carry a version that
-- DEcreases with (created_block, log_index) so the FIRST creation event
-- wins, 'rpc' rows carry 1 and 'unresolved' rows carry 0.
-- Block scoped through created_block (0 for rpc rows: never purged).
CREATE TABLE IF NOT EXISTS dex_pools (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(20),
  factory FixedString(20),
  protocol LowCardinality(String),
  token0 FixedString(20),
  token1 FixedString(20),
  tokens Array(FixedString(20)),
  underlying_tokens Array(FixedString(20)),
  fee UInt32,
  tick_spacing Int32,
  hooks FixedString(20),
  stable Bool,
  created_block UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  transaction_hash FixedString(32),
  log_index UInt32,
  source LowCardinality(String),
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, pool_id, emitter)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS dex_swaps (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  transaction_hash FixedString(32),
  log_index UInt32,
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  sender FixedString(20),
  recipient FixedString(20),
  tx_from FixedString(20),
  tx_to FixedString(20),
  trader FixedString(20),
  amount0 Int256,
  amount1 Int256,
  token_in FixedString(20),
  token_out FixedString(20),
  amount_in UInt256,
  amount_out UInt256,
  coin_in UInt8,
  coin_out UInt8,
  underlying Bool,
  sqrt_price_x96 UInt256,
  liquidity UInt256,
  tick Int32,
  fee UInt32,
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS dex_liquidity (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  transaction_hash FixedString(32),
  log_index UInt32,
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  kind LowCardinality(String),
  sender FixedString(20),
  owner FixedString(20),
  tx_from FixedString(20),
  tx_to FixedString(20),
  amount0 Int256,
  amount1 Int256,
  reserve0 UInt256,
  reserve1 UInt256,
  liquidity_delta Int256,
  tick_lower Int32,
  tick_upper Int32,
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- User populated, never written by the indexer. kind: 'stable' (valued at
-- 1 USD) or 'native' (priced from native/stable pools), '' retires the row.
-- decimals / symbol are only used when the token has no row in tokens
-- (native pseudo addresses: the zero address of Uniswap V4, 0xEeee...EEeE of
-- Curve). decimals is Nullable on purpose: NULL means "take it from tokens",
-- and a token with no decimals anywhere is unpriceable (NULL, never a guess).
CREATE TABLE IF NOT EXISTS quote_tokens (
  chain UInt64,
  token FixedString(20),
  kind LowCardinality(String),
  decimals Nullable(UInt8),
  symbol String DEFAULT '',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, token);

-- Read path: swaps of a pool.
CREATE TABLE IF NOT EXISTS dex_swaps_by_pool (
  chain UInt64,
  pool_id FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  emitter FixedString(20),
  protocol LowCardinality(String),
  transaction_hash FixedString(32),
  trader FixedString(20),
  amount0 Int256,
  amount1 Int256,
  token_in FixedString(20),
  token_out FixedString(20),
  amount_in UInt256,
  amount_out UInt256,
  coin_in UInt8,
  coin_out UInt8,
  underlying Bool,
  sqrt_price_x96 UInt256,
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, pool_id, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_pool_mv
TO dex_swaps_by_pool AS
SELECT
  chain, pool_id, block_number, log_index, timestamp, emitter, protocol,
  transaction_hash, trader, amount0, amount1, token_in, token_out,
  amount_in, amount_out, coin_in, coin_out, underlying, sqrt_price_x96,
  _version
FROM dex_swaps;

-- Read path: swaps of a trader (tx sender when known, see dex_swaps.trader).
CREATE TABLE IF NOT EXISTS dex_swaps_by_trader (
  chain UInt64,
  trader FixedString(20),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  transaction_hash FixedString(32),
  tx_to FixedString(20),
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, trader, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_trader_mv
TO dex_swaps_by_trader AS
SELECT
  chain, trader, block_number, log_index, timestamp, pool_id, emitter,
  protocol, transaction_hash, tx_to, _version
FROM dex_swaps;

-- Read path: pools of a token. block_number is the pool's created_block so
-- the table is purged like every other block scoped table.
CREATE TABLE IF NOT EXISTS dex_pools_by_token (
  chain UInt64,
  token FixedString(20),
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  block_number UInt64 CODEC(Delta, ZSTD),
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, token, pool_id, emitter)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_pools_by_token_mv
TO dex_pools_by_token AS
SELECT
  chain,
  arrayJoin(arrayDistinct(arrayConcat(tokens, underlying_tokens))) AS token,
  pool_id, emitter, protocol, created_block AS block_number, _version
FROM dex_pools
WHERE source != 'unresolved';
