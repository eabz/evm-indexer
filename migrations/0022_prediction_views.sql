-- Prediction markets: the views a trading UI reads (docs/design.md,
-- section 10). One query per screen, no client side joins or math - the
-- cookbook with every query is src/predictions/README.md.
--
-- Two kinds of views:
--   * prediction_markets_v: one row per market, kept by ClickHouse itself
--     (a refreshable materialized view recomputes prediction_market_list
--     from the live view every minute and swaps it in atomically). Lists,
--     search and the market header are a plain scan / primary key lookup.
--   * parameterized views - SELECT ... FROM view(chain = 137, ...) - for
--     everything scoped to one token, one market or one wallet. The token
--     id <-> market mapping is resolved at QUERY time (it heals itself when
--     indexing starts mid chain), and a parameter is the only way to push
--     "which market / wallet" down into every subquery, so each of them is
--     a primary key range read.
--
-- Amounts: *_raw columns are the on chain integers as Float64, the others
-- are divided by 10^decimals of the collateral token (shares of a CTF
-- position use the unit of their collateral). They are NULL while the
-- token worker has not stored the collateral's decimals - never guessed.
-- Prices are probabilities: collateral per share, 0..1.

CREATE VIEW IF NOT EXISTS prediction_candles_1m_v AS
WITH (
  SELECT any(toNullable(decimals)) FROM tokens FINAL
  WHERE chain = {chain:UInt64} AND address IN (
    SELECT collateral_token FROM prediction_outcome_tokens FINAL
    WHERE chain = {chain:UInt64} AND registry = {registry:FixedString(20)} AND outcome_token_id = {outcome_token_id:UInt256})
) AS collateral_decimals
SELECT
  a.chain AS chain, a.registry AS registry, a.outcome_token_id AS outcome_token_id, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume)) / pow(10, collateral_decimals) AS volume,
  toFloat64(sum(a.shares)) / pow(10, collateral_decimals) AS shares,
  toFloat64(sum(a.volume)) AS volume_raw,
  toUInt64(sum(a.trades)) AS trades,
  uniqMerge(a.traders) AS traders
FROM prediction_candles_1m AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.chain = {chain:UInt64} AND a.registry = {registry:FixedString(20)} AND a.outcome_token_id = {outcome_token_id:UInt256}
  AND a.epoch >= f.epoch_floor
GROUP BY chain, registry, outcome_token_id, bucket;

CREATE VIEW IF NOT EXISTS prediction_candles_1h_v AS
WITH (
  SELECT any(toNullable(decimals)) FROM tokens FINAL
  WHERE chain = {chain:UInt64} AND address IN (
    SELECT collateral_token FROM prediction_outcome_tokens FINAL
    WHERE chain = {chain:UInt64} AND registry = {registry:FixedString(20)} AND outcome_token_id = {outcome_token_id:UInt256})
) AS collateral_decimals
SELECT
  a.chain AS chain, a.registry AS registry, a.outcome_token_id AS outcome_token_id, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume)) / pow(10, collateral_decimals) AS volume,
  toFloat64(sum(a.shares)) / pow(10, collateral_decimals) AS shares,
  toFloat64(sum(a.volume)) AS volume_raw,
  toUInt64(sum(a.trades)) AS trades,
  uniqMerge(a.traders) AS traders
FROM prediction_candles_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.chain = {chain:UInt64} AND a.registry = {registry:FixedString(20)} AND a.outcome_token_id = {outcome_token_id:UInt256}
  AND a.epoch >= f.epoch_floor
GROUP BY chain, registry, outcome_token_id, bucket;

CREATE VIEW IF NOT EXISTS prediction_candles_1d_v AS
WITH (
  SELECT any(toNullable(decimals)) FROM tokens FINAL
  WHERE chain = {chain:UInt64} AND address IN (
    SELECT collateral_token FROM prediction_outcome_tokens FINAL
    WHERE chain = {chain:UInt64} AND registry = {registry:FixedString(20)} AND outcome_token_id = {outcome_token_id:UInt256})
) AS collateral_decimals
SELECT
  a.chain AS chain, a.registry AS registry, a.outcome_token_id AS outcome_token_id, a.bucket AS bucket,
  argMinMerge(a.open) AS open,
  toFloat64(max(a.high)) AS high,
  toFloat64(min(a.low)) AS low,
  argMaxMerge(a.close) AS close,
  toFloat64(sum(a.volume)) / pow(10, collateral_decimals) AS volume,
  toFloat64(sum(a.shares)) / pow(10, collateral_decimals) AS shares,
  toFloat64(sum(a.volume)) AS volume_raw,
  toUInt64(sum(a.trades)) AS trades,
  uniqMerge(a.traders) AS traders
