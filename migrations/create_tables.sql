CREATE DATABASE IF NOT EXISTS indexer;

CREATE TABLE indexer.blocks (
  base_fee_per_gas Nullable(Float64),
  chain Int64,
  difficulty String,
  extra_data String,
  gas_limit Int64,
  gas_used Int64,
  hash String,
  logs_bloom String,
  miner String,
  mix_hash String,
  nonce String,
  number Int64,
  parent_hash String,
  receipts_root String,
  sha3_uncles String,
  size Int32,
  state_root String,
  status Enum('unfinalized', 'secure', 'finalized'),
  timestamp Int64,
  total_difficulty String,
  transactions Int32,
  transactions_root String,
  uncles Array(String)
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.transactions (
  block_hash String,
  block_number Int64,
  chain Int64,
  from_address String,
  gas Int64,
  gas_price Nullable(Int64),
  hash String,
  input String,
  max_fee_per_gas Nullable(Int64),
  max_priority_fee_per_gas Nullable(Int64),
  method String,
  nonce Int32,
  timestamp Int64,
  to_address Nullable(String),
  transaction_index Int16,
  transaction_type Int16,
  value Float64
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.methods (
  name String,
  method String
)
ENGINE = MergeTree()
PRIMARY KEY (method);

CREATE TABLE indexer.receipts (
  contract_address Nullable(String),
  cumulative_gas_used Int64,
  effective_gas_price Nullable(Int64),
  gas_used Int64,
  hash String,
  status Enum('reverted', 'succeed', 'pending') NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.contracts (
  block Int64,
  contract_address String,
  chain Int64,
  creator String,
  hash String,
)
ENGINE = MergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.contract_metadata (
  abi String,
  chain Int64,
  contract_address String,
  name String,
)
ENGINE = MergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.logs (
  address String,
  chain Int64,
  data String,
  hash String,
  log_index Int32,
  log_type Nullable(String),
  removed boolean,
  topics Array(String),
  transaction_log_index Nullable(Int32),
  timestamp Int64,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc20_transfers (
  amount Float64,
  chain Int64,
  from_address String,
  hash String,
  log_index Int32,
  to_address String,
  token String,
  transaction_log_index Nullable(Int32),
  timestamp Int64,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc721_transfers (
  chain Int64,
  from_address String,
  hash String,
  log_index Int32,
  to_address String,
  token String,
  transaction_log_index Nullable(Int32),
  id String,
  timestamp Int64,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc1155_transfers (
  chain Int64,
  operator String,
  from_address String,
  hash String,
  log_index Int32,
  to_address String,
  token String ,
  transaction_log_index Nullable(Int32),
  ids Array(String),
  values Array(Float64),
  timestamp Int64,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.dex_trades (
  chain Int64,
  maker String,
  hash String,
  log_index Int32,
  receiver String,
  token0 String,
  token1 String,
  pair_address String,
  factory String,
  token0_amount Float64,
  token1_amount Float64,
  swap_rate Float64,
  transaction_log_index Nullable(Int32),
  timestamp Int64,
  trade_type Enum('buy', 'sell'),
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.token_details 
(
  chain Int64,
  token String,
  name String,
  symbol String,
  decimals Int64,
  token0 Nullable(String),
  token1 Nullable(String),
  factory Nullable(String),
)
ENGINE = MergeTree()
PRIMARY KEY (token, chain);

CREATE TABLE indexer.chains_indexed_state
(
    chain Int64,
    indexed_blocks_amount Int64
)
ENGINE = ReplacingMergeTree
ORDER BY chain
PRIMARY KEY chain;