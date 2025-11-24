CREATE DATABASE IF NOT EXISTS indexer;

CREATE TABLE indexer.blocks (
  base_fee_per_gas Nullable(UInt64),
  chain UInt64,
  difficulty UInt256,
  extra_data String CODEC(ZSTD(9)),
  gas_limit UInt32,
  gas_used UInt32,
  hash String,
  is_uncle Boolean,
  logs_bloom String CODEC(ZSTD(9)),
  miner String,
  mix_hash Nullable(String),
  nonce String,
  number UInt32 CODEC(Delta, ZSTD),
  parent_hash String,
  receipts_root String,
  sha3_uncles String,
  size UInt32,
  state_root String,
  timestamp DateTime CODEC(Delta, ZSTD),
  total_difficulty Nullable(UInt256),
  transactions UInt16,
  transactions_root String,
  uncles Array(String),
  withdrawals_root Nullable(String)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, number, hash)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.contracts (
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  contract_address String,
  creator String,
  transaction_index UInt16 CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree()
ORDER BY (chain, block_number, transaction_hash, log_index)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.logs (
  address String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  data String CODEC(ZSTD(9)),
  log_index UInt16 CODEC(Delta, ZSTD),
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime CODEC(Delta, ZSTD),
  topic0 Nullable(String),
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index, transaction_hash)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc20_transfers (
  address String,
  amount UInt256,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  from String,
  log_index UInt16 CODEC(Delta, ZSTD),
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime CODEC(Delta, ZSTD),
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index, transaction_hash)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc721_transfers (
  address String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  from String,
  id UInt256,
  log_index UInt16 CODEC(Delta, ZSTD),
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime CODEC(Delta, ZSTD),
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index, transaction_hash)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc1155_transfers (
  address String,
  amounts Array(UInt256),
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  from String,
  ids Array(UInt256),
  log_index UInt16 CODEC(Delta, ZSTD),
  log_type Nullable(String),
  operator String,
  removed Boolean,
  timestamp DateTime CODEC(Delta, ZSTD),
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, log_index, transaction_hash)
SETTINGS index_granularity = 8192;


CREATE TABLE indexer.traces (
  action_type LowCardinality(Enum8('call' = 1, 'create' = 2, 'suicide' = 3, 'reward' = 4)),
  address Nullable(String),
  author Nullable(String),
  balance Nullable(UInt256),
  block_hash String,
  block_number UInt32 CODEC(Delta, ZSTD),
  call_type LowCardinality(Nullable(Enum8('none' = 0, 'call' = 1, 'callcode' = 2, 'delegatecall' = 3, 'staticcall' = 4))),
  chain UInt64,
  code Nullable(String),
  error Nullable(String),
  from Nullable(String),
  gas Nullable(UInt32),
  gas_used Nullable(UInt32),
  init Nullable(String) CODEC(ZSTD(9)),
  input Nullable(String) CODEC(ZSTD(9)),
  output Nullable(String) CODEC(ZSTD(9)),
  refund_address Nullable(String),
  reward_type LowCardinality(Nullable(Enum8('block' = 1, 'uncle' = 2, 'emptyStep' = 3, 'external' = 4))),
  subtraces UInt16,
  to Nullable(String),
  trace_address Array(UInt16),
  transaction_hash Nullable(String),
  transaction_position Nullable(UInt16),
  value Nullable(UInt256)
)
ENGINE = ReplacingMergeTree()
ORDER BY (chain, block_number, transaction_position, trace_address)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  base_fee_per_gas Nullable(UInt64),
  block_hash String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  contract_created Nullable(String),
  cumulative_gas_used Nullable(UInt32),
  effective_gas_price Nullable(UInt256),
  from String,
  gas UInt32,
  gas_price Nullable(UInt256),
  gas_used Nullable(UInt32),
  hash String,
  input String CODEC(ZSTD(9)),
  max_fee_per_gas Nullable(UInt256),
  max_priority_fee_per_gas Nullable(UInt256),
  method String,
  nonce UInt32,
  status LowCardinality(Nullable(Enum8('unknown' = 0, 'failure' = 1, 'success' = 2))),
  timestamp DateTime CODEC(Delta, ZSTD),
  to String,
  transaction_index UInt16,
  transaction_type LowCardinality(Enum8('legacy' = 0, 'access_list' = 1, 'eip_1559' = 2)),
  value UInt256
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, transaction_index, hash)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.withdrawals (
  address String,
  amount UInt256,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  timestamp DateTime CODEC(Delta, ZSTD),
  validator_index UInt32,
  withdrawal_index UInt32
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, withdrawal_index)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.dex_trades (
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  transaction_hash String,
  log_index UInt16 CODEC(Delta, ZSTD),
  pool_address String,
  sender String,
  recipient String,
  amount0_in String,
  amount1_in String,
  amount0_out String,
  amount1_out String,
  dex_name String,
  timestamp DateTime CODEC(Delta, ZSTD)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, pool_address, block_number, log_index)
SETTINGS index_granularity = 8192;