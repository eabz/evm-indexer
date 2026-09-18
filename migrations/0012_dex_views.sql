-- Analyst views over the DEX tables. Plain views: nothing is stored, tokens
-- and pools are joined at QUERY time, so rows get better as the background
-- resolvers fill tokens / dex_pools.
--
-- Rules shared by every view:
--   unknown stays NULL - unknown pool tokens, unknown decimals and
--     unpriceable amounts are NULL, never 0,
--   *_adj columns are decimals adjusted Float64 (~15.9 significant digits),
--     raw columns stay exact UInt256 / Int256,
--   USD: a quote_tokens 'stable' is worth 1, a 'native' is worth the volume
--     weighted price of the chain's native/stable two token pools
--     (dex_native_price_*_v, last known bucket), everything else is NULL.

-- Symbol / decimals / quote kind of a token: tokens first, quote_tokens as
-- the fallback for pseudo addresses that have no contract.
CREATE VIEW IF NOT EXISTS dex_token_info_v AS
SELECT
  chain,
  token,
  if(anyIf(info_symbol, origin = 1) != '', anyIf(info_symbol, origin = 1), anyIf(info_symbol, origin = 0)) AS symbol,
  anyIf(info_name, origin = 1) AS name,
  coalesce(maxIf(info_decimals, origin = 1), maxIf(info_decimals, origin = 0)) AS decimals,
  anyIf(info_kind, origin = 0) AS kind
FROM
(
  SELECT chain, address AS token, symbol AS info_symbol, name AS info_name, toNullable(decimals) AS info_decimals, '' AS info_kind, 1 AS origin
  FROM tokens FINAL
  UNION ALL
  SELECT chain, token, symbol AS info_symbol, '' AS info_name, decimals AS info_decimals, toString(kind) AS info_kind, 0 AS origin
  FROM quote_tokens FINAL
  WHERE kind IN ('stable', 'native')
)
GROUP BY chain, token;

CREATE VIEW IF NOT EXISTS dex_pools_v AS
SELECT
  p.chain AS chain,
  p.pool_id AS pool_id,
  p.emitter AS emitter,
  concat('0x', lower(hex(p.pool_id))) AS pool,
  p.protocol AS protocol,
  p.factory AS factory,
  p.token0 AS token0,
  p.token1 AS token1,
  if(length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t0.symbol, '') AS symbol0,
  if(length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t1.symbol, '') AS symbol1,
  if(length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t0.decimals, NULL) AS decimals0,
  if(length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t1.decimals, NULL) AS decimals1,
  p.tokens AS tokens,
  p.underlying_tokens AS underlying_tokens,
  p.fee AS fee,
  p.tick_spacing AS tick_spacing,
  p.hooks AS hooks,
  p.stable AS stable,
  p.created_block AS created_block,
  p.timestamp AS created_at,
  p.source AS source
FROM (SELECT * FROM dex_pools FINAL WHERE source != 'unresolved') AS p
LEFT JOIN dex_token_info_v AS t0 ON t0.chain = p.chain AND t0.token = p.token0
LEFT JOIN dex_token_info_v AS t1 ON t1.chain = p.chain AND t1.token = p.token1;

