-- Core aggregates (docs/design.md, section 1, "Aggregates").
--
-- Incremental AggregatingMergeTree tables fed by materialized views, one
-- row per UTC day. Every table here has a DerivedTable entry in
-- src/db/derived.rs whose rebuild_sql is the SELECT of its view over
-- `<base> FINAL` (a unit test keeps the two identical): after a rollback the
-- affected days are deleted and rebuilt from the surviving base rows.
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
-- Consumers read the *_v views, never the -State columns. The views only
-- expose plain types (no SimpleAggregateFunction leaks into a client).

CREATE TABLE IF NOT EXISTS daily_block_stats (
  chain UInt64,
  day DateTime('UTC'),
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
PARTITION BY (chain, toYear(day))
ORDER BY (chain, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_block_stats_mv TO daily_block_stats AS
SELECT
  chain,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
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
GROUP BY chain, day;

CREATE VIEW IF NOT EXISTS daily_block_stats_v AS
SELECT
  chain,
  day,
  sum(s.blocks) AS blocks,
  sum(s.transactions) AS transactions,
  sum(s.gas_used) AS gas_used,
  sum(s.gas_limit) AS gas_limit,
  sum(s.size) AS size,
  toUInt64(min(s.first_block)) AS first_block,
  toUInt64(max(s.last_block)) AS last_block,
  uniqMerge(s.miners) AS unique_miners,
  avgMerge(s.base_fee_per_gas) AS avg_base_fee_per_gas
FROM daily_block_stats AS s
GROUP BY chain, day;

CREATE TABLE IF NOT EXISTS daily_transaction_stats (
  chain UInt64,
  day DateTime('UTC'),
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
PARTITION BY (chain, toYear(day))
ORDER BY (chain, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_transaction_stats_mv TO daily_transaction_stats AS
SELECT
  chain,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
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
GROUP BY chain, day;

CREATE VIEW IF NOT EXISTS daily_transaction_stats_v AS
SELECT
  chain,
  day,
  sum(s.transactions) AS transactions,
  sum(s.successful) AS successful,
  sum(s.failed) AS failed,
  sum(s.contract_creations) AS contract_creations,
  sum(s.gas_used) AS gas_used,
  sum(s.value) AS value,
  sum(s.fees) AS fees,
  uniqMerge(s.senders) AS unique_senders,
  uniqMerge(s.recipients) AS unique_recipients,
  avgMerge(s.effective_gas_price) AS avg_effective_gas_price
FROM daily_transaction_stats AS s
GROUP BY chain, day;

CREATE TABLE IF NOT EXISTS daily_erc20_transfer_stats (
  chain UInt64,
  token_address FixedString(20),
  day DateTime('UTC'),
  transfers SimpleAggregateFunction(sum, UInt64),
  -- Raw token units as Float64: a token emitting 2^256-1 can not wrap it.
  volume_raw SimpleAggregateFunction(sum, Float64),
  senders AggregateFunction(uniq, FixedString(20)),
  recipients AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY (chain, toYear(day))
ORDER BY (chain, token_address, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_erc20_transfer_stats_mv TO daily_erc20_transfer_stats AS
SELECT
  chain,
  token_address,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
  count() AS transfers,
  sum(toFloat64(e.amount)) AS volume_raw,
  uniqState(e.`from`) AS senders,
  uniqState(e.`to`) AS recipients
FROM erc20_transfers AS e
GROUP BY chain, token_address, day;

-- volume is volume_raw scaled by the token decimals. NULL (never 0) while
-- the token has no metadata row yet, or is not an ERC20.
CREATE VIEW IF NOT EXISTS daily_erc20_transfer_stats_v AS
SELECT
  s.chain AS chain,
  s.token_address AS token_address,
  s.day AS day,
  s.transfers AS transfers,
  s.volume_raw AS volume_raw,
  if(t.type = 'ERC20', s.volume_raw / pow(10, t.decimals), NULL) AS volume,
  s.unique_senders AS unique_senders,
  s.unique_recipients AS unique_recipients
FROM
(
  SELECT
    chain,
    token_address,
    day,
    sum(a.transfers) AS transfers,
    sum(a.volume_raw) AS volume_raw,
    uniqMerge(a.senders) AS unique_senders,
    uniqMerge(a.recipients) AS unique_recipients
  FROM daily_erc20_transfer_stats AS a
  GROUP BY chain, token_address, day
) AS s
LEFT JOIN
(
  SELECT chain, address, decimals, type FROM tokens FINAL
) AS t ON t.chain = s.chain AND t.address = s.token_address;

CREATE TABLE IF NOT EXISTS daily_contract_deployments (
  chain UInt64,
  day DateTime('UTC'),
  contracts SimpleAggregateFunction(sum, UInt64),
  deployers AggregateFunction(uniq, FixedString(20))
)
ENGINE = AggregatingMergeTree
PARTITION BY (chain, toYear(day))
ORDER BY (chain, day);

CREATE MATERIALIZED VIEW IF NOT EXISTS daily_contract_deployments_mv TO daily_contract_deployments AS
SELECT
  chain,
  toDateTime(intDiv(toUnixTimestamp(timestamp), 86400) * 86400, 'UTC') AS day,
  count() AS contracts,
  uniqState(c.creator) AS deployers
FROM contracts AS c
GROUP BY chain, day;

CREATE VIEW IF NOT EXISTS daily_contract_deployments_v AS
SELECT
  chain,
  day,
  sum(s.contracts) AS contracts,
  uniqMerge(s.deployers) AS unique_deployers
FROM daily_contract_deployments AS s
GROUP BY chain, day;
