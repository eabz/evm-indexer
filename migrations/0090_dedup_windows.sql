-- Retried inserts must not double count (docs/design.md, section 2).
--
-- Every insert of a flush carries a deterministic
-- insert_deduplication_token (table, chain, block span, _version) and runs
-- with deduplicate_blocks_in_dependent_materialized_views = 1, so a retry
-- of an insert that WAS applied (timeout, lost answer) is dropped by the
-- server instead of firing the materialized views a second time.
--
-- On non-replicated tables this only works when the table keeps a
-- deduplication log, and (verified on 25.12) EVERY TARGET of a materialized
-- view needs its own: with the window on the base table only, the base
-- table deduplicates and the side tables / aggregates still receive the
-- rows again. (It also can not be combined with async_insert: the server
-- refuses that with code 344, so the inserts of a flush are synchronous.)
--
-- So: every table a flush writes, and every table fed by a materialized
-- view of one, in every module. A unit test (src/pipeline/dedup.rs) checks
-- the embedded migrations. An applied migration never changes: a module
-- added later sets the window in its own CREATE TABLE statements or ships
-- its own ALTER migration; the unit test accepts either.
--
-- 50000 = the most recent insert blocks remembered per table: 50 chains
-- flushing every 2 s is ~25 inserts/s, so more than 30 minutes, longer
-- than the retries of a flush can last.

-- core: base tables
ALTER TABLE blocks MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE transactions MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE logs MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE withdrawals MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc20_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc721_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc1155_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE checkpoints MODIFY SETTING non_replicated_deduplication_window = 50000;
-- core: side tables and aggregates
ALTER TABLE tx_lookup MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE block_lookup MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE transactions_by_address MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE logs_by_address MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE erc20_transfers_by_account MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE nft_transfers_by_account MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE seen_tokens MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_block_stats MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_transaction_stats MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE daily_erc20_transfer_stats MODIFY SETTING non_replicated_deduplication_window = 50000;
-- dex (0010 - 0012)
ALTER TABLE dex_swaps MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_liquidity MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_pools MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_swaps_by_pool MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_swaps_by_trader MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_pools_by_token MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_candles_1m MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_candles_1h MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_candles_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE dex_pool_volume_1h MODIFY SETTING non_replicated_deduplication_window = 50000;
-- predictions (0020 - 0022)
ALTER TABLE prediction_outcome_tokens MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_markets MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_questions MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_resolutions MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_position_events MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_transfers MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_trades MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_outcome_tokens_by_market MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_trades_by_token MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_ledger_by_holder MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_ledger_by_token MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_candles_1m MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_candles_1h MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_candles_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_market_flows_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_trader_trades_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE prediction_trader_flows_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
