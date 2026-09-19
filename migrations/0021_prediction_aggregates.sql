-- Incremental prediction market aggregates (docs/design.md, sections 1
-- "Aggregates", 2 and 10).
--
-- Every table here is declared in Rust as a DerivedTable
-- (src/predictions/derived.rs) whose rebuild_sql repeats the SELECT of its
-- materialized view - a unit test compares the two texts, keep them in
-- sync. Buckets are computed arithmetically from the unix time (UTC).
--
-- Reorgs without DELETE: every row carries the purge generation (epoch) of
-- its chain, every aggregate is keyed by it (LAST sorting key column), the
-- views only aggregate live rows (is_deleted = 0). A purge bumps the
-- epoch, records (chain, epoch, from_ts) in reorgs and re-aggregates the
-- surviving rows of every bucket >= from_ts under the NEW epoch. Readers
-- apply the VALIDITY RULE through the shared epoch_floor_v (migration
-- 0004): a contribution of epoch e in bucket b counts iff e >= the largest
-- epoch among the chain's reorgs with from_ts <= b. The *_v views ASOF join
-- it and filter BEFORE they merge aggregate states. Never read the tables
-- below directly.
--
-- Chain neutral (docs/design.md section 13): identity columns (registry,
-- collateral_token, trader, exchange, and the uniq states over them) are
-- FixedString(32), and the open / close of a candle is argMin / argMax by
-- the position tuple (block_number, tx_index, ordinal).
--
-- 256-bit arithmetic rule: amounts are summed as Float64, never as raw
-- UInt256 (sum() wraps silently and hostile contracts emit 2^256-1). Exact
-- amounts stay in prediction_trades.
--
-- Candles are per OUTCOME TOKEN. A fill prints once in the taker's token
-- at collateral_amount / share_amount and, when both orders were on the
-- same side (mint / merge), once more in the maker's token at
-- maker_collateral_amount / share_amount. A print above 1 collateral per
-- share is not a probability (forged events) and is left out.
--   volume: collateral of the prints (raw units, Float64)
--   trades: prints in this token
--   fills:  taker side prints only - adds up to fills per market

CREATE TABLE IF NOT EXISTS prediction_candles_1m (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32, UInt64)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32, UInt64)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume SimpleAggregateFunction(sum, Float64),
  shares SimpleAggregateFunction(sum, Float64),
  trades SimpleAggregateFunction(sum, UInt64),
  fills SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32)),
  last_trade_at SimpleAggregateFunction(max, DateTime)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, registry, outcome_token_id, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_candles_1m_mv
TO prediction_candles_1m AS
WITH toFloat64(tupleElement(print, 2)) / toFloat64(share_amount) AS price
SELECT
  chain, registry, tupleElement(print, 1) AS outcome_token_id,
  toDateTime(intDiv(toUInt32(timestamp), 60) * 60, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, tx_index, ordinal)) AS open,
  argMaxState(price, (block_number, tx_index, ordinal)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(toFloat64(tupleElement(print, 2))) AS volume,
  sum(toFloat64(share_amount)) AS shares,
  count() AS trades,
  sum(toUInt64(tupleElement(print, 3))) AS fills,
  uniqArrayState([maker, taker]) AS traders,
  max(timestamp) AS last_trade_at
FROM
(
  SELECT
    chain, registry, block_number, tx_index, ordinal, timestamp, epoch, maker, taker, share_amount,
    arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id, collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)), (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print
  FROM prediction_trades
  WHERE is_deleted = 0 AND share_amount != 0
)
WHERE tupleElement(print, 2) <= share_amount
GROUP BY chain, registry, outcome_token_id, bucket, epoch;

CREATE TABLE IF NOT EXISTS prediction_candles_1h (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32, UInt64)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32, UInt64)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume SimpleAggregateFunction(sum, Float64),
  shares SimpleAggregateFunction(sum, Float64),
  trades SimpleAggregateFunction(sum, UInt64),
  fills SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32)),
  last_trade_at SimpleAggregateFunction(max, DateTime)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, registry, outcome_token_id, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_candles_1h_mv