FROM prediction_candles_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.chain = {chain:UInt64} AND a.registry = {registry:FixedString(20)} AND a.outcome_token_id = {outcome_token_id:UInt256}
  AND a.epoch >= f.epoch_floor
GROUP BY chain, registry, outcome_token_id, bucket;

-- Every market, computed from scratch. Correct at any instant and the
-- definition of every column of prediction_markets_v, but it reads every
-- market: consumers use prediction_markets_v.
--
-- A market exists as soon as its ConditionPreparation OR a split / merge
-- / redemption of it was seen (indexing from the middle of the chain: the
-- preparation is older than the first indexed block, oracle / question /
-- title are then unknown).
CREATE VIEW IF NOT EXISTS prediction_markets_live_v AS
WITH
token_map AS (
  SELECT chain, registry, market_id, collateral_token, outcome_index, outcome_token_id, first_seen_block
  FROM prediction_outcome_tokens_by_market FINAL
),
-- A condition split against several collaterals has one position set per
-- collateral. The first one seen is the market's.
primary_collateral AS (
  SELECT chain, registry, market_id, argMin(collateral_token, first_seen_block) AS collateral_token
  FROM token_map
  GROUP BY chain, registry, market_id
),
token_totals AS (
  SELECT
    a.chain AS chain, a.registry AS registry, a.outcome_token_id AS outcome_token_id,
    toFloat64(sum(a.volume)) AS volume,
    toUInt64(sum(a.fills)) AS fills,
    toUInt64(sum(a.trades)) AS prints,
    argMaxMerge(a.close) AS last_price,
    max(a.last_trade_at) AS last_trade_at,
    uniqMergeState(a.traders) AS traders
  FROM prediction_candles_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.epoch >= f.epoch_floor
  GROUP BY chain, registry, outcome_token_id
),
token_day AS (
  SELECT
    a.chain AS chain, a.registry AS registry, a.outcome_token_id AS outcome_token_id,
    toFloat64(sum(a.volume)) AS volume,
    toUInt64(sum(a.fills)) AS fills
  FROM prediction_candles_1h AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.bucket >= now() - INTERVAL 24 HOUR AND a.epoch >= f.epoch_floor
  GROUP BY chain, registry, outcome_token_id
),
trading AS (
  SELECT
    m.chain AS chain, m.registry AS registry, m.market_id AS market_id, m.collateral_token AS collateral_token,
    arrayMap(x -> tupleElement(x, 2), arraySort(x -> tupleElement(x, 1), groupArray((m.outcome_index, m.outcome_token_id)))) AS outcome_token_ids,
    arrayMap(x -> tupleElement(x, 2), arraySort(x -> tupleElement(x, 1), groupArray((m.outcome_index, if(t.prints > 0, toNullable(t.last_price), NULL))))) AS outcome_prices,
    sum(t.volume) AS volume_total_raw,
    sum(d.volume) AS volume_24h_raw,
    sum(t.fills) AS trades_total,
    sum(d.fills) AS trades_24h,
    uniqMerge(t.traders) AS traders,
    max(t.last_trade_at) AS last_trade_at
  FROM token_map AS m
  INNER JOIN primary_collateral AS p ON p.chain = m.chain AND p.registry = m.registry AND p.market_id = m.market_id AND p.collateral_token = m.collateral_token
  LEFT JOIN token_totals AS t ON t.chain = m.chain AND t.registry = m.registry AND t.outcome_token_id = m.outcome_token_id
  LEFT JOIN token_day AS d ON d.chain = m.chain AND d.registry = m.registry AND d.outcome_token_id = m.outcome_token_id
  GROUP BY chain, registry, market_id, collateral_token
),
prepared AS (
  SELECT
    chain, registry, market_id,
    argMin(protocol, (block_number, log_index)) AS protocol,
    argMin(oracle, (block_number, log_index)) AS oracle,
    argMin(question_id, (block_number, log_index)) AS question_id,
    argMin(outcome_count, (block_number, log_index)) AS outcome_count,
    min(block_number) AS created_block,
    argMin(timestamp, (block_number, log_index)) AS created_at,
    argMin(transaction_hash, (block_number, log_index)) AS created_tx
  FROM prediction_markets FINAL
  GROUP BY chain, registry, market_id
),
flows AS (
  SELECT
    a.chain AS chain, a.registry AS registry, a.market_id AS market_id, a.collateral_token AS collateral_token,
    toFloat64(sum(a.split)) - toFloat64(sum(a.merged)) - toFloat64(sum(a.redeemed)) AS open_interest_raw
  FROM prediction_market_flows_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.epoch >= f.epoch_floor
  GROUP BY chain, registry, market_id, collateral_token
),
-- The token map is arithmetic and survives a reorg, so it proves nothing:
-- a market exists when a live ConditionPreparation or live collateral
-- flows (every outcome token is born in a split) say so.
market_keys AS (
  SELECT chain, registry, market_id FROM prepared
  UNION DISTINCT
  SELECT chain, registry, market_id FROM flows
),
questions AS (
  SELECT
    chain, question_id, emitter,
    argMin(toString(kind), (block_number, log_index)) AS question_kind,
    argMin(event_id, (block_number, log_index)) AS event_id,
    argMin(question_index, (block_number, log_index)) AS question_index,
    argMin(title, (block_number, log_index)) AS title,
    argMin(description, (block_number, log_index)) AS description,
    argMin(outcomes, (block_number, log_index)) AS outcomes
  FROM prediction_questions FINAL
  WHERE kind IN ('uma_question', 'neg_risk_question')
  GROUP BY chain, question_id, emitter
),
event_titles AS (
  SELECT chain, event_id, emitter, argMin(title, (block_number, log_index)) AS title
  FROM prediction_questions FINAL
  WHERE kind = 'neg_risk_event'
  GROUP BY chain, event_id, emitter
),
disputes AS (
  SELECT chain, question_id, emitter, count() AS disputes
  FROM prediction_questions FINAL
  WHERE kind IN ('uma_reset', 'uma_flagged')
  GROUP BY chain, question_id, emitter
),
resolved AS (
  SELECT
    chain, registry, market_id,
    argMax(payout_numerators, (block_number, log_index)) AS payout_numerators,
    argMax(payout_denominator, (block_number, log_index)) AS payout_denominator,
    argMax(timestamp, (block_number, log_index)) AS resolved_at,
    count() AS resolutions
  FROM prediction_resolutions FINAL
  GROUP BY chain, registry, market_id
),
collaterals AS (
  SELECT chain, address, symbol, decimals, toUInt8(1) AS known
  FROM tokens FINAL
  WHERE (chain, address) IN (SELECT chain, collateral_token FROM primary_collateral)
),
enriched AS (
  SELECT *, toUInt8(1) AS present FROM prediction_market_metadata FINAL
),
labels AS (
  SELECT chain, address, venue FROM prediction_venue_labels FINAL
)
SELECT
  k.chain AS chain,
  k.market_id AS market_id,
  k.registry AS registry,
  if(l.venue != '', l.venue, if(p.protocol != '', toString(p.protocol), 'ctf')) AS venue,
  if(p.protocol != '', toString(p.protocol), 'ctf') AS protocol,
  -- identity on chain
  p.oracle AS oracle,
  p.question_id AS question_id,
  if(p.created_block > 0, toNullable(p.created_at), NULL) AS created_at,
  p.created_block AS created_block,
  p.created_tx AS created_tx,
  -- multi outcome grouping (zero bytes / NULL when the market stands alone)
  q.event_id AS event_id,
  if(e.title != '', toNullable(e.title), md.event_title) AS event_title,
  q.question_index AS event_index,
  -- what it is about: the external enricher wins, then the chain, else NULL
  multiIf(md.title IS NOT NULL, md.title, q.title != '', toNullable(q.title), NULL) AS title,
  multiIf(md.description IS NOT NULL, md.description, q.description != '', toNullable(q.description), NULL) AS description,
  md.slug AS slug,
  md.category AS category,
  md.tags AS tags,
  md.image_url AS image_url,
  md.end_date AS end_date,
  multiIf(notEmpty(md.outcomes), md.outcomes, notEmpty(q.outcomes), q.outcomes, q.question_kind = 'neg_risk_question', ['Yes', 'No'], emptyArrayString()) AS outcomes,
  greatest(p.outcome_count, toUInt16(length(t.outcome_token_ids))) AS outcome_count,
  t.outcome_token_ids AS outcome_token_ids,
  -- current price = implied probability = last print of each outcome
  t.outcome_prices AS outcome_prices,
  if(t.last_trade_at > 0, toNullable(t.last_trade_at), NULL) AS last_trade_at,
  t.collateral_token AS collateral_token,
  if(c.known = 1, toNullable(c.symbol), NULL) AS collateral_symbol,
  if(c.known = 1, toNullable(c.decimals), NULL) AS collateral_decimals,
  t.volume_24h_raw / pow(10, collateral_decimals) AS volume_24h,
  t.volume_total_raw / pow(10, collateral_decimals) AS volume_total,
  fl.open_interest_raw / pow(10, collateral_decimals) AS open_interest,
  t.volume_24h_raw AS volume_24h_raw,
  t.volume_total_raw AS volume_total_raw,
  fl.open_interest_raw AS open_interest_raw,
  t.trades_24h AS trades_24h,
  t.trades_total AS trades_total,
  t.traders AS traders,
  -- 'open' | 'disputed' | 'resolved'
  multiIf(r.resolutions > 0, 'resolved', d.disputes > 0, 'disputed', 'open') AS status,
  r.payout_numerators AS payout_numerators,
  arrayMap(x -> toFloat64(x) / toFloat64(r.payout_denominator), r.payout_numerators) AS payouts,
  if(arrayCount(x -> x != 0, r.payout_numerators) = 1, toNullable(toUInt16(arrayFirstIndex(x -> x != 0, r.payout_numerators) - 1)), NULL) AS winning_outcome,
  if(r.resolutions > 0, toNullable(r.resolved_at), NULL) AS resolved_at,
  now() AS computed_at
