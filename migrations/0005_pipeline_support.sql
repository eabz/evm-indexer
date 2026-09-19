-- Pipeline support (docs/design.md, sections 2 and 4).
--
-- 1. Retried inserts must not double count. Every insert of a flush carries
--    a deterministic insert_deduplication_token (table, chain, block span,
--    _version) and runs with deduplicate_blocks_in_dependent_materialized_views
--    = 1, so a retry of an insert that WAS applied (timeout, lost answer) is
--    dropped by the server instead of firing the materialized views a second
--    time. On non-replicated tables this only works when the table keeps a
--    deduplication log, and (verified on 25.12) EVERY TARGET of a
--    materialized view needs its own: with the window on the base table
--    only, the base table deduplicates and the side tables / aggregates
--    still receive the rows again.
--
--    Every block scoped base table, every side table and every aggregate
--    must therefore carry this setting (a unit test in src/pipeline checks
--    the embedded migrations). 50000 = the most recent insert blocks
--    remembered per table: 50 chains flushing every 2 s is ~25 inserts/s,
--    so ~30 minutes, longer than the retries of a flush can last.

ALTER TABLE blocks MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE transactions MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE logs MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE withdrawals MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc20_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc721_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc1155_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE tx_lookup MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE block_lookup MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE transactions_by_address MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE logs_by_address MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc20_transfers_by_account MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE nft_transfers_by_account MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_block_stats MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_transaction_stats MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_erc20_transfer_stats MODIFY SETTING non_replicated_deduplication_window = 50000;

-- 2. Token contracts seen in transfers, for the token metadata backfill
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
ORDER BY (chain, address)
SETTINGS non_replicated_deduplication_window = 50000;

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