-- Every swap as token_in / token_out / amount_in / amount_out, whatever the
-- family: event carried tokens (Balancer), Curve coin indices and the
-- signed token0 / token1 amounts of the two token families are resolved
-- through dex_pools. token_in_known / token_out_known = 0 when the pool (or
-- the coin) is not known yet, the token column is then all zero bytes.
CREATE VIEW IF NOT EXISTS dex_swaps_v AS
SELECT
  r.chain AS chain,
  r.block_number AS block_number,
  r.timestamp AS timestamp,
  r.transaction_hash AS transaction_hash,
  r.log_index AS log_index,
  r.pool_id AS pool_id,
  r.emitter AS emitter,
  r.protocol AS protocol,
  r.sender AS sender,
  r.recipient AS recipient,
  r.tx_from AS tx_from,
  r.tx_to AS tx_to,
  r.trader AS trader,
  r.amount0 AS amount0,
  r.amount1 AS amount1,
  r.sqrt_price_x96 AS sqrt_price_x96,
  r.tick AS tick,
  r.pool_known AS pool_known,
  r.r_token_in AS token_in,
  r.r_token_out AS token_out,
  r.r_in_known AS token_in_known,
  r.r_out_known AS token_out_known,
  r.r_amount_in AS amount_in,
  r.r_amount_out AS amount_out,
  if(r.r_in_known, ti.symbol, '') AS symbol_in,
  if(r.r_out_known, tout.symbol, '') AS symbol_out,
  if(r.r_in_known, ti.decimals, NULL) AS decimals_in,
  if(r.r_out_known, tout.decimals, NULL) AS decimals_out,
  if(r.r_in_known, ti.kind, '') AS quote_in,
  if(r.r_out_known, tout.kind, '') AS quote_out,
  toFloat64(r.r_amount_in) / pow(10, if(r.r_in_known, ti.decimals, NULL)) AS amount_in_adj,
  toFloat64(r.r_amount_out) / pow(10, if(r.r_out_known, tout.decimals, NULL)) AS amount_out_adj
FROM
(
  SELECT
    s.*,
    p.found = 1 AS pool_known,
    (s.token_in != toFixedString('', 20) OR s.token_out != toFixedString('', 20)) AS carried,
    if(s.underlying, p.underlying_tokens, p.tokens) AS coins,
    multiIf(carried, s.token_in, s.protocol = 'curve', arrayElement(coins, s.coin_in + 1), s.amount0 > 0, p.token0, p.token1) AS r_token_in,
    multiIf(carried, s.token_out, s.protocol = 'curve', arrayElement(coins, s.coin_out + 1), s.amount0 > 0, p.token1, p.token0) AS r_token_out,
    multiIf(carried, 1, NOT pool_known, 0, s.protocol = 'curve', s.coin_in < length(coins), s.amount0 != 0 OR s.amount1 != 0) AS r_in_known,
    multiIf(carried, 1, NOT pool_known, 0, s.protocol = 'curve', s.coin_out < length(coins), s.amount0 != 0 OR s.amount1 != 0) AS r_out_known,
    multiIf(s.amount0 = 0 AND s.amount1 = 0, s.amount_in, s.amount0 > 0, abs(s.amount0), s.amount1 > 0, abs(s.amount1), toUInt256(0)) AS r_amount_in,
    multiIf(s.amount0 = 0 AND s.amount1 = 0, s.amount_out, s.amount0 > 0, if(s.amount1 < 0, abs(s.amount1), toUInt256(0)), if(s.amount0 < 0, abs(s.amount0), toUInt256(0))) AS r_amount_out
  FROM (SELECT * FROM dex_swaps FINAL) AS s
  LEFT JOIN
  (
    SELECT chain, pool_id, emitter, token0, token1, tokens, underlying_tokens, 1 AS found
    FROM dex_pools FINAL
    WHERE source != 'unresolved'
  ) AS p ON p.chain = s.chain AND p.pool_id = s.pool_id AND p.emitter = s.emitter
) AS r
LEFT JOIN dex_token_info_v AS ti ON ti.chain = r.chain AND ti.token = r.r_token_in
LEFT JOIN dex_token_info_v AS tout ON tout.chain = r.chain AND tout.token = r.r_token_out;

-- Decimals adjusted candles: prices are token1 per token0 in human units.
CREATE VIEW IF NOT EXISTS dex_pool_prices_1m_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  c.open * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS open,
  c.high * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS high,
  c.low * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS low,
  c.close * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS close,
  toFloat64(c.volume0) / pow(10, p.decimals0) AS volume0_adj,
  toFloat64(c.volume1) / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1m_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter;

CREATE VIEW IF NOT EXISTS dex_pool_prices_1h_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  c.open * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS open,
  c.high * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS high,
  c.low * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS low,
  c.close * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS close,
  toFloat64(c.volume0) / pow(10, p.decimals0) AS volume0_adj,
  toFloat64(c.volume1) / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1h_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter;

