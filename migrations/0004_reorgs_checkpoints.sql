-- STUB (predictions worktree only, NOT to be merged): the schema engineer
-- owns migration 0004. The prediction views only need `reorgs` (chain,
-- epoch, from_ts) and the shared validity step function `epoch_floor_v`
-- (chain, from_ts, epoch_floor) described in docs/design.md, section 1
-- "Aggregates" and section 2.
CREATE TABLE IF NOT EXISTS reorgs (
  chain UInt64,
  epoch UInt32,
  from_ts DateTime('UTC'),
  detected_at DateTime('UTC') DEFAULT now(),
  fork_block UInt64 DEFAULT 0,
  old_head UInt64 DEFAULT 0,
  old_hash FixedString(32) DEFAULT toFixedString('', 32),
  new_hash FixedString(32) DEFAULT toFixedString('', 32),
  depth UInt64 DEFAULT 0,
  rows_tombstoned UInt64 DEFAULT 0,
  reason LowCardinality(String) DEFAULT 'reorg'
)
ENGINE = MergeTree
ORDER BY (chain, epoch);

-- For every (chain, from_ts): the largest epoch of all purges of the chain
-- starting at or before from_ts. ASOF joined by every *_v view.
CREATE VIEW IF NOT EXISTS epoch_floor_v AS
SELECT
  chain,
  from_ts,
  max(step) OVER (PARTITION BY chain ORDER BY from_ts ASC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS epoch_floor
FROM
(
  SELECT chain, toDateTime(from_ts, 'UTC') AS from_ts, max(epoch) AS step
  FROM reorgs
  GROUP BY chain, from_ts
);
