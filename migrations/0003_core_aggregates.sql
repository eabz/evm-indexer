-- Core aggregates (docs/design.md, section 1 "Aggregates" and section 2
-- "No DELETE, ever: tombstones + epochs").
--
-- Incremental AggregatingMergeTree tables fed by materialized views, one
-- row per UTC day AND epoch. Every table here has a DerivedTable entry in
-- src/db/derived.rs whose rebuild_sql is the SELECT of its view over
-- `<base> FINAL` (a unit test keeps the two identical).
--
-- How they stay correct without ever deleting anything:
--   * a view only aggregates live rows (is_deleted = 0): the tombstones a
--     rollback inserts add nothing
--   * every contribution is filed under the epoch (the chain's purge
--     generation) of the rows it came from
--   * a rollback records (chain, new epoch, from_ts) in `reorgs` and
--     re-aggregates the surviving rows from from_ts on under the new epoch
--   * VALIDITY RULE, applied by every *_v view (0004): a contribution with
--     epoch e in bucket b counts iff e >= max(r.epoch) over the reorgs r of
--     the chain with r.from_ts <= b (0 when there is none). Buckets before the fork
--     keep everything, repaired buckets only show the repair and what was
--     streamed after it, an abandoned half finished repair is invisible, and
--     a later gap fill into an old bucket adds to what is there.
--
-- The day is computed with integer arithmetic on the unix timestamp so it
-- never depends on the server time zone and matches the bucket computed in
-- Rust (ts - ts % 86400).
--
-- 256-bit arithmetic rule: sum() over UInt256 wraps silently on overflow and
-- hostile tokens emit 2^256-1 amounts, so every amount is aggregated as
-- Float64 (toFloat64(x)), never as the raw integer. The base tables keep the
-- exact values.
--
-- Consumers read the *_v views, never the -State columns. They live in
-- 0004, next to `reorgs` and `epoch_floor_v`, which they need.

CREATE TABLE IF NOT EXISTS daily_block_stats (
  chain UInt64,
  day DateTime('UTC'),
  epoch UInt32,
  blocks SimpleAggregateFunction(sum, UInt64),
  transactions SimpleAggregateFunction(sum, UInt64),
  gas_used SimpleAggregateFunction(sum, UInt64),
  gas_limit SimpleAggregateFunction(sum, UInt64),
  size SimpleAggregateFunction(sum, UInt64),
  first_block SimpleAggregateFunction(min, UInt64),
  last_block SimpleAggregateFunction(max, UInt64),
  miners AggregateFunction(uniq, FixedString(20)),
  base_fee_per_gas AggregateFunction(avg, Nullable(Float64))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (chain, day, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_block_stats_mv TO daily_block_stats AS
SELECT
  chain,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
  epoch,
  count() AS blocks,
  sum(toUInt64(b.transactions)) AS transactions,
  sum(b.gas_used) AS gas_used,
  sum(b.gas_limit) AS gas_limit,
  sum(b.size) AS size,
  min(number) AS first_block,
  max(number) AS last_block,
  uniqState(miner) AS miners,
  avgState(toFloat64(b.base_fee_per_gas)) AS base_fee_per_gas
FROM blocks AS b
WHERE is_deleted = 0
GROUP BY chain, day, epoch;

CREATE TABLE IF NOT EXISTS daily_transaction_stats (
  chain UInt64,
  day DateTime('UTC'),
  epoch UInt32,
  transactions SimpleAggregateFunction(sum, UInt64),
  successful SimpleAggregateFunction(sum, UInt64),
  failed SimpleAggregateFunction(sum, UInt64),
  contract_creations SimpleAggregateFunction(sum, UInt64),
  gas_used SimpleAggregateFunction(sum, UInt64),
  -- Wei, as Float64 (see the 256-bit arithmetic rule above).
  value SimpleAggregateFunction(sum, Float64),
  fees SimpleAggregateFunction(sum, Float64),
  senders AggregateFunction(uniq, FixedString(20)),
  recipients AggregateFunction(uniq, Nullable(FixedString(20))),
  effective_gas_price AggregateFunction(avg, Float64)
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (chain, day, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_transaction_stats_mv TO daily_transaction_stats AS
SELECT
  chain,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
  epoch,
  count() AS transactions,
  countIf(t.status = 'success') AS successful,
  countIf(t.status = 'failure') AS failed,
  countIf(t.`to` IS NULL) AS contract_creations,
  sum(t.gas_used) AS gas_used,
  sum(toFloat64(t.value)) AS value,
  sum(toFloat64(t.gas_used) * toFloat64(t.effective_gas_price)) AS fees,
  uniqState(t.`from`) AS senders,
  uniqState(t.`to`) AS recipients,
  avgState(toFloat64(t.effective_gas_price)) AS effective_gas_price
FROM transactions AS t
WHERE is_deleted = 0
GROUP BY chain, day, epoch;

CREATE TABLE IF NOT EXISTS daily_erc20_transfer_stats (
  chain UInt64,
  token_address FixedString(20),
  day DateTime('UTC'),
  epoch UInt32,
  transfers SimpleAggregateFunction(sum, UInt64),
  -- Raw token units as Float64: a token emitting 2^256-1 can not wrap it.
  volume_raw SimpleAggregateFunction(sum, Float64),
  senders AggregateFunction(uniq, FixedString(20)),
  recipients AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY toYYYYMM(day)
ORDER BY (chain, token_address, day, epoch);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_erc20_transfer_stats_mv TO daily_erc20_transfer_stats AS
SELECT
  chain,
  token_address,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
  epoch,
  count() AS transfers,
  sum(toFloat64(e.amount)) AS volume_raw,
  uniqState(e.`from`) AS senders,
  uniqState(e.`to`) AS recipients
FROM erc20_transfers AS e
WHERE is_deleted = 0
GROUP BY chain, token_address, day, epoch;
