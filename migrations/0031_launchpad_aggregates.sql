-- Incremental launchpad aggregates (docs/design.md section 1 "Aggregates",
-- section 2).
--
-- Every table here is declared in Rust as a DerivedTable
-- (src/launchpads/derived.rs) whose rebuild_sql repeats the SELECT of its
-- materialized view - a unit test compares the two texts, keep them in
-- sync. Buckets are computed arithmetically from the unix time (UTC, never
-- the server time zone) so Rust and SQL always agree on a bucket start.
--
-- Reorgs without DELETE: epochs. Every row carries the purge generation of
-- its chain, every aggregate is keyed by it, the views aggregate live rows
-- only (is_deleted = 0 - tombstones add nothing). A purge bumps the epoch,
-- records (chain, epoch, from_ts) in reorgs and re-inserts the surviving
-- rows of every bucket >= from_ts under the NEW epoch (rebuild_sql, one
-- INSERT per month). Readers apply the VALIDITY RULE through the shared
-- epoch_floor_v of migration 0004: a contribution with epoch e in bucket b
-- counts iff e >= ifNull(epoch_floor, 0) at b. Every *_v view below ASOF
-- joins it and filters BEFORE it merges aggregate states, so a stale epoch
-- can not leak an open / close. NEVER read the tables below directly.
--
-- Volumes are Float64 sums on purpose (docs/design.md, 256-bit arithmetic
-- rule): sum() over UInt256 wraps silently and a forged trade can carry
-- 2^256-1. The exact integers stay in launchpad_trades.
--
-- Prices are quote per token in RAW units (no decimals): the candle views
-- of 0032 scale them. Trades whose token leg is unknown (32 zero bytes)
-- are not in the candles at all.
--
-- emitter is part of every key on purpose: it is the ONLY thing a reader
-- can use afterwards to keep a forger's rows out (launchpad_trusted_emitters).

CREATE TABLE IF NOT EXISTS launchpad_candles_1m (
  chain UInt64,
  token FixedString(32),
  emitter FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  high SimpleAggregateFunction(max, Nullable(Float64)),
  low SimpleAggregateFunction(min, Nullable(Float64)),
  priced_trades SimpleAggregateFunction(sum, UInt64),
  trades SimpleAggregateFunction(sum, UInt64),
  buys SimpleAggregateFunction(sum, UInt64),
  volume_quote SimpleAggregateFunction(sum, Float64),
  volume_token SimpleAggregateFunction(sum, Float64),
  volume_quote_verified SimpleAggregateFunction(sum, Float64),
  traders AggregateFunction(uniq, FixedString(32)),
  progress_wad AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32, UInt64))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, token, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_candles_1m_mv
TO launchpad_candles_1m AS
WITH
  token_amount != 0 AND quote_amount != 0 AS priced,
  toFloat64(quote_amount) / toFloat64(token_amount) AS price,
  (block_number, tx_index, ordinal) AS position
SELECT
  chain, token, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS bucket,
  epoch,
  argMinStateIf(price, position, priced) AS open,
  argMaxStateIf(price, position, priced) AS close,
  max(if(priced, price, NULL)) AS high,
  min(if(priced, price, NULL)) AS low,
  countIf(priced) AS priced_trades,
  count() AS trades,
  countIf(side = 'buy') AS buys,
  sum(toFloat64(quote_amount)) AS volume_quote,
  sum(toFloat64(token_amount)) AS volume_token,
  sumIf(toFloat64(quote_amount), quote_verified = 1) AS volume_quote_verified,
  uniqState(trader) AS traders,
  argMaxState(toFloat64(progress_wad), position) AS progress_wad
FROM launchpad_trades
WHERE is_deleted = 0 AND token != toFixedString('', 32)
GROUP BY chain, token, emitter, bucket, epoch;

CREATE TABLE IF NOT EXISTS launchpad_candles_1h (
  chain UInt64,
  token FixedString(32),
  emitter FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMinIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  close AggregateFunction(argMaxIf, Float64, Tuple(UInt64, UInt32, UInt64), UInt8),
  high SimpleAggregateFunction(max, Nullable(Float64)),
  low SimpleAggregateFunction(min, Nullable(Float64)),
  priced_trades SimpleAggregateFunction(sum, UInt64),
  trades SimpleAggregateFunction(sum, UInt64),
  buys SimpleAggregateFunction(sum, UInt64),
  volume_quote SimpleAggregateFunction(sum, Float64),
  volume_token SimpleAggregateFunction(sum, Float64),
  volume_quote_verified SimpleAggregateFunction(sum, Float64),
  traders AggregateFunction(uniq, FixedString(32)),
  progress_wad AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32, UInt64))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, token, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_candles_1h_mv
