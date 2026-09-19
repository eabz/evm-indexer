-- Analyst views over the DEX tables. Plain views: nothing is stored, tokens
-- and pools are joined at QUERY time, so rows get better as the background
-- resolvers fill tokens / dex_pools.
--
-- Reorg safety comes from what they read, nothing here knows about epochs or
-- tombstones: base tables only with FINAL (hides tombstones), pools only
-- through dex_pool_current_v, aggregates only through their *_v views
-- (validity rule of docs/design.md §2).
--
-- Forgery safety - "a wrong number is worse than a missing one":
--   every USD number is built ONLY on verified swap legs (dex_swaps
--     .verified_in / verified_out: the token itself reported the transfer of
--     exactly that amount to / from the emitter in the same transaction).
--     What an event merely claims (amounts without a transfer, token_in /
--     token_out of a Balancer shaped event) is never valued,
--   swaps of the singleton families (uniswap_v4, balancer_v2) are valued
--     only when their emitter is listed in dex_trusted_emitters,
--   pool metadata (dex_pool_current_v) is used only when trusted = 1, and
--     only for what verified legs do not already say,
--   unknown stays NULL: unknown tokens, unknown decimals, unverified legs
--     and unpriceable amounts are NULL, never 0.
-- ONE valuation rule everywhere: a swap is valued once, by its best verified
-- leg - a 'stable' quote token (worth 1) before a 'native' one (worth the
-- hourly native price) - and every rollup is a sum of those values.
-- *_adj columns are decimals adjusted Float64 (~15.9 significant digits),
-- raw columns stay exact UInt256 / Int256.

-- Symbol / decimals / quote kind of a token: tokens first, quote_tokens as
-- the fallback for pseudo addresses that have no contract. A tokens row
-- with no name, no symbol and 0 decimals is the "checked, nothing there"
-- row of the token resolver: its decimals are UNKNOWN, not 0.
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
  SELECT chain, address AS token, symbol AS info_symbol, name AS info_name, if(name = '' AND symbol = '' AND decimals = 0, NULL, toNullable(decimals)) AS info_decimals, '' AS info_kind, 1 AS origin
  FROM tokens FINAL
  UNION ALL
  SELECT chain, token, symbol AS info_symbol, '' AS info_name, decimals AS info_decimals, toString(kind) AS info_kind, 0 AS origin
  FROM quote_tokens FINAL
  WHERE kind IN ('stable', 'native')
)
GROUP BY chain, token;

-- Emitters whose singleton family swaps may be valued.
CREATE VIEW IF NOT EXISTS dex_trusted_emitters_v AS
SELECT chain, emitter, toString(protocol) AS protocol, price_source
FROM dex_trusted_emitters FINAL
WHERE protocol != '';

-- Pools with symbols and decimals. Token columns of a pool that is not
-- trusted (status 'unverified' / 'contested') are what an event CLAIMS: they
-- are shown, their symbols / decimals are not.
CREATE VIEW IF NOT EXISTS dex_pools_v AS
SELECT
  p.chain AS chain,
  p.pool_id AS pool_id,
  p.emitter AS emitter,
  concat('0x', lower(hex(p.pool_id))) AS pool,
  p.status AS status,
  p.trusted AS trusted,
  p.candidates AS candidates,
  p.protocol AS protocol,
  p.factory AS factory,
  p.token0 AS token0,
  p.token1 AS token1,
  if(p.trusted AND length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t0.symbol, '') AS symbol0,
  if(p.trusted AND length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t1.symbol, '') AS symbol1,
  if(p.trusted AND length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t0.decimals, NULL) AS decimals0,
  if(p.trusted AND length(p.tokens) = 2 AND p.protocol NOT IN ('balancer_v2', 'curve'), t1.decimals, NULL) AS decimals1,
  p.tokens AS tokens,
  p.underlying_tokens AS underlying_tokens,
  p.fee AS fee,
  p.tick_spacing AS tick_spacing,
  p.hooks AS hooks,
  p.stable AS stable,
  p.created_block AS created_block,
  p.timestamp AS created_at,
  p.source AS source