FROM market_keys AS k
LEFT JOIN prepared AS p ON p.chain = k.chain AND p.registry = k.registry AND p.market_id = k.market_id
LEFT JOIN trading AS t ON t.chain = k.chain AND t.registry = k.registry AND t.market_id = k.market_id
LEFT JOIN questions AS q ON q.chain = k.chain AND q.question_id = p.question_id AND q.emitter = p.oracle
LEFT JOIN event_titles AS e ON e.chain = k.chain AND e.event_id = q.event_id AND e.emitter = q.emitter
LEFT JOIN disputes AS d ON d.chain = k.chain AND d.question_id = p.question_id AND d.emitter = p.oracle
LEFT JOIN resolved AS r ON r.chain = k.chain AND r.registry = k.registry AND r.market_id = k.market_id
LEFT JOIN flows AS fl ON fl.chain = k.chain AND fl.registry = k.registry AND fl.market_id = k.market_id AND fl.collateral_token = t.collateral_token
LEFT JOIN collaterals AS c ON c.chain = k.chain AND c.address = t.collateral_token
LEFT JOIN enriched AS md ON md.chain = k.chain AND md.market_id = k.market_id
LEFT JOIN labels AS l ON l.chain = k.chain AND l.address = k.registry;

-- The market list as a table: recomputed from prediction_markets_live_v
-- once a minute by ClickHouse (refreshable materialized view - the new
-- contents replace the old ones atomically, no DELETE involved, no
-- indexer process takes part). At most a minute behind the chain - the
-- chart and the tape of a market page are live.
CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_market_list
REFRESH EVERY 1 MINUTE
ENGINE = MergeTree
ORDER BY (chain, market_id, registry)
AS SELECT * FROM prediction_markets_live_v;

