CREATE DATABASE IF NOT EXISTS indexer;

CREATE TABLE indexer.blocks (
  base_fee_per_gas Float64,
  chain Int64 NOT NULL,
  difficulty String NOT NULL,
  extra_data String NOT NULL,
  gas_limit Int64 NOT NULL,
  gas_used Int64 NOT NULL,
  hash String,
  logs_bloom String NOT NULL,
  miner String NOT NULL,
  mix_hash String NOT NULL,
  nonce String NOT NULL,
  number Int64 NOT NULL,
  parent_hash String NOT NULL,
  receipts_root String NOT NULL,
  sha3_uncles String NOT NULL,
  size Int32 NOT NULL,
  state_root String NOT NULL,
  status Enum('unfinalized', 'secure', 'finalized') NOT NULL,
  timestamp Date NOT NULL,
  total_difficulty String NOT NULL,
  transactions Int32 NOT NULL,
  transactions_root String NOT NULL,
  uncles Array(String) NOT NULL
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.transactions (
  block_hash String NOT NULL,
  block_number Int64 NOT NULL,
  chain Int64 NOT NULL,
  from_address String NOT NULL,
  gas Int64 NOT NULL,
  gas_price Int64 NOT NULL,
  hash String,
  input String NOT NULL,
  max_fee_per_gas Int64,
  max_priority_fee_per_gas Int64,
  method String NOT NULL,
  nonce Int32 NOT NULL,
  timestamp Date NOT NULL,
  to_address String,
  transaction_index Int32 NOT NULL,
  transaction_type Int32 NOT NULL,
  value Float64 NOT NULL
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.methods (
  name String NOT NULL,
  method String
)
ENGINE = MergeTree()
PRIMARY KEY (method);

CREATE TABLE indexer.receipts (
  contract_address String,
  cumulative_gas_used Int64 NOT NULL,
  effective_gas_price Int64,
  gas_used Int64 NOT NULL,
  hash String,
  status Enum('reverted', 'succeed', 'pending') NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash);

CREATE TABLE indexer.contracts (
  block Int64 NOT NULL,
  contract_address String NOT NULL,
  chain Int64 NOT NULL,
  creator String NOT NULL,
  hash String NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.contract_metadata (
  abi String NOT NULL,
  chain Int64 NOT NULL,
  contract_address String NOT NULL,
  name String NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (contract_address, chain);

CREATE TABLE indexer.logs (
  address String NOT NULL,
  chain Int64 NOT NULL,
  data String NOT NULL,
  hash String NOT NULL,
  log_index Int32 NOT NULL,
  log_type Int8,
  removed boolean NOT NULL,
  topics Array(String) NOT NULL,
  timestamp Date NOT NULL,
  transaction_log_index Int32,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc20_transfers (
  amount Float64 NOT NULL,
  chain Int64 NOT NULL,
  from_address String NOT NULL,
  hash String NOT NULL,
  log_index Int32 NOT NULL,
  to_address String NOT NULL,
  token String NOT NULL,
  transaction_log_index Int32,
  timestamp Date NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc721_transfers (
  chain Int64 NOT NULL,
  from_address String NOT NULL,
  hash String NOT NULL,
  log_index Int32 NOT NULL,
  to_address String NOT NULL,
  token String NOT NULL,
  transaction_log_index INT,
  id String NOT NULL,
  timestamp Date NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.erc1155_transfers (
  chain Int64 NOT NULL,
  operator String NOT NULL,
  from_address String NOT NULL,
  hash String NOT NULL,
  log_index Int32 NOT NULL,
  to_address String NOT NULL,
  token String NOT NULL,
  transaction_log_index INT,
  ids Array(String) NOT NULL,
  values Array(Float64) NOT NULL,
  timestamp Date NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.dex_trades (
  chain Int64 NOT NULL,
  maker String NOT NULL,
  hash String NOT NULL,
  log_index Int32 NOT NULL,
  receiver String NOT NULL,
  token0 String NOT NULL,
  token1 String NOT NULL,
  pair_address String NOT NULL,
  token0_amount Float64 NOT NULL,
  token1_amount Float64 NOT NULL,
  swap_rate Float64 NOT NULL,
  transaction_log_index Int32,
  timestamp Date NOT NULL,
  trade_type Enum('buy', 'sell') NOT NULL,
)
ENGINE = MergeTree()
PRIMARY KEY (hash, log_index);

CREATE TABLE indexer.token_details 
(
  chain Int64 NOT NULL,
  token String NOT NULL,
  name String NOT NULL,
  symbol String NOT NULL,
  decimals Int64,
  token0 String,
  token1 String,
  factory String,
)
ENGINE = MergeTree()
PRIMARY KEY (token, chain);

CREATE TABLE indexer.chains_indexed_state
(
    chain Int64 NOT NULL,
    indexed_blocks_amount Int64 NOT NULL
)
ENGINE = ReplacingMergeTree
ORDER BY chain
PRIMARY KEY chain;