CREATE VIEW IF NOT EXISTS dex_pool_prices_1d_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  c.open * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS open,
  c.high * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS high,
  c.low * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS low,
  c.close * pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS close,
  toFloat64(c.volume0) / pow(10, p.decimals0) AS volume0_adj,
  toFloat64(c.volume1) / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1d_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter;

-- USD price of the native coin per hour / day: volume weighted over every
-- two token pool pairing a 'native' with a 'stable' quote token.
CREATE VIEW IF NOT EXISTS dex_native_price_1h_v AS
SELECT
  chain,
  bucket,
  sum(stable_side) / sum(native_side) AS price,
  sum(native_side) AS native_volume,
  sum(stable_side) AS stable_volume,
  count() AS pools
FROM
(
  SELECT
    c.chain AS chain,
    c.bucket AS bucket,
    if(q0.kind = 'native', c.volume0_adj, c.volume1_adj) AS native_side,
    if(q0.kind = 'native', c.volume1_adj, c.volume0_adj) AS stable_side
  FROM dex_pool_prices_1h_v AS c
  INNER JOIN dex_token_info_v AS q0 ON q0.chain = c.chain AND q0.token = c.token0
  INNER JOIN dex_token_info_v AS q1 ON q1.chain = c.chain AND q1.token = c.token1
  WHERE ((q0.kind = 'native' AND q1.kind = 'stable') OR (q0.kind = 'stable' AND q1.kind = 'native'))
    AND c.volume0_adj > 0 AND c.volume1_adj > 0
)
GROUP BY chain, bucket;

CREATE VIEW IF NOT EXISTS dex_native_price_1d_v AS
SELECT
  chain,
  bucket,
  sum(stable_side) / sum(native_side) AS price,
  sum(native_side) AS native_volume,
  sum(stable_side) AS stable_volume,
  count() AS pools
FROM
(
  SELECT
    c.chain AS chain,
    c.bucket AS bucket,
    if(q0.kind = 'native', c.volume0_adj, c.volume1_adj) AS native_side,
    if(q0.kind = 'native', c.volume1_adj, c.volume0_adj) AS stable_side
  FROM dex_pool_prices_1d_v AS c
  INNER JOIN dex_token_info_v AS q0 ON q0.chain = c.chain AND q0.token = c.token0
  INNER JOIN dex_token_info_v AS q1 ON q1.chain = c.chain AND q1.token = c.token1
  WHERE ((q0.kind = 'native' AND q1.kind = 'stable') OR (q0.kind = 'stable' AND q1.kind = 'native'))
    AND c.volume0_adj > 0 AND c.volume1_adj > 0
)
GROUP BY chain, bucket;

-- A swap is valued on its input side when that is a quote token, else on
-- its output side, else it has no USD value (NULL). native_price is the
-- hourly price in force (last bucket at or before the swap).
CREATE VIEW IF NOT EXISTS dex_swaps_usd_v AS
SELECT
  s.*,
  nullIf(n.price, 0) AS native_price,
  coalesce(
    if(s.quote_in = 'stable', s.amount_in_adj, NULL),
    if(s.quote_out = 'stable', s.amount_out_adj, NULL),
    if(s.quote_in = 'native', s.amount_in_adj * native_price, NULL),
    if(s.quote_out = 'native', s.amount_out_adj * native_price, NULL)
  ) AS amount_usd
FROM dex_swaps_v AS s
ASOF LEFT JOIN dex_native_price_1h_v AS n ON n.chain = s.chain AND n.bucket <= toDateTime(s.timestamp, 'UTC');

