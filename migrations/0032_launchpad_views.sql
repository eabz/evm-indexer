-- Token launchpads: the views a trading UI reads (docs/design.md section
-- 11). One cheap query per screen, no client side joins or math. The
-- cookbook with every query and its hand-checked numbers is
-- src/launchpads/README.md section 4.
--
-- Two layers:
--   * the *_v views over the aggregates of 0031. Each one ASOF joins the
--     shared epoch_floor_v of 0004 and filters epoch >= ifNull(epoch_floor,
--     0) BEFORE it merges any aggregate state (the validity rule), so a
--     stale epoch can never leak an open / close.
--   * the screen views. Everything scoped to one token / creator / wallet
--     is a PARAMETERIZED view - SELECT ... FROM view(chain = 56, token =
--     ...) - because a parameter is the only way to push the scope into
--     every subquery and turn each of them into a primary key range read.
--
-- TRUST. Anyone can emit a TokenLaunched or a CurveBuy: this module
-- decodes by event family and nothing on chain says who a venue is. So
-- EVERY view whose numbers a screen shows counts only emitters an operator
-- put in launchpad_trusted_emitters, and every one of them has an *_all_v
-- twin that counts everything and exists for exploration and for deciding
-- what to trust. launchpad_trusted_curves_v is the trust set: the listed
-- singletons PLUS every per-token curve a trusted factory announced, which
-- is exactly the set a forger can not enter.
--
-- Picking a token is NOT a trust decision, and treating it as one was a
-- real hole: the token page, the price chart, the sniper view and the
-- holder list are scoped to a token an attacker does not control, so
-- without the filter a forged curve naming a REAL token moves that token's
-- trades, volume, prices, raised / curve_progress and share of supply, and
-- a forged TokenLaunched emitted EARLIER than the real one (a token
-- address is predictable) wins the argMin and hides the real launch. The
-- token-scoped *_v views below therefore restrict every source - launches,
-- trades, graduations, candles - to launchpad_trusted_curves_v, and their
-- *_all_v twins keep the unfiltered view. A token with no trusted launch
-- yields NO rows from the *_v views: missing numbers, never wrong ones.
--
-- Picking a CREATOR is not a trust decision either, and for the same
-- reason - only there the victim is a wallet that did nothing at all. A
-- launch names its creator in the event, so a forger can hang a launch
-- that never graduates on any address it likes and manufacture that
-- wallet's serial-rugger signal, and a forged fee sweep can name it as
-- the recipient. The creator screens are scoped exactly like the token
-- ones, in the creator page section below.
--
-- What is deliberately NOT filtered: the six aggregate *_v views keep one
-- row per (key, emitter) and carry emitter through, because they are the
-- validated layer the screen views are built from - the screens above
-- apply the trust rule. Read them directly only for exploration.
--
-- PARAMETERS. Every {name:Type} is a ClickHouse BOUND parameter, sent
-- beside the statement, never pasted into its text (src/launchpads/
-- cookbook.rs holds the queries and a unit test enforces it). An id
-- parameter is a String of PLAIN HEX, no 0x: 64 characters for a 32 byte
-- id, or the 40 characters of an EVM address, which these views left pad
-- with 12 zero bytes themselves. The padding is a constant expression
-- ClickHouse folds before it reads a part, so the primary key range read
-- survives it (the if / concat form is used because leftPad() is not
-- folded - same as 0022).
--
-- A WRONG LENGTH MATCHES NOTHING. It used not to, and in THIS module that
-- was the sharpest version of the bug: unhex('') is the empty string,
-- toFixedString('', 32) is 32 zero bytes, and 32 zero bytes is a real
-- populated bucket here - the trades whose token leg stayed unverified
-- and whose family does not name the token (see 0030). An empty token
-- parameter therefore returned that bucket, as if a UI with an unset
-- field had asked for it. A truncated 39 or 63 character id pads the same
-- way.
--
-- Every parameterized view below therefore carries
--
--   AND length({<id>:String}) IN (40, 64)
--   AND match({<id>:String}, '^[0-9a-fA-F]+$')
--
-- exactly once, in the filter that gates its output. "Gates its output"
-- is exact about the ANSWER and not always about the I/O: in four views
-- the pair sits in a HAVING or in the outer WHERE of a cross join of
-- materialized subqueries, where a constant-false condition returns no
-- row but does not stop the subqueries reading (review F, MINOR 14). The
-- answer is the same in all of them, and only the cost of a malformed id
-- differs, which is not what the guard is for. The second line is
-- there because unhex does NOT raise on a non-hex character: measured on
-- 25.12, every one of the 74 printable non-hex characters becomes the
-- nibble 0xE or 0xF, so unhex('zz' x 20) is 20 bytes of 0xEF. That cannot
-- reach the 32-zero-byte bucket (only '0' is a zero nibble, and '00' is
-- valid hex), so the old guard was safe - but safe by accident of that
-- mapping rather than by construction, which is the whole point of a
-- guard. A malformed id now makes the WHERE constant false, exactly like
-- a malformed length (review round 4, MINOR 23). Both conjuncts name
-- no column, so ClickHouse folds them while it analyses the query: a
-- valid id leaves the primary key range read exactly as it was (verified
-- with EXPLAIN indexes = 1 - the key condition still names the id column
-- and reads one granule), a wrong one makes the WHERE constant false and
-- no part is read. An id longer than 64 characters still raises
-- TOO_LARGE_STRING_SIZE from toFixedString, as it always did: loud, never
-- a silent match. To look at the unverified-token bucket on purpose, read
-- launchpad_trades_by_token directly - it is not a screen.
--
-- Ids are 32 bytes (docs/design.md section 13) and NOTHING here assumes
-- the top 12 bytes are zero. Print one with the family of its chain, using
-- THE expression documented in migration 0006, where chains_v gives the
-- family: concat('0x', lower(hex(substring(id, 13)))) for 'evm',
-- base58Encode(substring(id, 1, 32)) for 'svm'. The substring() is NOT
-- decoration - toString(id) and CAST(id AS String) trim trailing zero
-- bytes, and only substring() / concat(id, '') keep every one of them.
-- A pool_id prints as all 32 bytes, because a Uniswap V4 / Balancer pool
-- id is not an address. Joins into the EVM-only erc20_transfers table PAD that table's
-- FixedString(20) address up to 32 bytes (the dex_token_info_v rule of
-- 0012): padding lets a non-EVM id simply find no row, while truncating
-- the 32 byte side would map every pubkey onto some address.
--
-- *_raw columns are the on-chain integers as Float64. Columns without the
-- suffix are divided by 10^decimals of the asset and are NULL - never 0 -
-- while the token worker has not stored those decimals.

-- The trust set: what a headline number may be built from.
CREATE VIEW IF NOT EXISTS launchpad_trusted_curves_v AS
SELECT chain, emitter AS curve, family
FROM launchpad_trusted_emitters FINAL
WHERE family != ''
UNION ALL
SELECT t.chain AS chain, t.curve AS curve, t.family AS family
FROM launchpad_tokens AS t FINAL
WHERE t.is_deleted = 0
  AND t.curve != toFixedString('', 32)
  AND (t.chain, t.emitter) IN (
    SELECT chain, emitter FROM launchpad_trusted_emitters FINAL
    WHERE family != '');

