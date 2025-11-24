CREATE DATABASE IF NOT EXISTS indexer;

CREATE TABLE IF NOT EXISTS indexer.blocks (
  base_fee_per_gas Nullable(UInt64),
  chain UInt64,
  difficulty String,
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
  total_difficulty Nullable(String),
  transactions UInt16,
  transactions_root String,
  uncles Array(String),
  withdrawals_root Nullable(String)
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, number, hash)
SETTINGS index_granularity = 8192;

CREATE TABLE IF NOT EXISTS indexer.contracts (
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  contract_address String,
  creator String,
  transaction_hash String
)
ENGINE = ReplacingMergeTree()
ORDER BY (chain, block_number, contract_address)
SETTINGS index_granularity = 8192;

CREATE TABLE IF NOT EXISTS indexer.logs (
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

CREATE TABLE IF NOT EXISTS indexer.erc20_transfers (
  address String,
  amount String,
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

CREATE TABLE IF NOT EXISTS indexer.erc721_transfers (
  address String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  from String,
  id String,
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

CREATE TABLE IF NOT EXISTS indexer.erc1155_transfers (
  address String,
  amounts Array(String),
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  from String,
  ids Array(String),
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


CREATE TABLE IF NOT EXISTS indexer.traces (
  action_type String,
  address Nullable(String),
  author Nullable(String),
  balance Nullable(String),
  block_hash String,
  block_number UInt32 CODEC(Delta, ZSTD),
  call_type Nullable(String),
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
  reward_type Nullable(String),
  subtraces UInt16,
  to Nullable(String),
  trace_address Array(UInt16),
  transaction_hash Nullable(String),
  transaction_position Nullable(UInt16),
  value Nullable(String)
)
ENGINE = ReplacingMergeTree()
ORDER BY (chain, block_number)
SETTINGS index_granularity = 8192;

CREATE TABLE IF NOT EXISTS indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  base_fee_per_gas Nullable(UInt64),
  block_hash String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  contract_created Nullable(String),
  cumulative_gas_used Nullable(UInt32),
  effective_gas_price Nullable(String),
  from String,
  gas UInt32,
  gas_price Nullable(String),
  gas_used Nullable(UInt32),
  hash String,
  input String CODEC(ZSTD(9)),
  max_fee_per_gas Nullable(String),
  max_priority_fee_per_gas Nullable(String),
  method String,
  nonce UInt32,
  status Nullable(String),
  timestamp DateTime CODEC(Delta, ZSTD),
  to String,
  transaction_index UInt16,
  transaction_type String,
  value String
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, transaction_index, hash)
SETTINGS index_granularity = 8192;

CREATE TABLE IF NOT EXISTS indexer.withdrawals (
  address String,
  amount String,
  block_number UInt32 CODEC(Delta, ZSTD),
  chain UInt64,
  timestamp DateTime CODEC(Delta, ZSTD),
  validator_index UInt64,
  withdrawal_index UInt64
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, withdrawal_index)
SETTINGS index_granularity = 8192;

CREATE TABLE IF NOT EXISTS indexer.dex_trades (
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

CREATE TABLE IF NOT EXISTS indexer.tokens (
  address String,
  name String,
  symbol String,
  decimals UInt8,
  type String,
  chain UInt64
)
ENGINE = ReplacingMergeTree()
ORDER BY (chain, address)
SETTINGS index_granularity = 8192;