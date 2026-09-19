-- The registry-only history pass: what a prediction market needs from
-- BELOW the coverage floor (docs/design.md section 16).
--
-- Prediction markets are the one dataset that is wrong without older data.
-- A market whose `ConditionPreparation` happened before the floor has no
-- question and no outcomes at all, and its open interest - split minus
-- merged minus redeemed collateral - counts only the part of its life this
-- database saw, so it can go NEGATIVE. Neither is a bug in the decoder:
-- both are what "we started indexing in the middle" looks like.
--
-- The fix is narrow on purpose. One pass, once, over the blocks below the
-- floor, asking the source for the logs of the operator's OWN trusted
-- addresses and nothing else (`prediction_trusted`, migration 0020): the
-- registry, the exchanges, the NegRisk adapters. It stores market
-- metadata, questions, outcome-token mappings and the split / merge /
-- redeem / convert / resolution events that open interest is made of, and
-- it stores NO TRADE outside the covered window - a trade below the floor
-- would be volume this database does not otherwise have, and mixing it in
-- would make every total a different shape below the floor than above it.
--
-- ## Why there is no table of deployment blocks
--
-- Design section 16 says "from their deployment block to the floor", and
-- this schema deliberately ships no deployment block for anything, for the
-- same reason `prediction_trusted` ships no addresses: a list of contract
-- addresses in a repository is exactly the thing the operator is supposed
-- to be deciding for themselves.
--
-- It costs nothing to leave it out. The pass is a LOG FILTER over a handful
-- of addresses, and a source that serves those filters skips the blocks
-- before a contract existed without reading them - it reports how far it
-- got and the pass jumps straight there. Starting at block 0 is therefore
-- the same amount of work as starting at the real deployment block, minus
-- the risk of a number in a README being wrong.
--
-- An operator who knows better may still say so: `from_block` below is the
-- first block that address is worth asking about. It defaults to 0, which
-- means "wherever the source says it starts".
ALTER TABLE prediction_trusted
  ADD COLUMN IF NOT EXISTS from_block UInt64 DEFAULT 0;

-- How far the registry-only pass has got, per chain.
--
-- One row per chain, insert only, and the newest row wins BY THE BLOCK it
-- reached: `_version` IS `done_to_block`, so a pass that got further always
-- beats one that got less far, whatever order two inserts land in and
-- whichever of them ClickHouse showed to which reader (there is no
-- read-your-writes, design section 2).
--
-- That is the whole of the resumability. The pass always walks upwards from
-- the bottom, so one high-water mark says everything: a crash, a restart or
-- a second run picks up where the last chunk finished, and re-running a
-- chunk that was already written is harmless because every insert carries
-- the same deterministic dedup token and every row is keyed the same way.
--
-- `floor_block` is recorded next to it so an operator can see which floor
-- the pass was aimed at. A floor that was later LOWERED by `indexer
-- backfill` leaves a pass whose `done_to_block` is below the new floor; the
-- next run simply carries on from there.
--
-- No seed rows: the migrator refuses INSERTs.
CREATE TABLE IF NOT EXISTS prediction_history (
  chain UInt64,
  -- Exclusive: blocks [0, done_to_block) have been asked for.
  done_to_block UInt64,
  -- The coverage floor this pass was walking towards.
  floor_block UInt64,
  finished_at DateTime DEFAULT now(),
  -- = done_to_block, so the furthest pass wins.
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
-- One row per chain; partitioning them would only make parts.
PARTITION BY tuple()
ORDER BY chain;

-- How far the pass has got, without a FINAL to remember at the call site.
CREATE VIEW IF NOT EXISTS prediction_history_v AS
SELECT chain, done_to_block, floor_block, finished_at
FROM prediction_history FINAL;
