CREATE DATABASE IF NOT EXISTS indexer;

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
  timestamp UInt64,
  total_difficulty Nullable(UInt256),
  total_fee_reward UInt256,
  transactions UInt64,
  transactions_root String,
  uncles Array(String),
  withdrawals_root Nullable(String),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.contracts (
  block UInt64,
  chain UInt64,
  contract_address String,
  creator String,
  transaction_hash String,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.dex_trades (
  chain UInt64,
  log_index UInt256,
  maker String,
  pair_address String,
  receiver String,
  timestamp UInt64,
  token0_amount UInt256,
  token1_amount UInt256,
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc1155_transfers (
  chain UInt64,
  from String,
  id UInt256,
  log_index UInt256,
  operator String,
  timestamp UInt64,
  to String,
  token String,
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
  value UInt256,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc20_transfers (
  amount UInt256,
  chain UInt64,
  from String,
  log_index UInt256,
  timestamp UInt64,
  to String,
  token String,
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc721_transfers (
  chain UInt64,
  from String,
  id UInt256,
  log_index UInt256,
  timestamp UInt64,
  to String,
  token String,
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.logs (
  address String,
  chain UInt64,
  data String,
  log_index UInt256,
  log_type Nullable(String),
  removed boolean,
  timestamp UInt64,
  topic0 Nullable(String),
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_hash String,
  transaction_log_index Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.receipts (
  chain UInt64,
  contract_address Nullable(String),
  cumulative_gas_used UInt256,
  effective_gas_price Nullable(UInt256),
  gas_used Nullable(UInt256),
  hash String,
  status UInt64,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (hash);

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
ENGINE = MergeTree()
PRIMARY KEY (block_hash);

CREATE TABLE indexer.transactions (
  access_list Array(Tuple(String, Array(String))),
  block_hash String,
  block_number UInt64,
  chain UInt64,
  from String,
  gas UInt256,
  gas_price Nullable(UInt256),
  hash String,
  input String,
  max_fee_per_gas Nullable(UInt256),
  max_priority_fee_per_gas Nullable(UInt256),
  method String,
  nonce UInt256,
  timestamp UInt64,
  to String,
  transaction_index UInt16,
  transaction_type UInt16,
  value UInt256,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.withdrawals (
  address String,
  amount UInt256,
  block_number UInt64,
  chain UInt64,
  index UInt64,
  timestamp UInt64,
  validator_index UInt64,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (block_number, index, chain);