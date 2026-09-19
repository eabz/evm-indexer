-- DEX base tables and their read-path side tables (docs/design.md §1, §2, §5,
-- §13).
--
-- CHAIN NEUTRAL IDENTITY (§13). These tables are shared by every chain
-- family, so every identity column - pool, emitter, factory, token, trader,
-- sender, recipient, owner, tx_from, tx_to - is FixedString(32):
--   EVM address    12 zero bytes + the 20 address bytes
--   Solana pubkey  32 raw bytes
-- Print an EVM id with concat('0x', lower(hex(substring(x, 13)))), an SVM one
-- with base58Encode(substring(x, 1, 32)). The family comes from chains_v
-- (migration 0006), which also documents why the substring() is mandatory.
-- Compare with unhex(concat(repeat('00', 12), '<40 hex>')) on EVM.
-- A pool_id is NOT an address even on EVM (Uniswap V4 / Balancer ids are
-- native 32 byte values): print all 32 bytes.
--
-- POSITION KEY (§13): (chain, block_number, tx_index, ordinal).
--   block_number  block on EVM, slot on Solana - the NAME stays, purge /
--                 tombstone / checkpoint code keys on it
--   tx_index      UInt32, the transaction's index inside the block / slot
--   ordinal       UInt64, the log index on EVM, the packed instruction tree
--                 path on Solana
-- tx_id is the raw transaction id as a String: 32 bytes on EVM, 64 on
-- Solana. It is never part of a sorting key.
--
-- Other binary column types: amounts UInt256 / Int256. Nothing is stored as
-- hex. Query every table here with FINAL.
--
-- Sign convention of amount0 / amount1 everywhere: pool relative,
-- positive = INTO the pool, negative = out of the pool.
--
-- Insert-only reorg support (docs/design.md §2): nothing is ever deleted.
-- Every block scoped table is ReplacingMergeTree(_version, is_deleted), a
-- purge INSERTs tombstones (the row again with a newer _version and
-- is_deleted = 1) and FINAL hides them. Side tables are fed by materialized
-- views that pass _version, is_deleted and epoch through, so a tombstone on
-- a base table tombstones its side table rows by itself. epoch is the
-- chain's purge generation, stamped by the writer - the aggregates of
-- 0011 are keyed by it.
--
-- Partitioning (50+ chains share one database): base tables by month only,
-- never by chain - chain is the first sorting key column. Side tables and
-- dex_pools by chain (lookups must not fan out per month). A tombstone
-- copies its row, so it always lands in the partition of the row it kills.

