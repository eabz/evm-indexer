-- Prediction markets: base and read path tables (docs/design.md, section 10).
-- Rust side: src/predictions (models.rs mirrors every table written here).
--
-- Designed backwards from the screens of a trading UI - see
-- src/predictions/README.md for the screen -> query cookbook.
--
-- Storage rules (docs/design.md, sections 1 and 2): binary FixedString /
-- UInt256 columns, no hex. Every block scoped table is
-- ReplacingMergeTree(_version, is_deleted) and carries epoch (the purge
-- generation of its chain). Nothing is ever deleted: a purge INSERTs
-- tombstones (the row again, newer _version, is_deleted = 1), FINAL hides
-- them. Tables written in block order are partitioned by month only (50+
-- chains share the database, chain is the first sorting key column),
-- lookup / side tables by chain. Side tables are fed by materialized views
-- that pass _version, is_deleted and epoch through, so a tombstone on a
-- base table tombstones its side table rows by itself.
--
-- Identity: market_id = the CTF conditionId, registry = the ERC-1155
-- contract holding the positions (a forged registry never collides with
-- the real one), outcome_token_id = the ERC-1155 id of one outcome.
--
-- CHAIN NEUTRAL (docs/design.md section 13, docs/solana-research.md
-- section 0). Prediction markets on a non-EVM chain feed the SAME tables,
-- so nothing here is EVM shaped:
--   * every identity column (registry, exchange, emitter, maker, taker,
--     holder, trader, oracle, creator, collateral_token, tx_from, tx_to,
--     counterparty...) is FixedString(32). An EVM address is stored as 12
--     zero bytes + its 20 bytes, a Solana pubkey its 32 raw bytes.
--     Readers print an id with the family of its chain (the chains
--     registry): concat('0x', lower(hex(substring(id, 13)))) for 'evm',
--     base58Encode(id) for 'svm'.
--   * transaction id = tx_id String, the RAW bytes (32 on EVM, 64 on
--     Solana). Never a sorting key column.
--   * position = (chain, block_number, tx_index, ordinal). block_number
--     KEEPS its name (purge / tombstone / checkpoint code keys on it) and
--     holds the slot on Solana. tx_index UInt32 is the EVM transaction
--     index, ordinal UInt64 the EVM log index (on Solana the packed
--     instruction tree path).
-- market_id / question_id / order_hash are 32 bytes on every chain already
-- and outcome_token_id stays UInt256.

-- USER POPULATED, and the ONE thing that separates a real market from a
-- forgery. Nothing on chain does: ConditionPreparation, PositionSplit and
-- OrderFilled are permissionless, so anyone can mint a market carrying a
-- real oracle + questionId (the decoder re-derives the conditionId, and a
-- forger can too) and trade against it at any price. The indexer ships NO
-- address list - it decodes by event family - so the operator says which
-- contracts it believes. src/predictions/README.md has ready INSERTs for
-- Polymarket and its known forks.
--   kind 'registry': address = the ERC-1155 conditional tokens contract.
--     registry = the same address.
--   kind 'exchange': address = the order book / AMM, registry = the
--     ERC-1155 contract it settles into.
-- The HEADLINE views (prediction_markets_v, prediction_trades_v,
-- prediction_holders_v, prediction_positions_v, prediction_candles_*_v,
-- prediction_leaderboard_v) count trusted emitters ONLY, so an empty table
-- means empty screens - missing numbers, never wrong ones. The *_all_v
-- views and prediction_markets_live_v stay unfiltered for forensics.
CREATE TABLE IF NOT EXISTS prediction_trusted (
  chain UInt64,
  -- 'registry' | 'exchange'
  kind LowCardinality(String),
  address FixedString(32),
  registry FixedString(32),
  note String DEFAULT '',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3)),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, kind, address)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The two lenses on it, so a view never has to remember the kind string.
CREATE VIEW IF NOT EXISTS prediction_trusted_registries_v AS
SELECT chain, address AS registry
FROM prediction_trusted FINAL
WHERE kind = 'registry' AND is_deleted = 0;

CREATE VIEW IF NOT EXISTS prediction_trusted_exchanges_v AS
SELECT chain, address AS exchange, registry
FROM prediction_trusted FINAL
WHERE kind = 'exchange' AND is_deleted = 0;

