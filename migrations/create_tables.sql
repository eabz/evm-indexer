CREATE DATABASE IF NOT EXISTS indexer;

SET optimize_on_insert = 1;

CREATE TABLE indexer.blocks (
  base_block_reward UInt256,
  base_fee_per_gas Nullable(UInt256),
  burned UInt256,
  chain UInt64,
  difficulty UInt256,
  excess_data_gas Nullable(UInt256),
  extra_data String,
  gas_limit UInt256,
  gas_used UInt256,
  hash String,
  logs_bloom String,
  miner String,
  mix_hash String,
  nonce String,
  number UInt64,
  parent_hash String,
  receipts_root String,
  sha3_uncles String,
  size Nullable(UInt256),
  state_root String,
  timestamp DateTime,
  total_difficulty Nullable(UInt256),
  total_fee_reward UInt256,
  transactions UInt64,
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
  block UInt64,
  chain UInt64,
  contract_address String,
  creator String,
  transaction_hash String,
)
ENGINE = ReplacingMergeTree()
ORDER BY (contract_address, transaction_hash, creator, chain)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.logs (
  address String,
  chain UInt64,
  data String,
  dex_trade_maker Nullable(String),
  dex_trade_pair Nullable(String),
  dex_trade_receiver Nullable(String),
  dex_trade_token0_amount Nullable(UInt256),
  dex_trade_token1_amount Nullable(UInt256),
  log_index UInt256,
  log_type Nullable(String),
  removed boolean,
  timestamp DateTime,
  token_transfer_amount Nullable(UInt256),
  token_transfer_from Nullable(String),
  token_transfer_id Nullable(UInt256),
  token_transfer_operator Nullable(String),
  token_transfer_to Nullable(String),
  token_transfer_token_address Nullable(String),
  token_transfer_type Nullable(Enum('erc20' = 1, 'erc721' = 1, 'erc1155' = 1))
  topic0 Nullable(String),
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
)

ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (transaction_hash, address, chain, topic0, log_index, timestamp)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE TABLE indexer.traces (
  action_type String,
  address Nullable(String),
  author Nullable(String),
  balance Nullable(UInt256),
  block_hash String,
  block_number UInt64,
  call_type Nullable(String),
  chain UInt64,
  code Nullable(String),
  error Nullable(String),
  from Nullable(String),
  gas Nullable(UInt256),
  gas_used Nullable(UInt256),
  init Nullable(String),
  input Nullable(String),
  output Nullable(String),
  refund_address Nullable(String),
  reward_type Nullable(String),
  subtraces UInt64,
  to Nullable(String),
  trace_address Array(UInt64),
  transaction_hash Nullable(String),
  transaction_position Nullable(UInt64),
  value Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
ORDER BY (block_hash, transaction_hash, call_type, trace_address, author)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE TABLE indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  base_fee_per_gas Nullable(UInt256),
  block_hash String,
  block_number UInt64,
  burned UInt256,
  chain UInt64,
  contract_created Nullable(String),
  cumulative_gas_used UInt256,
  effective_gas_price Nullable(UInt256),
  from String,
  gas UInt256,
  gas_price Nullable(UInt256),
  gas_used Nullable(UInt256),
  hash String,
  input String,
  max_fee_per_gas Nullable(UInt256),
  max_priority_fee_per_gas Nullable(UInt256),
  method String,
  nonce UInt256,
  status UInt64,
  timestamp DateTime,
  to String,
  transaction_index UInt16,
  transaction_type UInt16,
  value UInt256,
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (hash, from, to, timestamp, chain, method)
SETTINGS index_granularity = 8192;

CREATE TABLE indexer.withdrawals (
  address String,
  amount UInt256,
  block_number UInt64,
  chain UInt64,
  timestamp DateTime,
  validator_index UInt64,
  withdrawal_index UInt64,
)
ENGINE = ReplacingMergeTree()
PARTITION BY toYYYYMM(timestamp)
ORDER BY (address, block_number, chain, timestamp, validator_index)
SETTINGS index_granularity = 8192;