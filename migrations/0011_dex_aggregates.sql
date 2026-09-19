-- Incremental DEX aggregates (docs/design.md §1 "Aggregates", §2, §13).
--
-- Chain neutral like 0010: identity columns (pool_id, emitter, token_in,
-- token_out, and the uniq state over traders) are FixedString(32), and a
-- candle's open / close are the price at the smallest / largest POSITION of
-- the bucket - the (block_number, tx_index, ordinal) tuple of §13, not a
-- (block, log index) pair.
--
-- Every table here is declared in Rust as a DerivedTable (src/dex/derived.rs)
-- whose rebuild_sql repeats the SELECT of its materialized view - a unit
-- test compares the two texts, keep them in sync. Buckets are computed
-- arithmetically from the unix time (UTC, never the server time zone) so
-- Rust and SQL always agree on a bucket start.
--
-- Reorgs without DELETE: epochs. Every swap carries the purge generation
-- (epoch) of its chain, every aggregate is keyed by it, the views only
-- aggregate live rows (is_deleted = 0 - tombstones add nothing). A purge
-- bumps the epoch, records (chain, epoch, from_ts) in reorgs and re-inserts
-- the surviving swaps of every bucket >= from_ts under the NEW epoch
-- (rebuild_sql). Readers apply the VALIDITY RULE: a contribution with epoch e
-- in bucket b counts iff e >= the largest epoch among the chain's reorgs with
-- from_ts <= b (0 when there is none). epoch_floor_v - the SHARED view of
-- migration 0004, next to `reorgs` - turns reorgs into that step function,
-- every *_v view ASOF joins it and filters BEFORE it merges aggregate
-- states, so a stale epoch can not leak an open / close.
-- Never read the tables below directly: only their *_v views are correct.
--
-- Prices are token1 per token0 in RAW units (not decimals adjusted), see
-- the candle views below. Float64 keeps ~15.9 significant digits: relative error of the price is
-- below 1e-15, amounts above 2^53 raw units lose their low digits.
-- Volumes are Float64 sums on purpose (docs/design.md, 256-bit arithmetic
-- rule): sum() over UInt256 / Int256 wraps silently, and spam tokens emit
-- amounts near 2^256. Exact amounts stay in dex_swaps.
-- Multi asset families (Balancer, Curve) have no token0 / token1 and
-- therefore no candles. Their volume is in dex_pool_volume_1h.

-- The validity rule as a step function - for every (chain, from_ts) the
-- largest epoch of all reorgs of the chain starting at or before from_ts -
-- is `epoch_floor_v`, created by migration 0004 next to `reorgs` itself
-- (docs/design.md section 1, "Aggregates"). This module used to carry a
-- byte-for-byte equivalent copy of it, `dex_epoch_floor_v`; the copy is
-- gone and every view below joins the shared one, exactly like the
-- prediction and launchpad views do. 0004 runs before 0011, so the
-- dependency order holds. The copy only differed by a
-- `toDateTime(from_ts, 'UTC')` that was a no-op: `reorgs.from_ts` is
-- already `DateTime('UTC')`.

CREATE TABLE IF NOT EXISTS dex_candles_1m (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
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
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1m_mv
TO dex_candles_1m AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, sqrt_price_x96 != 0 OR (reserve0 != 0 AND reserve1 != 0) AS pool_ok, if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), toFloat64(reserve1) / toFloat64(reserve0)) AS pool_price
SELECT
  chain, pool_id, emitter,
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
FROM dex_swaps
WHERE is_deleted = 0 AND protocol NOT IN ('balancer_v2', 'curve')
GROUP BY chain, pool_id, emitter, bucket, epoch;

CREATE TABLE IF NOT EXISTS dex_candles_1h (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
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
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1h_mv
TO dex_candles_1h AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, sqrt_price_x96 != 0 OR (reserve0 != 0 AND reserve1 != 0) AS pool_ok, if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), toFloat64(reserve1) / toFloat64(reserve0)) AS pool_price
SELECT
  chain, pool_id, emitter,
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
FROM dex_swaps
WHERE is_deleted = 0 AND protocol NOT IN ('balancer_v2', 'curve')
GROUP BY chain, pool_id, emitter, bucket, epoch;

