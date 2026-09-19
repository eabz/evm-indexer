-- Incremental DEX aggregates (docs/design.md §1 "Aggregates", §2).
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
-- from_ts <= b (0 when there is none). dex_epoch_floor_v turns reorgs into
-- that step function, every *_v view ASOF joins it and filters BEFORE it
-- merges aggregate states, so a stale epoch can not leak an open / close.
-- Never read the tables below directly: only their *_v views are correct.
--
-- Price = token1 per token0 in RAW units (not decimals adjusted), Float64:
--   sqrt_price_x96 present (V3 / V4 / Algebra): (sqrt_price_x96 / 2^96)^2,
--     the pool price AFTER the swap,
--   otherwise (V2 / Solidly): |amount1| / |amount0|, the execution price
--     of the swap, fee included.
-- Float64 keeps ~15.9 significant digits: relative error of the price is
-- below 1e-15, amounts above 2^53 raw units lose their low digits.
-- Volumes are Float64 sums on purpose (docs/design.md, 256-bit arithmetic
-- rule): sum() over UInt256 / Int256 wraps silently, and spam tokens emit
-- amounts near 2^256. Exact amounts stay in dex_swaps.
-- Multi asset families (Balancer, Curve) have no token0 / token1 and
-- therefore no candles. Their volume is in dex_pool_volume_1d.

