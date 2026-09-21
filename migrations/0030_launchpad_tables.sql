-- Token launchpads: base tables, read-path side tables and the two
-- operator tables (docs/design.md section 11). Evidence, trust rule and
-- the query cookbook: src/launchpads/README.md.
--
-- CHAIN NEUTRAL IDENTITY (docs/design.md section 13). Every identity
-- column here is FixedString(32): on EVM the 20 address bytes left padded
-- with 12 zero bytes, on Solana the 32 raw pubkey bytes, exactly like
-- dex_pools.pool_id, so a pubkey fits the same column later without
-- rebuilding a sorting key. Joins into dex_pools.pool_id are direct, and
-- joins into the EVM only tables (tokens, erc20_transfers) PAD those
-- tables' FixedString(20) address up to 32 bytes, they never truncate the
-- 32 byte side (see dex_token_info_v in 0012 and the holder view of 0032).
-- Readers print an id with the family of its chain (the chains registry of
-- migration 0006, which is also where THE expression lives):
--   concat('0x', lower(hex(substring(id, 13))))  for 'evm'
--   base58Encode(substring(id, 1, 32))           for 'svm'
-- The substring() is NOT optional: toString(FixedString) and CAST(id AS
-- String) TRIM TRAILING ZERO BYTES, so anything that routes an id through
-- them silently shortens a pubkey. substring(id, 1, 32) and concat(id, '')
-- keep every byte, which is why hex(substring(id, 13)) is safe too. Do not
-- rely on base58Encode(id) doing the conversion right: it keeps all 32
-- bytes on 25.12.1.322, but the form above is correct on every build.
-- A pool_id is NOT an address even on EVM (a Uniswap V4 / Balancer pool id
-- is a native 32 byte value): print all 32 bytes, never the 'evm' branch.
--
-- tx_id is the raw transaction id as a String: 32 bytes on EVM, 64 on
-- Solana (a signature does not fit a FixedString(32)). It is never part of
-- a sorting key.
--
-- The position of a row is (chain, block_number, tx_index, ordinal)
-- instead of (chain, block_number, log_index): ordinal IS the log index
-- on EVM, and the packed instruction tree path on Solana.
--
-- Insert-only reorg support (docs/design.md section 2): nothing is ever
-- deleted. Every block scoped table is ReplacingMergeTree(_version,
-- is_deleted), a purge INSERTs tombstones and FINAL hides them. Side
-- tables are fed by materialized views that pass _version, is_deleted and
-- epoch through, so a tombstone on a base table tombstones its side table
-- rows by itself.
--
-- Partitioning: the three event-stream tables by month, the launch
-- registry and every side table by chain. Deduplication windows are
-- turned on by 0033, which is this module's own copy of 0090 (an applied
-- migration never changes).
--
-- WHY launchpad_tokens is PARTITION BY chain and not by month, which is
-- design section 1's default for a base table. It is a REGISTRY, read by
-- IDENTITY and never by time: the token page, the price chart, the sniper
-- view, the holder list and the curve -> token join every fee row needs
-- all ask `WHERE chain = ? AND token = ?`, and its sorting key starts
-- (chain, token, ...) for exactly that. Month partitioning would fan a
-- single token's FINAL over every month that token was ever touched, on
-- the module's most-read screen. The partition budget is unaffected
-- because there is one partition per chain (the ~50 the design budgets
-- for), not chain x months. Same table shape, same reason and same
-- exception as dex_pools (0010) and prediction_markets / _resolutions /
-- _questions (0020). The three event streams - trades, graduations,
-- creator_fees - are written and purged by block range, so they keep the
-- month partitioning the design asks for. src/launchpads/mod.rs's
-- `migrations_follow_the_schema_rules` pins this table by name: a new
-- block-scoped table gets month partitioning unless it is added there
-- with a reason.
--
-- NOTHING HERE IS TRUSTED. Any contract can emit a TokenLaunched or a
-- CurveBuy: read through the views of 0032, which count only emitters an
-- operator listed in launchpad_trusted_emitters.