TO prediction_candles_1h AS
WITH toFloat64(tupleElement(print, 2)) / toFloat64(share_amount) AS price
SELECT
  chain, registry, tupleElement(print, 1) AS outcome_token_id,
  toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, tx_index, ordinal)) AS open,
  argMaxState(price, (block_number, tx_index, ordinal)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(toFloat64(tupleElement(print, 2))) AS volume,
  sum(toFloat64(share_amount)) AS shares,
  count() AS trades,
  sum(toUInt64(tupleElement(print, 3))) AS fills,
  uniqArrayState([maker, taker]) AS traders,
  max(timestamp) AS last_trade_at
FROM
(
  SELECT
    chain, registry, block_number, tx_index, ordinal, timestamp, epoch, maker, taker, share_amount,
    arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id, collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)), (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print
  FROM prediction_trades
  WHERE is_deleted = 0 AND share_amount != 0
)
WHERE tupleElement(print, 2) <= share_amount
GROUP BY chain, registry, outcome_token_id, bucket, epoch;

CREATE TABLE IF NOT EXISTS prediction_candles_1d (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  open AggregateFunction(argMin, Float64, Tuple(UInt64, UInt32, UInt64)),
  close AggregateFunction(argMax, Float64, Tuple(UInt64, UInt32, UInt64)),
  high SimpleAggregateFunction(max, Float64),
  low SimpleAggregateFunction(min, Float64),
  volume SimpleAggregateFunction(sum, Float64),
  shares SimpleAggregateFunction(sum, Float64),
  trades SimpleAggregateFunction(sum, UInt64),
  fills SimpleAggregateFunction(sum, UInt64),
  traders AggregateFunction(uniq, FixedString(32)),
  last_trade_at SimpleAggregateFunction(max, DateTime)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, registry, outcome_token_id, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_candles_1d_mv
TO prediction_candles_1d AS
WITH toFloat64(tupleElement(print, 2)) / toFloat64(share_amount) AS price
SELECT
  chain, registry, tupleElement(print, 1) AS outcome_token_id,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  argMinState(price, (block_number, tx_index, ordinal)) AS open,
  argMaxState(price, (block_number, tx_index, ordinal)) AS close,
  max(price) AS high,
  min(price) AS low,
  sum(toFloat64(tupleElement(print, 2))) AS volume,
  sum(toFloat64(share_amount)) AS shares,
  count() AS trades,
  sum(toUInt64(tupleElement(print, 3))) AS fills,
  uniqArrayState([maker, taker]) AS traders,
  max(timestamp) AS last_trade_at
FROM
(
  SELECT
    chain, registry, block_number, tx_index, ordinal, timestamp, epoch, maker, taker, share_amount,
    arrayJoin(if(maker_outcome_token_id = outcome_token_id, [(outcome_token_id, collateral_amount, toUInt8(1))], [(outcome_token_id, collateral_amount, toUInt8(1)), (maker_outcome_token_id, maker_collateral_amount, toUInt8(0))])) AS print
  FROM prediction_trades
  WHERE is_deleted = 0 AND share_amount != 0
)
WHERE tupleElement(print, 2) <= share_amount
GROUP BY chain, registry, outcome_token_id, bucket, epoch;

-- Collateral locked in a market per day. Registry level events only
-- (protocol 'ctf'): adapters repeat them to name the user.
--   open interest = sum(split) - sum(merged) - sum(redeemed)
CREATE TABLE IF NOT EXISTS prediction_market_flows_1d (
  chain UInt64,
  registry FixedString(32),
  market_id FixedString(32),
  collateral_token FixedString(32),
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  epoch UInt32,
  split SimpleAggregateFunction(sum, Float64),
  merged SimpleAggregateFunction(sum, Float64),
  redeemed SimpleAggregateFunction(sum, Float64),
  events SimpleAggregateFunction(sum, UInt64),
  stakeholders AggregateFunction(uniq, FixedString(32))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, registry, market_id, collateral_token, bucket, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_market_flows_1d_mv
TO prediction_market_flows_1d AS
SELECT
  chain, emitter AS registry, market_id, collateral_token,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  epoch,
  sum(if(kind = 'split', toFloat64(amount), 0.)) AS split,
  sum(if(kind = 'merge', toFloat64(amount), 0.)) AS merged,
  sum(if(kind = 'redeem', toFloat64(amount), 0.)) AS redeemed,
  count() AS events,
  uniqState(stakeholder) AS stakeholders
FROM prediction_position_events
WHERE is_deleted = 0 AND protocol = 'ctf'
GROUP BY chain, registry, market_id, collateral_token, bucket, epoch;

-- Leaderboard, trading part: per trader, exchange and day. Both parties
-- of a fill trade (the maker its own token at its own price). Keyed by
-- exchange because that is what fixes the collateral token (and with it
-- the decimals) of the amounts, see prediction_venues.
--   bought / sold: collateral paid / received, fees excluded
--   fees: fees charged in collateral (V1 buy orders pay theirs in shares)
CREATE TABLE IF NOT EXISTS prediction_trader_trades_1d (
  chain UInt64,
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  trader FixedString(32),
  exchange FixedString(32),
  epoch UInt32,
  bought SimpleAggregateFunction(sum, Float64),
  sold SimpleAggregateFunction(sum, Float64),
  fees SimpleAggregateFunction(sum, Float64),
  trades SimpleAggregateFunction(sum, UInt64),
  tokens AggregateFunction(uniq, UInt256)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, bucket, trader, exchange, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_trader_trades_1d_mv
TO prediction_trader_trades_1d AS
SELECT
  chain,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  tupleElement(party, 1) AS trader, exchange,
  epoch,
  sum(if(tupleElement(party, 2) = 'buy', toFloat64(tupleElement(party, 3)), 0.)) AS bought,
  sum(if(tupleElement(party, 2) = 'sell', toFloat64(tupleElement(party, 3)), 0.)) AS sold,
  sum(if(tupleElement(party, 5) = 'collateral', toFloat64(tupleElement(party, 4)), 0.)) AS fees,
  count() AS trades,
  uniqState(tupleElement(party, 6)) AS tokens
FROM
(
  SELECT
    chain, timestamp, exchange, epoch, share_amount,
    arrayJoin([(maker, toString(maker_side), maker_collateral_amount, maker_fee_amount, toString(maker_fee_unit), maker_outcome_token_id), (taker, toString(side), collateral_amount, taker_fee_amount, toString(taker_fee_unit), outcome_token_id)]) AS party
  FROM prediction_trades
  WHERE is_deleted = 0
)
WHERE tupleElement(party, 3) <= share_amount
GROUP BY chain, bucket, trader, exchange, epoch;

-- Leaderboard, funding part: collateral a stakeholder put into full sets
-- (split) and took out (merge, redemption) per day. Registry AND adapter
-- events: an adapter mediated action shows the adapter in the registry
-- event and the user in the adapter event - two different accounts, so
-- nothing is counted twice per account.
CREATE TABLE IF NOT EXISTS prediction_trader_flows_1d (
  chain UInt64,
  bucket DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  trader FixedString(32),
  collateral_token FixedString(32),
  epoch UInt32,
  split SimpleAggregateFunction(sum, Float64),
  merged SimpleAggregateFunction(sum, Float64),
  redeemed SimpleAggregateFunction(sum, Float64),
  events SimpleAggregateFunction(sum, UInt64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(bucket)
ORDER BY (chain, bucket, trader, collateral_token, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_trader_flows_1d_mv
TO prediction_trader_flows_1d AS
SELECT
  chain,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  stakeholder AS trader, collateral_token,
  epoch,
  sum(if(kind = 'split', toFloat64(amount), 0.)) AS split,
  sum(if(kind = 'merge', toFloat64(amount), 0.)) AS merged,
  sum(if(kind = 'redeem', toFloat64(amount), 0.)) AS redeemed,
  count() AS events
FROM prediction_position_events
WHERE is_deleted = 0 AND kind != 'convert'
GROUP BY chain, bucket, trader, collateral_token, epoch;
