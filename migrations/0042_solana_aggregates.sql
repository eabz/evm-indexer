-- Solana candle aggregates and the deduplication windows of the sol_*
-- tables. Reserved range 0040-0049 (docs/design.md §14).
--
-- WHY THIS MIGRATION EXISTS AT ALL. 0040 / 0041 created the sol_* tables
-- but nothing aggregates them and nothing protects them from a retried
-- insert, because phase 1 had no pipeline writing them. `indexer run
-- --chain solana` does, so both are needed now:
--
--   * the candles are what a chart reads, and they are the thing a purge
--     has to repair (an epoch-keyed AggregatingMergeTree, never a DELETE);
--   * `non_replicated_deduplication_window` is what makes the flush's
--     `insert_deduplication_token` actually drop a retried insert - on the
--     table AND on everything its materialized views feed. Verified on
--     25.12: with the window on the base table only, the base table
--     deduplicates and the aggregates still count the rows twice.
--
-- Shape follows the DEX candles of 0011 exactly (same bucket arithmetic,
-- same epoch/validity rule, same *_v reader views joining the SHARED
-- `epoch_floor_v` of 0004), with two deliberate differences:
--
--   1. The series is keyed by (pool_id, venue_program) rather than
--      (pool_id, emitter). `venue_program` is Solana's emitter: the program
--      that executed the fill. Two venues can and do quote the same mint
--      pair, and a router splitting a fill across them must not merge into
--      one candle.
--   2. There is no sqrt-price series. `sol_dex_swaps` has no
--      `sqrt_price_x96`; the pool series therefore comes from
--      reserve0/reserve1, which the per-program decoders fill for the
--      venues that publish pool state and leave at 0 for the rest. A
--      `movement`-only row contributes to the TRADE series and not to the
--      pool one, which is exactly right: its price is exact, its pool
--      state is unknown.
--
-- Prices are token1 per token0 in RAW units (not decimals adjusted), like
-- the EVM candles; `sol_tokens.decimals` is what a reader scales with.
-- Volumes are Float64 sums on purpose (docs/design.md, the 256-bit rule):
-- sum() over Int256 wraps silently and a spam mint emits amounts near
-- 2^256. The exact amounts stay in `sol_dex_swaps`.
--
-- Never read the tables below directly: only their *_v views apply the
-- validity rule, and only they are correct.