-- Market list / search / header. One row per market with outcome arrays
-- (index i of every array = outcome i).
CREATE VIEW IF NOT EXISTS prediction_markets_v AS
SELECT * FROM prediction_market_list;

-- Trades tape of a market, from the taker's point of view.
--   SELECT * FROM prediction_trades_v(chain = 137, market_id = unhex('..'))
--   ORDER BY block_number DESC, log_index DESC LIMIT 50
CREATE VIEW IF NOT EXISTS prediction_trades_v AS
WITH
mapping AS (
  SELECT registry, outcome_token_id, outcome_index
  FROM prediction_outcome_tokens_by_market FINAL
  WHERE chain = {chain:UInt64} AND market_id = {market_id:FixedString(32)}
),
market AS (
  SELECT registry, outcomes, collateral_decimals, collateral_symbol
  FROM prediction_market_list
  WHERE chain = {chain:UInt64} AND market_id = {market_id:FixedString(32)}
)
SELECT
  s.chain AS chain,
  {market_id:FixedString(32)} AS market_id,
  s.registry AS registry,
  s.timestamp AS timestamp,
  s.block_number AS block_number,
  s.log_index AS log_index,
  s.transaction_hash AS transaction_hash,
  o.outcome_index AS outcome_index,
  if(length(m.outcomes) > o.outcome_index, toNullable(m.outcomes[o.outcome_index + 1]), NULL) AS outcome,
  toString(s.side) AS side,
  toFloat64(s.collateral_amount) / toFloat64(s.share_amount) AS price,
  toFloat64(s.share_amount) / pow(10, m.collateral_decimals) AS shares,
  toFloat64(s.collateral_amount) / pow(10, m.collateral_decimals) AS collateral,
  s.share_amount AS share_amount,
  s.collateral_amount AS collateral_amount,
  m.collateral_symbol AS collateral_symbol,
  s.taker AS trader,
  s.maker AS maker,
  s.tx_from AS tx_from,
  toFloat64(s.taker_fee_amount) / pow(10, m.collateral_decimals) AS fee,
  toString(s.taker_fee_unit) AS fee_unit,
  toString(s.match_type) AS match_type,
  toString(s.protocol) AS protocol,
  s.exchange AS exchange