-- ------------------------------------------------ aggregates, validated

-- The candles of every (token, emitter), forged curves included: the
-- validated aggregate layer, and the join source of the launch feed.
-- A price chart reads launchpad_candles_1m_v below, never this.
CREATE VIEW IF NOT EXISTS launchpad_candles_1m_all_v AS
SELECT
  a.chain AS chain, a.token AS token, a.emitter AS emitter,
  a.bucket AS bucket,
  if(sum(a.priced_trades) > 0, argMinIfMerge(a.open), NULL) AS open_raw,
  toFloat64(max(a.high)) AS high_raw,
  toFloat64(min(a.low)) AS low_raw,
  if(sum(a.priced_trades) > 0, argMaxIfMerge(a.close), NULL) AS close_raw,
  toUInt64(sum(a.trades)) AS trades,
  toUInt64(sum(a.priced_trades)) AS priced_trades,
  toUInt64(sum(a.buys)) AS buys,
  sum(a.volume_quote) AS volume_quote_raw,
  sum(a.volume_token) AS volume_token_raw,
  sum(a.volume_quote_verified) AS volume_quote_verified_raw,
  uniqMerge(a.traders) AS unique_traders,
  argMaxMerge(a.progress_wad) / 1e18 AS curve_progress
FROM launchpad_candles_1m AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, token, emitter, bucket;

CREATE VIEW IF NOT EXISTS launchpad_candles_1h_all_v AS
SELECT
  a.chain AS chain, a.token AS token, a.emitter AS emitter,
  a.bucket AS bucket,
  if(sum(a.priced_trades) > 0, argMinIfMerge(a.open), NULL) AS open_raw,
  toFloat64(max(a.high)) AS high_raw,
  toFloat64(min(a.low)) AS low_raw,
  if(sum(a.priced_trades) > 0, argMaxIfMerge(a.close), NULL) AS close_raw,
  toUInt64(sum(a.trades)) AS trades,
  toUInt64(sum(a.priced_trades)) AS priced_trades,
  toUInt64(sum(a.buys)) AS buys,
  sum(a.volume_quote) AS volume_quote_raw,
  sum(a.volume_token) AS volume_token_raw,
  sum(a.volume_quote_verified) AS volume_quote_verified_raw,
  uniqMerge(a.traders) AS unique_traders,
  argMaxMerge(a.progress_wad) / 1e18 AS curve_progress
FROM launchpad_candles_1h AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, token, emitter, bucket;

-- THE price chart of one token: the same candles restricted to the curves
-- an operator trusts, so a forged curve that names this token can not add
-- a series to its chart. Parameterized because the trust set is looked up
-- per chain. Several trusted curves for one token would each keep their
-- own series - the emitter column says which.
CREATE VIEW IF NOT EXISTS launchpad_candles_1m_v AS
SELECT * FROM launchpad_candles_1m_all_v
WHERE chain = {chain:UInt64}
  AND length({token:String}) IN (40, 64)
  AND match({token:String}, '^[0-9a-fA-F]+$')
  AND token = toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32)
  AND emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64});

CREATE VIEW IF NOT EXISTS launchpad_candles_1h_v AS
SELECT * FROM launchpad_candles_1h_all_v
WHERE chain = {chain:UInt64}
  AND length({token:String}) IN (40, 64)
  AND match({token:String}, '^[0-9a-fA-F]+$')
  AND token = toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32)
  AND emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64});

CREATE VIEW IF NOT EXISTS launchpad_venue_trades_1d_v AS
SELECT
  a.chain AS chain, a.family AS family, a.emitter AS emitter,
  a.bucket AS bucket,
  toUInt64(sum(a.trades)) AS trades,
  toUInt64(sum(a.buys)) AS buys,
  sum(a.volume_quote) AS volume_quote_raw,
  sum(a.volume_quote_verified) AS volume_quote_verified_raw,
  sum(a.fees) AS fees_raw,
  uniqMerge(a.traders) AS unique_traders,
  uniqMerge(a.tokens) AS unique_tokens
FROM launchpad_venue_trades_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, family, emitter, bucket;

CREATE VIEW IF NOT EXISTS launchpad_launches_1d_v AS
SELECT
  a.chain AS chain, a.family AS family, a.emitter AS emitter,
  a.creator AS creator, a.bucket AS bucket,
  toUInt64(sum(a.launches)) AS launches
FROM launchpad_launches_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, family, emitter, creator, bucket;

CREATE VIEW IF NOT EXISTS launchpad_graduations_1d_v AS
SELECT
  a.chain AS chain, a.family AS family, a.emitter AS emitter,
  a.bucket AS bucket,
  toUInt64(sum(a.graduations)) AS graduations,
  sum(a.quote_in) AS quote_in_raw,
  uniqMerge(a.tokens) AS unique_tokens
FROM launchpad_graduations_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, family, emitter, bucket;

CREATE VIEW IF NOT EXISTS launchpad_creator_fees_1d_v AS
SELECT
  a.chain AS chain, a.family AS family, a.emitter AS emitter,
  a.recipient AS recipient, a.kind AS kind, a.phase AS phase,
  a.bucket AS bucket,
  toUInt64(sum(a.events)) AS events,
  sum(a.amount) AS amount_raw
FROM launchpad_creator_fees_1d AS a
ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)
GROUP BY chain, family, emitter, recipient, kind, phase, bucket;

-- ------------------------------------------------- screen: new launches
--
-- Newest first over one chain partition, with the first-minute statistics
-- of the curve (a key read of launchpad_candles_1m, not a scan of the
-- trades). initial_price_raw is the OPEN of the launch minute: quote units
-- per token unit, both raw.
CREATE VIEW IF NOT EXISTS launchpad_new_launches_all_v AS
SELECT
  l.chain AS chain, l.timestamp AS launch_time,
  l.block_number AS launch_block, l.token AS token, l.family AS family,
  l.emitter AS emitter, l.curve AS curve, l.creator AS creator,
  l.name AS name, l.symbol AS symbol, l.quote_token AS quote_token,
  toFloat64(l.initial_supply) AS initial_supply_raw,
  toFloat64(l.graduation_threshold) AS graduation_threshold_raw,
  l.pool_id AS launch_pool_id, l.tx_id AS launch_tx,
  l.emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted,
  c.open_raw AS initial_price_raw,
  c.close_raw AS first_minute_price_raw,
  ifNull(c.trades, 0) AS first_minute_trades,
  ifNull(c.buys, 0) AS first_minute_buys,
  ifNull(c.volume_quote_raw, 0.) AS first_minute_volume_raw,
  ifNull(c.unique_traders, 0) AS first_minute_traders
FROM launchpad_launches_by_time AS l FINAL
LEFT JOIN launchpad_candles_1m_all_v AS c
  ON c.chain = l.chain AND c.token = l.token AND c.emitter = l.curve
 AND c.bucket = toDateTime(intDiv(toUInt32(l.timestamp), 60) * 60, 'UTC')
WHERE l.chain = {chain:UInt64} AND l.is_deleted = 0
  AND l.timestamp >= toDateTime({since:UInt32})
ORDER BY l.timestamp DESC;

CREATE VIEW IF NOT EXISTS launchpad_new_launches_v AS
SELECT * FROM
(
  SELECT * FROM launchpad_new_launches_all_v(
    chain = {chain:UInt64}, since = {since:UInt32})
)
WHERE trusted = 1;