CREATE TABLE IF NOT EXISTS sol_dex_candles_1m (
  chain UInt64,
  pool_id FixedString(32),
  venue_program FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  high SimpleAggregateFunction(max, Nullable(Float64)),
  low SimpleAggregateFunction(min, Nullable(Float64)),
  trades SimpleAggregateFunction(sum, UInt64),
  pool_open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_high SimpleAggregateFunction(max, Nullable(Float64)),
  pool_low SimpleAggregateFunction(min, Nullable(Float64)),
  pool_prices SimpleAggregateFunction(sum, UInt64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, venue_program, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS sol_dex_candles_1m_mv
TO sol_dex_candles_1m AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, reserve0 != 0 AND reserve1 != 0 AS pool_ok, toFloat64(reserve1) / toFloat64(reserve0) AS pool_price
SELECT
  chain, pool_id, venue_program,
  toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS bucket,
  epoch,
  argMinStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS open,
  argMaxStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS close,
  max(if(trade_ok, trade_price, NULL)) AS high,
  min(if(trade_ok, trade_price, NULL)) AS low,
  countIf(trade_ok) AS trades,
  argMinStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_open,
  argMaxStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_close,
  max(if(pool_ok, pool_price, NULL)) AS pool_high,
  min(if(pool_ok, pool_price, NULL)) AS pool_low,
  countIf(pool_ok) AS pool_prices,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM sol_dex_swaps
WHERE is_deleted = 0
GROUP BY chain, pool_id, venue_program, bucket, epoch;

CREATE TABLE IF NOT EXISTS sol_dex_candles_1h (
  chain UInt64,
  pool_id FixedString(32),
  venue_program FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  high SimpleAggregateFunction(max, Nullable(Float64)),
  low SimpleAggregateFunction(min, Nullable(Float64)),
  trades SimpleAggregateFunction(sum, UInt64),
  pool_open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_high SimpleAggregateFunction(max, Nullable(Float64)),
  pool_low SimpleAggregateFunction(min, Nullable(Float64)),
  pool_prices SimpleAggregateFunction(sum, UInt64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, venue_program, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS sol_dex_candles_1h_mv
TO sol_dex_candles_1h AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, reserve0 != 0 AND reserve1 != 0 AS pool_ok, toFloat64(reserve1) / toFloat64(reserve0) AS pool_price
SELECT
  chain, pool_id, venue_program,
  toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,
  epoch,
  argMinStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS open,
  argMaxStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS close,
  max(if(trade_ok, trade_price, NULL)) AS high,
  min(if(trade_ok, trade_price, NULL)) AS low,
  countIf(trade_ok) AS trades,
  argMinStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_open,
  argMaxStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_close,
  max(if(pool_ok, pool_price, NULL)) AS pool_high,
  min(if(pool_ok, pool_price, NULL)) AS pool_low,
  countIf(pool_ok) AS pool_prices,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM sol_dex_swaps
WHERE is_deleted = 0
GROUP BY chain, pool_id, venue_program, bucket, epoch;

CREATE TABLE IF NOT EXISTS sol_dex_candles_1d (
  chain UInt64,
  pool_id FixedString(32),
  venue_program FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  high SimpleAggregateFunction(max, Nullable(Float64)),
  low SimpleAggregateFunction(min, Nullable(Float64)),
  trades SimpleAggregateFunction(sum, UInt64),
  pool_open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  pool_high SimpleAggregateFunction(max, Nullable(Float64)),
  pool_low SimpleAggregateFunction(min, Nullable(Float64)),
  pool_prices SimpleAggregateFunction(sum, UInt64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, venue_program, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS sol_dex_candles_1d_mv
TO sol_dex_candles_1d AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, reserve0 != 0 AND reserve1 != 0 AS pool_ok, toFloat64(reserve1) / toFloat64(reserve0) AS pool_price
SELECT
  chain, pool_id, venue_program,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  argMinStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS open,
  argMaxStateIf(trade_price, (block_number, tx_index, ordinal), trade_ok) AS close,
  max(if(trade_ok, trade_price, NULL)) AS high,
  min(if(trade_ok, trade_price, NULL)) AS low,
  countIf(trade_ok) AS trades,
  argMinStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_open,
  argMaxStateIf(pool_price, (block_number, tx_index, ordinal), pool_ok) AS pool_close,
  max(if(pool_ok, pool_price, NULL)) AS pool_high,
  min(if(pool_ok, pool_price, NULL)) AS pool_low,
  countIf(pool_ok) AS pool_prices,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM sol_dex_swaps
WHERE is_deleted = 0
GROUP BY chain, pool_id, venue_program, bucket, epoch;

-- Reader views. They apply the validity rule of docs/design.md §2 BEFORE
-- merging the aggregate states, against the shared `epoch_floor_v` of
-- migration 0004, exactly like the DEX views of 0011.
--
-- ifNull(epoch_floor, 0): a chain that never had a purge has no row to
-- join, and under `join_use_nulls = 1` (a per-user setting a BI tool may
-- set) a bare comparison would silently drop EVERY row.

CREATE VIEW IF NOT EXISTS sol_dex_candles_1m_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.venue_program AS venue_program, a.bucket AS bucket,
  if(sum(a.trades) > 0, argMinIfMerge(a.open), NULL) AS open,
  toNullable(toFloat64(max(a.high))) AS high,
  toNullable(toFloat64(min(a.low))) AS low,
  if(sum(a.trades) > 0, argMaxIfMerge(a.close), NULL) AS close,
  toUInt64(sum(a.trades)) AS trades,
  if(sum(a.pool_prices) > 0, argMinIfMerge(a.pool_open), NULL) AS pool_open,
  toNullable(toFloat64(max(a.pool_high))) AS pool_high,
  toNullable(toFloat64(min(a.pool_low))) AS pool_low,
  if(sum(a.pool_prices) > 0, argMaxIfMerge(a.pool_close), NULL) AS pool_close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM sol_dex_candles_1m AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, venue_program, bucket;

CREATE VIEW IF NOT EXISTS sol_dex_candles_1h_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.venue_program AS venue_program, a.bucket AS bucket,
  if(sum(a.trades) > 0, argMinIfMerge(a.open), NULL) AS open,
  toNullable(toFloat64(max(a.high))) AS high,
  toNullable(toFloat64(min(a.low))) AS low,
  if(sum(a.trades) > 0, argMaxIfMerge(a.close), NULL) AS close,
  toUInt64(sum(a.trades)) AS trades,
  if(sum(a.pool_prices) > 0, argMinIfMerge(a.pool_open), NULL) AS pool_open,
  toNullable(toFloat64(max(a.pool_high))) AS pool_high,
  toNullable(toFloat64(min(a.pool_low))) AS pool_low,
  if(sum(a.pool_prices) > 0, argMaxIfMerge(a.pool_close), NULL) AS pool_close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM sol_dex_candles_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, venue_program, bucket;

CREATE VIEW IF NOT EXISTS sol_dex_candles_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.venue_program AS venue_program, a.bucket AS bucket,
  if(sum(a.trades) > 0, argMinIfMerge(a.open), NULL) AS open,
  toNullable(toFloat64(max(a.high))) AS high,
  toNullable(toFloat64(min(a.low))) AS low,
  if(sum(a.trades) > 0, argMaxIfMerge(a.close), NULL) AS close,
  toUInt64(sum(a.trades)) AS trades,
  if(sum(a.pool_prices) > 0, argMinIfMerge(a.pool_open), NULL) AS pool_open,
  toNullable(toFloat64(max(a.pool_high))) AS pool_high,
  toNullable(toFloat64(min(a.pool_low))) AS pool_low,
  if(sum(a.pool_prices) > 0, argMaxIfMerge(a.pool_close), NULL) AS pool_close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM sol_dex_candles_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, venue_program, bucket;

-- Deduplication windows (docs/design.md §2, "Retried inserts must not
-- double count"), for every table the Solana flush writes and every table
-- fed by a materialized view of one. `src/pipeline/dedup.rs` asserts this
-- over the embedded migrations.
--
-- `sol_tokens` is not block scoped and is written by the same flush, so it
-- carries a window too: its token is derived from the flush like every
-- other table's.
ALTER TABLE sol_slots MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_transactions MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_tokens MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_dex_swaps MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_dex_candles_1m MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_dex_candles_1h MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_dex_candles_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
