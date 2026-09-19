-- Pipeline support (docs/design.md, sections 2 and 4).
--
-- 1. Token contracts seen in transfers, for the token metadata backfill
--    (MissingTokenSource): "which token addresses have no tokens row" must
--    never scan the transfer tables (billions of rows). The views keep one
--    row per (chain, token) - a few million at most - and the anti-join
--    against `tokens` runs over that. Not block scoped on purpose: a token
--    only ever seen on an abandoned fork is still worth a metadata row,
--    and resolving it is harmless. (DEX pool tokens are read from
--    dex_pools_by_token, which is just as small.)
CREATE TABLE IF NOT EXISTS seen_tokens (
  chain UInt64,
  address FixedString(20),
  -- Standard hint: the transfer table the address was seen in.
  type LowCardinality(String)
)
ENGINE = ReplacingMergeTree
PARTITION BY chain
ORDER BY (chain, address);

CREATE MATERIALIZED VIEW IF NOT EXISTS seen_tokens_erc20_mv TO seen_tokens AS
SELECT DISTINCT chain, token_address AS address, 'ERC20' AS type
FROM erc20_transfers
WHERE is_deleted = 0;

CREATE MATERIALIZED VIEW IF NOT EXISTS seen_tokens_erc721_mv TO seen_tokens AS
SELECT DISTINCT chain, token_address AS address, 'ERC721' AS type
FROM erc721_transfers
WHERE is_deleted = 0;

CREATE MATERIALIZED VIEW IF NOT EXISTS seen_tokens_erc1155_mv TO seen_tokens AS
SELECT DISTINCT chain, token_address AS address, 'ERC1155' AS type
FROM erc1155_transfers
WHERE is_deleted = 0;

-- 2. One indexer process per chain (the epoch of a chain lives in the
--    memory of its writer; two writers would hide each other's aggregates).
--    Insert only, like everything else: every process writes a heartbeat
--    row every few seconds, stamped with the SERVER clock, and refuses to
--    start (or stops) while another instance of the same chain is alive.
--    A clean shutdown writes released = 1, so a restart does not wait.
CREATE TABLE IF NOT EXISTS indexer_instances (
  chain UInt64,
  -- Random per process.
  instance String,
  host String,
  started_at DateTime64(3, 'UTC'),
  heartbeat DateTime64(3, 'UTC'),
  released UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(heartbeat)
ORDER BY (chain, instance)
TTL toDateTime(heartbeat) + INTERVAL 7 DAY;