CREATE TABLE IF NOT EXISTS dex_candles_1d (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
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
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1d_mv
TO dex_candles_1d AS
WITH (amount0 > 0) != (amount1 > 0) AND amount0 != 0 AND amount1 != 0 AND abs(toFloat64(amount0)) >= 1000 AND abs(toFloat64(amount1)) >= 1000 AS trade_ok, abs(toFloat64(amount1)) / abs(toFloat64(amount0)) AS trade_price, sqrt_price_x96 != 0 OR (reserve0 != 0 AND reserve1 != 0) AS pool_ok, if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), toFloat64(reserve1) / toFloat64(reserve0)) AS pool_price
SELECT
  chain, pool_id, emitter,
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
FROM dex_swaps
WHERE is_deleted = 0 AND protocol NOT IN ('balancer_v2', 'curve')
GROUP BY chain, pool_id, emitter, bucket, epoch;

-- Hourly volume of every pool of every family, keyed by the VERIFIED tokens
-- of the swaps (dex_swaps.verified_in / verified_out, zero bytes = not
-- proven). Everything USD is built on this table: all swaps of a row share
-- their tokens, so valuing the row is valuing each of its swaps by the same
-- rule as dex_swaps_usd_v, and the hourly native price is the same for all
-- of them. Also the source of swaps / traders per pool and protocol, of the
-- per token volumes and of the resolver's work list.
CREATE TABLE IF NOT EXISTS dex_pool_volume_1h (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
  protocol LowCardinality(String),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  token_in FixedString(32),
  token_out FixedString(32),
  epoch UInt32,
  volume_in SimpleAggregateFunction(sum, Float64),
  volume_out SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, emitter, protocol, bucket, token_in, token_out, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_pool_volume_1h_mv
TO dex_pool_volume_1h AS
SELECT
  chain, pool_id, emitter, protocol,
  toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,
  verified_in AS token_in,
  verified_out AS token_out,
  epoch,
  sum(toFloat64(dex_swaps.amount_in)) AS volume_in,
  sum(toFloat64(dex_swaps.amount_out)) AS volume_out,
  count() AS swaps,
  uniqState(trader) AS traders
FROM dex_swaps
WHERE is_deleted = 0
GROUP BY chain, pool_id, emitter, protocol, bucket, token_in, token_out, epoch;

-- Finalizing views: the validity rule, then the merge. Consumers never touch
-- -State columns nor epochs. The casts strip the SimpleAggregateFunction
-- wrapper that would otherwise leak into the column types (many clients can
-- not parse it). ifNull(): with join_use_nulls = 1 in a user profile an
-- unmatched ASOF row is NULL, not 0, and would hide every bucket.
--
-- Candles carry two price series, both token1 per token0 in raw units:
--   open / high / low / close: TRADE prices |amount1| / |amount0| of swaps
--     whose sides have opposite signs and at least 1000 raw units each (a
--     dust swap of 10 for 1 says nothing about the price). Fee included.
--   pool_open ... pool_close: the POOL price after each swap, (sqrt_price_x96
--     / 2^96)^2 for V3 / V4 / Algebra, reserve1 / reserve0 for V2 / Solidly.
--     Exact for concentrated liquidity and constant product pools - and
--     WRONG for Solidly stable pools (x3y + y3x), which is why it is a
--     separate series: dex_pool_prices_*_v picks per pool.
-- NULL when the bucket has no swap that defines the series.

CREATE VIEW IF NOT EXISTS dex_candles_1m_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
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
FROM dex_candles_1m AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_candles_1h_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
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
FROM dex_candles_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_candles_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
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
FROM dex_candles_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_pool_volume_1h_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.protocol AS protocol, a.bucket AS bucket,
  a.token_in AS token_in, a.token_out AS token_out,
  toFloat64(sum(a.volume_in)) AS volume_in,
  toFloat64(sum(a.volume_out)) AS volume_out,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_pool_volume_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, emitter, protocol, bucket, token_in, token_out;

-- Swaps and unique traders per pool and UTC day.
CREATE VIEW IF NOT EXISTS dex_pool_stats_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.protocol AS protocol,
  toDateTime(intDiv(toUInt32(a.bucket), 86400) * 86400, 'UTC') AS day,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_pool_volume_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, pool_id, emitter, protocol, day;
