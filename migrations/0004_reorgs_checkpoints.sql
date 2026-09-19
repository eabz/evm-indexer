-- Reorg bookkeeping, checkpoints, and the reader views of the core
-- aggregates (docs/design.md, sections 2 and 3). Everything is insert only:
-- the indexer never issues DELETE, ALTER ... DELETE or DROP PARTITION.

-- One row per purge (rollback, gap heal or module re-decode). Insert only,
-- like everything else: audit trail, metric source, and the input of the
-- validity rule.
--
-- Two rows per purge: one when it ARMS the validity rule (completed = 0,
-- before the bucket repair, so readers under-count rather than double
-- count) and one when it has FINISHED (completed = 1, everything durable).
-- They are not collapsed into one row on purpose - an epoch can be reused
-- by a purge that started while the `reorgs` row of the previous one was
-- not readable yet (ClickHouse gives no read-your-writes), and replacing by
-- (chain, epoch) would then lose a purge's `from_ts` and let the
-- contributions it hid come back. A purge with no completed row is one that
-- died: `SELECT ... FROM reorgs WHERE completed = 1` is the audit trail of
-- what really happened.
--
-- `completed` + `tombstone_version` are not cosmetic: they are how the next
-- start tells the debris of a FINISHED purge - tombstoned rows at block
-- numbers a shorter chain does not have any more, which nothing will ever
-- stream again - from the leftovers of a purge that died half way, which
-- must be run again. Without them every restart purged that tail once more
-- (a new epoch and a rebuild of every aggregate from that day to now).
CREATE TABLE IF NOT EXISTS reorgs (
  chain UInt64,
  -- The purge generation this purge started. Rows written afterwards
  -- carry it.
  epoch UInt32,
  -- Start of the first aggregate bucket the purge touched (start of day,
  -- UTC): buckets from here on only count contributions of epoch >= epoch.
  from_ts DateTime('UTC'),
  detected_at DateTime('UTC') DEFAULT now(),
  -- The purged block range [fork_block, to_block); to_block = max UInt64
  -- means open ended (a rollback at the tip).
  fork_block UInt64,
  to_block UInt64 DEFAULT 18446744073709551615,
  -- Highest stored block when the purge started.
  old_head UInt64,
  -- Stored / canonical hash at the height the mismatch was detected
  -- (zero bytes for a gap heal).
  old_hash FixedString(32),
  new_hash FixedString(32),
  depth UInt64,
  rows_tombstoned UInt64,
  -- 'reorg' | 'gap_heal' | 'redecode'
  reason LowCardinality(String),
  -- `_version` this purge stamped on every tombstone it wrote. Together
  -- with `completed` it says which tombstones are settled: one written by
  -- a purge that FINISHED needs no further attention, one with a higher
  -- version was written by a purge that died and has to be run again
  -- (its rows are still counted by the aggregates).
  tombstone_version UInt64 DEFAULT 0,
  -- 1 once every step of this purge finished, the bucket repair included.
  completed UInt8 DEFAULT 0
)
ENGINE = MergeTree
ORDER BY (chain, epoch);

-- The validity rule as a step function per chain: from from_ts on (until
-- the next row of the chain), contributions need epoch >= epoch_floor.
-- Joined by the *_v views with ASOF (the closest from_ts <= bucket): one
-- hash lookup plus a binary search per aggregate row, however many reorgs a
-- chain has had. Several purges sharing a from_ts collapse into one row and
-- the running maximum makes a later from_ts never lower the bar.
CREATE VIEW IF NOT EXISTS epoch_floor_v AS
SELECT
  chain,
  from_ts,
  max(epoch_at) OVER (
    PARTITION BY chain ORDER BY from_ts ASC
    ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
  ) AS epoch_floor
FROM
(
  SELECT chain, from_ts, max(epoch) AS epoch_at
  FROM reorgs
  GROUP BY chain, from_ts
);