-- Daily volume per pool and token (one row per leg of dex_pool_volume_1d)
-- with the leg resolved to its token. price_usd: 1 for a stable, the daily
-- native price for a native, NULL otherwise.
CREATE VIEW IF NOT EXISTS dex_pool_token_volume_1d_v AS
SELECT
  r.chain AS chain, r.pool_id AS pool_id, r.emitter AS emitter, r.protocol AS protocol, r.bucket AS bucket,
  r.leg_kind AS leg_kind, r.leg_index AS leg_index,
  r.token AS token,
  r.token_known AS token_known,
  if(r.token_known, t.symbol, '') AS symbol,
  if(r.token_known, t.decimals, NULL) AS decimals,
  if(r.token_known, t.kind, '') AS quote,
  r.volume_in AS volume_in,
  r.volume_out AS volume_out,
  toFloat64(r.volume_in) / pow(10, decimals) AS volume_in_adj,
  toFloat64(r.volume_out) / pow(10, decimals) AS volume_out_adj,
  multiIf(quote = 'stable', 1., quote = 'native', nullIf(n.price, 0), NULL) AS price_usd,
  r.swaps AS swaps,
  r.traders AS traders
FROM
(
  SELECT
    l.*,
    multiIf(l.leg_kind = 'token', l.leg_token, l.leg_kind = 'side', if(l.leg_index = 0, p.token0, p.token1), l.leg_kind = 'coin', arrayElement(p.tokens, l.leg_index + 1), arrayElement(p.underlying_tokens, l.leg_index + 1)) AS token,
    multiIf(l.leg_kind = 'token', 1, p.found != 1, 0, l.leg_kind = 'side', 1, l.leg_kind = 'coin', l.leg_index < length(p.tokens), l.leg_index < length(p.underlying_tokens)) AS token_known
  FROM dex_pool_volume_1d_v AS l
  LEFT JOIN
  (
    SELECT chain, pool_id, emitter, token0, token1, tokens, underlying_tokens, 1 AS found
    FROM dex_pools FINAL
    WHERE source != 'unresolved'
  ) AS p ON p.chain = l.chain AND p.pool_id = l.pool_id AND p.emitter = l.emitter
) AS r
LEFT JOIN dex_token_info_v AS t ON t.chain = r.chain AND t.token = r.token
ASOF LEFT JOIN dex_native_price_1d_v AS n ON n.chain = r.chain AND n.bucket <= r.bucket;

-- Daily USD volume per pool on the daily aggregates (daily instead of hourly
-- native price). A leg is priced when its token is a quote token with known
-- decimals. Two token pools follow dex_swaps_usd_v exactly: both legs priced
-- = input side of each leg, one leg priced = both sides of that leg. Multi
-- asset pools take the larger of (priced inputs, priced outputs): swaps
-- between two non quote tokens are not counted (documented underestimate).
-- Nothing priced = NULL, never 0.
CREATE VIEW IF NOT EXISTS dex_pool_volume_usd_1d_v AS
SELECT
  v.chain AS chain, v.pool_id AS pool_id, v.emitter AS emitter, v.protocol AS protocol, v.bucket AS bucket,
  v.volume_usd AS volume_usd,
  v.swaps AS swaps,
  u.traders AS traders
FROM
(
  SELECT
    chain, pool_id, emitter, protocol, bucket,
    countIf(price_usd IS NOT NULL AND volume_in_adj IS NOT NULL) AS priced_legs,
    sumIf(volume_in_adj * price_usd, price_usd IS NOT NULL AND volume_in_adj IS NOT NULL) AS in_usd,
    sumIf(volume_out_adj * price_usd, price_usd IS NOT NULL AND volume_in_adj IS NOT NULL) AS out_usd,
    nullIf(multiIf(
      priced_legs = 0, NULL,
      any(leg_kind) = 'side' AND priced_legs = 1, in_usd + out_usd,
      any(leg_kind) = 'side', in_usd,
      greatest(in_usd, out_usd)
    ), 0) AS volume_usd,
    toUInt64(sum(swaps) / 2) AS swaps
  FROM dex_pool_token_volume_1d_v
  GROUP BY chain, pool_id, emitter, protocol, bucket
) AS v
INNER JOIN
(
  SELECT chain, pool_id, emitter, protocol, bucket, uniqMerge(traders) AS traders
  FROM dex_pool_volume_1d
  GROUP BY chain, pool_id, emitter, protocol, bucket
) AS u ON u.chain = v.chain AND u.pool_id = v.pool_id AND u.emitter = v.emitter AND u.protocol = v.protocol AND u.bucket = v.bucket;

