-- Fleet mode: the chains ONE process was asked to index, and the settings
-- each of them starts with (docs/design.md section 15).
--
-- This table is DESIRED STATE, not live state. `indexer fleet` reads it
-- ONCE at start and from then on its own memory is the authority: ClickHouse
-- has no read-your-writes, so a panel that wrote a row a moment ago cannot
-- read it back reliably, and a supervisor that polled this table would keep
-- changing its mind. Every start / stop / settings change made through the
-- panel is applied in memory FIRST and written here afterwards, so the next
-- start of the process comes up the way the owner left it.
--
-- Live state - is the chain running, how far behind is it, what failed
-- last - is never stored here. It lives in the supervisor's memory and is
-- served by the panel's JSON API; the heartbeat rows of OTHER processes are
-- in `indexer_instances` (migration 0005).
--
-- Insert only, like every other table in this schema: a chain is removed
-- from the fleet by setting `desired = 'stopped'`, never by a DELETE. The
-- panel has no destructive endpoint at all.
--
-- `settings` is a JSON object whose keys are the LONG FLAG NAMES of
-- `indexer run` without the dashes and whose values are strings, exactly as
-- they would be typed on a command line:
--
--   {"start-block":"18000000","confirmations":"12","no-predictions":"true"}
--
-- An absent key means "the `run` default applies", which is why adding a
-- chain needs nothing but its id. The strings are fed through the SAME
-- parsers the command line uses (src/configs), so the panel cannot invent a
-- setting the CLI would reject. An empty object is the whole configuration
-- of a normal chain.
--
-- No seed rows: the migrator refuses INSERTs, and which chains a deployment
-- indexes is the owner's decision, not the schema's.
CREATE TABLE IF NOT EXISTS fleet_chains (
  chain UInt64,
  -- 'running' or 'stopped'. Anything else is read as 'stopped' by the
  -- supervisor: an unknown word must never start a chain by accident.
  desired LowCardinality(String),
  -- JSON object of `indexer run` options; '{}' = every default applies.
  settings String DEFAULT '{}',
  _version UInt64 DEFAULT toUnixTimestamp64Milli(now64(3))
)
ENGINE = ReplacingMergeTree(_version)
-- A handful of rows, one per chain: partitioning them would only make parts.
PARTITION BY tuple()
ORDER BY chain;

-- The desired state as the supervisor reads it at start: newest row per
-- chain, no FINAL to remember at the call site.
CREATE VIEW IF NOT EXISTS fleet_chains_v AS
SELECT chain, desired, settings, _version
FROM fleet_chains FINAL;