TO launchpad_candles_1h AS
WITH
  token_amount != 0 AND quote_amount != 0 AS priced,
  toFloat64(quote_amount) / toFloat64(token_amount) AS price,
  (block_number, tx_index, ordinal) AS position
SELECT
  chain, token, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,
  epoch,
  argMinStateIf(price, position, priced) AS open,
  argMaxStateIf(price, position, priced) AS close,
  max(if(priced, price, NULL)) AS high,
  min(if(priced, price, NULL)) AS low,
  countIf(priced) AS priced_trades,
  count() AS trades,
  countIf(side = 'buy') AS buys,
  sum(toFloat64(quote_amount)) AS volume_quote,
  sum(toFloat64(token_amount)) AS volume_token,
  sumIf(toFloat64(quote_amount), quote_verified = 1) AS volume_quote_verified,
  uniqState(trader) AS traders,
  argMaxState(toFloat64(progress_wad), position) AS progress_wad
FROM launchpad_trades
WHERE is_deleted = 0 AND token != toFixedString('', 32)
GROUP BY chain, token, emitter, bucket, epoch;

-- Curve activity per venue emitter and day.
CREATE TABLE IF NOT EXISTS launchpad_venue_trades_1d (
  chain UInt64,
  family LowCardinality(String),
  emitter FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  trades SimpleAggregateFunction(sum, UInt64),
  buys SimpleAggregateFunction(sum, UInt64),
  volume_quote SimpleAggregateFunction(sum, Float64),
  volume_quote_verified SimpleAggregateFunction(sum, Float64),
  fees SimpleAggregateFunction(sum, Float64),
  traders AggregateFunction(uniq, FixedString(32)),
  tokens AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, family, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_venue_trades_1d_mv
TO launchpad_venue_trades_1d AS
SELECT
  chain, family, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  count() AS trades,
  countIf(side = 'buy') AS buys,
  sum(toFloat64(quote_amount)) AS volume_quote,
  sumIf(toFloat64(quote_amount), quote_verified = 1) AS volume_quote_verified,
  sum(toFloat64(fee_amount)) AS fees,
  uniqState(trader) AS traders,
  uniqState(token) AS tokens
FROM launchpad_trades
WHERE is_deleted = 0
GROUP BY chain, family, emitter, bucket, epoch;

-- Launches per venue emitter, creator and day. Keyed by creator so the
-- same table serves the venue screen (sum over creators) and the creator
-- screen (one creator): one row per creator per venue per day.
CREATE TABLE IF NOT EXISTS launchpad_launches_1d (
  chain UInt64,
  family LowCardinality(String),
  emitter FixedString(32),
  creator FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  launches SimpleAggregateFunction(sum, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, family, emitter, creator, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_launches_1d_mv
TO launchpad_launches_1d AS
SELECT
  chain, family, emitter, creator,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  count() AS launches
FROM launchpad_tokens
WHERE is_deleted = 0
GROUP BY chain, family, emitter, creator, bucket, epoch;

CREATE TABLE IF NOT EXISTS launchpad_graduations_1d (
  chain UInt64,
  family LowCardinality(String),
  emitter FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  graduations SimpleAggregateFunction(sum, UInt64),
  quote_in SimpleAggregateFunction(sum, Float64),
  tokens AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, family, emitter, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_graduations_1d_mv
TO launchpad_graduations_1d AS
SELECT
  chain, family, emitter,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  count() AS graduations,
  sum(toFloat64(quote_amount)) AS quote_in,
  uniqState(token) AS tokens
FROM launchpad_graduations
WHERE is_deleted = 0
GROUP BY chain, family, emitter, bucket, epoch;

-- Realised fees per beneficiary and day: the creator page's "did this
-- wallet earn anything" column, and the protocol's own take.
CREATE TABLE IF NOT EXISTS launchpad_creator_fees_1d (
  chain UInt64,
  family LowCardinality(String),
  emitter FixedString(32),
  recipient FixedString(32),
  kind LowCardinality(String),
  phase LowCardinality(String),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  events SimpleAggregateFunction(sum, UInt64),
  amount SimpleAggregateFunction(sum, Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, family, emitter, recipient, kind, phase, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_creator_fees_1d_mv
TO launchpad_creator_fees_1d AS
SELECT
  chain, family, emitter, recipient, kind, phase,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  count() AS events,
  sum(toFloat64(amount)) AS amount
FROM launchpad_creator_fees
WHERE is_deleted = 0
GROUP BY chain, family, emitter, recipient, kind, phase, bucket, epoch;