-- --------------------------------------------------- screen: token page
--
-- One row: the launch, the curve's progress, what has traded so far and
-- where it graduated to. curve_progress is 0..1 and comes from the venue
-- itself when it reports one (flap_portal), otherwise from the quote
-- RAISED against the graduation threshold (pons_v2: quoteIn is gross, so
-- what counts towards the threshold is quoteIn - fee - tax on buys, minus
-- the net quoteOut of sells - checked digit for digit on the real curve
-- that graduated, see README section 4).
--
-- launchpad_token_all_v counts EVERY emitter: the launch is whichever
-- TokenLaunched came first, the trades are every curve's, and `trusted`
-- says whether that first launch emitter is one an operator listed. It is
-- the exploration twin and must not be put on a screen - a forger who
-- predicts a token address can emit an earlier launch and win the argMin,
-- and any contract can emit curve trades naming this token.
CREATE VIEW IF NOT EXISTS launchpad_token_all_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  l.launch_chain AS chain, l.launch_token AS token,
  l.first_family AS family, l.first_emitter AS emitter,
  l.first_curve AS curve, l.first_creator AS creator,
  l.first_name AS name, l.first_symbol AS symbol,
  l.first_metadata_uri AS metadata_uri,
  l.first_quote_token AS quote_token, l.launch_block AS launch_block,
  l.launch_time AS launch_time, l.launch_tx AS launch_tx,
  l.initial_supply_raw AS initial_supply_raw,
  l.graduation_threshold_raw AS graduation_threshold_raw,
  l.first_emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted,
  t.trades AS trades, t.buys AS buys, t.unique_traders AS unique_traders,
  t.volume_quote_raw AS volume_quote_raw,
  t.volume_quote_verified_raw AS volume_quote_verified_raw,
  t.token_volume_raw AS token_volume_raw,
  t.first_price_raw AS first_price_raw,
  t.last_price_raw AS last_price_raw,
  t.last_trade_time AS last_trade_time,
  t.raised_raw AS raised_raw,
  multiIf(
    l.graduation_threshold_raw > 0,
      least(t.raised_raw / l.graduation_threshold_raw, 1.),
    t.last_progress_wad > 0, t.last_progress_wad / 1e18,
    NULL) AS curve_progress,
  g.graduated AS graduated, g.last_pool_id AS pool_id,
  g.last_pool_kind AS pool_kind, g.graduation_time AS graduation_time,
  g.graduation_block AS graduation_block
FROM
(
  SELECT
    chain AS launch_chain, token AS launch_token,
    argMin(family, (block_number, tx_index, ordinal)) AS first_family,
    argMin(emitter, (block_number, tx_index, ordinal)) AS first_emitter,
    argMin(curve, (block_number, tx_index, ordinal)) AS first_curve,
    argMin(creator, (block_number, tx_index, ordinal)) AS first_creator,
    argMin(name, (block_number, tx_index, ordinal)) AS first_name,
    argMin(symbol, (block_number, tx_index, ordinal)) AS first_symbol,
    argMin(metadata_uri, (block_number, tx_index, ordinal)) AS first_metadata_uri,
    argMin(quote_token, (block_number, tx_index, ordinal)) AS first_quote_token,
    min(block_number) AS launch_block,
    argMin(timestamp, (block_number, tx_index, ordinal)) AS launch_time,
    argMin(tx_id, (block_number, tx_index, ordinal)) AS launch_tx,
    argMin(toFloat64(initial_supply), (block_number, tx_index, ordinal)) AS initial_supply_raw,
    argMin(toFloat64(graduation_threshold), (block_number, tx_index, ordinal)) AS graduation_threshold_raw
  FROM launchpad_tokens FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
    AND length({token:String}) IN (40, 64)
    AND match({token:String}, '^[0-9a-fA-F]+$')
  GROUP BY chain, token
) AS l
CROSS JOIN
(
  SELECT
    toUInt64(count()) AS trades,
    toUInt64(countIf(side = 'buy')) AS buys,
    uniqExact(trader) AS unique_traders,
    sum(toFloat64(quote_amount)) AS volume_quote_raw,
    sumIf(toFloat64(quote_amount), quote_verified = 1) AS volume_quote_verified_raw,
    sum(toFloat64(token_amount)) AS token_volume_raw,
    argMinIf(toFloat64(quote_amount) / toFloat64(token_amount),
             (block_number, tx_index, ordinal),
             token_amount != 0 AND quote_amount != 0) AS first_price_raw,
    argMaxIf(toFloat64(quote_amount) / toFloat64(token_amount),
             (block_number, tx_index, ordinal),
             token_amount != 0 AND quote_amount != 0) AS last_price_raw,
    max(timestamp) AS last_trade_time,
    sumIf(toFloat64(quote_amount) - toFloat64(fee_amount) - toFloat64(tax_amount), side = 'buy')
      - sumIf(toFloat64(quote_amount), side = 'sell') AS raised_raw,
    argMax(toFloat64(progress_wad), (block_number, tx_index, ordinal)) AS last_progress_wad
  FROM launchpad_trades_by_token FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
) AS t
CROSS JOIN
(
  SELECT
    toUInt8(count() > 0) AS graduated,
    argMax(pool_id, (block_number, tx_index, ordinal)) AS last_pool_id,
    argMax(pool_kind, (block_number, tx_index, ordinal)) AS last_pool_kind,
    max(timestamp) AS graduation_time,
    max(block_number) AS graduation_block
  FROM launchpad_graduations FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
) AS g;

