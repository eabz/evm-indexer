CREATE DATABASE IF NOT EXISTS indexer;
CREATE TABLE indexer.blocks (
  base_fee_per_gas Nullable(UInt256),
  chain UInt64,
  difficulty UInt256,
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
  transactions UInt64,
  transactions_root String,
  uncles Array(String),
  withdrawals_root Nullable(String),
  excess_data_gas Nullable(UInt256),
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.contracts (
  block UInt64,
  contract_address String,
  chain UInt64,
  creator String,
  transaction_hash String,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.dex_trades (
  chain UInt64,
  maker String,
  transaction_hash String,
  log_index UInt256,
  receiver String,
  pair_address String,
  token0_amount UInt256,
  token1_amount UInt256,
  transaction_log_index Nullable(UInt256),
  timestamp UInt64
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc1155_transfers (
  chain UInt64,
  operator String,
  from String,
  transaction_hash String,
  log_index UInt256,
  to String,
  token String ,
  transaction_log_index Nullable(UInt256),
  id UInt256,
  value UInt256,
  timestamp UInt64,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc20_transfers (
  amount UInt256,
  chain UInt64,
  from String,
  transaction_hash String,
  log_index UInt256,
  to String,
  token String,
  transaction_log_index Nullable(UInt256),
  timestamp UInt64,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.erc721_transfers (
  chain UInt64,
  from String,
  transaction_hash String,
  log_index UInt256,
  to String,
  token String,
  transaction_log_index Nullable(UInt256),
  id UInt256,
  timestamp UInt64,
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (transaction_hash, log_index);

CREATE TABLE indexer.logs (
  address String,
  chain UInt64,
  data String,
  transaction_hash String,
  log_index UInt256,
  log_type Nullable(String),
  removed boolean,
  topic0 Nullable(String),
  topic1 Nullable(String),
  topic2 Nullable(String),
  topic3 Nullable(String),
  transaction_log_index Nullable(UInt256),
  timestamp UInt64,
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
  value UInt256
)
ENGINE = ReplacingMergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.traces (
  from Nullable(String),
  to Nullable(String),
  value Nullable(UInt256),
  gas Nullable(UInt256),
  input Nullable(String),
  call_type Nullable(String),
  init Nullable(String),
  address Nullable(String),
  refund_address Nullable(String),
  balance Nullable(UInt256),
  author Nullable(String),
  reward_type Nullable(String),
  gas_used Nullable(UInt256),
  output Nullable(String),
  code Nullable(String),
  trace_address Array(UInt64),
  subtraces UInt64,
  transaction_position Nullable(UInt64),
  transaction_hash Nullable(String),
  block_number UInt64,
  block_hash String,
  action_type String,
  chain UInt64,
  error Nullable(String),
)
ENGINE = MergeTree()
PRIMARY KEY (block_hash);