-- One row per ConditionPreparation (source 'event'). Keyed by identity
-- first - a condition can be prepared once per registry - then by
-- position, so a re-inserted block replaces itself and a reorged-out
-- preparation is tombstoned by block_number.
--
-- PARTITION BY chain, not by month (design section 1's default for base
-- tables): this table, prediction_resolutions and prediction_questions are
-- read by IDENTITY (market_id / question_id), never by time - every view
-- here does `FROM ... FINAL GROUP BY chain, registry, market_id`. A month
-- partitioning would fan a single market's FINAL over every month it was
-- ever touched. One row per market per chain also keeps the part count at
-- the chain count, which is the 50 the design budgets for.
CREATE TABLE IF NOT EXISTS prediction_markets (
  chain UInt64,
  market_id FixedString(32),
  registry FixedString(32),
  -- event family, 'ctf'
  protocol LowCardinality(String),
  oracle FixedString(32),
  question_id FixedString(32),
  outcome_count UInt16,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  tx_from FixedString(32),
  -- 'event' | 'rpc'
  source LowCardinality(String),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, market_id, registry, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- One row per ConditionResolution: the payout vector.
CREATE TABLE IF NOT EXISTS prediction_resolutions (
  chain UInt64,
  market_id FixedString(32),
  registry FixedString(32),
  oracle FixedString(32),
  question_id FixedString(32),
  outcome_count UInt16,
  -- one numerator per outcome. [1, 0] = outcome 0 won, [1, 1] = 50 / 50
  payout_numerators Array(UInt256),
  payout_denominator UInt256,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, market_id, registry, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- What the chain says ABOUT a market: titles and outcome labels (UMA
-- ancillary data, NegRisk payloads), the event a market belongs to,
-- disputes. A row describes the market whose question_id it carries when
-- its emitter is the market's oracle.
--   kind: 'uma_question' | 'uma_reset' | 'uma_flagged' |
--         'neg_risk_event' (question_id = event_id) | 'neg_risk_question'
CREATE TABLE IF NOT EXISTS prediction_questions (
  chain UInt64,
  question_id FixedString(32),
  emitter FixedString(32),
  kind LowCardinality(String),
  protocol LowCardinality(String),
  -- groups the markets of one multi outcome event, zero bytes when none
  event_id FixedString(32),
  question_index UInt32,
  title String,
  description String CODEC(ZSTD(3)),
  -- labels by outcome index, empty when the text does not name them
  outcomes Array(String),
  -- the raw payload
  data String CODEC(ZSTD(3)),
  creator FixedString(32),
  oracle FixedString(32),
  reward_token FixedString(32),
  reward UInt256,
  proposal_bond UInt256,
  fee_bips UInt32,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, question_id, emitter, kind, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Outcome index <-> ERC-1155 token id. NOT block scoped: the id is
-- keccak(collateral, collection(condition, 1 << index)), arithmetic no
-- reorg can change. Rows are COMPUTED by the decoder from every split /
-- merge / redemption, the earliest sighting has the highest _version.
CREATE TABLE IF NOT EXISTS prediction_outcome_tokens (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  market_id FixedString(32),
  outcome_index UInt16,
  collateral_token FixedString(32),
  first_seen_block UInt64,
  first_seen_timestamp DateTime,
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, registry, outcome_token_id)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The same map from the market's side.
CREATE TABLE IF NOT EXISTS prediction_outcome_tokens_by_market (
  chain UInt64,
  market_id FixedString(32),
  registry FixedString(32),
  collateral_token FixedString(32),
  outcome_index UInt16,
  outcome_token_id UInt256,
  first_seen_block UInt64,
  first_seen_timestamp DateTime,
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, market_id, registry, collateral_token, outcome_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_outcome_tokens_by_market_mv
TO prediction_outcome_tokens_by_market AS
SELECT
  chain, market_id, registry, collateral_token, outcome_index,
  outcome_token_id, first_seen_block, first_seen_timestamp, _version
FROM prediction_outcome_tokens;

-- THE canonical trade: one row per filled MAKER order, told from the
-- TAKER's point of view. The taker order's own OrderFilled and
-- OrdersMatched are never rows (they describe the same shares again).
--   price of the taker's token  = collateral_amount / share_amount
--   price of the maker's token  = maker_collateral_amount / share_amount
--   match_type 'complementary' | 'direct' | 'amm': one token, one price
--   match_type 'mint' | 'merge': two tokens, the prices add up to 1
CREATE TABLE IF NOT EXISTS prediction_trades (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  -- event family: 'ctf_exchange' | 'ctf_exchange_v2' | 'fpmm'
  protocol LowCardinality(String),
  -- emitter: the exchange or the AMM pool
  exchange FixedString(32),
  -- the ERC-1155 contract that moved the token in the same transaction
  registry FixedString(32),
  order_hash FixedString(32),
  maker FixedString(32),
  taker FixedString(32),
  tx_from FixedString(32),
  tx_to FixedString(32),
  outcome_token_id UInt256,
  -- the TAKER's side: 'buy' | 'sell'
  side LowCardinality(String),
  share_amount UInt256,
  -- collateral of the taker for share_amount, fees excluded
  collateral_amount UInt256,
  match_type LowCardinality(String),
  -- 1 when the SHARES of this fill are proven: the same transaction
  -- carries ERC-1155 transfers of this exact outcome_token_id emitted by
  -- this registry, and the fills of that (registry, token) do not claim
  -- more shares than actually moved. A lone forged OrderFilled naming a
  -- real token id is 0. Candles, the ledger's priced legs and the
  -- leaderboard count verified = 1 ONLY (see 0021). The raw row is always
  -- kept - prediction_trades_all_v shows it.
  verified UInt8 DEFAULT 0,
  maker_outcome_token_id UInt256,
  maker_side LowCardinality(String),
  maker_collateral_amount UInt256,
  maker_fee_amount UInt256,
  -- 'collateral' | 'shares'
  maker_fee_unit LowCardinality(String),
  taker_fee_amount UInt256,
  taker_fee_unit LowCardinality(String),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Collateral entering / leaving a market.
--   protocol 'ctf': the registry itself - the source of open interest
--   protocol 'ctf_adapter' | 'neg_risk': an adapter naming the real user
--     (attribution only, never added to open interest again)
--   kind: 'split' | 'merge' | 'redeem' | 'convert'
CREATE TABLE IF NOT EXISTS prediction_position_events (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  protocol LowCardinality(String),
  emitter FixedString(32),
  kind LowCardinality(String),
  stakeholder FixedString(32),
  -- conditionId, for 'convert' the NegRisk event id
  market_id FixedString(32),
  collateral_token FixedString(32),
  parent_collection_id FixedString(32),
  index_sets Array(UInt256),
  -- collateral: split / merged amount, payout of a redemption
  amount UInt256,
  tx_from FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- One row per ERC-1155 id moved. from_reason / to_reason say why the two
-- accounts' balances changed:
--   'split' | 'merge' | 'redeem' | 'trade' | 'transfer'
CREATE TABLE IF NOT EXISTS prediction_transfers (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  -- position inside a TransferBatch
  batch_index UInt32,
  registry FixedString(32),
  operator FixedString(32),
  `from` FixedString(32),
  `to` FixedString(32),
  outcome_token_id UInt256,
  amount UInt256,
  from_reason LowCardinality(String),
  to_reason LowCardinality(String),
  -- cost basis convention of a split / merge leg: amount / outcomes of
  -- the partition (a full set costs exactly amount). Zero otherwise.
  priced_collateral UInt256,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal, batch_index)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- What only the contract can tell about an exchange / pool: the token it
-- is paid in. Written by the background resolver (never on the commit
-- path), not block scoped. source: 'rpc' | 'unresolved'.
CREATE TABLE IF NOT EXISTS prediction_venues (
  chain UInt64,
  exchange FixedString(32),
  protocol LowCardinality(String),
  collateral_token FixedString(32),
  registry FixedString(32),
  source LowCardinality(String),
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, exchange)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- USER POPULATED: brand names. The indexer decodes by event family and
-- ships no address list. A label on a registry names every market of it,
-- a label on an exchange names the trades and excludes the exchange
-- contract from the leaderboard. address is a 32 byte id: an EVM address
-- is left padded with 12 zero bytes, which unhex does for you when the
-- hex string carries them.
--   INSERT INTO prediction_venue_labels (chain, address, venue) VALUES
--     (137, unhex('0000000000000000000000004D97DCd97eC945f40cF65F87097ACe5EA0476045'), 'polymarket')
CREATE TABLE IF NOT EXISTS prediction_venue_labels (
  chain UInt64,
  address FixedString(32),
  venue String,
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, address)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- FILLED BY AN EXTERNAL ENRICHER, never by the indexer: what is not on
-- chain (Polymarket Gamma API: slug, category, tags, image, end date,
-- outcome labels of markets whose text is off chain). prediction_markets_v
-- LEFT JOINs it, so the UI query does not change when a row appears. Every
-- column is NULL / empty until then.
CREATE TABLE IF NOT EXISTS prediction_market_metadata (
  chain UInt64,
  market_id FixedString(32),
  title Nullable(String),
  description Nullable(String),
  slug Nullable(String),
  category Nullable(String),
  tags Array(String),
  image_url Nullable(String),
  outcomes Array(String),
  end_date Nullable(DateTime('UTC')),
  event_title Nullable(String),
  event_slug Nullable(String),
  -- who wrote the row: 'gamma' ...
  source LowCardinality(String),
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, market_id)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Trades tape: the trades of one outcome token, newest last. A market's
-- tape is the union of its (two) tokens.
CREATE TABLE IF NOT EXISTS prediction_trades_by_token (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  block_number UInt64,
  tx_index UInt32,
  ordinal UInt64,
  timestamp DateTime,
  tx_id String,
  protocol LowCardinality(String),
  exchange FixedString(32),
  maker FixedString(32),
  taker FixedString(32),
  tx_from FixedString(32),
  side LowCardinality(String),
  share_amount UInt256,
  collateral_amount UInt256,
  match_type LowCardinality(String),
  verified UInt8 DEFAULT 0,
  taker_fee_amount UInt256,
  taker_fee_unit LowCardinality(String),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, registry, outcome_token_id, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_trades_by_token_mv
TO prediction_trades_by_token AS
SELECT
  chain, registry, outcome_token_id, block_number, tx_index, ordinal,
  timestamp, tx_id, protocol, exchange, maker, taker, tx_from, side,
  share_amount, collateral_amount, match_type, verified, taker_fee_amount,
  taker_fee_unit, epoch, _version, is_deleted
FROM prediction_trades;

-- The ledger of an account: everything that changed a balance (transfer
-- legs, share_delta != 0) and everything that has a price (trade legs,
-- share_delta = 0: the shares of a trade move in its transfer legs).
--   reason: 'split' | 'merge' | 'redeem' | 'trade' | 'transfer' (transfer
--           legs), 'buy' | 'sell' (trade legs)
--   leg: 0 = sender / maker, 1 = receiver / taker
-- Balances are exact: sum(share_delta) per (registry, token, holder).
CREATE TABLE IF NOT EXISTS prediction_ledger_by_holder (
  chain UInt64,
  holder FixedString(32),
  registry FixedString(32),
  outcome_token_id UInt256,
  block_number UInt64,
  tx_index UInt32,
  ordinal UInt64,
  sub_index UInt32,
  leg UInt8,
  timestamp DateTime,
  tx_id String,
  reason LowCardinality(String),
  share_delta Int256,
  shares UInt256,
  -- priced legs only (buy / sell / split / merge), else 0
  collateral UInt256,
  fee UInt256,
  fee_unit LowCardinality(String),
  counterparty FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, holder, registry, outcome_token_id, block_number, tx_index, ordinal, sub_index, leg)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The same ledger by token: holders of an outcome.
CREATE TABLE IF NOT EXISTS prediction_ledger_by_token (
  chain UInt64,
  registry FixedString(32),
  outcome_token_id UInt256,
  holder FixedString(32),
  block_number UInt64,
  tx_index UInt32,
  ordinal UInt64,
  sub_index UInt32,
  leg UInt8,
  timestamp DateTime,
  tx_id String,
  reason LowCardinality(String),
  share_delta Int256,
  shares UInt256,
  collateral UInt256,
  fee UInt256,
  fee_unit LowCardinality(String),
  counterparty FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, registry, outcome_token_id, holder, block_number, tx_index, ordinal, sub_index, leg)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_ledger_by_holder_transfers_mv
TO prediction_ledger_by_holder AS
SELECT
  chain, tupleElement(entry, 1) AS holder, registry, outcome_token_id,
  block_number, tx_index, ordinal, batch_index AS sub_index,
  tupleElement(entry, 2) AS leg, timestamp, tx_id,
  tupleElement(entry, 3) AS reason,
  tupleElement(entry, 4) AS share_delta,
  amount AS shares,
  tupleElement(entry, 5) AS collateral,
  toUInt256(0) AS fee, 'collateral' AS fee_unit,
  tupleElement(entry, 6) AS counterparty,
  epoch, _version, is_deleted
FROM
(
  SELECT *, arrayJoin([
    (`from`, toUInt8(0), toString(from_reason), negate(toInt256(amount)), if(from_reason = 'merge', priced_collateral, toUInt256(0)), `to`),
    (`to`, toUInt8(1), toString(to_reason), toInt256(amount), if(to_reason = 'split', priced_collateral, toUInt256(0)), `from`)
  ]) AS entry
  FROM prediction_transfers
)
WHERE tupleElement(entry, 1) != toFixedString('', 32);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_ledger_by_token_transfers_mv
TO prediction_ledger_by_token AS
SELECT
  chain, registry, outcome_token_id, tupleElement(entry, 1) AS holder,
  block_number, tx_index, ordinal, batch_index AS sub_index,
  tupleElement(entry, 2) AS leg, timestamp, tx_id,
  tupleElement(entry, 3) AS reason,
  tupleElement(entry, 4) AS share_delta,
  amount AS shares,
  tupleElement(entry, 5) AS collateral,
  toUInt256(0) AS fee, 'collateral' AS fee_unit,
  tupleElement(entry, 6) AS counterparty,
  epoch, _version, is_deleted
FROM
(
  SELECT *, arrayJoin([
    (`from`, toUInt8(0), toString(from_reason), negate(toInt256(amount)), if(from_reason = 'merge', priced_collateral, toUInt256(0)), `to`),
    (`to`, toUInt8(1), toString(to_reason), toInt256(amount), if(to_reason = 'split', priced_collateral, toUInt256(0)), `from`)
  ]) AS entry
  FROM prediction_transfers
)
WHERE tupleElement(entry, 1) != toFixedString('', 32);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_ledger_by_holder_trades_mv
TO prediction_ledger_by_holder AS
SELECT
  chain, tupleElement(entry, 1) AS holder, registry,
  tupleElement(entry, 4) AS outcome_token_id,
  block_number, tx_index, ordinal, toUInt32(0) AS sub_index,
  tupleElement(entry, 2) AS leg, timestamp, tx_id,
  tupleElement(entry, 3) AS reason,
  toInt256(0) AS share_delta,
  share_amount AS shares,
  tupleElement(entry, 5) AS collateral,
  tupleElement(entry, 6) AS fee,
  tupleElement(entry, 7) AS fee_unit,
  tupleElement(entry, 8) AS counterparty,
  epoch, _version, is_deleted
FROM
(
  SELECT *, arrayJoin([
    (maker, toUInt8(0), toString(maker_side), maker_outcome_token_id, maker_collateral_amount, maker_fee_amount, toString(maker_fee_unit), taker),
    (taker, toUInt8(1), toString(side), outcome_token_id, collateral_amount, taker_fee_amount, toString(taker_fee_unit), maker)
  ]) AS entry
  FROM prediction_trades
  WHERE verified = 1
);

CREATE MATERIALIZED VIEW IF NOT EXISTS prediction_ledger_by_token_trades_mv
TO prediction_ledger_by_token AS
SELECT
  chain, registry,
  tupleElement(entry, 4) AS outcome_token_id,
  tupleElement(entry, 1) AS holder,
  block_number, tx_index, ordinal, toUInt32(0) AS sub_index,
  tupleElement(entry, 2) AS leg, timestamp, tx_id,
  tupleElement(entry, 3) AS reason,
  toInt256(0) AS share_delta,
  share_amount AS shares,
  tupleElement(entry, 5) AS collateral,
  tupleElement(entry, 6) AS fee,
  tupleElement(entry, 7) AS fee_unit,
  tupleElement(entry, 8) AS counterparty,
  epoch, _version, is_deleted
FROM
(
  SELECT *, arrayJoin([
    (maker, toUInt8(0), toString(maker_side), maker_outcome_token_id, maker_collateral_amount, maker_fee_amount, toString(maker_fee_unit), taker),
    (taker, toUInt8(1), toString(side), outcome_token_id, collateral_amount, taker_fee_amount, toString(taker_fee_unit), maker)
  ]) AS entry
  FROM prediction_trades
  WHERE verified = 1
);