-- The same page, restricted to launchpad_trusted_curves_v in all three
-- sources: the launch (so the earlier-forged-launch trick can not win the
-- argMin), the trades (so trades / buys / unique_traders / volume /
-- first_price / last_price / raised_raw and therefore curve_progress are
-- the real curve's) and the graduation. A token whose launch emitter is
-- not trusted yields no row at all, and `trusted` is then always 1.
CREATE VIEW IF NOT EXISTS launchpad_token_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  l.launch_chain AS chain, l.launch_token AS token,
  l.first_family AS family, l.first_emitter AS emitter,
  l.first_curve AS curve, l.first_creator AS creator,
  l.first_name AS name, l.first_symbol AS symbol,
  l.first_metadata_uri AS metadata_uri,
  l.first_quote_token AS quote_token, l.launch_block AS launch_block,
  l.launch_time AS launch_time, l.launch_tx AS launch_tx,
  l.initial_supply_raw AS initial_supply_raw,
  l.graduation_threshold_raw AS graduation_threshold_raw,
  l.first_emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted,
  t.trades AS trades, t.buys AS buys, t.unique_traders AS unique_traders,
  t.volume_quote_raw AS volume_quote_raw,
  t.volume_quote_verified_raw AS volume_quote_verified_raw,
  t.token_volume_raw AS token_volume_raw,
  t.first_price_raw AS first_price_raw,
  t.last_price_raw AS last_price_raw,
  t.last_trade_time AS last_trade_time,
  t.raised_raw AS raised_raw,
  multiIf(
    l.graduation_threshold_raw > 0,
      least(t.raised_raw / l.graduation_threshold_raw, 1.),
    t.last_progress_wad > 0, t.last_progress_wad / 1e18,
    NULL) AS curve_progress,
  g.graduated AS graduated, g.last_pool_id AS pool_id,
  g.last_pool_kind AS pool_kind, g.graduation_time AS graduation_time,
  g.graduation_block AS graduation_block
FROM
(
  SELECT
    chain AS launch_chain, token AS launch_token,
    argMin(family, (block_number, tx_index, ordinal)) AS first_family,
    argMin(emitter, (block_number, tx_index, ordinal)) AS first_emitter,
    argMin(curve, (block_number, tx_index, ordinal)) AS first_curve,
    argMin(creator, (block_number, tx_index, ordinal)) AS first_creator,
    argMin(name, (block_number, tx_index, ordinal)) AS first_name,
    argMin(symbol, (block_number, tx_index, ordinal)) AS first_symbol,
    argMin(metadata_uri, (block_number, tx_index, ordinal)) AS first_metadata_uri,
    argMin(quote_token, (block_number, tx_index, ordinal)) AS first_quote_token,
    min(block_number) AS launch_block,
    argMin(timestamp, (block_number, tx_index, ordinal)) AS launch_time,
    argMin(tx_id, (block_number, tx_index, ordinal)) AS launch_tx,
    argMin(toFloat64(initial_supply), (block_number, tx_index, ordinal)) AS initial_supply_raw,
    argMin(toFloat64(graduation_threshold), (block_number, tx_index, ordinal)) AS graduation_threshold_raw
  FROM launchpad_tokens FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
    AND length({token:String}) IN (40, 64)
    AND match({token:String}, '^[0-9a-fA-F]+$')
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY chain, token
) AS l
CROSS JOIN
(
  SELECT
    toUInt64(count()) AS trades,
    toUInt64(countIf(side = 'buy')) AS buys,
    uniqExact(trader) AS unique_traders,
    sum(toFloat64(quote_amount)) AS volume_quote_raw,
    sumIf(toFloat64(quote_amount), quote_verified = 1) AS volume_quote_verified_raw,
    sum(toFloat64(token_amount)) AS token_volume_raw,
    argMinIf(toFloat64(quote_amount) / toFloat64(token_amount),
             (block_number, tx_index, ordinal),
             token_amount != 0 AND quote_amount != 0) AS first_price_raw,
    argMaxIf(toFloat64(quote_amount) / toFloat64(token_amount),
             (block_number, tx_index, ordinal),
             token_amount != 0 AND quote_amount != 0) AS last_price_raw,
    max(timestamp) AS last_trade_time,
    sumIf(toFloat64(quote_amount) - toFloat64(fee_amount) - toFloat64(tax_amount), side = 'buy')
      - sumIf(toFloat64(quote_amount), side = 'sell') AS raised_raw,
    argMax(toFloat64(progress_wad), (block_number, tx_index, ordinal)) AS last_progress_wad
  FROM launchpad_trades_by_token FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
) AS t
CROSS JOIN
(
  SELECT
    toUInt8(count() > 0) AS graduated,
    argMax(pool_id, (block_number, tx_index, ordinal)) AS last_pool_id,
    argMax(pool_kind, (block_number, tx_index, ordinal)) AS last_pool_kind,
    max(timestamp) AS graduation_time,
    max(block_number) AS graduation_block
  FROM launchpad_graduations FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
) AS g;

-- The trades tape of one token, newest first. The all_v twin is every
-- emitter's trades, the _v one keeps the trusted curves, so a forged
-- curve can not write lines into a real token's tape. `emitter` is
-- carried through either way, because the tape is the one screen where
-- seeing WHO claimed a trade is the point.
CREATE VIEW IF NOT EXISTS launchpad_token_trades_all_v AS
SELECT
  chain, token, timestamp, block_number, tx_index, ordinal, family,
  emitter, side, trader, caller, tx_from,
  toFloat64(token_amount) AS token_amount_raw,
  toFloat64(quote_amount) AS quote_amount_raw,
  toFloat64(fee_amount) AS fee_amount_raw,
  toFloat64(tax_amount) AS tax_amount_raw,
  if(token_amount != 0 AND quote_amount != 0,
     toFloat64(quote_amount) / toFloat64(token_amount), NULL) AS price_raw,
  token_verified, quote_verified, quote_token, graduating, tx_id
FROM launchpad_trades_by_token FINAL
WHERE chain = {chain:UInt64} AND token = toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32)
  AND is_deleted = 0 AND block_number >= {from_block:UInt64}
  AND length({token:String}) IN (40, 64)
  AND match({token:String}, '^[0-9a-fA-F]+$')
ORDER BY block_number DESC, tx_index DESC, ordinal DESC;

CREATE VIEW IF NOT EXISTS launchpad_token_trades_v AS
SELECT * FROM launchpad_token_trades_all_v(
  chain = {chain:UInt64}, token = {token:String},
  from_block = {from_block:UInt64})
WHERE emitter IN (
  SELECT curve FROM launchpad_trusted_curves_v
  WHERE chain = {chain:UInt64})
ORDER BY block_number DESC, tx_index DESC, ordinal DESC;

-- Top holders of a launchpad token, from the ERC-20 transfers the TOKEN
-- itself emitted. Cheap because a launchpad token has no history before
-- its launch block: the scalar subquery prunes erc20_transfers by its
-- primary key (chain, block_number). as_of_block = the concentration at
-- graduation, or a huge number for "now".
--
-- THE 20 vs 32 byte seam of this module (the dex_token_info_v rule of
-- migration 0012): erc20_transfers is EVM only, so its FixedString(20)
-- token_address / from / to are PADDED up to 32 bytes here. Never
-- substring(token, 13, 20) on the 32 byte side - that would map every
-- Solana pubkey onto some EVM address, while padding just finds no row.
-- token_address is not in the table's sorting key, so nothing is lost:
-- the block_number range above is what prunes.
--
-- The BALANCES are the token contract's own claims (erc20_transfers), so
-- they need no trust rule. The two things taken from launchpad_tokens do:
-- the initial supply that share_of_initial_supply divides by, and the
-- launch block that bounds the scan. A forged TokenLaunched naming a real
-- token with a huge initial_supply would otherwise crush everyone's
-- share, so the _v twin reads only rows from a trusted emitter and the
-- all_v twin keeps every row. With no trusted launch row the denominator
-- falls back to 1 (shares are then raw balances) and the scan starts at
-- block 0, exactly as it does for a token this module never saw.
CREATE VIEW IF NOT EXISTS launchpad_token_holders_all_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  account,
  sum(delta) AS balance_raw,
  sum(delta) / greatest(
    (SELECT max(toFloat64(initial_supply)) FROM launchpad_tokens FINAL
     WHERE chain = {chain:UInt64} AND token = token_id
       AND is_deleted = 0), 1.) AS share_of_initial_supply,
  countIf(delta > 0) AS received,
  countIf(delta < 0) AS sent