FROM dex_pool_current_v AS p
LEFT JOIN dex_token_info_v AS t0 ON t0.chain = p.chain AND t0.token = p.token0
LEFT JOIN dex_token_info_v AS t1 ON t1.chain = p.chain AND t1.token = p.token1;

-- USD price of the native coin per COMPLETE hour. Built only from swaps
-- with BOTH legs verified, one a 'native' and one a 'stable' quote token,
-- of contract pools or trusted singleton emitters. Per pool the hour's
-- volume weighted price, pools with less than 1000 stable units of volume
-- in the hour do not vote, and the price is the MEDIAN over the pools: one
-- manipulated pool can not move it. When the chain has price_source rows in
-- dex_trusted_emitters only those pools vote (a median can still be
-- outvoted by many wash traded fake pools - listing the real ones is the
-- only registry free system's honest answer to that).
-- valid_from = end of the hour: a price is only ever applied to LATER hours
-- (no look ahead into a running bucket), and for at most 24 hours.
CREATE VIEW IF NOT EXISTS dex_native_price_1h_v AS
SELECT
  chain,
  bucket,
  bucket + 3600 AS valid_from,
  quantileExact(0.5)(pool_price) AS price,
  toUInt64(count()) AS pools,
  sum(pool_stable) AS stable_volume
FROM
(
  SELECT
    v.chain AS chain, v.bucket AS bucket, v.pool_id AS pool_id, v.emitter AS emitter,
    sum(if(qi.kind = 'stable', v.volume_in / pow(10, qi.decimals), v.volume_out / pow(10, qo.decimals))) AS pool_stable,
    sum(if(qi.kind = 'native', v.volume_in / pow(10, qi.decimals), v.volume_out / pow(10, qo.decimals))) AS pool_native,
    pool_stable / pool_native AS pool_price
  FROM dex_pool_volume_1h_v AS v
  INNER JOIN dex_token_info_v AS qi ON qi.chain = v.chain AND qi.token = v.token_in
  INNER JOIN dex_token_info_v AS qo ON qo.chain = v.chain AND qo.token = v.token_out
  LEFT JOIN (SELECT chain AS e_chain, emitter AS e_emitter, protocol AS e_protocol, price_source AS e_price_source FROM dex_trusted_emitters_v) AS e ON e.e_chain = v.chain AND e.e_emitter = v.emitter
  LEFT JOIN (SELECT chain, 1 AS restricted FROM dex_trusted_emitters_v WHERE price_source = 1 GROUP BY chain) AS r ON r.chain = v.chain
  WHERE v.token_in != toFixedString('', 20) AND v.token_out != toFixedString('', 20)
    AND ((qi.kind = 'native' AND qo.kind = 'stable') OR (qi.kind = 'stable' AND qo.kind = 'native'))
    AND qi.decimals IS NOT NULL AND qo.decimals IS NOT NULL
    AND (v.protocol NOT IN ('uniswap_v4', 'balancer_v2') OR ifNull(e.e_protocol, '') != '')
    AND (ifNull(r.restricted, 0) = 0 OR ifNull(e.e_price_source, 0) = 1)
  GROUP BY chain, bucket, pool_id, emitter
  HAVING pool_stable >= 1000 AND pool_native > 0
)
GROUP BY chain, bucket;

-- Every swap as token_in / token_out / amount_in / amount_out, whatever the
-- family. A token is, in this order: the VERIFIED token of the leg
-- (token_*_verified = 1), what the event names (Balancer), or what a TRUSTED
-- pool row says (Curve coin index, token0 / token1 by the sign of amount0).
-- token_*_known = 0: nothing says which token it is (all zero bytes).
-- quote_in / quote_out are only set for verified legs of swaps that may be
-- valued (contract pools, or a trusted singleton emitter).
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
  r.reserve0 AS reserve0,
  r.reserve1 AS reserve1,
  r.pool_trusted AS pool_trusted,
  r.valued AS emitter_valued,
  r.r_token_in AS token_in,
  r.r_token_out AS token_out,
  r.r_in_known AS token_in_known,
  r.r_out_known AS token_out_known,
  r.in_verified AS token_in_verified,
  r.out_verified AS token_out_verified,
  r.amount_in AS amount_in,
  r.amount_out AS amount_out,
  if(r.r_in_known, ti.symbol, '') AS symbol_in,
  if(r.r_out_known, tout.symbol, '') AS symbol_out,
  if(r.r_in_known, ti.decimals, NULL) AS decimals_in,
  if(r.r_out_known, tout.decimals, NULL) AS decimals_out,
  if(r.in_verified AND r.valued, ti.kind, '') AS quote_in,
  if(r.out_verified AND r.valued, tout.kind, '') AS quote_out,
  toFloat64(r.amount_in) / pow(10, if(r.r_in_known, ti.decimals, NULL)) AS amount_in_adj,
  toFloat64(r.amount_out) / pow(10, if(r.r_out_known, tout.decimals, NULL)) AS amount_out_adj
FROM
(
  SELECT
    s.*,
    toFixedString('', 20) AS zero,
    ifNull(p.trusted, 0) = 1 AS pool_trusted,
    (s.protocol NOT IN ('uniswap_v4', 'balancer_v2') OR ifNull(e.e_protocol, '') != '') AS valued,
    s.verified_in != zero AS in_verified,
    s.verified_out != zero AS out_verified,
    if(s.underlying, p.underlying_tokens, p.tokens) AS coins,
    s.protocol NOT IN ('balancer_v2', 'curve') AND (s.amount0 != 0 OR s.amount1 != 0) AS sided,
    multiIf(in_verified, s.verified_in, s.token_in != zero, s.token_in, NOT pool_trusted, zero, s.protocol = 'curve', arrayElement(coins, s.coin_in + 1), s.amount0 > 0, p.token0, p.token1) AS r_token_in,
    multiIf(out_verified, s.verified_out, s.token_out != zero, s.token_out, NOT pool_trusted, zero, s.protocol = 'curve', arrayElement(coins, s.coin_out + 1), s.amount0 > 0, p.token1, p.token0) AS r_token_out,
    multiIf(in_verified, 1, s.token_in != zero, 1, NOT pool_trusted, 0, s.protocol = 'curve', s.coin_in < length(coins), sided) AS r_in_known,
    multiIf(out_verified, 1, s.token_out != zero, 1, NOT pool_trusted, 0, s.protocol = 'curve', s.coin_out < length(coins), sided) AS r_out_known
  FROM (SELECT * FROM dex_swaps FINAL) AS s
  LEFT JOIN
  (
    SELECT chain AS p_chain, pool_id AS p_pool_id, emitter AS p_emitter, token0, token1, tokens, underlying_tokens, trusted
    FROM dex_pool_current_v
  ) AS p ON p.p_chain = s.chain AND p.p_pool_id = s.pool_id AND p.p_emitter = s.emitter
  LEFT JOIN (SELECT chain AS e_chain, emitter AS e_emitter, protocol AS e_protocol, price_source AS e_price_source FROM dex_trusted_emitters_v) AS e ON e.e_chain = s.chain AND e.e_emitter = s.emitter
) AS r
LEFT JOIN dex_token_info_v AS ti ON ti.chain = r.chain AND ti.token = r.r_token_in
LEFT JOIN dex_token_info_v AS tout ON tout.chain = r.chain AND tout.token = r.r_token_out;

-- THE valuation rule, swap level: a verified stable leg (in before out),
-- else a verified native leg (in before out) at the native price in force
-- for the swap's hour, else NULL. native_price is the last complete hour's
-- price, at most 24 hours old.
CREATE VIEW IF NOT EXISTS dex_swaps_usd_v AS
SELECT
  s.*,
  if(n.price > 0 AND s.hour - n.valid_from <= 86400, n.price, NULL) AS native_price,
  coalesce(
    if(s.quote_in = 'stable', s.amount_in_adj, NULL),
    if(s.quote_out = 'stable', s.amount_out_adj, NULL),
    if(s.quote_in = 'native', s.amount_in_adj * native_price, NULL),
    if(s.quote_out = 'native', s.amount_out_adj * native_price, NULL)
  ) AS amount_usd
FROM (SELECT *, toDateTime(intDiv(toUInt32(timestamp), 3600) * 3600, 'UTC') AS hour FROM dex_swaps_v) AS s
ASOF LEFT JOIN dex_native_price_1h_v AS n ON n.chain = s.chain AND n.valid_from <= s.hour;

-- The same rule on the hourly aggregate: all swaps of a row share their
-- verified tokens and their hour, so volume_usd of a row IS the sum of the
-- amount_usd of its swaps. priced_swaps / swaps says how much of the
-- activity could be valued.
CREATE VIEW IF NOT EXISTS dex_pool_volume_usd_1h_v AS
SELECT
  v.chain AS chain, v.pool_id AS pool_id, v.emitter AS emitter, v.protocol AS protocol, v.bucket AS bucket,
  v.token_in AS token_in, v.token_out AS token_out,
  v.volume_in AS volume_in, v.volume_out AS volume_out,
  v.volume_in / pow(10, if(v.token_in != toFixedString('', 20), qi.decimals, NULL)) AS volume_in_adj,
  v.volume_out / pow(10, if(v.token_out != toFixedString('', 20), qo.decimals, NULL)) AS volume_out_adj,
  (v.protocol NOT IN ('uniswap_v4', 'balancer_v2') OR ifNull(e.e_protocol, '') != '') AS valued,
  if(valued AND v.token_in != toFixedString('', 20), qi.kind, '') AS quote_in,
  if(valued AND v.token_out != toFixedString('', 20), qo.kind, '') AS quote_out,
  if(n.price > 0 AND v.bucket - n.valid_from <= 86400, n.price, NULL) AS native_price,
  coalesce(
    if(quote_in = 'stable', volume_in_adj, NULL),
    if(quote_out = 'stable', volume_out_adj, NULL),
    if(quote_in = 'native', volume_in_adj * native_price, NULL),
    if(quote_out = 'native', volume_out_adj * native_price, NULL)
  ) AS volume_usd,
  v.swaps AS swaps,
  if(volume_usd IS NULL, 0, v.swaps) AS priced_swaps
FROM dex_pool_volume_1h_v AS v
LEFT JOIN dex_token_info_v AS qi ON qi.chain = v.chain AND qi.token = v.token_in
LEFT JOIN dex_token_info_v AS qo ON qo.chain = v.chain AND qo.token = v.token_out
LEFT JOIN (SELECT chain AS e_chain, emitter AS e_emitter, protocol AS e_protocol, price_source AS e_price_source FROM dex_trusted_emitters_v) AS e ON e.e_chain = v.chain AND e.e_emitter = v.emitter
ASOF LEFT JOIN dex_native_price_1h_v AS n ON n.chain = v.chain AND n.valid_from <= v.bucket;

-- Daily USD volume per pool: the sum of the hourly values. NULL when no swap
-- of the day could be valued, never 0.
CREATE VIEW IF NOT EXISTS dex_pool_volume_usd_1d_v AS
SELECT
  u.chain AS chain, u.pool_id AS pool_id, u.emitter AS emitter, u.protocol AS protocol, u.day AS bucket,
  u.day_usd AS volume_usd,
  t.swaps AS swaps,
  u.day_priced AS priced_swaps,
  t.traders AS traders
FROM
(
  SELECT
    chain, pool_id, emitter, protocol,
    toDateTime(intDiv(toUInt32(bucket), 86400) * 86400, 'UTC') AS day,
    sum(volume_usd) AS day_usd,
    toUInt64(sum(priced_swaps)) AS day_priced
  FROM dex_pool_volume_usd_1h_v
  GROUP BY chain, pool_id, emitter, protocol, day
) AS u
INNER JOIN dex_pool_stats_1d_v AS t ON t.chain = u.chain AND t.pool_id = u.pool_id AND t.emitter = u.emitter AND t.protocol = u.protocol AND t.day = u.day;

-- Daily activity and USD volume per protocol. The protocol is the one of the
-- POOL when it is trusted (its creation event or its own getters), else the
-- family of the swap event: families that share a topic0 (the V2 Swap is
-- also emitted by Solidly V1 forks - Velodrome V1, Thena, Ramses...) are
-- attributed by their pool, not by the event.
CREATE VIEW IF NOT EXISTS dex_protocol_stats_1d_v AS
SELECT
  a.chain AS chain,
  if(ifNull(p.trusted, 0) = 1, p.protocol, toString(a.protocol)) AS protocol,
  toDateTime(intDiv(toUInt32(a.bucket), 86400) * 86400, 'UTC') AS bucket,
  toUInt64(sum(a.swaps)) AS swaps,
  uniqMerge(a.traders) AS traders,
  uniq(a.pool_id, a.emitter) AS pools
FROM dex_pool_volume_1h AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
LEFT JOIN (SELECT chain, pool_id, emitter, protocol, trusted FROM dex_pool_current_v) AS p ON p.chain = a.chain AND p.pool_id = a.pool_id AND p.emitter = a.emitter
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, protocol, bucket;

CREATE VIEW IF NOT EXISTS dex_protocol_volume_usd_1d_v AS
SELECT
  s.chain AS chain, s.protocol AS protocol, s.bucket AS bucket,
  v.protocol_usd AS volume_usd,
  ifNull(v.protocol_priced, 0) AS priced_swaps,
  s.swaps AS swaps,
  s.pools AS pools,
  s.traders AS traders
FROM dex_protocol_stats_1d_v AS s
LEFT JOIN
(
  SELECT
    u.chain AS chain,
    if(ifNull(p.trusted, 0) = 1, p.protocol, toString(u.protocol)) AS pool_protocol,
    u.bucket AS bucket,
    sum(u.volume_usd) AS protocol_usd,
    toUInt64(sum(u.priced_swaps)) AS protocol_priced
  FROM dex_pool_volume_usd_1d_v AS u
  LEFT JOIN (SELECT chain, pool_id, emitter, protocol, trusted FROM dex_pool_current_v) AS p ON p.chain = u.chain AND p.pool_id = u.pool_id AND p.emitter = u.emitter
  GROUP BY chain, pool_protocol, bucket
) AS v ON v.chain = s.chain AND v.pool_protocol = s.protocol AND v.bucket = s.bucket;

-- Daily volume per token, VERIFIED legs only. volume / volume_adj: what
-- moved of the token itself. volume_usd: the USD value of the swaps the
-- token took part in - the FULL value of a swap is attributed to both of
-- its tokens, so summing volume_usd over tokens counts every swap twice.
CREATE VIEW IF NOT EXISTS dex_token_volume_1d_v AS
SELECT
  chain,
  tupleElement(side, 1) AS token,
  toDateTime(intDiv(toUInt32(bucket), 86400) * 86400, 'UTC') AS day,
  any(tupleElement(side, 2)) AS symbol,
  sum(tupleElement(side, 3)) AS volume,
  sum(tupleElement(side, 4)) AS volume_adj,
  sum(volume_usd) AS volume_usd,
  toUInt64(sum(swaps)) AS swaps,
  uniq(pool_id, emitter) AS pools
FROM
(
  SELECT
    u.chain AS chain, u.bucket AS bucket, u.pool_id AS pool_id, u.emitter AS emitter, u.volume_usd AS volume_usd, u.swaps AS swaps,
    arrayJoin(arrayFilter(x -> tupleElement(x, 1) != toFixedString('', 20), [(u.token_in, qi.symbol, u.volume_in, u.volume_in_adj), (u.token_out, qo.symbol, u.volume_out, u.volume_out_adj)])) AS side
  FROM dex_pool_volume_usd_1h_v AS u
  LEFT JOIN dex_token_info_v AS qi ON qi.chain = u.chain AND qi.token = u.token_in
  LEFT JOIN dex_token_info_v AS qo ON qo.chain = u.chain AND qo.token = u.token_out
  WHERE u.valued
)
GROUP BY chain, token, day;

-- Decimals adjusted candles, token1 per token0 in human units, for TRUSTED
-- pools only (decimals of a claimed token are worth nothing). The price
-- series is the pool price (sqrt price / reserves) except for Solidly
-- stable pools, whose reserves do not give the price: those use trades.
CREATE VIEW IF NOT EXISTS dex_pool_prices_1m_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS scale,
  if(p.stable OR c.pool_close IS NULL, 'trades', 'pool') AS price_source,
  if(price_source = 'pool', c.pool_open, c.open) * scale AS open,
  if(price_source = 'pool', c.pool_high, c.high) * scale AS high,
  if(price_source = 'pool', c.pool_low, c.low) * scale AS low,
  if(price_source = 'pool', c.pool_close, c.close) * scale AS close,
  c.volume0 / pow(10, p.decimals0) AS volume0_adj,
  c.volume1 / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1m_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter
WHERE p.trusted;

CREATE VIEW IF NOT EXISTS dex_pool_prices_1h_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS scale,
  if(p.stable OR c.pool_close IS NULL, 'trades', 'pool') AS price_source,
  if(price_source = 'pool', c.pool_open, c.open) * scale AS open,
  if(price_source = 'pool', c.pool_high, c.high) * scale AS high,
  if(price_source = 'pool', c.pool_low, c.low) * scale AS low,
  if(price_source = 'pool', c.pool_close, c.close) * scale AS close,
  c.volume0 / pow(10, p.decimals0) AS volume0_adj,
  c.volume1 / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1h_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter
WHERE p.trusted;

CREATE VIEW IF NOT EXISTS dex_pool_prices_1d_v AS
SELECT
  c.chain AS chain, c.pool_id AS pool_id, c.emitter AS emitter, c.bucket AS bucket,
  p.protocol AS protocol, p.token0 AS token0, p.token1 AS token1, p.symbol0 AS symbol0, p.symbol1 AS symbol1,
  pow(10, toInt32(p.decimals0) - toInt32(p.decimals1)) AS scale,
  if(p.stable OR c.pool_close IS NULL, 'trades', 'pool') AS price_source,
  if(price_source = 'pool', c.pool_open, c.open) * scale AS open,
  if(price_source = 'pool', c.pool_high, c.high) * scale AS high,
  if(price_source = 'pool', c.pool_low, c.low) * scale AS low,
  if(price_source = 'pool', c.pool_close, c.close) * scale AS close,
  c.volume0 / pow(10, p.decimals0) AS volume0_adj,
  c.volume1 / pow(10, p.decimals1) AS volume1_adj,
  c.swaps AS swaps,
  c.traders AS traders
FROM dex_candles_1d_v AS c
INNER JOIN dex_pools_v AS p ON p.chain = c.chain AND p.pool_id = c.pool_id AND p.emitter = c.emitter
WHERE p.trusted;

-- Pools ranked by USD volume of the trailing 30 days.
CREATE VIEW IF NOT EXISTS dex_top_pools_v AS
SELECT
  v.chain AS chain, v.pool_id AS pool_id, v.emitter AS emitter, v.protocol AS protocol,
  p.pool AS pool, p.status AS status, p.symbol0 AS symbol0, p.symbol1 AS symbol1, p.tokens AS tokens,
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
