CREATE DATABASE IF NOT EXISTS indexer;

CREATE TABLE indexer.blocks (
  base_block_reward UInt256,
  base_fee_per_gas Nullable(UInt64),
  burned UInt256,
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
  number UInt32,
  parent_hash String,
  receipts_root String,
  sha3_uncles String,
  size UInt32,
  state_root String,
  timestamp DateTime,
  total_difficulty Nullable(UInt256),
  total_fee_reward UInt256,
  transactions UInt16,
  transactions_root String,
  uncle_rewards UInt256,
  uncles Array(String),
  withdrawals_root Nullable(String)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (hash, miner, chain, timestamp, number)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.contracts (
  block_number UInt32,
  chain UInt64,
  contract_address String,
  creator String,
  transaction_hash String
)
ENGINE = ReplacingMergeTree()
ORDER BY (contract_address, transaction_hash, creator, chain)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.logs (
  address String,
  block_number UInt32,
  chain UInt64,
  data String CODEC(ZSTD(9)),
  log_index UInt16,
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime,
  topic0 String,
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, topic0, log_index, timestamp)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc20_transfers (
  address String,
  amount UInt256,
  block_number UInt32,
  chain UInt64,
  from String,
  log_index UInt16,
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime,
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, log_index, timestamp)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc721_transfers (
  address String,
  block_number UInt32,
  chain UInt64,
  from String,
  id UInt256,
  log_index UInt16,
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime,
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, log_index, timestamp)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.erc1155_transfers (
  address String,
  amounts Array(UInt256),
  block_number UInt32,
  chain UInt64,
  from String,
  ids Array(UInt256),
  log_index UInt16,
  log_type Nullable(String),
  operator String,
  removed Boolean,
  timestamp DateTime,
  to String,
  token_address String,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, log_index, timestamp)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.dex_trades (
  address String,
  block_number UInt32,
  chain UInt64,
  log_index UInt16,
  log_type Nullable(String),
  maker String,
  pair String,
  receiver String,
  removed Boolean,
  timestamp DateTime,
  token0_amount UInt256,
  token1_amount UInt256,
  transaction_hash String,
  transaction_log_index Nullable(UInt16)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, log_index, timestamp)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.traces (
  action_type Enum8('call' = 1, 'create' = 2, 'suicide' = 3, 'reward' = 4),
  address Nullable(String),
  author Nullable(String),
  balance Nullable(UInt256),
  block_hash String,
  block_number UInt32,
  call_type Nullable(Enum8('none' = 0, 'call' = 1, 'callcode' = 2, 'delegatecall' = 3, 'staticcall' = 4)),
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
  reward_type Nullable(Enum8('block' = 1, 'uncle' = 2, 'emptyStep' = 3, 'external' = 4)),
  subtraces UInt16,
  to Nullable(String),
  trace_address Array(UInt16),
  transaction_hash Nullable(String),
  transaction_position Nullable(UInt16),
  value Nullable(UInt256)
)
ENGINE = ReplacingMergeTree()
ORDER BY (block_hash, trace_address)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  base_fee_per_gas Nullable(UInt64),
  block_hash String,
  block_number UInt32,
  burned Nullable(UInt256),
  chain UInt64,
  contract_created Nullable(String),
  cumulative_gas_used Nullable(UInt32),
  effective_gas_price Nullable(UInt256),
  effective_transaction_fee Nullable(UInt256),
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
  status Nullable(Enum8('unknown' = 0, 'failure' = 1, 'success' = 2)),
  timestamp DateTime,
  to String,
  transaction_index UInt16,
  transaction_type Enum8('legacy' = 0, 'access_list' = 1, 'eip_1559' = 2),
  value UInt256
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (hash, from, to, timestamp, chain, method)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.withdrawals (
  address String,
  amount UInt256,
  block_number UInt32,
  chain UInt64,
  timestamp DateTime,
  validator_index UInt32,
  withdrawal_index UInt32
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (address, block_number, chain, timestamp, validator_index)
SETTINGS index_granularity = 8192;