FROM prediction_trades_by_token AS s FINAL
INNER JOIN mapping AS o ON o.registry = s.registry AND o.outcome_token_id = s.outcome_token_id
LEFT JOIN market AS m ON m.registry = s.registry
WHERE s.chain = {chain:UInt64}
  AND (s.registry, s.outcome_token_id) IN (SELECT registry, outcome_token_id FROM mapping)
  AND s.share_amount != 0;

-- Holders of a market, per outcome.
--   SELECT * FROM prediction_holders_v(chain = 137, market_id = unhex('..'))
--   WHERE outcome_index = 0 ORDER BY shares DESC LIMIT 100
CREATE VIEW IF NOT EXISTS prediction_holders_v AS
WITH
mapping AS (
  SELECT registry, outcome_token_id, outcome_index
  FROM prediction_outcome_tokens_by_market FINAL
  WHERE chain = {chain:UInt64} AND market_id = {market_id:FixedString(32)}
),
market AS (
  SELECT registry, outcomes, outcome_prices, payouts, status, collateral_decimals
  FROM prediction_market_list
  WHERE chain = {chain:UInt64} AND market_id = {market_id:FixedString(32)}
),
ledger AS (
  SELECT
    registry, outcome_token_id, holder,
    sum(share_delta) AS balance,
    sumIf(toFloat64(shares) - if(fee_unit = 'shares', toFloat64(fee), 0.), reason = 'buy') + sumIf(toFloat64(shares), reason = 'split') AS acquired_raw,
    sumIf(toFloat64(collateral) + if(fee_unit = 'collateral', toFloat64(fee), 0.), reason = 'buy') + sumIf(toFloat64(collateral), reason = 'split') AS cost_raw,
    max(timestamp) AS last_activity_at
  FROM prediction_ledger_by_token FINAL
  WHERE chain = {chain:UInt64}
    AND (registry, outcome_token_id) IN (SELECT registry, outcome_token_id FROM mapping)
  GROUP BY registry, outcome_token_id, holder
  HAVING balance > 0
)
SELECT
  {chain:UInt64} AS chain,
  {market_id:FixedString(32)} AS market_id,
  l.registry AS registry,
  o.outcome_index AS outcome_index,
  if(length(m.outcomes) > o.outcome_index, toNullable(m.outcomes[o.outcome_index + 1]), NULL) AS outcome,
  l.outcome_token_id AS outcome_token_id,
  l.holder AS holder,
  l.balance AS balance,
  toFloat64(l.balance) / pow(10, m.collateral_decimals) AS shares,
  if(l.acquired_raw > 0, toNullable(l.cost_raw / l.acquired_raw), NULL) AS avg_entry_price,
  if(length(m.outcome_prices) > o.outcome_index, m.outcome_prices[o.outcome_index + 1], NULL) AS current_price,
  shares * current_price AS value,
  l.last_activity_at AS last_activity_at