FROM
(
  SELECT toFixedString(concat(unhex('000000000000000000000000'), `to`), 32) AS account, toFloat64(amount) AS delta
  FROM erc20_transfers FINAL
  WHERE chain = {chain:UInt64}
    AND toFixedString(concat(unhex('000000000000000000000000'), token_address), 32) = token_id
    AND block_number >= (
      SELECT min(block_number) FROM launchpad_tokens FINAL
      WHERE chain = {chain:UInt64} AND token = token_id
        AND is_deleted = 0)
    AND block_number <= {as_of_block:UInt64}
    AND is_deleted = 0
  UNION ALL
  SELECT toFixedString(concat(unhex('000000000000000000000000'), `from`), 32) AS account, -toFloat64(amount) AS delta
  FROM erc20_transfers FINAL
  WHERE chain = {chain:UInt64}
    AND toFixedString(concat(unhex('000000000000000000000000'), token_address), 32) = token_id
    AND block_number >= (
      SELECT min(block_number) FROM launchpad_tokens FINAL
      WHERE chain = {chain:UInt64} AND token = token_id
        AND is_deleted = 0)
    AND block_number <= {as_of_block:UInt64}
    AND is_deleted = 0
)
GROUP BY account
HAVING balance_raw > 0 AND length({token:String}) IN (40, 64)
AND match({token:String}, '^[0-9a-fA-F]+$')
ORDER BY balance_raw DESC;

CREATE VIEW IF NOT EXISTS launchpad_token_holders_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  account,
  sum(delta) AS balance_raw,
  sum(delta) / greatest(
    (SELECT max(toFloat64(initial_supply)) FROM launchpad_tokens FINAL
     WHERE chain = {chain:UInt64} AND token = token_id
       AND is_deleted = 0
       AND emitter IN (
         SELECT curve FROM launchpad_trusted_curves_v
         WHERE chain = {chain:UInt64})), 1.) AS share_of_initial_supply,
  countIf(delta > 0) AS received,
  countIf(delta < 0) AS sent
FROM
(
  SELECT toFixedString(concat(unhex('000000000000000000000000'), `to`), 32) AS account, toFloat64(amount) AS delta
  FROM erc20_transfers FINAL
  WHERE chain = {chain:UInt64}
    AND toFixedString(concat(unhex('000000000000000000000000'), token_address), 32) = token_id
    AND block_number >= (
      SELECT min(block_number) FROM launchpad_tokens FINAL
      WHERE chain = {chain:UInt64} AND token = token_id
        AND is_deleted = 0
        AND emitter IN (
          SELECT curve FROM launchpad_trusted_curves_v
          WHERE chain = {chain:UInt64}))
    AND block_number <= {as_of_block:UInt64}
    AND is_deleted = 0
  UNION ALL
  SELECT toFixedString(concat(unhex('000000000000000000000000'), `from`), 32) AS account, -toFloat64(amount) AS delta
  FROM erc20_transfers FINAL
  WHERE chain = {chain:UInt64}
    AND toFixedString(concat(unhex('000000000000000000000000'), token_address), 32) = token_id
    AND block_number >= (
      SELECT min(block_number) FROM launchpad_tokens FINAL
      WHERE chain = {chain:UInt64} AND token = token_id
        AND is_deleted = 0
        AND emitter IN (
          SELECT curve FROM launchpad_trusted_curves_v
          WHERE chain = {chain:UInt64}))
    AND block_number <= {as_of_block:UInt64}
    AND is_deleted = 0
)
GROUP BY account
HAVING balance_raw > 0 AND length({token:String}) IN (40, 64)
AND match({token:String}, '^[0-9a-fA-F]+$')
ORDER BY balance_raw DESC;

-- ------------------------------------------------ screen: graduations
--
-- The graduation feed, joined to what the DEX module knows about the
-- destination pool: pool_status / pool_trusted come from
-- dex_pool_current_v, so the same page can keep charting the token from
-- the DEX candles after the curve is gone. A pool nobody indexed yet has
-- pool_status = '' - never a guess.
CREATE VIEW IF NOT EXISTS launchpad_graduations_all_v AS
SELECT
  g.chain AS chain, g.timestamp AS graduation_time,
  g.block_number AS graduation_block, g.token AS token,
  g.family AS family, g.emitter AS emitter, g.pool_id AS pool_id,
  g.pool_kind AS pool_kind, g.quote_token AS quote_token,
  toFloat64(g.token_amount) AS token_amount_raw,
  toFloat64(g.quote_amount) AS quote_amount_raw,
  toFloat64(g.position_id) AS position_id,
  g.tx_id AS graduation_tx,
  g.emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted,
  ifNull(p.best_status, '') AS pool_status,
  ifNull(p.best_trusted, 0) AS pool_trusted,
  ifNull(p.best_protocol, '') AS pool_protocol,
  ifNull(p.best_emitter, toFixedString('', 32)) AS pool_emitter
FROM launchpad_graduations AS g FINAL
LEFT JOIN
(
  SELECT
    chain AS pool_chain, pool_id AS joined_pool_id,
    argMax(status, trusted) AS best_status,
    max(trusted) AS best_trusted,
    argMax(protocol, trusted) AS best_protocol,
    argMax(emitter, trusted) AS best_emitter
  FROM dex_pool_current_v
  WHERE chain = {chain:UInt64}
  GROUP BY chain, pool_id
) AS p ON p.pool_chain = g.chain AND p.joined_pool_id = g.pool_id
WHERE g.chain = {chain:UInt64} AND g.is_deleted = 0
  AND g.timestamp >= toDateTime({since:UInt32})
ORDER BY g.timestamp DESC;

CREATE VIEW IF NOT EXISTS launchpad_graduations_v AS
SELECT * FROM
(
  SELECT * FROM launchpad_graduations_all_v(
    chain = {chain:UInt64}, since = {since:UInt32})
)
WHERE trusted = 1;

-- ------------------------------------------------- screen: creator page
--
-- One row per token a wallet launched: did it graduate, is it still
-- trading, what did the creator take out of it. "died" = never graduated
-- and no trade for dead_after seconds.
--
-- THE SAME FORGERY CLASS AS THE TOKEN PAGE, and arguably a nastier one,
-- because the victim is a WALLET that did nothing. The creator of a
-- launch is NAMED BY THE EVENT: anyone can emit a TokenLaunched that
-- names a stranger as `creator`, and unfiltered that launch lands on the
-- stranger's page. It never graduates, so it raises `launches`, raises
-- `died` and drags `graduation_rate` down - manufacturing exactly the
-- serial-rugger signal this screen exists to report. The other three
-- sources are open in the same way: a forged CurveBuy naming one of
-- those tokens moves `trades` / `volume_quote_raw` / `last_trade_time`
-- (and through it `died`), a forged Graduated flips `graduated`, and a
-- forged fee sweep naming the wallet as `recipient` inflates
-- `realised_creator_fees_raw`.
--
-- So the _v views below restrict ALL FOUR sources - launches,
-- graduations, curve trades and creator fees - to
-- launchpad_trusted_curves_v, exactly as launchpad_token_v does, and the
-- _all_v twins keep the unfiltered view for deciding what to trust. A
-- creator whose launches are all untrusted yields no token rows at all:
-- missing numbers, never wrong ones.

-- The exploration twin: every emitter counts, and `trusted` says whether
-- the launch emitter is one an operator listed. Not for a screen.
CREATE VIEW IF NOT EXISTS launchpad_creator_tokens_all_v AS
WITH toFixedString(unhex(if(length({creator:String}) = 40,
  concat('000000000000000000000000', {creator:String}), {creator:String})), 32) AS creator_id
