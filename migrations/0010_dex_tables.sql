-- DEX base tables and their read-path side tables (docs/design.md §1, §2, §5).
--
-- Binary column types throughout: addresses FixedString(20), hashes and
-- pool ids FixedString(32), amounts UInt256 / Int256. Readers format with
-- concat('0x', lower(hex(x))) and query every table here with FINAL.
--
-- Sign convention of amount0 / amount1 everywhere: pool relative,
-- positive = INTO the pool, negative = out of the pool.
--
-- Insert-only reorg support (docs/design.md §2): nothing is ever deleted.
-- Every block scoped table is ReplacingMergeTree(_version, is_deleted), a
-- purge INSERTs tombstones (the row again with a newer _version and
-- is_deleted = 1) and FINAL hides them. Side tables are fed by materialized
-- views that pass _version, is_deleted and epoch through, so a tombstone on
-- a base table tombstones its side table rows by itself. epoch is the
-- chain's purge generation, stamped by the writer - the aggregates of
-- 0011 are keyed by it.
--
-- Partitioning (50+ chains share one database): base tables by month only,
-- never by chain - chain is the first sorting key column. Side tables and
-- dex_pools by chain (lookups must not fan out per month). A tombstone
-- copies its row, so it always lands in the partition of the row it kills.

-- One row per CREATION EVENT of a pool (plus at most one row written by
-- the RPC resolver), positional like every other block scoped table: a
-- re-inserted block replaces itself, a reorged-out creation is tombstoned
-- by created_block, a re-creation on the canonical chain is simply
-- another (or a newer) row. Several live rows of one pool are possible -
-- forged PairCreated events cost one transaction - so readers never pick
-- "a" row, they pick THE row through dex_pool_current_v below: creation
-- events before rpc rows, then the earliest (created_block, log_index).
-- The first creation event wins, a later forgery can not replace it.
-- Rows of the RPC resolver (source 'rpc' / 'unresolved') have
-- created_block = 0 and log_index = 0: no purge range ever contains them,
-- pool metadata read from the chain state does not depend on the fork.
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
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, pool_id, emitter, created_block, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- THE row of every pool (see dex_pools). Negative resolver results
-- ('unresolved') are not pools.
CREATE VIEW IF NOT EXISTS dex_pool_current_v AS
SELECT
  chain, pool_id, emitter,
  tupleElement(best, 1) AS factory,
  tupleElement(best, 2) AS protocol,
  tupleElement(best, 3) AS token0,
  tupleElement(best, 4) AS token1,
  tupleElement(best, 5) AS tokens,
  tupleElement(best, 6) AS underlying_tokens,
  tupleElement(best, 7) AS fee,
  tupleElement(best, 8) AS tick_spacing,
  tupleElement(best, 9) AS hooks,
  tupleElement(best, 10) AS stable,
  tupleElement(best, 11) AS created_block,
  tupleElement(best, 12) AS timestamp,
  tupleElement(best, 13) AS transaction_hash,
  tupleElement(best, 14) AS log_index,
  tupleElement(best, 15) AS source,
  candidates
FROM
(
  SELECT
    chain, pool_id, emitter,
    argMin((factory, toString(protocol), token0, token1, tokens, underlying_tokens, fee, tick_spacing, hooks, stable, created_block, timestamp, transaction_hash, log_index, toString(source)), (source != 'event', created_block, log_index)) AS best,
    count() AS candidates
  FROM dex_pools FINAL
  WHERE source != 'unresolved'
  GROUP BY chain, pool_id, emitter
);

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
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- tx_from / tx_to are the sender and the target of the TRANSACTION. Who
-- seeded a pool is dex_liquidity.tx_from - the event sender is usually a
-- router and must not be used for attribution.
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
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- User populated, never written by the indexer, not block scoped. kind:
-- 'stable' (valued at 1 USD) or 'native' (priced from native/stable pools),
-- '' retires the row. decimals / symbol are only used when the token has no
-- row in tokens (native pseudo addresses: the zero address of Uniswap V4,
-- 0xEeee...EEeE of Curve). decimals is Nullable on purpose: NULL means "take
-- it from tokens", and a token with no decimals anywhere is unpriceable
-- (NULL, never a guess).
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
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, pool_id, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_pool_mv
TO dex_swaps_by_pool AS
SELECT
  chain, pool_id, block_number, log_index, timestamp, emitter, protocol,
  transaction_hash, trader, amount0, amount1, token_in, token_out,
  amount_in, amount_out, coin_in, coin_out, underlying, sqrt_price_x96,
  epoch, _version, is_deleted
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
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, trader, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_trader_mv
TO dex_swaps_by_trader AS
SELECT
  chain, trader, block_number, log_index, timestamp, pool_id, emitter,
  protocol, transaction_hash, tx_to, epoch, _version, is_deleted
FROM dex_swaps;

-- Read path: pools of a token, one row per (token, dex_pools row).
-- block_number / log_index are the position of the creation event (0 / 0
-- for resolver rows), so the key mirrors dex_pools and tombstones match.
CREATE TABLE IF NOT EXISTS dex_pools_by_token (
  chain UInt64,
  token FixedString(20),
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  source LowCardinality(String),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, token, pool_id, emitter, block_number, log_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_pools_by_token_mv
TO dex_pools_by_token AS
SELECT
  chain,
  arrayJoin(arrayDistinct(arrayConcat(tokens, underlying_tokens))) AS token,
  pool_id, emitter, protocol, created_block AS block_number, log_index,
  source, epoch, _version, is_deleted
FROM dex_pools
WHERE source != 'unresolved';