-- The validity rule as a step function: for every (chain, from_ts) the
-- largest epoch of all reorgs of the chain starting at or before from_ts.
-- reorgs (chain, epoch, from_ts, ...) is created by migration 0004.
CREATE VIEW IF NOT EXISTS dex_epoch_floor_v AS
SELECT
  chain,
  from_ts,
  max(step) OVER (PARTITION BY chain ORDER BY from_ts ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS epoch_floor
FROM
(
  SELECT chain, toDateTime(from_ts, 'UTC') AS from_ts, max(epoch) AS step
  FROM reorgs
  GROUP BY chain, from_ts
);

CREATE TABLE IF NOT EXISTS dex_candles_1m (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(20),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1m_mv
TO dex_candles_1m AS
WITH if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), abs(toFloat64(amount1)) / abs(toFloat64(amount0))) AS price
SELECT
  chain, pool_id, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, log_index)) AS open,
  argMaxState(price, (block_number, log_index)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM dex_swaps
WHERE is_deleted = 0 AND (sqrt_price_x96 != 0 OR (amount0 != 0 AND amount1 != 0))
GROUP BY chain, pool_id, emitter, bucket, epoch;

CREATE TABLE IF NOT EXISTS dex_candles_1h (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(20),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1h_mv
TO dex_candles_1h AS
WITH if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), abs(toFloat64(amount1)) / abs(toFloat64(amount0))) AS price
SELECT
  chain, pool_id, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, log_index)) AS open,
  argMaxState(price, (block_number, log_index)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM dex_swaps
WHERE is_deleted = 0 AND (sqrt_price_x96 != 0 OR (amount0 != 0 AND amount1 != 0))
GROUP BY chain, pool_id, emitter, bucket, epoch;

CREATE TABLE IF NOT EXISTS dex_candles_1d (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(20),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume0 SimpleAggregateFunction(sum, Float64),
  volume1 SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_candles_1d_mv
TO dex_candles_1d AS
WITH if(sqrt_price_x96 != 0, pow(toFloat64(sqrt_price_x96) / 79228162514264337593543950336., 2), abs(toFloat64(amount1)) / abs(toFloat64(amount0))) AS price
SELECT
  chain, pool_id, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, log_index)) AS open,
  argMaxState(price, (block_number, log_index)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(abs(toFloat64(amount0))) AS volume0,
  sum(abs(toFloat64(amount1))) AS volume1,
  count() AS swaps,
  uniqState(trader) AS traders
FROM dex_swaps
WHERE is_deleted = 0 AND (sqrt_price_x96 != 0 OR (amount0 != 0 AND amount1 != 0))
GROUP BY chain, pool_id, emitter, bucket, epoch;

-- Daily volume of every pool of every family, one row per LEG of the pool:
--   leg_kind 'side'  : leg_index 0 / 1 = token0 / token1 of the pool
--   leg_kind 'token' : leg_token is the token itself (Balancer)
--   leg_kind 'coin'  : leg_index = index into dex_pools.tokens (Curve)
--   leg_kind 'ucoin' : leg_index = index into dex_pools.underlying_tokens
-- Every swap feeds exactly two legs (the one it pays into, volume_in, and
-- the one it takes from, volume_out), so swaps of a pool = sum(swaps) / 2.
CREATE TABLE IF NOT EXISTS dex_pool_volume_1d (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(20),
  protocol LowCardinality(String),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  leg_kind LowCardinality(String),
  leg_index UInt8,
  leg_token FixedString(20),
  epoch UInt32,
  volume_in SimpleAggregateFunction(sum, Float64),
  volume_out SimpleAggregateFunction(sum, Float64),
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, pool_id, emitter, protocol, bucket, leg_kind, leg_index, leg_token, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_pool_volume_1d_mv
TO dex_pool_volume_1d AS
WITH toFixedString('', 20) AS zero, toFloat64(0) AS none, arrayJoin(multiIf(protocol = 'curve', [(if(underlying, 'ucoin', 'coin'), coin_in, zero, toFloat64(amount_in), none), (if(underlying, 'ucoin', 'coin'), coin_out, zero, none, toFloat64(amount_out))], token_in != zero OR token_out != zero, [('token', toUInt8(0), token_in, toFloat64(amount_in), none), ('token', toUInt8(0), token_out, none, toFloat64(amount_out))], [('side', toUInt8(0), zero, if(amount0 > 0, abs(toFloat64(amount0)), none), if(amount0 < 0, abs(toFloat64(amount0)), none)), ('side', toUInt8(1), zero, if(amount1 > 0, abs(toFloat64(amount1)), none), if(amount1 < 0, abs(toFloat64(amount1)), none))])) AS leg
SELECT
  chain, pool_id, emitter, protocol,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  leg.1 AS leg_kind,
  leg.2 AS leg_index,
  leg.3 AS leg_token,
  epoch,
  sum(leg.4) AS volume_in,
  sum(leg.5) AS volume_out,
  count() AS swaps,
  uniqState(trader) AS traders
FROM dex_swaps
WHERE is_deleted = 0
GROUP BY chain, pool_id, emitter, protocol, bucket, leg_kind, leg_index, leg_token, epoch;

-- Daily activity of a protocol (event family). Raw token volumes can not be
-- added across pools: USD volume per protocol is dex_protocol_volume_usd_1d_v.
CREATE TABLE IF NOT EXISTS dex_protocol_stats_1d (
  chain UInt64,
  protocol LowCardinality(String),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  swaps SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(20)),
  pools AggregateFunction(uniq, FixedString(32), FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, protocol, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_protocol_stats_1d_mv
TO dex_protocol_stats_1d AS
SELECT
  chain, protocol,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  count() AS swaps,
  uniqState(trader) AS traders,
  uniqState(pool_id, emitter) AS pools
FROM dex_swaps
WHERE is_deleted = 0
GROUP BY chain, protocol, bucket, epoch;

-- Finalizing views: the validity rule, then the merge. Consumers never touch
-- -State columns nor epochs. The casts strip the SimpleAggregateFunction
-- wrapper that would otherwise leak into the column types (many clients can
-- not parse it).

CREATE VIEW IF NOT EXISTS dex_candles_1m_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_candles_1m AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_candles_1h_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_candles_1h AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_candles_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume0)) AS volume0,
  toFloat64(sum(a.volume1)) AS volume1,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_candles_1d AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, pool_id, emitter, bucket;

CREATE VIEW IF NOT EXISTS dex_pool_volume_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.protocol AS protocol, a.bucket AS bucket,
  a.leg_kind AS leg_kind, a.leg_index AS leg_index, a.leg_token AS leg_token,
  toFloat64(sum(a.volume_in)) AS volume_in,
  toFloat64(sum(a.volume_out)) AS volume_out,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_pool_volume_1d AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, pool_id, emitter, protocol, bucket, leg_kind, leg_index, leg_token;

-- Per pool and day over all legs: every swap feeds two legs.
CREATE VIEW IF NOT EXISTS dex_pool_stats_1d_v AS
SELECT
  a.chain AS chain, a.pool_id AS pool_id, a.emitter AS emitter, a.protocol AS protocol, a.bucket AS bucket,
  toUInt64(sum(a.swaps) / 2) AS swaps,
  uniqMerge(a.traders) AS traders
FROM dex_pool_volume_1d AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, pool_id, emitter, protocol, bucket;

CREATE VIEW IF NOT EXISTS dex_protocol_stats_1d_v AS
SELECT
  a.chain AS chain, a.protocol AS protocol, a.bucket AS bucket,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders,
  uniqMerge(a.pools) AS pools
FROM dex_protocol_stats_1d AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor
GROUP BY chain, protocol, bucket;