SELECT
  l.chain AS chain, l.creator AS creator, l.token AS token,
  l.family AS family, l.emitter AS emitter, l.curve AS curve,
  l.name AS name, l.symbol AS symbol, l.timestamp AS launch_time,
  l.block_number AS launch_block, l.tx_id AS launch_tx,
  toFloat64(l.graduation_threshold) AS graduation_threshold_raw,
  toUInt8(g.g_token != toFixedString('', 32)) AS graduated,
  g.g_pool_id AS pool_id, g.g_time AS graduation_time,
  ifNull(t.t_trades, 0) AS trades,
  ifNull(t.t_volume_quote_raw, 0.) AS volume_quote_raw,
  ifNull(t.t_last_trade_time, l.timestamp) AS last_trade_time,
  toUInt8(g.g_token = toFixedString('', 32)
    AND ifNull(t.t_last_trade_time, l.timestamp)
        < toDateTime({as_of:UInt32}) - {dead_after:UInt32}) AS died,
  l.emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted
FROM launchpad_launches_by_creator AS l FINAL
LEFT JOIN
(
  SELECT chain AS g_chain, token AS g_token,
         argMax(pool_id, block_number) AS g_pool_id,
         max(timestamp) AS g_time
  FROM launchpad_graduations FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
  GROUP BY chain, token
) AS g ON g.g_chain = l.chain AND g.g_token = l.token
LEFT JOIN
(
  SELECT chain AS t_chain, token AS t_token,
         toUInt64(count()) AS t_trades,
         sum(toFloat64(quote_amount)) AS t_volume_quote_raw,
         max(timestamp) AS t_last_trade_time
  FROM launchpad_trades_by_token FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
  GROUP BY chain, token
) AS t ON t.t_chain = l.chain AND t.t_token = l.token
WHERE l.chain = {chain:UInt64} AND l.creator = creator_id
  AND l.is_deleted = 0
  AND length({creator:String}) IN (40, 64)
  AND match({creator:String}, '^[0-9a-fA-F]+$')
ORDER BY l.timestamp DESC;

-- THE screen. Same shape, every source restricted to the trusted curves,
-- so `trusted` is always 1 here (kept so the two twins are union
-- compatible and a UI can read either).
CREATE VIEW IF NOT EXISTS launchpad_creator_tokens_v AS
WITH toFixedString(unhex(if(length({creator:String}) = 40,
  concat('000000000000000000000000', {creator:String}), {creator:String})), 32) AS creator_id
SELECT
  l.chain AS chain, l.creator AS creator, l.token AS token,
  l.family AS family, l.emitter AS emitter, l.curve AS curve,
  l.name AS name, l.symbol AS symbol, l.timestamp AS launch_time,
  l.block_number AS launch_block, l.tx_id AS launch_tx,
  toFloat64(l.graduation_threshold) AS graduation_threshold_raw,
  toUInt8(g.g_token != toFixedString('', 32)) AS graduated,
  g.g_pool_id AS pool_id, g.g_time AS graduation_time,
  ifNull(t.t_trades, 0) AS trades,
  ifNull(t.t_volume_quote_raw, 0.) AS volume_quote_raw,
  ifNull(t.t_last_trade_time, l.timestamp) AS last_trade_time,
  toUInt8(g.g_token = toFixedString('', 32)
    AND ifNull(t.t_last_trade_time, l.timestamp)
        < toDateTime({as_of:UInt32}) - {dead_after:UInt32}) AS died,
  toUInt8(1) AS trusted
FROM launchpad_launches_by_creator AS l FINAL
LEFT JOIN
(
  SELECT chain AS g_chain, token AS g_token,
         argMax(pool_id, block_number) AS g_pool_id,
         max(timestamp) AS g_time
  FROM launchpad_graduations FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY chain, token
) AS g ON g.g_chain = l.chain AND g.g_token = l.token
LEFT JOIN
(
  SELECT chain AS t_chain, token AS t_token,
         toUInt64(count()) AS t_trades,
         sum(toFloat64(quote_amount)) AS t_volume_quote_raw,
         max(timestamp) AS t_last_trade_time
  FROM launchpad_trades_by_token FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY chain, token
) AS t ON t.t_chain = l.chain AND t.t_token = l.token
WHERE l.chain = {chain:UInt64} AND l.creator = creator_id
  AND l.is_deleted = 0
  AND length({creator:String}) IN (40, 64)
  AND match({creator:String}, '^[0-9a-fA-F]+$')
  AND l.emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64})
ORDER BY l.timestamp DESC;

-- The creator header: the serial-rugger signal in one row. The
-- exploration twin, over every emitter.
CREATE VIEW IF NOT EXISTS launchpad_creator_all_v AS
WITH toFixedString(unhex(if(length({creator:String}) = 40,
  concat('000000000000000000000000', {creator:String}), {creator:String})), 32) AS creator_id
SELECT
  {chain:UInt64} AS chain, creator_id AS creator,
  c.launches AS launches, c.graduated AS graduated, c.died AS died,
  if(c.launches > 0, c.graduated / c.launches, NULL) AS graduation_rate,
  c.first_launch AS first_launch, c.last_launch AS last_launch,
  c.volume_quote_raw AS volume_quote_raw,
  f.fees_raw AS realised_creator_fees_raw,
  f.fee_events AS creator_fee_events,
  c.trusted_launches AS trusted_launches
FROM
(
  SELECT
    toUInt64(count()) AS launches,
    toUInt64(countIf(graduated = 1)) AS graduated,
    toUInt64(countIf(died = 1)) AS died,
    min(launch_time) AS first_launch,
    max(launch_time) AS last_launch,
    sum(volume_quote_raw) AS volume_quote_raw,
    toUInt64(countIf(trusted = 1)) AS trusted_launches
  FROM launchpad_creator_tokens_all_v(
    chain = {chain:UInt64}, creator = {creator:String},
    as_of = {as_of:UInt32}, dead_after = {dead_after:UInt32})
) AS c
CROSS JOIN
(
  SELECT
    sum(toFloat64(amount)) AS fees_raw,
    toUInt64(count()) AS fee_events
  FROM launchpad_creator_fees FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
    AND recipient = creator_id AND kind = 'creator'
) AS f
WHERE length({creator:String}) IN (40, 64)
  AND match({creator:String}, '^[0-9a-fA-F]+$');

-- THE screen. Every launch counted, every graduation, every trade and
-- every fee row comes from a trusted curve, so a forger can neither add
-- a launch to this wallet nor pay it a fee it never earned.
CREATE VIEW IF NOT EXISTS launchpad_creator_v AS
WITH toFixedString(unhex(if(length({creator:String}) = 40,
  concat('000000000000000000000000', {creator:String}), {creator:String})), 32) AS creator_id
SELECT
  {chain:UInt64} AS chain, creator_id AS creator,
  c.launches AS launches, c.graduated AS graduated, c.died AS died,
  if(c.launches > 0, c.graduated / c.launches, NULL) AS graduation_rate,
  c.first_launch AS first_launch, c.last_launch AS last_launch,
  c.volume_quote_raw AS volume_quote_raw,
  f.fees_raw AS realised_creator_fees_raw,
  f.fee_events AS creator_fee_events,
  c.launches AS trusted_launches