-- One row per CREATION EVENT of a pool plus at most one row written by the
-- RPC resolver (created_block = 0, tx_index = 0, ordinal = 0), positional like every
-- other block scoped table: a re-inserted block replaces itself, a
-- reorged-out creation is tombstoned by created_block, a re-creation on the
-- canonical chain is simply another (or a newer) row.
-- A creation event is a CLAIM of whoever emitted it - a forged PairCreated
-- costs one transaction, and V2 / V3 pool addresses are predictable, so it
-- can even be emitted BEFORE the real one. Never read this table directly:
-- dex_pool_current_v decides what is known about a pool.
-- source: 'event' | 'rpc' (the pool's own getters answered) | 'unresolved'
-- (it is not a pool) | 'no_answer' (no code / no usable answer yet, asked
-- again with a backoff on attempts). Resolver rows are chain STATE, not
-- part of a block: no purge range ever contains them.
CREATE TABLE IF NOT EXISTS dex_pools (
  chain UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
  factory FixedString(32),
  protocol LowCardinality(String),
  token0 FixedString(32),
  token1 FixedString(32),
  tokens Array(FixedString(32)),
  underlying_tokens Array(FixedString(32)),
  fee UInt32,
  tick_spacing Int32,
  hooks FixedString(32),
  stable Bool,
  created_block UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  source LowCardinality(String),
  attempts UInt32 DEFAULT 0,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, pool_id, emitter, created_block, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- What is known about every pool, and how well (status):
--   'verified'   a pool that is its own contract answered token0() /
--                token1() / coins(i) itself (rpc row). That answer WINS over
--                every creation event: it is the one thing a third party can
--                not forge. factory, fee, created_block... come from the
--                first creation event that names the same tokens, if any.
--   'event'      pools of a singleton (Uniswap V4 PoolManager, Balancer
--                Vault): only the singleton can emit for its own (pool_id,
--                emitter) key, so its first event is authoritative FOR THAT
--                EMITTER. Whether the emitter is the real singleton is a
--                separate question: dex_trusted_emitters.
--   'unverified' creation event(s) naming one token set, not confirmed yet.
--   'contested'  creation events naming DIFFERENT token sets.
-- trusted = status IN ('verified', 'event'). Views that depend on pool
-- metadata use it only when trusted = 1 and yield NULL otherwise. Verified
-- swap legs (dex_swaps.verified_in / verified_out) do not depend on this
-- view at all.
CREATE VIEW IF NOT EXISTS dex_pool_current_v AS
SELECT
  chain, pool_id, emitter,
  multiIf(singleton, 'event', has_rpc, 'verified', token_sets > 1, 'contested', 'unverified') AS status,
  status IN ('verified', 'event') AS trusted,
  multiIf(singleton, events[1], has_rpc AND length(matching) > 0, matching[1], has_rpc, rpcs[1], events[1]) AS chosen,
  tupleElement(chosen, 1) AS created_block,
  tupleElement(chosen, 2) AS tx_index,
  tupleElement(chosen, 3) AS ordinal,
  tupleElement(chosen, 4) AS factory,
  tupleElement(chosen, 5) AS protocol,
  tupleElement(chosen, 6) AS token0,
  tupleElement(chosen, 7) AS token1,
  tupleElement(chosen, 8) AS tokens,
  if(has_rpc, tupleElement(rpcs[1], 9), tupleElement(chosen, 9)) AS underlying_tokens,
  tupleElement(chosen, 10) AS fee,
  tupleElement(chosen, 11) AS tick_spacing,
  tupleElement(chosen, 12) AS hooks,
  tupleElement(chosen, 13) AS stable,
  tupleElement(chosen, 14) AS timestamp,
  tupleElement(chosen, 15) AS tx_id,
  tupleElement(chosen, 16) AS source,
  toUInt64(length(events) + length(rpcs)) AS candidates,
  token_sets
FROM
(
  SELECT
    chain, pool_id, emitter, events, rpcs, singleton,
    length(rpcs) > 0 AND NOT singleton AS has_rpc,
    arrayFilter(e -> tupleElement(e, 8) = tupleElement(rpcs[1], 8), events) AS matching,
    toUInt64(length(arrayDistinct(arrayMap(e -> tupleElement(e, 8), events)))) AS token_sets
  FROM
  (
    SELECT
      chain, pool_id, emitter,
      arraySort(groupArrayIf(facts, source = 'event')) AS events,
      groupArrayIf(facts, source = 'rpc') AS rpcs,
      countIf(source = 'event' AND protocol IN ('uniswap_v4', 'balancer_v2')) > 0 AS singleton
    FROM
    (
      SELECT
        chain, pool_id, emitter, source, protocol,
        (created_block, tx_index, ordinal, factory, toString(protocol), token0, token1, tokens, underlying_tokens, fee, tick_spacing, hooks, stable, timestamp, tx_id, toString(source)) AS facts
      FROM dex_pools FINAL
      WHERE source IN ('event', 'rpc')
    )
    GROUP BY chain, pool_id, emitter
  )
);

-- token_in / token_out are what the EVENT says (Balancer), a claim.
-- verified_in / verified_out are the tokens PROVEN to have moved: an ERC-20
-- Transfer of exactly amount_in to the emitter / amount_out from the
-- emitter in the same transaction, emitted by that token (all zero bytes
-- when nothing proves the leg). Every USD number is built on them and on
-- nothing else. reserve0 / reserve1: V2 / Solidly reserves after the swap.
CREATE TABLE IF NOT EXISTS dex_swaps (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
  protocol LowCardinality(String),
  sender FixedString(32),
  recipient FixedString(32),
  tx_from FixedString(32),
  tx_to FixedString(32),
  trader FixedString(32),
  amount0 Int256,
  amount1 Int256,
  token_in FixedString(32),
  token_out FixedString(32),
  amount_in UInt256,
  amount_out UInt256,
  verified_in FixedString(32),
  verified_out FixedString(32),
  reserve0 UInt256,
  reserve1 UInt256,
  coin_in UInt8,
  coin_out UInt8,
  underlying Bool,
  sqrt_price_x96 UInt256,
  liquidity UInt256,
  tick Int32,
  fee UInt32,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- tx_from / tx_to are the sender and the target of the TRANSACTION. Who
-- seeded a pool is dex_liquidity.tx_from - the event sender is usually a
-- router and must not be used for attribution.
CREATE TABLE IF NOT EXISTS dex_liquidity (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  tx_index UInt32,
  ordinal UInt64,
  pool_id FixedString(32),
  emitter FixedString(32),
  protocol LowCardinality(String),
  kind LowCardinality(String),
  sender FixedString(32),
  owner FixedString(32),
  tx_from FixedString(32),
  tx_to FixedString(32),
  amount0 Int256,
  amount1 Int256,
  reserve0 UInt256,
  reserve1 UInt256,
  liquidity_delta Int256,
  tick_lower Int32,
  tick_upper Int32,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- User populated, never written by the indexer, not block scoped. kind:
-- 'stable' (valued at 1 USD) or 'native' (priced from native/stable pools),
-- '' retires the row. decimals / symbol are only used when the token has no
-- row in tokens (native pseudo addresses: the zero address of Uniswap V4,
-- 0xEeee...EEeE of Curve). decimals is Nullable on purpose: NULL means "take
-- it from tokens", and a token with no decimals anywhere is unpriceable
-- (NULL, never a guess).
CREATE TABLE IF NOT EXISTS quote_tokens (
  chain UInt64,
  token FixedString(32),
  kind LowCardinality(String),
  decimals Nullable(UInt8),
  symbol String DEFAULT '',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, token);

-- User populated, never written by the indexer, not block scoped. Pools of
-- the singleton families (Uniswap V4, Balancer V2) can not be asked
-- anything over RPC, and any contract can emit their events: their swaps
-- count towards USD numbers ONLY when the emitter is listed here (protocol
-- '' retires a row). price_source = 1 on any row of a chain additionally
-- restricts the native coin price of that chain to the listed emitters
-- (pool addresses for the contract families): the only defence against
-- wash trades at a fake price that does not depend on counting pools.
CREATE TABLE IF NOT EXISTS dex_trusted_emitters (
  chain UInt64,
  emitter FixedString(32),
  protocol LowCardinality(String),
  price_source UInt8 DEFAULT 0,
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, emitter);

-- Read path: swaps of a pool.
CREATE TABLE IF NOT EXISTS dex_swaps_by_pool (
  chain UInt64,
  pool_id FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  emitter FixedString(32),
  protocol LowCardinality(String),
  tx_id String,
  trader FixedString(32),
  amount0 Int256,
  amount1 Int256,
  token_in FixedString(32),
  token_out FixedString(32),
  amount_in UInt256,
  amount_out UInt256,
  verified_in FixedString(32),
  verified_out FixedString(32),
  coin_in UInt8,
  coin_out UInt8,
  underlying Bool,
  sqrt_price_x96 UInt256,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, pool_id, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_pool_mv
TO dex_swaps_by_pool AS
SELECT
  chain, pool_id, block_number, tx_index, ordinal, timestamp, emitter, protocol,
  tx_id, trader, amount0, amount1, token_in, token_out,
  amount_in, amount_out, verified_in, verified_out, coin_in, coin_out,
  underlying, sqrt_price_x96, epoch, _version, is_deleted
FROM dex_swaps;

-- Read path: swaps of a trader (tx sender when known, see dex_swaps.trader).
CREATE TABLE IF NOT EXISTS dex_swaps_by_trader (
  chain UInt64,
  trader FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  pool_id FixedString(32),
  emitter FixedString(32),
  protocol LowCardinality(String),
  tx_id String,
  tx_to FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, trader, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_swaps_by_trader_mv
TO dex_swaps_by_trader AS
SELECT
  chain, trader, block_number, tx_index, ordinal, timestamp, pool_id, emitter,
  protocol, tx_id, tx_to, epoch, _version, is_deleted
FROM dex_swaps;

-- Read path: pools of a token, one row per (token, dex_pools row).
-- block_number / tx_index / ordinal are the position of the creation event
-- (all 0 for resolver rows), so the key mirrors dex_pools and tombstones match.
-- A CLAIM index like dex_pools: join dex_pool_current_v for what is known.
-- The resolver never replaces an 'rpc' row by one with other tokens (it
-- does not ask again once a pool answered), so resolver rows here can not
-- go stale. 'unresolved' / 'no_answer' rows have no tokens and no rows.
CREATE TABLE IF NOT EXISTS dex_pools_by_token (
  chain UInt64,
  token FixedString(32),
  pool_id FixedString(32),
  emitter FixedString(32),
  protocol LowCardinality(String),
  block_number UInt64 CODEC(Delta, ZSTD),
  tx_index UInt32,
  ordinal UInt64,
  source LowCardinality(String),
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY chain
ORDER BY (chain, token, pool_id, emitter, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS dex_pools_by_token_mv
TO dex_pools_by_token AS
SELECT
  chain,
  arrayJoin(arrayDistinct(arrayConcat(tokens, underlying_tokens))) AS token,
  pool_id, emitter, protocol, created_block AS block_number, tx_index, ordinal,
  source, epoch, _version, is_deleted
FROM dex_pools
WHERE source IN ('event', 'rpc');
