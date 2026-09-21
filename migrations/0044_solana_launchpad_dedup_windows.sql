-- Deduplication windows for the Solana launchpad tables of `0043`.
--
-- Same reason as `0090` and `0033`, restated because it is easy to miss
-- when a module lands before the pipeline that writes it: the flush's
-- `insert_deduplication_token` only drops a retried insert on a table that
-- keeps a deduplication log, and (verified on 25.12) EVERY target of a
-- materialized view needs its own - with the window on the base table only
-- the base table deduplicates and the aggregates still count the rows
-- twice.
--
-- `0043` created these three and set no window, because at that point
-- nothing wrote them: the launchpad decoder was tested through
-- `insert_flush` by hand. `indexer run --chain solana` writes all three in
-- its flush now (`pipeline::solana_writer::store_children`), so a timed-out
-- insert that had in fact been applied would be counted twice without
-- this.
--
-- The shared `launchpad_*` tables the same flush writes already have their
-- windows from `0033`, and the `sol_*` DEX tables from `0042`.
--
-- Two of the three are not block scoped (`sol_dex_programs` is an
-- operator's judgement, `sol_launchpad_configs` is chain state, neither is
-- ever purged) and they feed no materialized view. They carry a window
-- anyway: they travel in the flush and therefore under its token, so the
-- retry semantics have to hold for them too.
ALTER TABLE sol_token_balances MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_launchpad_configs MODIFY SETTING non_replicated_deduplication_window = 50000;
ALTER TABLE sol_dex_programs MODIFY SETTING non_replicated_deduplication_window = 50000;
