-- Read path (docs/design.md, section 1, "Read-path tables").
--
-- One MV-fed side table per access pattern, no projections and no bloom
-- filters. Every side table:
--   * is ReplacingMergeTree(_version) and copies _version from the base
--     row, so a re-inserted block replaces its own side rows too
--   * carries (chain, block_number), so a rollback purges it with the same
--     DELETE ... WHERE chain = ? AND block_number >= ? as the base tables
--   * is partitioned by chain only: these tables exist to answer lookups
--     that do NOT know the time range, a monthly partition would turn every
--     lookup into one index probe per month
--   * has a minmax index on block_number, which is not a prefix of its
--     sorting key: it lets the rollback DELETE skip every part that only
--     holds older blocks instead of scanning the column
--
-- direction is -1 for the sending side and 1 for the receiving side, so
-- a balance is sum(toInt256(amount) * direction).

-- Transaction by hash.
CREATE TABLE IF NOT EXISTS tx_lookup (
  chain UInt64,
  hash FixedString(32),
  block_number UInt64,
  transaction_index UInt32,
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, hash)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS tx_lookup_mv TO tx_lookup AS
SELECT chain, hash, block_number, transaction_index, _version
FROM transactions;

-- Block by hash.
CREATE TABLE IF NOT EXISTS block_lookup (
  chain UInt64,
  hash FixedString(32),
  block_number UInt64,
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, hash)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS block_lookup_mv TO block_lookup AS
SELECT chain, hash, number AS block_number, _version
FROM blocks;

-- Transactions of an address: one row for the sender and one for the
-- recipient (the created contract when the transaction is a deployment).
CREATE TABLE IF NOT EXISTS transactions_by_address (
  chain UInt64,
  address FixedString(20),
  block_number UInt64 CODEC(Delta, ZSTD),
  transaction_index UInt32,
  direction Int8,
  counterparty FixedString(20),
  hash FixedString(32),
  value UInt256,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, address, block_number, transaction_index, direction)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS transactions_by_address_mv TO transactions_by_address AS
SELECT
  chain,
  side.1 AS address,
  block_number,
  transaction_index,
  side.2 AS direction,
  side.3 AS counterparty,
  hash,
  value,
  timestamp,
  _version
FROM
(
  SELECT
    chain,
    block_number,
    transaction_index,
    hash,
    value,
    timestamp,
    _version,
    `from` AS sender,
    ifNull(`to`, contract_created) AS recipient,
    isNotNull(`to`) OR contract_created != toFixedString('', 20) AS has_recipient
  FROM transactions
)
ARRAY JOIN arrayConcat(
  [(sender, toInt8(-1), recipient)],
  if(has_recipient, [(recipient, toInt8(1), sender)], [])
) AS side;

-- eth_getLogs style: contract + topic0 + block range. Slim on purpose, the
-- log itself is read from logs by (chain, block_number, log_index).
CREATE TABLE IF NOT EXISTS logs_by_address (
  chain UInt64,
  address FixedString(20),
  topic0 FixedString(32),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, address, topic0, block_number, log_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS logs_by_address_mv TO logs_by_address AS
SELECT chain, address, topic0, block_number, log_index, _version
FROM logs;

-- Wallet history / balances: two rows per transfer.
CREATE TABLE IF NOT EXISTS erc20_transfers_by_account (
  chain UInt64,
  account FixedString(20),
  token_address FixedString(20),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  direction Int8,
  counterparty FixedString(20),
  amount UInt256,
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, account, token_address, block_number, log_index, direction)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS erc20_transfers_by_account_mv TO erc20_transfers_by_account AS
SELECT
  chain,
  side.1 AS account,
  token_address,
  block_number,
  log_index,
  side.2 AS direction,
  side.3 AS counterparty,
  amount,
  transaction_hash,
  timestamp,
  _version
FROM erc20_transfers
ARRAY JOIN [(`from`, toInt8(-1), `to`), (`to`, toInt8(1), `from`)] AS side;

-- NFT history / holdings: ERC721 and ERC1155 in one table, two rows per
-- transferred id. batch_index is the position inside an ERC1155 batch (0
-- for ERC721 and TransferSingle) and keeps the ids of one log apart.
CREATE TABLE IF NOT EXISTS nft_transfers_by_account (
  chain UInt64,
  account FixedString(20),
  token_address FixedString(20),
  block_number UInt64 CODEC(Delta, ZSTD),
  log_index UInt32,
  direction Int8,
  batch_index UInt32,
  counterparty FixedString(20),
  token_id UInt256,
  amount UInt256,
  -- 'ERC721' | 'ERC1155'
  standard LowCardinality(String),
  transaction_hash FixedString(32),
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, account, token_address, block_number, log_index, direction, batch_index)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS nft_transfers_by_account_erc721_mv TO nft_transfers_by_account AS
SELECT
  chain,
  side.1 AS account,
  token_address,
  block_number,
  log_index,
  side.2 AS direction,
  toUInt32(0) AS batch_index,
  side.3 AS counterparty,
  id AS token_id,
  toUInt256(1) AS amount,
  'ERC721' AS standard,
  transaction_hash,
  timestamp,
  _version
FROM erc721_transfers
ARRAY JOIN [(`from`, toInt8(-1), `to`), (`to`, toInt8(1), `from`)] AS side;

CREATE MATERIALIZED VIEW IF NOT EXISTS nft_transfers_by_account_erc1155_mv TO nft_transfers_by_account AS
SELECT
  chain,
  side.1 AS account,
  token_address,
  block_number,
  log_index,
  side.2 AS direction,
  toUInt32(item.3 - 1) AS batch_index,
  side.3 AS counterparty,
  item.1 AS token_id,
  item.2 AS amount,
  'ERC1155' AS standard,
  transaction_hash,
  timestamp,
  _version
FROM erc1155_transfers
ARRAY JOIN arrayZip(ids, amounts, arrayEnumerate(ids)) AS item
ARRAY JOIN [(`from`, toInt8(-1), `to`), (`to`, toInt8(1), `from`)] AS side;

-- Traces of a transaction. Slim: the trace itself is read from traces by
-- (chain, block_number, transaction_position, trace_address). Reward
-- traces have no transaction and are not listed.
CREATE TABLE IF NOT EXISTS traces_by_tx (
  chain UInt64,
  transaction_hash FixedString(32),
  trace_address Array(UInt32),
  block_number UInt64,
  transaction_position UInt32,
  _version UInt64,
  INDEX idx_block_number block_number TYPE minmax GRANULARITY 1
)
ENGINE = ReplacingMergeTree(_version)
PARTITION BY chain
ORDER BY (chain, transaction_hash, trace_address)
SETTINGS index_granularity = 8192, do_not_merge_across_partitions_select_final = 1;

CREATE MATERIALIZED VIEW IF NOT EXISTS traces_by_tx_mv TO traces_by_tx AS
SELECT chain, transaction_hash, trace_address, block_number, transaction_position, _version
FROM traces
WHERE transaction_position != 4294967295;