CREATE VIEW IF NOT EXISTS dex_protocol_volume_usd_1d_v AS
SELECT
  s.chain AS chain, s.protocol AS protocol, s.bucket AS bucket,
  v.protocol_volume_usd AS volume_usd,
  v.priced_pools AS priced_pools,
  s.pools AS pools,
  s.swaps AS swaps,
  s.traders AS traders
FROM dex_protocol_stats_1d_v AS s
LEFT JOIN
(
  SELECT chain, protocol, bucket, sum(volume_usd) AS protocol_volume_usd, countIf(volume_usd IS NOT NULL) AS priced_pools
  FROM dex_pool_volume_usd_1d_v
  GROUP BY chain, protocol, bucket
) AS v ON v.chain = s.chain AND v.protocol = s.protocol AND v.bucket = s.bucket;

-- Daily volume per token over every pool and family (raw, adjusted and, for
-- quote tokens only, USD). USD volume of ANY token: dex_token_volume_usd_1d_v.
CREATE VIEW IF NOT EXISTS dex_token_volume_1d_v AS
SELECT
  chain, token, bucket,
  any(symbol) AS symbol,
  any(decimals) AS decimals,
  sum(volume_in) + sum(volume_out) AS volume,
  sum(volume_in_adj) + sum(volume_out_adj) AS volume_adj,
  (sum(volume_in_adj) + sum(volume_out_adj)) * any(price_usd) AS volume_usd,
  sum(swaps) AS swaps,
  uniq(pool_id, emitter) AS pools
FROM dex_pool_token_volume_1d_v
WHERE token_known
GROUP BY chain, token, bucket;

-- Swap level (scans dex_swaps: always filter chain and a time range).
CREATE VIEW IF NOT EXISTS dex_token_volume_usd_1d_v AS
SELECT
  chain,
  side.1 AS token,
  toDateTime(intDiv(toUInt32(timestamp), 86400) * 86400, 'UTC') AS bucket,
  any(side.2) AS symbol,
  sum(amount_usd) AS volume_usd,
  count() AS swaps,
  countIf(amount_usd IS NULL) AS unpriced_swaps
FROM
(
  SELECT
    chain, timestamp, amount_usd,
    arrayJoin(arrayFilter(x -> x.3, [(token_in, symbol_in, token_in_known), (token_out, symbol_out, token_out_known)])) AS side
  FROM dex_swaps_usd_v
)
GROUP BY chain, token, bucket;

-- Pools ranked by USD volume of the trailing 30 days.
CREATE VIEW IF NOT EXISTS dex_top_pools_v AS
SELECT
  v.chain AS chain, v.pool_id AS pool_id, v.emitter AS emitter, v.protocol AS protocol,
  p.pool AS pool, p.symbol0 AS symbol0, p.symbol1 AS symbol1, p.tokens AS tokens,
  v.volume_usd_30d AS volume_usd_30d,
  v.swaps_30d AS swaps_30d,
  v.active_days AS active_days
FROM
(
  SELECT chain, pool_id, emitter, protocol, sum(volume_usd) AS volume_usd_30d, sum(swaps) AS swaps_30d, count() AS active_days
  FROM dex_pool_volume_usd_1d_v
  WHERE bucket >= toDateTime(intDiv(toUInt32(now()), 86400) * 86400 - 30 * 86400, 'UTC')
  GROUP BY chain, pool_id, emitter, protocol
) AS v
LEFT JOIN dex_pools_v AS p ON p.chain = v.chain AND p.pool_id = v.pool_id AND p.emitter = v.emitter
ORDER BY v.volume_usd_30d DESC NULLS LAST;
