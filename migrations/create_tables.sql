CREATE DATABASE IF NOT EXISTS indexer;

SET optimize_on_insert = 1;

CREATE TABLE indexer.blocks (
  base_block_reward UInt256,
  base_fee_per_gas Nullable(UInt64) CODEC(Delta(8), ZSTD(1)),
  burned UInt256,
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  difficulty UInt256,
  extra_data String CODEC(ZSTD(9)),
  gas_limit UInt32 CODEC(Delta(4), ZSTD(1)),
  gas_used UInt32 CODEC(Delta(4), ZSTD(1)),
  hash LowCardinality(String),
  is_uncle: Boolean,
  logs_bloom String CODEC(ZSTD(9)),
  miner LowCardinality(String),
  mix_hash String,
  nonce String,
  number UInt32 CODEC(Delta(4), ZSTD(1)),
  parent_hash String,
  receipts_root String,
  sha3_uncles String,
  size UInt32 CODEC(Delta(4), ZSTD(1)),
  state_root String,
  timestamp DateTime CODEC(Delta(4), ZSTD(1)),
  total_difficulty UInt256,
  total_fee_reward UInt256,
  transactions UInt16 CODEC(Delta(2), ZSTD(1)),
  transactions_root String,
  uncles Array(String),
  uncle_rewards UInt256,
  withdrawals_root Nullable(String),
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (hash, miner, chain, timestamp, number)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.contracts (
  block_number UInt32 CODEC(Delta(4), ZSTD(1)),
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  contract_address LowCardinality(String),
  creator LowCardinality(String),
  transaction_hash LowCardinality(String),
)
ENGINE = ReplacingMergeTree()
ORDER BY (contract_address, transaction_hash, creator, chain)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.logs (
  address LowCardinality(String),
  block_number UInt32 CODEC(Delta(4), ZSTD(1)),
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  data String CODEC(ZSTD(9)),
  dex_trade_maker Nullable(String),
  dex_trade_pair Nullable(String),
  dex_trade_receiver Nullable(String),
  dex_trade_token0_amount Nullable(UInt256),
  dex_trade_token1_amount Nullable(UInt256),
  log_index UInt16 CODEC(Delta(2), ZSTD(1)),
  log_type Nullable(String),
  removed Boolean,
  timestamp DateTime CODEC(Delta(4), ZSTD(1)),
  token_transfer_amount Nullable(UInt256),
  token_transfer_from Nullable(String),
  token_transfer_id Nullable(UInt256),
  token_transfer_operator Nullable(String),
  token_transfer_to Nullable(String),
  token_transfer_token_address Nullable(String),
  token_transfer_type Nullable(Enum8('erc20' = 1, 'erc721' = 2, 'erc1155' = 3)),
  topic0 LowCardinality(String),
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_hash LowCardinality(String),
  transaction_log_index Nullable(UInt16) CODEC(Delta(2), ZSTD(1)),
)

ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, topic0, log_index, timestamp)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE TABLE indexer.traces (
  action_type Enum('call' = 1, 'create' = 2, 'suicide' = 3, 'reward' = 4),
  address Nullable(String),
  author Nullable(String),
  balance Nullable(UInt256),
  block_hash LowCardinality(String),
  block_number UInt32 CODEC(Delta(4), ZSTD(1)),
  call_type Nullable(String),
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  code Nullable(String),
  error Nullable(String),
  from Nullable(String),
  gas Nullable(UInt256),
  gas_used Nullable(UInt256),
  init Nullable(String) CODEC(ZSTD(9)),
  input Nullable(String) CODEC(ZSTD(9)),
  output Nullable(String) CODEC(ZSTD(9)),
  refund_address Nullable(String),
  reward_type Nullable(String),
  subtraces UInt16 CODEC(Delta(2), ZSTD(1)),
  to Nullable(String),
  trace_address Array(UInt16),
  transaction_hash Nullable(String),
  transaction_position Nullable(UInt16),
  value Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
ORDER BY (block_hash, transaction_hash, call_type, trace_address, author)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE TABLE indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  base_fee_per_gas UInt256,
  block_hash LowCardinality(String),
  block_number UInt32 CODEC(Delta(4), ZSTD(1)),
  burned UInt256,
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  contract_created Nullable(String),
  cumulative_gas_used Nullable(UInt32) CODEC(Delta(4), ZSTD(1),
  effective_gas_price Nullable(UInt256),
  effective_transaction_fee UInt256,
  from LowCardinality(String),
  gas UInt32 CODEC(Delta(4), ZSTD(1)),
  gas_price Nullable(UInt256),
  gas_used Nullable(UInt32) CODEC(Delta(4), ZSTD(1),
  hash LowCardinality(String),
  input String CODEC(ZSTD(9)),
  max_fee_per_gas Nullable(UInt256),
  max_priority_fee_per_gas Nullable(UInt256),
  method LowCardinality(String),
  nonce UInt32 CODEC(Delta(4), ZSTD(1)),
  status Enum('unknown' = -1, 'failure' = 0, 'success' = 1),
  timestamp DateTime CODEC(Delta(4), ZSTD(1)),
  to LowCardinality(String),
  transaction_index UInt16 CODEC(Delta(2), ZSTD(1)),
  transaction_type UInt16 CODEC(Delta(2), ZSTD(1)),
  value UInt256,
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (hash, from, to, timestamp, chain, method)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.withdrawals (
  address LowCardinality(String),
  amount UInt256,
  block_number UInt32 CODEC(Delta(4), ZSTD(1)),
  chain UInt64 CODEC(Delta(8), ZSTD(1)),
  timestamp DateTime CODEC(Delta(4), ZSTD(1)),
  validator_index UInt32 CODEC(Delta(4), ZSTD(1)),
  withdrawal_index UInt32 CODEC(Delta(4), ZSTD(1)),
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (address, block_number, chain, timestamp, validator_index)
SETTINGS index_granularity = 8192;