FROM ledger AS l
INNER JOIN mapping AS o ON o.registry = l.registry AND o.outcome_token_id = l.outcome_token_id
LEFT JOIN market AS m ON m.registry = l.registry;

-- Portfolio of a wallet: one row per outcome token it ever held or traded.
--   SELECT * FROM prediction_positions_v(chain = 137, holder = unhex('..'))
--   WHERE balance > 0 ORDER BY value DESC
--
-- balance is EXACT (Int256 sum of every ERC-1155 transfer leg). The money
-- columns use the average cost method over everything that has a price:
--   acquisitions = buys (fee included) + splits (a full set costs 1, so
--                  each of its n outcomes costs 1 / n)
--   disposals    = sells (fee deducted) + merges (1 / n each)
--                  + redemptions (at the payout of the outcome)
--   avg_entry_price = cost of acquisitions / shares acquired
--   realized_pnl    = proceeds of disposals - avg_entry_price * shares disposed
--   unrealized_pnl  = (mark - avg_entry_price) * shares held, mark = the
--                     payout once resolved, else the last price
-- Shares that arrived or left WITHOUT a price (wallet to wallet, escrow,
-- NegRisk conversions) change balance but never the cost basis, they are
-- reported in unpriced_shares. avg_entry_price / pnl are NULL when the
-- wallet never acquired the token at a price.
CREATE VIEW IF NOT EXISTS prediction_positions_v AS
WITH
ledger AS (
  SELECT
    registry, outcome_token_id,
    sum(share_delta) AS balance,
    sumIf(toFloat64(shares) - if(fee_unit = 'shares', toFloat64(fee), 0.), reason = 'buy') + sumIf(toFloat64(shares), reason = 'split') AS acquired_raw,
    sumIf(toFloat64(collateral) + if(fee_unit = 'collateral', toFloat64(fee), 0.), reason = 'buy') + sumIf(toFloat64(collateral), reason = 'split') AS cost_raw,
    sumIf(toFloat64(shares), reason IN ('sell', 'merge')) AS disposed_raw,
    sumIf(toFloat64(collateral) - if(fee_unit = 'collateral', toFloat64(fee), 0.), reason = 'sell') + sumIf(toFloat64(collateral), reason = 'merge') AS proceeds_raw,
    sumIf(toFloat64(shares), reason = 'redeem') AS redeemed_raw,
    sumIf(toFloat64(shares), reason = 'transfer' AND share_delta > 0) - sumIf(toFloat64(shares), reason = 'transfer' AND share_delta < 0) AS unpriced_raw,
    countIf(reason IN ('buy', 'sell')) AS trades,
    min(timestamp) AS first_activity_at,
    max(timestamp) AS last_activity_at
  FROM prediction_ledger_by_holder FINAL
  WHERE chain = {chain:UInt64} AND holder = {holder:FixedString(20)}
  GROUP BY registry, outcome_token_id
),
mapping AS (
  SELECT registry, outcome_token_id, market_id, outcome_index
  FROM prediction_outcome_tokens FINAL
  WHERE chain = {chain:UInt64}
    AND (registry, outcome_token_id) IN (SELECT registry, outcome_token_id FROM ledger)
),
markets AS (
  SELECT market_id, registry, venue, title, event_title, outcomes, outcome_prices, payouts, status, end_date, collateral_symbol, collateral_decimals
  FROM prediction_market_list
  WHERE chain = {chain:UInt64}
    AND (market_id, registry) IN (SELECT market_id, registry FROM mapping)
)
SELECT
  {chain:UInt64} AS chain,
  {holder:FixedString(20)} AS holder,
  o.market_id AS market_id,
  l.registry AS registry,
  m.venue AS venue,
  m.title AS title,
  m.event_title AS event_title,
  m.status AS status,
  m.end_date AS end_date,
  o.outcome_index AS outcome_index,
  if(length(m.outcomes) > o.outcome_index, toNullable(m.outcomes[o.outcome_index + 1]), NULL) AS outcome,
  l.outcome_token_id AS outcome_token_id,
  l.balance AS balance,
  toFloat64(l.balance) / pow(10, m.collateral_decimals) AS shares,
  if(l.acquired_raw > 0, toNullable(l.cost_raw / l.acquired_raw), NULL) AS avg_entry_price,
  if(length(m.outcome_prices) > o.outcome_index, m.outcome_prices[o.outcome_index + 1], NULL) AS current_price,
  if(m.status = 'resolved' AND length(m.payouts) > o.outcome_index, toNullable(m.payouts[o.outcome_index + 1]), NULL) AS payout,
  coalesce(payout, current_price) AS mark_price,
  shares * mark_price AS value,
  (mark_price - avg_entry_price) * shares AS unrealized_pnl,
  (l.proceeds_raw + l.redeemed_raw * coalesce(payout, 0.) - avg_entry_price * (l.disposed_raw + l.redeemed_raw)) / pow(10, m.collateral_decimals) AS realized_pnl,
  if(m.status = 'resolved', shares * payout, toNullable(0.)) AS redeemable,
  l.cost_raw / pow(10, m.collateral_decimals) AS cost,
  l.proceeds_raw / pow(10, m.collateral_decimals) AS proceeds,
  l.unpriced_raw / pow(10, m.collateral_decimals) AS unpriced_shares,
  m.collateral_symbol AS collateral_symbol,
  l.trades AS trades,
  l.first_activity_at AS first_activity_at,
  l.last_activity_at AS last_activity_at