FROM
(
  SELECT
    toUInt64(count()) AS launches,
    toUInt64(countIf(graduated = 1)) AS graduated,
    toUInt64(countIf(died = 1)) AS died,
    min(launch_time) AS first_launch,
    max(launch_time) AS last_launch,
    sum(volume_quote_raw) AS volume_quote_raw
  FROM launchpad_creator_tokens_v(
    chain = {chain:UInt64}, creator = {creator:String},
    as_of = {as_of:UInt32}, dead_after = {dead_after:UInt32})
) AS c
CROSS JOIN
(
  SELECT
    sum(toFloat64(amount)) AS fees_raw,
    toUInt64(count()) AS fee_events
  FROM launchpad_creator_fees FINAL
  WHERE chain = {chain:UInt64} AND is_deleted = 0
    AND recipient = creator_id AND kind = 'creator'
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
) AS f
WHERE length({creator:String}) IN (40, 64)
  AND match({creator:String}, '^[0-9a-fA-F]+$');

-- -------------------------------------------------- screen: sniper view
--
-- Who bought in the launch block and the first `blocks` blocks after it,
-- how concentrated those buys were, and which of them came through one
-- transaction or one funder. bundle_size > 1 = several buyers served by a
-- single transaction (the real fixture has one transaction buying for 15
-- recipients one block after the launch).
--
-- Every number here comes from rows anyone can emit - the buys, the launch
-- block the window starts at, the creator a trader is compared against and
-- the supply the share divides by - so the _v twin takes all four from
-- launchpad_trusted_curves_v and the all_v twin takes them from everyone.
-- Without that a forged curve mints snipers into a real token's list.
CREATE VIEW IF NOT EXISTS launchpad_snipers_all_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  b.trader AS trader,
  b.first_block - l.launch_block AS blocks_after_launch,
  b.buys AS buys,
  b.token_amount_raw AS token_amount_raw,
  b.quote_amount_raw AS quote_amount_raw,
  b.token_amount_raw / greatest(l.initial_supply_raw, 1.) AS share_of_initial_supply,
  b.funder_of AS funder,
  b.bundle_size AS bundle_size,
  toUInt8(b.trader = l.launch_creator) AS is_creator
FROM
(
  SELECT
    trader,
    min(block_number) AS first_block,
    toUInt64(count()) AS buys,
    sum(toFloat64(token_amount)) AS token_amount_raw,
    sum(toFloat64(quote_amount)) AS quote_amount_raw,
    argMin(tx_from, (block_number, tx_index, ordinal)) AS funder_of,
    max(bundle) AS bundle_size
  FROM
  (
    SELECT
      trader, block_number, tx_index, ordinal, token_amount, quote_amount,
      tx_from,
      count(DISTINCT trader) OVER (PARTITION BY tx_id) AS bundle
    FROM launchpad_trades_by_token FINAL
    WHERE chain = {chain:UInt64} AND token = token_id
      AND is_deleted = 0 AND side = 'buy'
      AND length({token:String}) IN (40, 64)
      AND match({token:String}, '^[0-9a-fA-F]+$')
      AND block_number <= (
        SELECT min(block_number) + {blocks:UInt64}
        FROM launchpad_tokens FINAL
        WHERE chain = {chain:UInt64} AND token = token_id
          AND is_deleted = 0)
  )
  GROUP BY trader
) AS b
CROSS JOIN
(
  SELECT
    min(block_number) AS launch_block,
    argMin(creator, (block_number, tx_index, ordinal)) AS launch_creator,
    argMin(toFloat64(initial_supply), (block_number, tx_index, ordinal)) AS initial_supply_raw
  FROM launchpad_tokens FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
) AS l
ORDER BY token_amount_raw DESC;

CREATE VIEW IF NOT EXISTS launchpad_snipers_v AS
WITH toFixedString(unhex(if(length({token:String}) = 40,
  concat('000000000000000000000000', {token:String}), {token:String})), 32) AS token_id
SELECT
  b.trader AS trader,
  b.first_block - l.launch_block AS blocks_after_launch,
  b.buys AS buys,
  b.token_amount_raw AS token_amount_raw,
  b.quote_amount_raw AS quote_amount_raw,
  b.token_amount_raw / greatest(l.initial_supply_raw, 1.) AS share_of_initial_supply,
  b.funder_of AS funder,
  b.bundle_size AS bundle_size,
  toUInt8(b.trader = l.launch_creator) AS is_creator
FROM
(
  SELECT
    trader,
    min(block_number) AS first_block,
    toUInt64(count()) AS buys,
    sum(toFloat64(token_amount)) AS token_amount_raw,
    sum(toFloat64(quote_amount)) AS quote_amount_raw,
    argMin(tx_from, (block_number, tx_index, ordinal)) AS funder_of,
    max(bundle) AS bundle_size
  FROM
  (
    SELECT
      trader, block_number, tx_index, ordinal, token_amount, quote_amount,
      tx_from,
      count(DISTINCT trader) OVER (PARTITION BY tx_id) AS bundle
    FROM launchpad_trades_by_token FINAL
    WHERE chain = {chain:UInt64} AND token = token_id
      AND is_deleted = 0 AND side = 'buy'
      AND length({token:String}) IN (40, 64)
      AND match({token:String}, '^[0-9a-fA-F]+$')
      AND emitter IN (
        SELECT curve FROM launchpad_trusted_curves_v
        WHERE chain = {chain:UInt64})
      AND block_number <= (
        SELECT min(block_number) + {blocks:UInt64}
        FROM launchpad_tokens FINAL
        WHERE chain = {chain:UInt64} AND token = token_id
          AND is_deleted = 0
          AND emitter IN (
            SELECT curve FROM launchpad_trusted_curves_v
            WHERE chain = {chain:UInt64}))
  )
  GROUP BY trader
) AS b
CROSS JOIN
(
  SELECT
    min(block_number) AS launch_block,
    argMin(creator, (block_number, tx_index, ordinal)) AS launch_creator,
    argMin(toFloat64(initial_supply), (block_number, tx_index, ordinal)) AS initial_supply_raw
  FROM launchpad_tokens FINAL
  WHERE chain = {chain:UInt64} AND token = token_id
    AND is_deleted = 0
    AND emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
) AS l
ORDER BY token_amount_raw DESC;

-- ------------------------------------------------ screen: venue stats
--
-- Launches, graduations and curve volume per venue emitter and day.
-- graduation_rate is same-day graduations over same-day launches, which is
-- a THROUGHPUT ratio, not a cohort survival rate (a token launched on
-- Monday graduates on Tuesday): the cohort answer per token is
-- launchpad_creator_tokens_v / launchpad_token_v.
CREATE VIEW IF NOT EXISTS launchpad_venues_1d_all_v AS
SELECT
  chain, family, emitter, bucket,
  sum(p_launches) AS launches,
  sum(p_graduations) AS graduations,
  if(sum(p_launches) > 0, sum(p_graduations) / sum(p_launches), NULL) AS graduation_rate,
  sum(p_trades) AS trades,
  sum(p_buys) AS buys,
  sum(p_volume) AS volume_quote_raw,
  sum(p_volume_verified) AS volume_quote_verified_raw,
  sum(p_fees) AS fees_raw,
  sum(p_graduated_quote) AS graduated_quote_raw,
  max(p_traders) AS unique_traders,
  max(p_tokens_traded) AS unique_tokens_traded,
  max(p_creators) AS unique_creators,
  emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v
    WHERE chain = {chain:UInt64}) AS trusted