-- One row per LAUNCH EVENT. A launch event is a CLAIM of its emitter.
-- Columns a family's event does not carry stay zero / empty - nothing is
-- ever inferred:
--   curve       the contract that emits the token's curve trades (the
--               per-token curve for pons_v2 / bags, the portal itself for
--               flap_portal). Zero for attribution-only families, whose
--               trading is in dex_swaps.
--   pool_id     the destination pool NAMED BY THE LAUNCH EVENT
--               (attribution-only families launch straight into a pool).
--               Zero for curve families: their pool exists at graduation.
--   pool_kind   'pool_id' a Uniswap V4 style bytes32 id | 'pool_address'
--               a pool contract, left padded. Both join dex_pools.pool_id.
--   quote_token zero = the chain's native coin (pons_v2 and flap_portal
--               state it in the event / in TokenQuoteSet of the same
--               transaction), and zero too when a family never names it.
CREATE TABLE IF NOT EXISTS launchpad_tokens (
  chain UInt64,
  token FixedString(32),
  family LowCardinality(String),
  emitter FixedString(32),
  curve FixedString(32),
  creator FixedString(32),
  name String CODEC(ZSTD(3)),
  symbol String CODEC(ZSTD(3)),
  metadata_uri String CODEC(ZSTD(3)),
  quote_token FixedString(32),
  initial_supply UInt256,
  graduation_threshold UInt256,
  pool_id FixedString(32),
  pool_kind LowCardinality(String),
  launch_config_id UInt256,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  tx_from FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, token, emitter, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- One row per bonding-curve buy / sell.
--   token / quote_token are only trustworthy when their *_verified flag
--   is 1: that flag means the ASSET CONTRACT ITSELF reported a movement of
--   exactly this leg's amount to / from the emitter in the same
--   transaction (src/launchpads/decode.rs). A native coin leg has no log
--   and is NEVER verified.
--   quote_amount is what the venue reports: GROSS on a pons_v2 buy and on
--   every flap_portal trade, NET of fee and tax on a pons_v2 sell.
--   sole_unverified_quote = 1 when this is the only trade of the
--   transaction with an unverified quote leg. Only then does tx_value
--   bound the native amount paid (a router buying for fifteen wallets in
--   one transaction sends one value for all of them - a real fixture).
CREATE TABLE IF NOT EXISTS launchpad_trades (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  family LowCardinality(String),
  emitter FixedString(32),
  token FixedString(32),
  token_verified UInt8,
  quote_token FixedString(32),
  quote_verified UInt8,
  side LowCardinality(String),
  trader FixedString(32),
  caller FixedString(32),
  token_amount UInt256,
  quote_amount UInt256,
  fee_amount UInt256,
  tax_amount UInt256,
  progress_wad UInt256,
  graduating UInt8,
  sole_unverified_quote UInt8,
  tx_from FixedString(32),
  tx_to FixedString(32),
  tx_value UInt256,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- The curve finished and the liquidity moved into a DEX pool. pool_id is
-- THE join key into dex_pools / dex_pool_current_v / the DEX candles, so a
-- token's chart continues on the same page after graduation.
CREATE TABLE IF NOT EXISTS launchpad_graduations (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  family LowCardinality(String),
  emitter FixedString(32),
  token FixedString(32),
  pool_id FixedString(32),
  pool_kind LowCardinality(String),
  quote_token FixedString(32),
  token_amount UInt256,
  quote_amount UInt256,
  position_id UInt256,
  tx_from FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- One row per component of a fee sweep (protocol / buyback / creator /
-- locked) and per token tax. component is the position inside the log, so
-- several rows share (block_number, tx_index, ordinal).
--   recipient is filled only when the fee ESCROW named it in a Credited
--   event of the same transaction and amount (recipient_known = 1).
--   token is zero when the sweep names a pool or a curve instead: join
--   launchpad_tokens on curve / the graduation on pool_id.
CREATE TABLE IF NOT EXISTS launchpad_creator_fees (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  component UInt32,
  family LowCardinality(String),
  emitter FixedString(32),
  token FixedString(32),
  pool_id FixedString(32),
  phase LowCardinality(String),
  kind LowCardinality(String),
  recipient FixedString(32),
  recipient_known UInt8,
  quote_token FixedString(32),
  amount UInt256,
  tx_from FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal, component)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- Read path: the trades tape and the candle source of a token. Trades
-- whose token leg stayed unverified and whose family does not name the
-- token land under 32 zero bytes, where they can be looked at on purpose
-- and are never mistaken for a real token.
CREATE TABLE IF NOT EXISTS launchpad_trades_by_token (
  chain UInt64,
  token FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  family LowCardinality(String),
  emitter FixedString(32),
  side LowCardinality(String),
  trader FixedString(32),
  caller FixedString(32),
  tx_from FixedString(32),
  token_amount UInt256,
  quote_amount UInt256,
  fee_amount UInt256,
  tax_amount UInt256,
  progress_wad UInt256,
  token_verified UInt8,
  quote_verified UInt8,
  quote_token FixedString(32),
  graduating UInt8,
  tx_id String,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, token, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_trades_by_token_mv
TO launchpad_trades_by_token AS
SELECT
  chain, token, block_number, tx_index, ordinal, timestamp, family,
  emitter, side, trader, caller, tx_from, token_amount, quote_amount, fee_amount,
  tax_amount, progress_wad, token_verified, quote_verified, quote_token,
  graduating, tx_id, epoch, _version, is_deleted
FROM launchpad_trades;

-- Read path: what one wallet did on the curves (sniper and portfolio
-- screens). trader is the event's beneficiary, not the gas payer.
CREATE TABLE IF NOT EXISTS launchpad_trades_by_trader (
  chain UInt64,
  trader FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  family LowCardinality(String),
  emitter FixedString(32),
  token FixedString(32),
  side LowCardinality(String),
  token_amount UInt256,
  quote_amount UInt256,
  token_verified UInt8,
  quote_verified UInt8,
  caller FixedString(32),
  tx_from FixedString(32),
  tx_id String,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, trader, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_trades_by_trader_mv
TO launchpad_trades_by_trader AS
SELECT
  chain, trader, block_number, tx_index, ordinal, timestamp, family,
  emitter, token, side, token_amount, quote_amount, token_verified,
  quote_verified, caller, tx_from, tx_id, epoch, _version,
  is_deleted
FROM launchpad_trades;

-- Read path: the new-launch feed. Sorted by time so "newest first" is a
-- primary key read of the newest granules of one chain partition.
CREATE TABLE IF NOT EXISTS launchpad_launches_by_time (
  chain UInt64,
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  token FixedString(32),
  family LowCardinality(String),
  emitter FixedString(32),
  curve FixedString(32),
  creator FixedString(32),
  name String CODEC(ZSTD(3)),
  symbol String CODEC(ZSTD(3)),
  quote_token FixedString(32),
  initial_supply UInt256,
  graduation_threshold UInt256,
  pool_id FixedString(32),
  tx_id String,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, timestamp, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_launches_by_time_mv
TO launchpad_launches_by_time AS
SELECT
  chain, timestamp, block_number, tx_index, ordinal, token, family,
  emitter, curve, creator, name, symbol, quote_token, initial_supply,
  graduation_threshold, pool_id, tx_id, epoch, _version,
  is_deleted
FROM launchpad_tokens;

-- Read path: the creator page (every launch of one wallet).
CREATE TABLE IF NOT EXISTS launchpad_launches_by_creator (
  chain UInt64,
  creator FixedString(32),
  timestamp DateTime('UTC') CODEC(DoubleDelta, ZSTD),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64 CODEC(Delta, ZSTD),
  token FixedString(32),
  family LowCardinality(String),
  emitter FixedString(32),
  curve FixedString(32),
  name String CODEC(ZSTD(3)),
  symbol String CODEC(ZSTD(3)),
  quote_token FixedString(32),
  graduation_threshold UInt256,
  tx_id String,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, creator, timestamp, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS launchpad_launches_by_creator_mv
TO launchpad_launches_by_creator AS
SELECT
  chain, creator, timestamp, block_number, tx_index, ordinal, token,
  family, emitter, curve, name, symbol, quote_token,
  graduation_threshold, tx_id, epoch, _version, is_deleted
FROM launchpad_tokens;

-- OPERATOR DATA, never written by the indexer, not block scoped.
--
-- A real launchpad is a singleton contract with verified source, while a
-- forgery costs one transaction. Everything a headline view counts must
-- come from an emitter listed here (family '' retires a row). The
-- verified addresses found in Phase 1 are in src/launchpads/README.md as
-- ready-to-run INSERTs - migrations ship no chain-specific seed data
-- (same rule as quote_tokens and dex_trusted_emitters).
CREATE TABLE IF NOT EXISTS launchpad_trusted_emitters (
  chain UInt64,
  emitter FixedString(32),
  family LowCardinality(String),
  label String DEFAULT '',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, emitter);

-- Front ends (fomo, GMGN, Axiom, bots) have no contracts of their own:
-- they show up as the fee recipient or the router of somebody else's
-- venue. Their volume is NEVER venue volume (docs/design.md section 11).
-- launchpad_frontend_volume_v splits a venue's volume by front end.
--   kind: 'fee_recipient' | 'router'
CREATE TABLE IF NOT EXISTS launchpad_frontends (
  chain UInt64,
  address FixedString(32),
  name String DEFAULT '',
  kind LowCardinality(String),
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, address, kind);
