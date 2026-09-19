-- Core tables (docs/design.md, section 1).
--
-- Conventions shared by every table in this file:
--   * hashes / topics   FixedString(32)   raw bytes
--   * addresses         FixedString(20)   raw bytes
--   * amounts / prices  UInt256
--   * byte blobs        String            raw bytes, never hex
--   * enumerations      LowCardinality(String)
--   * Nullable only where NULL means something else than the default
--   * positional sorting keys, so a re-inserted block replaces itself
--   * readers query with FINAL and format with concat('0x', lower(hex(x)))
--
-- Reorgs are insert only (docs/design.md, section 2): the indexer never
-- issues DELETE, ALTER ... DELETE or DROP PARTITION. Every block scoped
-- table is a ReplacingMergeTree with a version and a deleted flag:
--   * _version    unix ms of the flush, strictly increasing per process
--   * is_deleted  0 for data. A rollback inserts a copy of the row with a
--                 newer _version and is_deleted = 1 (a tombstone), FINAL then
--                 hides the row. Never sent by the insert path.
--   * epoch       the chain's purge generation when the row was written,
--                 which is what keeps the aggregates of 0003 correct
--
-- The target is 50+ chains in one database, so the tables are partitioned
-- by month ONLY (never by chain: chains x months partitions). chain is the
-- first sorting key column, that is what prunes reads. A tombstone copies
-- the timestamp of its row, so it always lands in the partition of the row.
--
-- The database comes from the connection, never from the DDL.

CREATE TABLE IF NOT EXISTS blocks (
  chain UInt64,
  number UInt64 CODEC(Delta, ZSTD),
  hash FixedString(32),
  parent_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  miner FixedString(20),
  -- NULL before London. Zero is a real base fee on some chains.
  base_fee_per_gas Nullable(UInt256),
  difficulty UInt256,
  -- Zero when the node does not report it (post merge).
  total_difficulty UInt256,
  extra_data String CODEC(ZSTD(3)),
  gas_limit UInt64,
  gas_used UInt64,
  -- Zero bytes when absent.
  mix_hash FixedString(32),
  nonce FixedString(8),
  receipts_root FixedString(32),
  sha3_uncles FixedString(32),
  size UInt64,
  state_root FixedString(32),
  transactions UInt32,
  transactions_root FixedString(32),
  uncles Array(FixedString(32)),
  -- Zero bytes before Shanghai.
  withdrawals_root FixedString(32),
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, number)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS transactions (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  transaction_index UInt32 CODEC(Delta, ZSTD),
  hash FixedString(32),
  block_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  `from` FixedString(20),
  -- NULL for contract creations (the zero address is a real recipient).
  `to` Nullable(FixedString(20)),
  -- Zero bytes when the transaction did not create a contract.
  contract_created FixedString(20),
  value UInt256,
  input String CODEC(ZSTD(3)),
  -- First four bytes of the input, zeros when the input is shorter.
  method FixedString(4),
  nonce UInt64,
  transaction_type LowCardinality(String),
  -- 'success' | 'failure', NULL before Byzantium.
  status LowCardinality(Nullable(String)),
  gas UInt64,
  gas_used UInt64,
  cumulative_gas_used UInt64,
  -- NULL when the node does not report one for the transaction type.
  gas_price Nullable(UInt256),
  effective_gas_price UInt256,
  -- NULL on transactions without EIP-1559 fee fields.
  max_fee_per_gas Nullable(UInt256),
  max_priority_fee_per_gas Nullable(UInt256),
  -- Copied from the block for join free fee math, NULL before London.
  base_fee_per_gas Nullable(UInt256),
  access_list Array(Tuple(FixedString(20), Array(FixedString(32)))),
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, transaction_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS logs (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32 CODEC(Delta, ZSTD),
  transaction_index UInt32 CODEC(Delta, ZSTD),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  address FixedString(20),
  -- Number of topics (0-4): tells an absent topic from an all zero one.
  topic_count UInt8,
  topic0 FixedString(32),
  topic1 FixedString(32),
  topic2 FixedString(32),
  topic3 FixedString(32),
  data String CODEC(ZSTD(3)),
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS withdrawals (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  withdrawal_index UInt64 CODEC(Delta, ZSTD),
  validator_index UInt64,
  address FixedString(20),
  amount UInt256,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, withdrawal_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS erc20_transfers (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32 CODEC(Delta, ZSTD),
  transaction_index UInt32 CODEC(Delta, ZSTD),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  token_address FixedString(20),
  `from` FixedString(20),
  `to` FixedString(20),
  amount UInt256,
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS erc721_transfers (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32 CODEC(Delta, ZSTD),
  transaction_index UInt32 CODEC(Delta, ZSTD),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  token_address FixedString(20),
  `from` FixedString(20),
  `to` FixedString(20),
  id UInt256,
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS erc1155_transfers (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32 CODEC(Delta, ZSTD),
  transaction_index UInt32 CODEC(Delta, ZSTD),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  token_address FixedString(20),
  operator FixedString(20),
  `from` FixedString(20),
  `to` FixedString(20),
  -- TransferSingle is stored as one element arrays. Same length, always.
  ids Array(UInt256),
  amounts Array(UInt256),
  epoch UInt32 DEFAULT 0,
  _version UInt64 CODEC(Delta, ZSTD),
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

-- Contracts deployed DIRECTLY by a transaction (docs/design.md, section
-- 9). A view: nothing to insert, roll back or keep consistent. Contracts
-- created by other contracts (factories) are out of scope by design, so do
-- not build statistics on top of this. Query it with a chain and a block or
-- time range: `transactions FINAL` is what is being read.
--
-- `status` is NULL before Byzantium (receipts had no status field) and
-- `NULL = 'success'` is NULL: a creation that carries a contract address
-- and no status succeeded, so NULL counts as success.
CREATE VIEW IF NOT EXISTS contracts AS
SELECT
  chain,
  block_number,
  timestamp,
  contract_created AS contract_address,
  `from` AS creator,
  hash AS transaction_hash
FROM transactions FINAL
WHERE contract_created != toFixedString('', 20)
  AND ifNull(status, 'success') = 'success';

-- Token metadata is not block scoped (a name does not change with the
-- fork) and is written by the token worker, outside of the block flushes:
-- its _version is therefore assigned by the server at insert time.
CREATE TABLE IF NOT EXISTS tokens (
  chain UInt64,
  address FixedString(20),
  name String,
  symbol String,
  decimals UInt8,
  -- 'ERC20' | 'ERC721' | 'ERC1155'
  type LowCardinality(String),
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
ORDER BY (chain, address)
SETTINGS index_granularity = 8192;