FROM
(
  SELECT chain, family, emitter, bucket,
         toUInt64(0) AS p_launches, toUInt64(0) AS p_graduations,
         trades AS p_trades, buys AS p_buys,
         volume_quote_raw AS p_volume,
         volume_quote_verified_raw AS p_volume_verified,
         fees_raw AS p_fees, 0. AS p_graduated_quote,
         unique_traders AS p_traders,
         unique_tokens AS p_tokens_traded,
         toUInt64(0) AS p_creators
  FROM launchpad_venue_trades_1d_v WHERE chain = {chain:UInt64}
  UNION ALL
  SELECT chain, family, emitter, bucket,
         sum(launches) AS p_launches, toUInt64(0) AS p_graduations,
         toUInt64(0) AS p_trades, toUInt64(0) AS p_buys,
         0. AS p_volume, 0. AS p_volume_verified, 0. AS p_fees,
         0. AS p_graduated_quote, toUInt64(0) AS p_traders,
         toUInt64(0) AS p_tokens_traded,
         uniqExact(creator) AS p_creators
  FROM launchpad_launches_1d_v WHERE chain = {chain:UInt64}
  GROUP BY chain, family, emitter, bucket
  UNION ALL
  SELECT chain, family, emitter, bucket,
         toUInt64(0) AS p_launches, graduations AS p_graduations,
         toUInt64(0) AS p_trades, toUInt64(0) AS p_buys,
         0. AS p_volume, 0. AS p_volume_verified, 0. AS p_fees,
         quote_in_raw AS p_graduated_quote, toUInt64(0) AS p_traders,
         toUInt64(0) AS p_tokens_traded, toUInt64(0) AS p_creators
  FROM launchpad_graduations_1d_v WHERE chain = {chain:UInt64}
)
GROUP BY chain, family, emitter, bucket
ORDER BY bucket DESC, volume_quote_raw DESC;

CREATE VIEW IF NOT EXISTS launchpad_venues_1d_v AS
SELECT
  k.chain AS chain, k.family AS family, k.bucket AS bucket,
  ifNull(l.launches, 0) AS launches,
  ifNull(g.graduations, 0) AS graduations,
  if(ifNull(l.launches, 0) > 0,
     ifNull(g.graduations, 0) / l.launches, NULL) AS graduation_rate,
  ifNull(t.trades, 0) AS trades,
  ifNull(t.buys, 0) AS buys,
  ifNull(t.volume_quote_raw, 0.) AS volume_quote_raw,
  ifNull(t.volume_quote_verified_raw, 0.) AS volume_quote_verified_raw,
  ifNull(t.fees_raw, 0.) AS fees_raw,
  ifNull(g.graduated_quote_raw, 0.) AS graduated_quote_raw,
  ifNull(t.unique_traders, 0) AS unique_traders,
  ifNull(t.unique_tokens_traded, 0) AS unique_tokens_traded,
  ifNull(l.unique_creators, 0) AS unique_creators,
  ifNull(t.emitters, 0) AS emitters
FROM
(
  SELECT DISTINCT chain, family, bucket FROM
  (
    SELECT chain, family, bucket FROM launchpad_venue_trades_1d
    WHERE chain = {chain:UInt64}
    UNION ALL
    SELECT chain, family, bucket FROM launchpad_launches_1d
    WHERE chain = {chain:UInt64}
    UNION ALL
    SELECT chain, family, bucket FROM launchpad_graduations_1d
    WHERE chain = {chain:UInt64}
  )
) AS k
LEFT JOIN
(
  SELECT
    a.family AS t_family, a.bucket AS t_bucket,
    toUInt64(sum(a.trades)) AS trades,
    toUInt64(sum(a.buys)) AS buys,
    sum(a.volume_quote) AS volume_quote_raw,
    sum(a.volume_quote_verified) AS volume_quote_verified_raw,
    sum(a.fees) AS fees_raw,
    uniqMerge(a.traders) AS unique_traders,
    uniqMerge(a.tokens) AS unique_tokens_traded,
    uniqExact(a.emitter) AS emitters
  FROM launchpad_venue_trades_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.chain = {chain:UInt64} AND a.epoch >= ifNull(f.epoch_floor, 0)
    AND a.emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY t_family, t_bucket
) AS t ON t.t_family = k.family AND t.t_bucket = k.bucket
LEFT JOIN
(
  SELECT
    a.family AS l_family, a.bucket AS l_bucket,
    toUInt64(sum(a.launches)) AS launches,
    uniqExact(a.creator) AS unique_creators
  FROM launchpad_launches_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.chain = {chain:UInt64} AND a.epoch >= ifNull(f.epoch_floor, 0)
    AND a.emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY l_family, l_bucket
) AS l ON l.l_family = k.family AND l.l_bucket = k.bucket
LEFT JOIN
(
  SELECT
    a.family AS g_family, a.bucket AS g_bucket,
    toUInt64(sum(a.graduations)) AS graduations,
    sum(a.quote_in) AS graduated_quote_raw
  FROM launchpad_graduations_1d AS a
  ASOF LEFT JOIN epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
  WHERE a.chain = {chain:UInt64} AND a.epoch >= ifNull(f.epoch_floor, 0)
    AND a.emitter IN (
      SELECT curve FROM launchpad_trusted_curves_v
      WHERE chain = {chain:UInt64})
  GROUP BY g_family, g_bucket
) AS g ON g.g_family = k.family AND g.g_bucket = k.bucket
WHERE trades > 0 OR launches > 0 OR graduations > 0
ORDER BY bucket DESC, volume_quote_raw DESC;

-- ------------------------------------------- screen: front ends
--
-- Front ends (fomo, GMGN, Axiom, trading bots) have no contracts: they
-- show up as the router a curve names (`caller`) or as the transaction's
-- `to`. This splits a VENUE's volume by front end without ever adding the
-- two together - the venue total is the same number either way.
CREATE VIEW IF NOT EXISTS launchpad_frontend_volume_v AS
SELECT
  t.family AS family,
  t.emitter AS emitter,
  multiIf(fc.name != '', fc.name, ft.name != '', ft.name, 'direct') AS frontend,
  toUInt64(count()) AS trades,
  sum(toFloat64(t.quote_amount)) AS volume_quote_raw,
  sumIf(toFloat64(t.quote_amount), t.quote_verified = 1) AS volume_quote_verified_raw,
  uniqExact(t.trader) AS unique_traders
FROM launchpad_trades AS t FINAL
LEFT JOIN launchpad_frontends AS fc FINAL
  ON fc.chain = t.chain AND fc.address = t.caller
LEFT JOIN launchpad_frontends AS ft FINAL
  ON ft.chain = t.chain AND ft.address = t.tx_to
WHERE t.chain = {chain:UInt64} AND t.is_deleted = 0
  AND t.timestamp >= toDateTime({since:UInt32})
  AND t.emitter IN (
    SELECT curve FROM launchpad_trusted_curves_v WHERE chain = {chain:UInt64})
GROUP BY family, emitter, frontend
ORDER BY volume_quote_raw DESC;
