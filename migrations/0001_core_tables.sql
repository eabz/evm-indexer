-- Core tables (docs/design.md, section 1).
--
-- Conventions shared by every table in this file:
--   * hashes / topics   FixedString(32)   raw bytes
--   * addresses         FixedString(20)   raw bytes
--   * amounts / prices  UInt256
--   * byte blobs        String            raw bytes, never hex
--   * enumerations      LowCardinality(String)
--   * Nullable only where NULL means something else than the default
--   * ReplacingMergeTree(_version), _version = unix ms of the flush
--   * positional sorting keys, so a re-inserted block replaces itself
--   * readers query with FINAL and format with concat('0x', lower(hex(x)))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS traces (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  -- 4294967295 for block / uncle reward traces (no transaction).
  transaction_position UInt32 CODEC(Delta, ZSTD),
  trace_address Array(UInt32),
  -- Zero bytes for reward traces.
  transaction_hash FixedString(32),
  block_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  -- 'call' | 'create' | 'suicide' | 'reward'
  action_type LowCardinality(String),
  subtraces UInt32,
  -- Empty when the trace did not fail.
  error String,
  -- The columns below are NULL when they do not apply to the action type.
  `from` Nullable(FixedString(20)),
  `to` Nullable(FixedString(20)),
  value Nullable(UInt256),
  gas Nullable(UInt64),
  gas_used Nullable(UInt64),
  call_type LowCardinality(Nullable(String)),
  input Nullable(String) CODEC(ZSTD(3)),
  output Nullable(String) CODEC(ZSTD(3)),
  init Nullable(String) CODEC(ZSTD(3)),
  code Nullable(String) CODEC(ZSTD(3)),
  address Nullable(FixedString(20)),
  refund_address Nullable(FixedString(20)),
  balance Nullable(UInt256),
  author Nullable(FixedString(20)),
  reward_type LowCardinality(Nullable(String)),
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, transaction_position, trace_address)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS contracts (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  contract_address FixedString(20),
  creator FixedString(20),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, contract_address)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE TABLE IF NOT EXISTS withdrawals (
  chain UInt64,
  block_number UInt64 CODEC(Delta, ZSTD),
  withdrawal_index UInt64 CODEC(Delta, ZSTD),
  validator_index UInt64,
  address FixedString(20),
  amount UInt256,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
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
  _version UInt64 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY (chain, toYYYYMM(timestamp))
ORDER BY (chain, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

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