-- Checkpoints: one row per contiguous range a flush committed, written
-- after `blocks`. Resume = the highest contiguous to_block from the start
-- block, read with FINAL. A purge tombstones the overlapping rows (same
-- key, newer _version, is_deleted = 1) and, for a partially covered range,
-- inserts the surviving remainder.
CREATE TABLE IF NOT EXISTS checkpoints (
  chain UInt64,
  -- [from_block, to_block)
  from_block UInt64,
  to_block UInt64,
  epoch UInt32 DEFAULT 0,
  _version UInt64,
  is_deleted UInt8 DEFAULT 0
)
ENGINE = ReplacingMergeTree(_version, is_deleted)
ORDER BY (chain, from_block, to_block);

-- ifNull(epoch_floor, 0): a chain without any reorg has no row to join.
-- By default the unmatched side reads 0, but under join_use_nulls = 1 (a
-- per user / per profile setting a BI tool may set) it reads NULL, and a
-- bare comparison would silently drop EVERY row.
--
-- Reader views of the aggregates of 0003: they finalize the states and
-- apply the validity rule BEFORE merging them (a hidden contribution must
-- not reach a sum or a uniq state). The views only expose plain types (no
-- SimpleAggregateFunction leaks into a client).

CREATE VIEW IF NOT EXISTS daily_block_stats_v AS
SELECT
  chain,
  day,
  sum(s.blocks) AS blocks,
  sum(s.transactions) AS transactions,
  sum(s.gas_used) AS gas_used,
  sum(s.gas_limit) AS gas_limit,
  sum(s.size) AS size,
  toUInt64(min(s.first_block)) AS first_block,
  toUInt64(max(s.last_block)) AS last_block,
  uniqMerge(s.miners) AS unique_miners,
  avgMerge(s.base_fee_per_gas) AS avg_base_fee_per_gas
FROM daily_block_stats AS s
ASOF LEFT JOIN epoch_floor_v AS r ON r.chain = s.chain AND s.day >= r.from_ts
WHERE s.epoch >= ifNull(r.epoch_floor, 0)
GROUP BY chain, day;

CREATE VIEW IF NOT EXISTS daily_transaction_stats_v AS
SELECT
  chain,
  day,
  sum(s.transactions) AS transactions,
  sum(s.successful) AS successful,
  sum(s.failed) AS failed,
  sum(s.contract_creations) AS contract_creations,
  sum(s.gas_used) AS gas_used,
  sum(s.value) AS value,
  sum(s.fees) AS fees,
  uniqMerge(s.senders) AS unique_senders,
  uniqMerge(s.recipients) AS unique_recipients,
  avgMerge(s.effective_gas_price) AS avg_effective_gas_price
FROM daily_transaction_stats AS s
ASOF LEFT JOIN epoch_floor_v AS r ON r.chain = s.chain AND s.day >= r.from_ts
WHERE s.epoch >= ifNull(r.epoch_floor, 0)
GROUP BY chain, day;

-- volume is volume_raw scaled by the token decimals. NULL (never 0) while
-- the token has no metadata row yet, or is not an ERC20.
CREATE VIEW IF NOT EXISTS daily_erc20_transfer_stats_v AS
SELECT
  s.chain AS chain,
  s.token_address AS token_address,
  s.day AS day,
  s.transfers AS transfers,
  s.volume_raw AS volume_raw,
  if(t.type = 'ERC20', s.volume_raw / pow(10, t.decimals), NULL) AS volume,
  s.unique_senders AS unique_senders,
  s.unique_recipients AS unique_recipients
FROM
(
  SELECT
    chain,
    token_address,
    day,
    sum(a.transfers) AS transfers,
    sum(a.volume_raw) AS volume_raw,
    uniqMerge(a.senders) AS unique_senders,
    uniqMerge(a.recipients) AS unique_recipients
  FROM daily_erc20_transfer_stats AS a
  ASOF LEFT JOIN epoch_floor_v AS r ON r.chain = a.chain AND a.day >= r.from_ts
  WHERE a.epoch >= ifNull(r.epoch_floor, 0)
  GROUP BY chain, token_address, day
) AS s
LEFT JOIN
(
  SELECT chain, address, decimals, type FROM tokens FINAL
) AS t ON t.chain = s.chain AND t.address = s.token_address;