FROM ledger AS l
INNER JOIN mapping AS o ON o.registry = l.registry AND o.outcome_token_id = l.outcome_token_id
LEFT JOIN markets AS m ON m.registry = l.registry AND m.market_id = o.market_id;

-- Activity of a wallet: its trades (at its own price and side, whether it
-- was the maker or the taker), splits, merges, redemptions and transfers.
--   SELECT * FROM prediction_activity_v(chain = 137, holder = unhex('..'))
--   WHERE action IN ('buy', 'sell') ORDER BY block_number DESC, log_index DESC LIMIT 50
CREATE VIEW IF NOT EXISTS prediction_activity_v AS
WITH
ledger AS (
  SELECT *
  FROM prediction_ledger_by_holder FINAL
  WHERE chain = {chain:UInt64} AND holder = {holder:FixedString(20)}
    AND reason != 'trade'
),
mapping AS (
  SELECT registry, outcome_token_id, market_id, outcome_index
  FROM prediction_outcome_tokens FINAL
  WHERE chain = {chain:UInt64}
    AND (registry, outcome_token_id) IN (SELECT registry, outcome_token_id FROM ledger)
),
markets AS (
  SELECT market_id, registry, title, outcomes, collateral_symbol, collateral_decimals
  FROM prediction_market_list
  WHERE chain = {chain:UInt64}
    AND (market_id, registry) IN (SELECT market_id, registry FROM mapping)
)
SELECT
  l.chain AS chain,
  l.holder AS holder,
  l.timestamp AS timestamp,
  l.block_number AS block_number,
  l.log_index AS log_index,
  l.transaction_hash AS transaction_hash,
  o.market_id AS market_id,
  l.registry AS registry,
  m.title AS title,
  o.outcome_index AS outcome_index,
  if(length(m.outcomes) > o.outcome_index, toNullable(m.outcomes[o.outcome_index + 1]), NULL) AS outcome,
  -- 'buy' | 'sell' | 'split' | 'merge' | 'redeem' | 'transfer'
  toString(l.reason) AS action,
  if(l.reason IN ('buy', 'sell'), if(l.leg = 0, 'maker', 'taker'), '') AS role,
  if(l.collateral > 0 AND l.shares > 0, toNullable(toFloat64(l.collateral) / toFloat64(l.shares)), NULL) AS price,
  toFloat64(l.shares) / pow(10, m.collateral_decimals) AS shares,
  toFloat64(l.collateral) / pow(10, m.collateral_decimals) AS collateral,
  toFloat64(l.fee) / pow(10, m.collateral_decimals) AS fee,
  toString(l.fee_unit) AS fee_unit,
  m.collateral_symbol AS collateral_symbol,
  l.counterparty AS counterparty
FROM ledger AS l
INNER JOIN mapping AS o ON o.registry = l.registry AND o.outcome_token_id = l.outcome_token_id
LEFT JOIN markets AS m ON m.registry = l.registry AND m.market_id = o.market_id;

