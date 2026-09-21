-- The coverage floor: the date this database promises to be complete from
-- (docs/design.md section 16).
--
-- The product promise is "gap-free and consistent from a known date to
-- now", not "all of history". The FLOOR is that known date, resolved to a
-- block, and this table is where it lives. It is decided once, on a chain's
-- first start, and from then on it is a fact about the data rather than a
-- setting: a restart, a different `--start-block`, a different
-- `--start-date` or anything typed into the control panel keeps it exactly
-- where it is. The log says so and carries on.
--
-- Its own table rather than two columns on `chains` (migration 0006),
-- because the two answer different questions and have different writers.
-- `chains` is a naming registry - "what is chain 8453 called, and is it EVM
-- or Solana" - user populated, never touched by a running indexer, and read
-- by views to know how to PRINT an identity column. The floor is written by
-- the indexer at its own first start, is per deployment (the same chain id
-- in two databases can have two different floors), and would be a second,
-- silently-empty half of every `chains` row if it lived there. Migration
-- 0006's own header says `chains` ships no rows and the analytics tables
-- never join it; a floor that is missing for a chain nobody named would
-- have been a footgun.
--
-- ## Why the floor can only ever move EARLIER
--
-- Data is never dropped, so raising the floor would be a claim the stored
-- rows contradict. Lowering it is a real thing an owner does - `indexer
-- backfill --start-date 2020-01-01` - and it happens AFTER the older range
-- is complete and verified, so by the time the row is written the claim is
-- already true.
--
-- That rule is in the engine, not only in the code. `_version` is
--
--     18446744073709551615 - coverage_from_block
--
-- so the ReplacingMergeTree, which keeps the row with the HIGHEST
-- `_version`, keeps the row with the LOWEST `coverage_from_block`. "First
-- writer wins" and "a later start cannot raise it" therefore survive two
-- processes inserting at the same moment, a replayed insert, and a
-- restart - none of which a read-then-write could survive on its own,
-- because ClickHouse has no read-your-writes (design section 2).
--
-- The code still reads before it writes, and still refuses to write when a
-- floor is already there: the version rule is the floor under the floor,
-- not the mechanism. And the two are belt and braces rather than
-- duplicates, because the code is what produces the WARNING that tells the
-- owner their new `--start-date` was ignored.
--
-- Insert only, like everything else here: there is no DELETE and no ALTER
-- anywhere in this schema. A floor is corrected by inserting a lower one.
--
-- No seed rows: the migrator refuses INSERTs, and which date a deployment
-- is complete from is a fact about its data, not about its schema.
CREATE TABLE IF NOT EXISTS chain_coverage (
  chain UInt64,
  -- First block this deployment promises to have. On Solana, the first
  -- SLOT (the column name is shared with every other block-scoped table).
  coverage_from_block UInt64,
  -- That block's own timestamp, unix seconds. 0 when the source could not
  -- give one, which is a missing time and never a block mined in 1970.
  coverage_from_ts UInt32,
  -- How the floor was chosen, for the operator reading a row a year later:
  --   'default-1y'   no flag was given: now - 365 days (EVM)
  --   'head'         no flag was given: the head (Solana, --new-blocks-only)
  --   'start-block'  --start-block N on the chain's first start
  --   'start-date'   --start-date YYYY-MM-DD on the chain's first start
  --   'backfill'     lowered by `indexer backfill`, after the older range
  --                  was complete and verified
  --   'existing'     the oldest block this database already had when the
  --                  floor was first written (an upgrade of a deployment
  --                  that was indexing before floors existed)
  reason LowCardinality(String),
  set_at DateTime DEFAULT now(),
  -- See the header: MAX - coverage_from_block, so the LOWEST floor wins.
  _version UInt64
)
ENGINE = ReplacingMergeTree(_version)
-- One row per chain. Partitioning a handful of rows would only make parts.
PARTITION BY tuple()
ORDER BY chain;

-- What this deployment promises, per chain, in one row.
--
-- `covered_to_block` is the EXCLUSIVE end of the run of blocks that is
-- complete from the floor upwards, read from `checkpoints` exactly the way
-- `pipeline::verify::resume_point` reads it: sort the live checkpoint
-- ranges by their start and walk them, extending the reach while the next
-- range starts at or below it, and stop at the first hole. So
-- `covered_to_block = coverage_from_block` means nothing is covered yet,
-- and a value below the chain's stored head means there is a hole in
-- between - `indexer verify` is what says where.
--
-- `checkpoints` is written by both families, so this view answers for
-- Solana too; there `coverage_from_block` and `covered_to_block` are slots.
--
-- The fold is over the chain's own checkpoint rows, which are compacted
-- (migration 0004) and therefore a handful per chain, not one per flush.
CREATE VIEW IF NOT EXISTS coverage_v AS
WITH tiles AS (
  SELECT
    chain,
    arraySort(groupArray((from_block, to_block))) AS ranges
  FROM checkpoints FINAL
  GROUP BY chain
)
SELECT
  floors.chain AS chain,
  floors.coverage_from_block AS coverage_from_block,
  floors.coverage_from_ts AS coverage_from_ts,
  -- The floor as the owner wrote it. A 0 timestamp has no date, and
  -- printing 1970-01-01 would be a lie, so it prints as unknown.
  if(
    floors.coverage_from_ts = 0,
    'unknown',
    formatDateTime(toDateTime(floors.coverage_from_ts), '%F', 'UTC')
  ) AS coverage_from_date,
  floors.reason AS reason,
  floors.set_at AS set_at,
  arrayFold(
    (reach, tile) ->
      if(tile.1 <= reach, greatest(reach, tile.2), reach),
    ifNull(tiles.ranges, []),
    floors.coverage_from_block
  ) AS covered_to_block
FROM chain_coverage AS floors FINAL
LEFT JOIN tiles ON tiles.chain = floors.chain;
