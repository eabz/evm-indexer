-- Retried inserts must not double count, for the launchpad tables
-- (docs/design.md section 2 and the header of 0090).
--
-- 0090 does this for the modules that existed when it was written, and an
-- applied migration never changes, so a module added later ships its own:
-- every table a flush writes AND every target of a materialized view needs
-- its OWN deduplication log. With the window on the base table only, the
-- base table deduplicates and the side tables and aggregates receive the
-- rows a second time (verified on ClickHouse 25.12).
--
-- 50000 = the most recent insert blocks remembered per table, the same
-- value every other module uses.

-- base tables
ALTER TABLE launchpad_tokens MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_trades MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_graduations MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_creator_fees MODIFY SETTING non_replicated_deduplication_window = 50000;

-- read path side tables
ALTER TABLE launchpad_trades_by_token MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_trades_by_trader MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_launches_by_time MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_launches_by_creator MODIFY SETTING non_replicated_deduplication_window = 50000;

-- aggregates
ALTER TABLE launchpad_candles_1m MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_candles_1h MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_venue_trades_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_launches_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_graduations_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE launchpad_creator_fees_1d MODIFY SETTING non_replicated_deduplication_window = 50000;