-- Leaderboard of a period (whole UTC days, both ends included).
--   SELECT * FROM prediction_leaderboard_v(chain = 137, from_day = '2026-09-01', to_day = '2026-09-18')
--   ORDER BY volume DESC LIMIT 100
--
--   volume        = collateral the trader paid + received in trades
--   net_cash_flow = sold + merged + redeemed - bought - split - fees: what
--     the period put into the trader's pocket. It IS the realized profit of
--     everything opened and closed inside the period. Positions still open
--     at the end count at cost (their value is in prediction_positions_v).
-- Addresses labelled in prediction_venue_labels (exchanges, adapters) are
-- not traders. Amounts need the decimals of the collateral: trades take
-- them from prediction_venues (exchange -> collateral token) and tokens.
-- unpriced_trades counts the trades that could not be converted yet.
CREATE VIEW IF NOT EXISTS prediction_leaderboard_v AS
WITH
trading AS (
  SELECT
    a.trader AS trader, a.exchange AS exchange,
    toFloat64(sum(a.bought)) AS bought, toFloat64(sum(a.sold)) AS sold,
    toFloat64(sum(a.fees)) AS fees, toUInt64(sum(a.trades)) AS trades,
    uniqMergeState(a.tokens) AS tokens
  FROM prediction_trader_trades_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.chain = {chain:UInt64}
    AND a.bucket >= toDateTime({from_day:Date}, 'UTC') AND a.bucket <= toDateTime({to_day:Date}, 'UTC')
    AND a.epoch >= f.epoch_floor
  GROUP BY trader, exchange
),
funding AS (
  SELECT
    a.trader AS trader, a.collateral_token AS collateral_token,
    toFloat64(sum(a.split)) AS split, toFloat64(sum(a.merged)) AS merged,
    toFloat64(sum(a.redeemed)) AS redeemed
  FROM prediction_trader_flows_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.chain = {chain:UInt64}
    AND a.bucket >= toDateTime({from_day:Date}, 'UTC') AND a.bucket <= toDateTime({to_day:Date}, 'UTC')
    AND a.epoch >= f.epoch_floor
  GROUP BY trader, collateral_token
),
venues AS (
  SELECT exchange, collateral_token
  FROM prediction_venues FINAL
  WHERE chain = {chain:UInt64} AND source = 'rpc'
),
decimals AS (
  SELECT address, decimals, toUInt8(1) AS known
  FROM tokens FINAL
  WHERE chain = {chain:UInt64} AND (
    address IN (SELECT collateral_token FROM venues)
    OR address IN (SELECT collateral_token FROM funding))
),
labelled AS (
  SELECT address FROM prediction_venue_labels FINAL WHERE chain = {chain:UInt64}
),
lines AS (
  SELECT
    t.trader AS trader,
    if(d.known = 1, (t.bought + t.sold) / pow(10, d.decimals), 0.) AS volume,
    if(d.known = 1, (t.sold - t.bought - t.fees) / pow(10, d.decimals), 0.) AS cash,
    if(d.known = 1, t.fees / pow(10, d.decimals), 0.) AS fees,
    t.trades AS trades,
    if(d.known = 1, 0, t.trades) AS unpriced_trades,
    t.tokens AS tokens
  FROM trading AS t
  LEFT JOIN venues AS v ON v.exchange = t.exchange
  LEFT JOIN decimals AS d ON d.address = v.collateral_token
  UNION ALL
  SELECT
    u.trader AS trader,
    0. AS volume,
    if(d.known = 1, (u.merged + u.redeemed - u.split) / pow(10, d.decimals), 0.) AS cash,
    0. AS fees,
    toUInt64(0) AS trades,
    toUInt64(0) AS unpriced_trades,
    arrayReduce('uniqState', CAST([] AS Array(UInt256))) AS tokens
  FROM funding AS u
  LEFT JOIN decimals AS d ON d.address = u.collateral_token
)
SELECT
  {chain:UInt64} AS chain,
  trader,
  sum(volume) AS volume,
  sum(cash) AS net_cash_flow,
  sum(fees) AS fees,
  sum(lines.trades) AS trades,
  sum(lines.unpriced_trades) AS unpriced_trades,
  uniqMerge(lines.tokens) AS outcome_tokens_traded
FROM lines
WHERE trader NOT IN (SELECT address FROM labelled)
GROUP BY trader;
