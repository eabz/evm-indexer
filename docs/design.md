# Design (binding for all engineers)

**This is the only design document, and it is binding.** Everything the project
decided is here: the rules in sections 1–16, and in section 17 the decisions
that closed a question — what was chosen, why, and what was rejected. The
research files those decisions came out of were folded into this document and
removed on 2026-09-19; nothing outside it is authoritative.

The schema was designed against an empty database: no backward compatibility,
no 2.x migration path. If something here is wrong or impossible, say so — do
not silently deviate.

Deferred, and named here so it is not rediscovered: Arrow passthrough on the
EVM path (`stream_arrow` straight into `FORMAT ArrowStream`).

## 1. Schema rules

| Data | ClickHouse type | Rust side |
|---|---|---|
| hashes, topics | `FixedString(32)` | alloy `B256`, serialized as 32 raw bytes |
| addresses | `FixedString(20)` | alloy `Address`, 20 raw bytes |
| wei amounts, gas prices, difficulty, token amounts/ids | `UInt256` | alloy `U256`, 32 bytes **little-endian** |
| signed amounts (DEX) | `Int256` | alloy `I256`, 32 bytes little-endian two's complement |
| calldata, log data, code, output | `String` (raw bytes, not hex) | `Bytes` |
| 4-byte selector | `FixedString(4)` (zeros when input < 4 bytes) | |
| block number, gas*, nonce, size | `UInt64` | no saturation anywhere |
| tx index, log index, counts | `UInt32` | |
| enumerations (status, tx type, action/call/reward type, token type, dex protocol) | `LowCardinality(String)` | |
| timestamps | `DateTime CODEC(DoubleDelta, ZSTD)` | u32 |

- **256-bit arithmetic rule.** Raw columns are always exact `UInt256`/`Int256`
  (`Decimal256` is not an option: 76 digits < the 78 a `uint256` needs). But
  `sum()` over `UInt256`/`Int256` **wraps silently on overflow**, and hostile tokens
  routinely emit `2^256-1` amounts, so:
  analytics aggregates (volume, candles, USD) sum `toFloat64(amount) / pow(10, decimals)`
  — never the raw integer; exact accounting (balances) sums signed `Int256` per
  (token, account) only, where wrap-around needs an economically impossible supply.
  Wire format is 32 bytes little-endian (4 LE `u64` limbs of alloy's `U256`); every
  table's integration test round-trips a value > 2^128 and compares `toString()`.
- **No hex strings anywhere in storage.** Readers format with `concat('0x', lower(hex(x)))`.
- **`Nullable` only where NULL differs from the default in meaning** (`base_fee_per_gas`
  pre-London, tx `status` pre-Byzantium, EIP-1559 fee fields on legacy txs, tx `to` on
  creations). Log topics are four
  non-null columns defaulting to 32 zero bytes. Everything else non-null with a default.
- Dead columns are removed: `log_type`, `removed`, the duplicated `address` on transfer
  tables (keep `token_address`), `is_uncle`, `blocks.logs_bloom`. `logs.transaction_log_index`
  becomes `transaction_index`. Traces do not exist and `contracts` is a view (§9).
- Every block-scoped table: `ENGINE = ReplacingMergeTree(_version, is_deleted)`,
  `_version UInt64` (strictly increasing per process, unix-ms based), `is_deleted UInt8
  DEFAULT 0`, plus `epoch UInt32` (§2). **Target scale is 50+ chains in one database**, so
  base tables are `PARTITION BY toYYYYMM(timestamp)` with `timestamp DateTime('UTC')` —
  the timezone is part of the rule, because `toYYYYMM` of a plain `DateTime` takes the
  month in the SERVER's timezone while the writer splits a flush into whole UTC months —
  never by chain (50 chains x 120
  months would be ~6,000 partitions per table); `chain` is the first sorting-key column,
  which is what prunes reads. Lookup/side tables are `PARTITION BY chain` (hash/address
  lookups must not fan out per month). `SETTINGS do_not_merge_across_partitions_select_final = 1`
  everywhere (a tombstone copies its row's timestamp, so it always lands in the same partition).
- Sorting keys are positional, never hash based, so a re-inserted block replaces itself:

| Table | ORDER BY |
|---|---|
| blocks | (chain, number) |
| transactions | (chain, block_number, transaction_index) |
| logs, erc20/721/1155_transfers | (chain, block_number, log_index) |
| withdrawals | (chain, block_number, withdrawal_index) |
| contracts | — a VIEW over `transactions` (§9) |
| tokens | (chain, address) — not block scoped |

- Codecs: `ZSTD(3)` on large byte columns (not 9); `Delta`/`DoubleDelta` + `ZSTD` on
  monotonic integers.
- Read convention: consumers query base and side tables with `FINAL`, and aggregates
  through their `*_v` views. The README and every module README say so.

### Read-path tables

**No projections** (tombstones must propagate to every read path, and MVs do that for free). No
bloom-filter zoo. Each access pattern gets an MV-fed side table, itself
`ReplacingMergeTree(_version, is_deleted)`; its MV passes `_version` and `is_deleted`
through, so it follows rollbacks automatically:

| Table | Fed from | ORDER BY |
|---|---|---|
| `tx_lookup` | transactions | (chain, hash) → block_number, transaction_index |
| `block_lookup` | blocks | (chain, hash) → number |
| `transactions_by_address` (2 rows/tx: from, to) | transactions | (chain, address, block_number, transaction_index, direction) |
| `logs_by_address` (slim: keys + topic0) | logs | (chain, address, topic0, block_number, log_index) |
| `erc20_transfers_by_account` (2 rows/transfer, signed direction) | erc20_transfers | (chain, account, token_address, block_number, log_index, direction) |
| `nft_transfers_by_account` | erc721 + erc1155 | same shape |

Only skip index allowed: `bloom_filter GRANULARITY 1` on a unique-ish hash column when a
lookup table would be overkill.

### Aggregates

Aggregates are incremental `AggregatingMergeTree` tables fed by MVs, bucketed by time.
They are kept correct under reorgs and re-inserts by the **bucket repair** hook (§2), so
each one is declared in Rust:

```rust
// src/db/derived.rs
pub struct DerivedTable {
    pub name: &'static str,          // target table
    pub bucket_seconds: u32,         // 60, 3600, 86400
    pub bucket_column: &'static str, // DateTime column holding the bucket start
    /// `INSERT INTO <name> SELECT ..., toUInt32({epoch}) AS epoch FROM <base> FINAL
    ///  WHERE chain = {chain} AND timestamp >= {from_ts} GROUP BY ...` — must produce
    ///  exactly what the MV produces. Blocks-sourced aggregates also use
    ///  {purge_from}/{purge_to} (§2). There is no delete SQL.
    pub rebuild_sql: &'static str,
}
pub const CORE_DERIVED: &[DerivedTable] = &[ /* daily block / transaction / erc20-transfer stats */ ];
```

Aggregate tables are `PARTITION BY toYYYYMM(<bucket column>)` in every module (never by
chain or year), with `epoch` as the LAST sorting-key column. All modules apply the
validity rule with the same pattern: a per-chain running-max "epoch floor" view over
`reorgs` + `ASOF LEFT JOIN ... ON f.chain = a.chain AND f.from_ts <= a.bucket WHERE
a.epoch >= f.epoch_floor`, applied BEFORE aggregate states are merged (so a stale epoch
can never leak an open/close). The shared view is `epoch_floor_v`, created in migration
0004 next to `reorgs`.

Distinct counts use `uniqState`/`uniqMerge` (never `uniqExact` in a Summing table).
`status` comparisons use the real stored values. Provide plain SQL `VIEW`s on top that
finalize the states (`*_v`), so consumers never touch `-State` columns.

## 2. Reorgs and the `purge_range` primitive

Layers, outermost first:

1. **`--confirmations N`** (exists): never index within N of the head. Default stays 0.
2. **Detection** (exists): parent-hash continuity inside the stream, seeded from the
   stored hash of the predecessor on resume, plus HyperSync `rollback_guard`.
3. **Fork-point search** (new): on mismatch at block B, fetch canonical headers
   `[B-k, B)` from HyperSync with k = 8, 16, 32 … and compare with stored `blocks.hash`
   (`FINAL`) until a height matches. Bounded by `--max-reorg-depth` (default 512);
   deeper = fatal error with a clear message, never silent.
4. **Rollback = `purge_range(chain, fork, ∞)`** then resume streaming from `fork`.

### No DELETE, ever: tombstones + epochs

ClickHouse 25.12 silently loses one of two concurrent `DELETE`s on the same table (both
return OK, `is_done = 1`, rows stay; reproduced with plain `clickhouse-client`). With 50+
indexer processes sharing a database a lock-and-verify workaround is not acceptable, so
**the indexer never issues `DELETE`, `ALTER .. DELETE/UPDATE` or `DROP PARTITION`.
Everything is an `INSERT`**, which ClickHouse handles concurrently without coordination.
No cross-process lock exists or is needed. (Verified end to end on 25.12: base table,
MV-fed side table and aggregate all correct after a simulated reorg.)

- **Rows are removed by tombstone.** `ReplacingMergeTree(_version, is_deleted)`:
  `INSERT INTO t SELECT <all columns>, <new _version>, 1 AS is_deleted FROM t FINAL WHERE
  chain = ? AND block_number >= ? [AND < ?]` — server side, tiny. `FINAL` hides the row.
  A re-streamed canonical row with the same key simply carries a newer `_version` and
  wins; orphan keys (the canonical block has fewer logs) stay dead.
- **Side tables follow automatically:** every MV passes `_version` and `is_deleted`
  through, so a tombstone inserted into a base table tombstones its side-table rows too.
- **Aggregates use epochs instead of deleting buckets.** Every block-scoped row carries
  `epoch UInt32`, the chain's purge generation, stamped by the writer. Aggregate tables
  include `epoch` in their sorting key; their MVs select `WHERE is_deleted = 0` and group
  by `epoch`. A purge bumps the chain's epoch and records `(chain, epoch, from_ts)` in
  `reorgs`, where `from_ts` = start of the smallest bucket touched by the tombstoned rows
  (use the largest bucket width in play, i.e. start of day UTC). Then bucket repair =
  `INSERT INTO agg SELECT ..., <new epoch> FROM base FINAL WHERE chain = ? AND timestamp >= from_ts`.
  **Validity rule used by every `*_v` view:** a contribution with epoch `e` in bucket `b`
  counts iff `e >= max(r.epoch) over reorgs r where r.chain = chain and r.from_ts <= b`
  (0 when none). So stale contributions in repaired buckets vanish, buckets older than
  the fork are untouched, and a later gap-heal writing into an old bucket with a newer
  epoch still adds to the old contributions instead of replacing them.
  `DerivedTable::rebuild_sql` gains an `{epoch}` placeholder; `delete_sql` disappears.
- Readers use `FINAL` on base/side tables and the `*_v` views on aggregates; both are
  already the convention (C5). Nothing else is reorg-aware.

### `purge_range(chain, from, to)` — idempotent, crash-safe, lock-free

The one primitive that removes anything. Implemented in `src/reorg/`; the order of its
steps is binding, because every one of them was moved at least once and put back by a
crash matrix that failed.

1. **Tombstone the overlapping `checkpoints` first.** Not last: a crash after the blocks
   are dead but before the checkpoints are leaves a checkpoint claiming blocks that no
   longer exist, and nothing later notices.
2. Compute `new_epoch` = the chain's max epoch + 1, and the repair window
   `[from_ts, to_ts)` from `timestamp_span` over the range — `from_ts` = start of the UTC
   day of the smallest timestamp, `to_ts` = start of the day after the largest.
3. **Tombstone every block-scoped table in the range, children and side-less bases
   first, the commit marker (`blocks`, or `sol_slots` on Solana) LAST.** That mirrors the
   insert order: while the old marker row is alive, a crash is followed by re-detection
   and a full re-run, and re-running is harmless.
4. **Insert the `reorgs` row, ARMED** (chain, epoch, from_ts, to_ts, detected_at,
   fork_block, to_block, old_head, old_hash, new_hash, depth, rows_tombstoned,
   tombstone_version, reason `reorg` | `gap_heal` | `redecode`). The writer adopts
   `new_epoch` immediately, and re-reads it after any failed purge: otherwise a surviving
   process keeps writing rows the validity rule hides.
5. **Bucket repair** for every `DerivedTable` of every module, at `new_epoch`, over
   `[from_ts, to_ts)`.
6. **Tombstone the commit marker.**
7. **Verify and repair the side tables.** They normally follow through their MVs, but a
   lost view push would leave live orphans for ever, so the purge checks each side table
   for the range and tombstones it directly (`tombstone_sql_where`) until the count is
   zero.
8. **Write the second `reorgs` row, COMPLETED**, and evict cached discoveries from the
   range.

Two rows per purge (armed, completed), deliberately not collapsed: that is what tells the
debris of a FINISHED purge from the leftovers of one that died. A tombstoned orphan heals
only when its `_version` is newer than what a completed purge of that block wrote, so a
restart after a rollback that shortened the chain is a no-op. Audit queries use
`WHERE completed = 1`.

A crash anywhere re-runs the whole thing under a newer epoch; the validity rule makes the
abandoned partial epoch invisible.

**The validity rule is BOUNDED.** A contribution with epoch `e` in bucket `b` counts iff
`e >= max(r.epoch)` over the chain's `reorgs` rows with `r.from_ts <= b AND b < r.to_ts`.
A repair therefore covers exactly `[from_ts, to_ts)` instead of everything from `from_ts`
to now, which matters for a purge deep in history. `epoch_floor_v` is a per-day step
function with explicit segment ends, so every consumer's `ASOF LEFT JOIN ... WHERE
a.epoch >= ifNull(f.epoch_floor, 0)` stays byte-identical (measured: 0.28 s on 10k reorgs
x 1M aggregate rows; the array formulation needed 59 GiB).

Details that are load bearing, each with what goes wrong without it:

- **`timestamp_span` reads ALL row versions, without `FINAL`.** After a crash mid-purge
  the early part of the range is already tombstoned, and a minimum over live rows would
  move forward and leave the first bucket stale for ever. It includes the commit marker's
  own rows: a reorged range of EMPTY blocks has no child row, yet `daily_block_stats`
  needs repair.
- **`timestamp_span` reports the smallest timestamp ABOVE ZERO.** A `timestamp` of 0 is a
  MISSING block time, not a block time of 1970; taking it as the start of the repair
  window would arm the validity rule on every day since the epoch and hide a whole
  chain's aggregates until a rebuild of fifty years finished. `Purger` clamps a 0 it
  still gets to the last day of the range, loudly. **What that costs, deliberately:**
  when a purged range holds both zero and real timestamps the repair starts at the real
  day, so whatever the zero-timestamp rows contributed stays in the day-0 bucket for ever
  and is counted again when the range is re-streamed. A permanently wrong 1970 bucket is
  the accepted price of not hiding every bucket of the chain; the cure is to stop the
  source storing a missing block time as 0.
- **Every module's rebuild SQL excludes the purged block range itself**
  (`{purge_from}` / `{purge_to}`), so a rebuild never depends on seeing tombstones —
  which matters because the commit marker is still alive at step 5.
- **Epochs and the validity rule are per CHAIN, not per module.** Anything that writes a
  `reorgs` row — including `indexer backfill --module X` — must rebuild EVERY derived
  table of every module for the affected buckets, or it silently zeroes the others.
- **A MODULE purge settles only its OWN tables.** Its repair window is the timestamp span
  of the module's rows, which can be far narrower than its block range, so
  `has_orphan_children` must not treat a completed `reason = 'redecode'` row as having
  settled anybody else's tombstones.
- **Gap heal is only crash safe if `has_orphan_children` counts tombstoned rows too** (no
  `FINAL`): once orphans are tombstoned, nothing else marks the unfinished heal.
- **One writer per chain, by role.** Two indexer processes on the same chain are
  unsupported and refused at startup, and so are two `indexer backfill` runs of the same
  module. The lease carries a ROLE and only excludes processes of the same role, so a
  backfill is refused by another backfill while still being allowed next to a live
  `indexer run`. The writer asks the lease before every flush and every purge and refuses
  to write when the lease is lost or its own heartbeat is older than the ttl; a process
  whose heartbeats lapsed stops for ANY other live instance.
- **Checkpoints are an index, deliberately NOT the resume cursor.** Resuming from them
  would skip the one inspection that finds orphan children below the cursor. They are
  compacted (insert-only cover + tombstones, lease-fenced, bounded).
- **A flush spanning more than 90 monthly partitions is split by month**, oldest part
  first, each part complete in itself (commit marker last, own dedup tokens, own
  checkpoint).
- **The queue of flush spans that raced another process's purge is drained
  NON-DESTRUCTIVELY**: a span leaves it only after its purge succeeded, so a transient
  error does not lose it (nothing else asks for those blocks again — their rows are
  stored, so no gap query reports them). It does not have to survive in memory: every
  start re-derives the same spans from the database (`stale_flush_ranges`) as the live
  base rows inside a purge's `[from_ts, to_ts)` whose `epoch` is below that purge's and
  whose `_version` is above its `tombstone_version`, i.e. rows written after the rebuild
  had read its input. Conservative and self-terminating: the rows come back stamped with
  the newest epoch, which no `reorgs` row is above. Both families: the drain is
  `reorg::Purger::purge_queued`, and the query reads the family's commit marker.
- **`indexer backfill` re-reads the chain's epoch before each chunk** and stops loudly if
  the live indexer purged meanwhile.
- **`indexer verify` cross-checks aggregates against base tables per complete UTC day**
  (a doubled aggregate is INCONSISTENT), over the complete days of the GAP-FREE PARTS of
  the range, so that a backfill in progress does not switch the check off. A pending gap
  heal is INCONSISTENT too (the aggregates still count rows the next start removes), and
  a range where no complete day could be compared reads "CONSISTENT, NOT FULLY CHECKED",
  never plain CONSISTENT.

**Accepted trade-off.** The armed `reorgs` row lands before the rebuild, so readers
briefly UNDER-count the repaired buckets. The opposite order would double count; neither
is atomic across aggregates, and under-counting for a moment is the safe side.

**ClickHouse 25.12 landmines, worked around in code:** in
`SELECT * REPLACE (x AS c) ... WHERE c = ..` the `WHERE` sees the REPLACED value, and
`SELECT * REPLACE` with `LIMIT` silently returns no rows — generated tombstones therefore
use positional column lists. Test harnesses must re-issue tombstones until a count says 0
twice in a row and must re-read after an insert: the server misses ~3% of reads issued
right after an acknowledged INSERT, so one zero can be the answer from before the insert.

**No read-your-writes (ClickHouse 25.12, observed on the macOS build).** Right after an
`INSERT` returns, the next query can miss the new part for a few milliseconds when
several writers are active (44-137 misses per 3,200 in the schema engineer's repro; it
heals on the next try). So: (1) before purging, make sure the last flush is visible;
(2) a tombstone `INSERT .. SELECT` can miss freshly flushed rows: re-issue
`tombstone_sql` until `live_rows_sql` returns 0 TWICE IN A ROW, the second read taken
after the retry delay (one zero can be the answer from before this loop's own insert,
which is exactly the case where stopping is wrong; a miss heals on the next try, so two
cannot both be stale) - idempotent, lock-free, bounded, fatal if it never converges; (3) a rebuild never depends on seeing tombstones (it excludes the
purged range itself). The same caution applies to any read that decides what to write.

**Disk hygiene (operator note).** Tombstones and the rows they hide stay on disk until
ClickHouse merges them away; the indexer never issues `OPTIMIZE ... FINAL CLEANUP`.
Volume is negligible (only reorged/orphaned rows). An operator may run a cleanup during
maintenance; it is never required for correctness.

**Retried inserts must not double count.** A timed-out insert that was actually applied
and is retried would fire the MVs twice. Every insert therefore carries a deterministic
`insert_deduplication_token` (table, chain, block span, `_version`), base tables set
`non_replicated_deduplication_window`, and inserts run with
`deduplicate_blocks_in_dependent_materialized_views = 1`; if an insert outcome stays
ambiguous after retries, the affected range is purged (`gap_heal`) rather than trusted.

Gap queries, checkpoint reads and `block_hash` lookups use `FINAL` so tombstoned blocks
count as missing.

The list of block-scoped tables is code, not convention: each data module's own
`BASE_TABLES` + `SIDE_TABLES` (`core::`, `dex::`, `predictions::`, `launchpads::`,
`svm::`), children before the commit marker. A unit test per module asserts every table
in its migrations that has a `block_number` column is listed.

**Gap healing uses the same primitive** (reason `gap_heal`). A gap range may hold orphan children from a
flush that crashed before writing `blocks`. On the first pass after startup, for each gap
range, if any child table has rows in it → `purge_range(chain, from, to)` before
streaming it. This removes the last source of duplicate inserts, which is what makes
incremental aggregates trustworthy.

`tokens` and `dex_pools` metadata are not block scoped and are never purged by a reorg
(a token's name doesn't change with the fork); `dex_pools` rows carry `created_block` and
ARE purged when created inside the purged range.

## 3. Checkpoints

`checkpoints (chain, from_block, to_block, _version)` — one row per contiguous committed
range per flush, written after the commit marker. They are an INDEX of what was
committed, not the resume cursor: the gap query over the commit marker is what a start
resumes from, because resuming from checkpoints would skip the inspection that finds
orphan children below the cursor. The same query is the first-pass repair and is what
`indexer verify` runs. `purge_range` tombstones overlapping checkpoints FIRST (section 2,
step 1), insert-only like everything else, and they are compacted so the table stays
bounded.

On Solana the tiling of these ranges IS the resume oracle, because a slot with no row is
usually a skipped slot rather than a gap; see section 14.

## 4. Token metadata without trusting one RPC

eth_call is unavoidable (name/symbol/decimals live in contract state; HyperSync serves no
calls). So the RPC must never be able to block or lose anything:

- **Off the commit path.** The pipeline only does `TokenWorker::discover(HashMap<Address,
  TokenStandard>)` — non-blocking, bounded, drop-on-full (safe because of backfill). The
  worker resolves in the background and inserts `tokens` rows itself through a small sink
  trait, then marks the cache.
- **DB-driven backfill.** A periodic query finds `token_address` values present in
  transfer tables (and `dex_pools`) with no `tokens` row and feeds them to the worker. An
  RPC outage of any length self-heals; nothing depends on having seen the transfer live.
  Expose as a trait (`MissingTokenSource`) so the pipeline wires ClickHouse in.
- **`--rpc` defaults to `auto`** (owner decision: DEX analytics are on by default and
  are meaningless without token decimals). Unset/blank = `auto`; `none` disables RPC
  features explicitly; `https://mine,auto` = own endpoint first, public fallback
  (recommended for production). Discovery can never fail startup. README must state
  that the default fetches `https://chainid.network/chains.json` and that public
  endpoints are best-effort.
- **Multi-endpoint failover.** `--rpc` takes a comma-separated list; per-endpoint circuit
  breakers, rotate on failure, chain-id checked per endpoint. `--rpc auto` discovers
  public endpoints for the chain id from `https://chainid.network/chains.json` (filter
  out URLs containing `${`, non-https, websockets) — zero-config, best-effort.
- Unresolvable tokens (reverts/garbage) still get a row so analytics can tell "checked,
  nothing there" from "not checked yet".

The existing `TokenResolver` (two-phase `resolve_new`/`mark_stored`, Redis, LRU,
redaction) stays as the engine underneath.

## 5. DEX analytics — chain agnostic, DEX agnostic

Principle: **decode by event family from `logs`, never by router/factory registry.** A
fork of Uniswap V2 on a chain nobody has heard of works on day one. Module `src/dex/`,
**ON by default** (owner decision; opt out with `--no-dex` / env `NO_DEX=true`), pure function `decode(&[DatabaseLog]) -> DexRows` inside transform.

Families (topic0 + shape validated; wrong shape → not decoded, never panic):
`uniswap_v2` (PairCreated, Swap, Sync, Mint, Burn), `solidly` (its own Swap/Sync
variants), `uniswap_v3` (PoolCreated, Swap, Mint, Burn; PancakeSwap-v3 and Algebra Swap
variants), `uniswap_v4` (Initialize, Swap, ModifyLiquidity — emitter is the PoolManager,
pool = `bytes32` id), `balancer_v2` (PoolRegistered, TokensRegistered, Swap — tokens are
in the event), `curve` (TokenExchange / TokenExchangeUnderlying — coin indices).

Tables (all block scoped, §1 rules):
- `dex_pools`: chain, pool_id `FixedString(32)` (address left-padded, or V4 id), emitter,
  factory, protocol, token0, token1 (+ `tokens Array` for multi-asset), fee, tick_spacing,
  created_block, source (`event` | `rpc`).
- `dex_swaps`: chain, block_number, timestamp, transaction_hash, log_index, pool_id,
  protocol, sender, recipient, `amount0 Int256`, `amount1 Int256` (pool-relative, signed:
  positive = into the pool), token_in/token_out + amount_in/amount_out when the event
  itself carries them (Balancer, V4 via pool cache if known) else zero, sqrt_price_x96,
  liquidity, tick.
- `dex_liquidity`: mint/burn/sync/modify events, same conventions.
- **Never block on RPC to decode a swap.** Pool tokens come from creation events; pools
  first seen mid-history (partial sync) are resolved by a background worker
  (`token0()/token1()/fee()` via the same RPC pool as tokens) into `dex_pools`
  (`source = 'rpc'`). Token resolution for swaps happens at query/aggregation time via
  join/dictionary on `dex_pools`, not at decode time.
- Aggregates (`DerivedTable`s, so reorg-safe): per-pool candles 1m/1h/1d (open/high/low/
  close via `argMinState/argMaxState` on (block_number, log_index), volume0, volume1,
  swap count, unique traders), per-pool and per-protocol daily volume. Views join
  `dex_pools` + `tokens` to present decimals-adjusted amounts and per-token volume.
- USD: `quote_tokens (chain, token, kind 'stable'|'native')`, user populated (document
  it; ship no chain-specific seed data). Views: `dex_swaps_usd_v` values a swap when one
  side is a stable, or a native whose price comes from the native/stable pools'
  candles. Anything unpriceable has NULL usd, never 0.

## 6. Migrations

`migrations/NNNN_name.sql`, embedded in the binary at compile time, applied in order at
startup and via `indexer migrate`; `schema_migrations (version, name, checksum,
applied_at)`; refuse to start if an applied migration's checksum changed. Statement
splitting must survive `;` inside strings/comments. No more
`docker-entrypoint-initdb.d`. The database name comes from the URL (no hard-coded
`indexer.` prefix in DDL). Reserved numbers: `0001` core tables, `0002` read-path side
tables, `0003` core aggregates, `0004` checkpoints + reorgs, `0010`–`0019` DEX.

## 7. Observability

`--metrics-addr` (default off): Prometheus text endpoint + `/healthz` + `/readyz`.
Metrics: head, indexed height, lag (blocks, seconds), rows/s per table, flush latency
histogram, flush retries, channel fill, token queue depth / cache hit rate / rpc breaker
state, reorgs total + last depth, purge duration. Module `src/metrics/` with a cheap
clonable handle; no metrics crate lock-in leaking into other modules.

## 8. Field selection

Request from HyperSync only what a column stores. Dropping `logs_bloom` etc. from the
schema drops them from the query.

## 9. Traces and contracts (scope decision)

**Traces are removed entirely**: no `traces` table, no `traces_by_tx`, no trace model,
no trace query to HyperSync, no `--traces` flag. DEX analytics need neither traces nor
deployer data (pools come from factory events, tokens from transfers, liquidity providers
from `dex_liquidity.tx_from` — the event `sender` is usually a router, never use it for
attribution).

**`contracts` is a VIEW**, not a table: it selects from `transactions` where
`contract_created` is set and the transaction succeeded (`contract_address`, `creator` =
`from`, `transaction_hash`, `block_number`, `timestamp`). Nothing to insert, purge or keep
consistent. It lists directly deployed contracts only; factory-created contracts are
out of scope by design. There is NO contract-deployment aggregate (the data is partial by design, so a statistic over it would mislead).

## 10. Prediction markets — display-first

Module `src/predictions/`, ON by default like DEX (opt out with `--no-predictions` /
env `NO_PREDICTIONS=true`), same shape as `src/dex/` (pure decoders
by event family, tables under the §1–§2 storage rules incl. tombstones + epochs,
aggregates as `DerivedTable`s, background resolver off the commit path, re-decodable
from stored `logs`). Migrations `0020`–`0029`.

**The tables are designed backwards from the screens of a trading UI.** Each screen must
be servable by ONE cheap query against a view, with no client-side joins or math:

| Screen | Must show | Served by |
|---|---|---|
| Market list / search | title, category/tags if known, outcomes with **current price = implied probability**, 24h volume, total volume, open interest, trader count, end date, status (open / resolved / disputed), venue | `prediction_markets_v` (one row per market, outcome arrays) |
| Market page header | same + resolution source/oracle, creation time, winning outcome + payout vector once resolved | `prediction_markets_v` |
| Price chart | per-outcome candles 1m / 1h / 1d (OHLC of probability 0..1, volume in collateral units, trades) | `prediction_candles_*_v` |
| Trades tape | time, outcome, side (buy/sell from the taker's view), price, size, collateral amount, trader, tx hash | `prediction_trades` by (market, time desc) side table |
| Holders / top positions | per outcome: holder, net position, avg entry price | `prediction_positions_v` |
| Portfolio (a wallet) | open positions with avg entry, current price, unrealised PnL; realised PnL; redeemable winnings; trade history | `prediction_positions_v`, `prediction_trades` by trader |
| Leaderboard | volume and realised PnL per trader per period | daily aggregate |

Principles: prices are stored as the raw amounts AND exposed as Float64 probability in
views; collateral is decimals-adjusted in views via `tokens`; one normalised `market_id`
(`FixedString(32)`) per venue-market with outcome index → outcome token id mapping;
multi-outcome / negative-risk groupings are first class (an "event" groups markets);
everything is source-agnostic (`venue`, `protocol` columns) so a non-EVM venue could be
fed by an API adapter later. What is NOT on chain (order book depth, off-chain titles)
is explicitly out of scope — record what would be needed and where it lives; never fake it.

## 11. Token launchpads (EVM)

Module `src/launchpads/`, ON by default (`--no-launchpads`), same shape and storage rules
as `src/dex/` and `src/predictions/`. Migrations `0030`–`0039`.

**Why EVM first, and why these venues.** 73.2% of 30-day launchpad fees ($187.7M of
$256.5M) and 49.3% of bonding-curve volume sit on EVM chains HyperSync serves, and two
verified event families cover ~86.5% of that. Volume (49%) is the like-for-like number:
the fee share is flattered because Pons' fees include post-graduation Uniswap V4 fees
while pump.fun's exclude PumpSwap, and because Robinhood Chain's boom is ten weeks old
and rode a gas waiver that ends around the end of September 2026.

- **Families first:** `pons_v2` (factory `0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e` on
  Robinhood Chain, id 4663; `TokenLaunched`, `CurveBuy`, `CurveSell`, `PoolGraduated`;
  $128.6M/30d = 68.5% of the EVM-reachable total, and Pez Family is byte-identical) and
  `flap_portal` (one proxy per chain on BSC, Robinhood and Monad; $33.8M/30d, the richest
  events). Both read in verified source and checked against live logs.
- Then **launch attribution only** for venues that launch straight into Uniswap V3/V4
  pools the spot decoders already capture (Pons V1, Clanker, NOXA, o1, LetsCash, Zora,
  ...): one launch row each joined on the pool id, no curve decoder — their trades
  already arrive through `dex_swaps`.
- **Not built, and why:** the tail below ~$10M/month (Believe, boop, Heaven, time.fun,
  Moonshot, four.meme, Clanker), which is skipped until one of them grows; chains we
  cannot serve or have never exercised (Ignix on X Layer, SunPump on Tron); and Binance
  Alpha, which is a swap-fee contract rather than a launchpad.
- **Tables are chain neutral from day one** so a non-EVM pipeline could fill them later:
  `launchpad_tokens` (token, creator, venue/family, name/symbol when the event carries
  them, curve parameters, launch tx), `launchpad_trades` (trader, side, token amount,
  quote amount, price, fee, curve progress), `launchpad_graduations` (destination DEX +
  pool id — the join key into `dex_pools`/`dex_swaps`/candles), `launchpad_creator_fees`.
- **Display-first, like §10.** Screens: new-launch feed; token page (curve progress,
  price chart, trades tape, holders); graduation feed; creator page (history, how many
  of their launches graduated / died — serial-rugger signal); sniper view (buys in the
  launch block, bundled buys, dev holdings, top-holder concentration at graduation —
  only what is computable from events + ERC-20 transfers); post-graduation performance
  via the existing DEX candles. One cheap query per screen, cookbook in the module README.
- **Front ends are not venues.** fomo, GMGN, Axiom, Terminal, Maestro, Trojan and the
  rest have no contracts of their own, and their volume OVERLAPS venue volume — GMGN
  alone took $51.1M of fees in 30 days that are already counted at the venues.
  Attribution is by fee-recipient / router address in a user-populated
  `launchpad_frontends` table that ships no rows. Never add front-end volume to venue
  volume. Router matching is the reliable half: native-ETH fee payments inside a router
  call are internal transfers, which this indexer does not store.
- Forgery rules from the DEX review apply: curve trades are valued only when
  corroborated by the token/quote ERC-20 (or native value) movement in the same tx.
- **Attribution columns are three different things and stay separate.** `trader` is the
  event's beneficiary, which is often neither `tx.from` (forwarders, routers, bots) nor
  the event's `buyer`; `creator` is not `tx.from` either (whitelisted launchers, Flap's
  `VanityTokenCreated.beneficiary`, Clanker deploying on someone's behalf). `tx_from` and
  `payer` are kept as their own columns.
- **A graduation is a row that joins, not a second copy of the trading data.**
  `launchpad_graduations` carries the destination protocol and pool id, which is the join
  key into `dex_pools` / `dex_swaps` / the candles; post-graduation performance is read
  from the DEX candles and never re-decoded here. Curve trades go to `launchpad_trades`;
  the trades of a token born directly in a pool stay in `dex_swaps`, and the launchpad
  module contributes only the launch row (`launch_kind` = `curve` | `direct_pool`). A
  graduation can happen INSIDE a user's buy — on Pons one transaction holds the router
  buy, `PoolGraduated`, the V4 `Initialize` and the first V4 `Swap` — so the launch and
  graduation rows are assembled per TRANSACTION, not per log.
- Two things the data cannot give, stated so nobody looks for them: Uniswap V4 hook fees
  are taken outside the pool's `fee` field, so DEX-derived "fees" understate what a
  trader paid; and launches are spammy by design (13,658 in 24h on one venue, ~1%
  graduate), so the feed needs server-side filters and metadata resolution must not fire
  once per launch.

## 12. Code layout - ONE structure: feature modules

The codebase must not mix "by layer" (`db/models`, `utils`) and "by feature" (`dex/`,
`predictions/`). **Feature modules win.** Rule: *a dataset owns everything about itself;
infrastructure owns nothing about any dataset.*

This is the layout as it now is (the refactor landed 2026-09-19; `tests/layout.rs` keeps
it from rotting back):

```
src/
  configs/        CLI + env parsing
  source/         the sources, ingest only: mod.rs (the seam), evm.rs, solana.rs
  pipeline/       orchestration: stream -> transform -> writer, module seam, workers
  db/             INFRASTRUCTURE ONLY: client + insert path, migrate, schema helpers
                  (tombstone_sql...), ranges/checkpoints, the DerivedTable TYPE, format.rs
                  (ClickHouse serializers). No row models, no dataset constants.
  reorg/          fork-point search + purge orchestration (traits, no ClickHouse)
  tokens/         token metadata worker + RPC endpoints (models.rs = the `tokens` row)
  metrics/
  core/           DATA MODULE: blocks, transactions, logs, withdrawals, ERC-20/721/1155
                  transfers; also RowBatch and `store`, the EVM flush
  dex/            DATA MODULE
  predictions/    DATA MODULE
  launchpads/     DATA MODULE
  svm/            DATA MODULE of the second chain family (the `sol_*` tables)
  fleet/          `indexer fleet`: the supervisor, one task per chain (§15)
  admin/          the control panel served by the fleet process (§15)
```

`fleet/` and `admin/` are not DATA MODULES: they own no table of chain data
(only `fleet_chains`, which is desired state, not indexed data), so the
standard data-module file set does not apply to them. They do follow
everything else - a `README.md` of their own, their tests next to them - and
`tests/layout.rs` checks that much.

Every DATA MODULE has the same files and the same public surface, so the pipeline seam
treats them uniformly: `mod.rs` (API + `BASE_TABLES`, `SIDE_TABLES`, `*_DERIVED`),
`models.rs` or `models/` (row structs), `events.rs` (keccak-checked signatures),
`decode.rs` (pure, no I/O: source rows/logs -> module rows), `derived.rs`, optional
`worker.rs`/`resolve.rs`, `integration_tests.rs`, `README.md`; and owns a migration range
(`0001-0009` core, `0010-0019` dex, `0020-0029` predictions, `0030-0039` launchpads,
`0040-0049` svm, `0090+` cross-module).

Two things the tree above does not show, and why:

- **`core` has no `integration_tests.rs`.** Its server-backed coverage is
  `db::integration_tests`, which drives the INFRASTRUCTURE - insert path, tombstones,
  the validity rule, epochs, missing ranges - and core is the only dataset that write
  path has rows for. The two share one fixture; splitting them would duplicate it, not
  separate two suites.
- **`db` still names one table, `blocks`**, in `ranges` (resume cursor, gap scan) and in
  `Database::{block_hash, stored_head}`: it is the EVM chain's commit marker and this
  section assigns ranges and checkpoints to `db`. The `blocks` ROW type and everything
  written into it are `core`'s.

## 13. Chain-neutral analytics tables (owner decision 2026-09-18: YES, now)

Applies to the analytics DATA MODULES only - `dex_*`, `launchpad_*`, `prediction_*` -
NOT to the EVM `core` tables. The point is that one Solana pipeline can fill the same
tables as fifty EVM chains, so a screen like "curve trades -> graduation -> AMM candles"
is one query over one `dex_swaps` and one `launchpad_trades` whatever chain it happened
on.

- Identity columns (pool, token, trader, creator, emitter, factory, recipient, holder,
  `tx_from`, `tx_to`...) are `FixedString(32)`: EVM address = 12 zero bytes + 20 address
  bytes (same convention `dex_pools.pool_id` already uses); Solana pubkey = 32 raw bytes.
- Transaction id: `tx_id String` (raw bytes: 32 on EVM, 64 on Solana). Never in a sort key.
- Position key: `(chain, block_number, tx_index, ordinal)`; the column NAME
  `block_number` stays (purge/tombstone/checkpoint code keys on it) and holds the slot on
  Solana. EVM: `tx_index` = transaction index, `ordinal UInt64` = log index.
- `chains (chain UInt64, name String, family LowCardinality(String) 'evm'|'svm')`
  registry (migration `0006_chains.sql`, user/indexer populated) so views and UIs know how
  to print an id: `concat('0x', lower(hex(substring(id, 13))))` vs `base58Encode(id)`.
- Shared Rust helpers live with the serializers: `SerId32` (alloy `Address` <-> 32 bytes
  left-padded), `SerTxId` (raw bytes). Every module uses the same ones.

## 14. Solana (owner decision 2026-09-18: GO)

One binary, one database: `indexer run --chain solana`, or one more `--chain solana` in a
fleet. **Analytics-only, program-filtered** - no wallet history, no chain-wide transfers,
and the schema and READMEs must say so.

**Not a sister project, and not a workspace split.** The owner's screen is "curve trades
-> graduation -> AMM candles on one chart", which is one query over one `dex_swaps` and
one `launchpad_trades`; and the shared layer (`db/`, `reorg/`, `metrics/`, the migrator,
`DerivedTable`) was already chain-agnostic and needed a second caller, not an
abstraction.

- Layout (section 12 vocabulary): a second `source` (Envio Solana HyperSync,
  `hypersync-client-solana`), a small `svm/` core data module (`sol_*` tables: slots as
  the commit marker, the matched transactions/instructions actually needed), and the
  SAME `dex/` and `launchpads/` modules with a second decoder front end writing the same
  chain-neutral tables (section 13). `db/`, `reorg/` (detector variant on `parent_slot`;
  skipped slots are normal), migrator, metrics, `DerivedTable`, tombstones + epochs reuse.
- Decoding is PER INSTRUCTION SUBTREE, never per transaction net balance. Two always-on
  layers: a generic token-movement decoder (every venue: price, size, real trader) and
  per-program decoders for venues with a public format (fees, pool state). SPL/Token-2022
  transfer instructions must be selected in the same query (a matched instruction does
  not return its children). Aggregators/routers are attribution, never venue volume.
- The pool key is the venue's own pool account and NEVER the owner of its vaults: five
  of the ten streamed venues (Raydium AMM v4 and CPMM, Meteora DAMM v2 and DBC, Raydium
  LaunchLab) own every pool's vaults with one program-wide PDA, which is also what the
  movement layer finds as "the common counterparty". A fill whose pool cannot be named
  from the venue's event or from the instruction's account metas is written with a zero
  `pool_id` and excluded from the pool-keyed aggregates, never keyed on the authority.
- `trader` is the account the venue's own event names, and the transaction's fee payer
  only where no event names one - on Solana the fee payer is very often a relayer or a
  bot, and the candle `traders` series counts distinct traders.
- One instruction can execute SEVERAL fills (Orca `two_hop_swap`, Raydium CLMM
  `swap_router_base_in`). Each is its own row, told apart by a hop sub-index in the low
  bits of `ordinal`.
- A transaction whose logs the validator truncated (`has_dropped_log_messages`) is never
  enriched from those logs: for the venues whose event exists only as a log line the row
  keeps `movement` confidence and the case is counted.
- Chain id for Solana: 1399811149 (no standard exists; recorded in `chains`).
- Resume on Solana is the CHECKPOINT TILING and not a gap query over the commit marker
  (a slot with no row is usually a skipped slot, not a gap), but section 2's rule still
  binds: **the data decides.** Before a hole in the tiling is streamed again, it is
  purged whenever it still holds anything - orphan children, or live `sol_slots` rows
  whose checkpoint insert never landed. The tiling is read with no `LIMIT` and compacted
  after a covered pass, exactly as on the EVM path.
### 14.1 Which programs, and why only those

A program-filtered stream of about two dozen program ids, not the whole chain. The value
is concentrated: of Solana's $78.77B of 30-day DEX volume (measured 2026-09-19), the top
5 venues are 62.5%, the top 9 are 79.3%, the top 14 are 91.5% and the top 17 are 95.6%.
"Index everything" is ~150M transactions a day — roughly 30x the row rate the EVM
pipeline is tuned for — spent almost entirely on bot spam.

| Venue | 30d share | What the decoder has |
|---|---|---|
| PumpSwap | 23.1% | IDL + self-CPI event |
| BisonFi | 11.8% | proprietary AMM, no event |
| Orca Whirlpool | 10.0% | IDL + `Program data:` |
| Raydium v4 / CPMM / CLMM | 9.5% | IDL + logs |
| Meteora DLMM | 8.0% | IDL + self-CPI event |
| Manifest | 5.5% | source + `Program data:` |
| Tessera V | 4.1% | proprietary, free-text log only |
| Scorch | 3.8% | proprietary, no event |
| HumidiFi | 3.5% | proprietary, obfuscated data |
| pump.fun (curve) | 3.2% | IDL + self-CPI event |
| QuantumAMM, GoonFi (+v2), AlphaQ, Deriverse, SolFi V2, Aquifer, Byreal, ZeroFi, Quay, Obric, Whalestreet | ~13% combined | proprietary AMMs |
| Launchpads: Raydium LaunchLab, Meteora DBC, Meteora DAMM v2 | small by volume, large by launch count | IDL |
| SPL Token + Token-2022 transfers, Metaplex Token Metadata | — | not venues: the movement layer and the token names need them |

The exact ids are in `src/svm/programs.rs` and `src/svm/registry.rs`, which is where they
belong; a program that moves two mints and is not registered goes to the unclassified
table rather than being guessed into a venue.

**Routers and aggregators are attribution, never venue volume.** Jupiter v6, DFlow, OKX
Swap, Titan and the rest are $31.41B of 30-day volume — 40% of the chain's — and adding
them to venue volume would double count all of it.

### 14.2 What Envio's Solana HyperSync does and does not serve

Measured 2026-09-19. These numbers are the reason for several rules above.

- **Earliest served slot: 391,000,000** (`block_time` 2026-01-03 07:37 UTC), about 8.5
  months. A range entirely below it returns empty **with `next_slot` not advancing**, so
  a resume loop must treat "next_slot did not increase" as a stop condition — and a
  `--start-block` below it is refused at startup.
- **Rate limit: a flat cost of 1000 per query, 30 queries per 60 s on the free token.**
  The cost does not depend on the query: a 378-byte response and a 46.2 MB response both
  cost 1000. `remaining` counts BUDGET UNITS, not requests, so the follower divides it by
  `cost` rather than hard-coding 30. Each chain endpoint has its own pool, so Solana does
  not eat the EVM budget. Paid tiers: Starter $70/month = 100 req/min, Pro $480/month =
  1,000 req/min.
- **`GET /height` is unauthenticated and unmetered**, so discovering that nothing
  happened is free.
- **Arrow is 43% of the bytes of JSON for the identical query and costs the same 1000**
  (19.9 MB against 46.2 MB), which is why the follower uses it. It is also the only form
  that surfaces the `x-ratelimit-*` headers.
- **~35 slots per query is the planning number**, bounded by the server's ~5 s execution
  budget, and only once `max_num_blocks` / `transactions` / `instructions` /
  `account_activity` are ALL raised — with only one of them raised the production query
  returns a single slot. Never assume the number: follow the server's `next_slot`. The
  client's Solana `StreamConfig` defaults (`response_bytes_ceiling` 500,000 against a
  measured 0.50 MB/slot) converge on one slot per batch and must be raised, with a test.
- **Join mode:** a matched instruction returns itself, its parent transaction and ALL
  `log` and `account_activity` rows of that transaction — but NOT its child or sibling
  instructions. That is why the SPL / Token-2022 transfer selection is in the same query.
- **No live-tail mode and no reorg handling**, which is why the head follower is ours.
  The commitment level is undocumented; measured just behind RPC `finalized` (4–19 slots)
  and 33–49 behind `processed`. End-to-end lag at the chosen cadence is 13.6–20.0 s from
  execution. The wire format is not frozen — **pin the client crate version**.

### 14.3 History before the head: the plan, and why it is parked

**Decided: not now.** A chain starts at the head on its first launch and going back is an
explicit choice; no command streams a range below the coverage floor yet (section 16).
The plan is kept so the decision does not have to be re-derived.

The job is 57.3M slots (8 months 16 days) = ~1.64M queries and ~28.6 TB of Arrow.

| Option | Calendar | Money |
|---|---|---|
| Envio free tier, head paused | 48.3 days | $0 |
| Envio free tier, head followed in parallel | 75.8 days | $0 |
| **Envio Starter — the preferred one** | **12.2 days, or 13.4 with the head followed** | **$70 once, then back to free** |
| Envio Pro | not achievable — the budget buys throughput one node cannot consume | $480/month |
| Old Faithful + Jetstreamer | 10.5 days at a saturated 1 Gbps, and **113 TB** | $0 |

Starter wins because the same 8.5 months is 28.6 TB through Envio and 113 TB through Old
Faithful (Jetstreamer has no server-side filtering, so narrowing to our programs saves no
bandwidth at all), and because Envio needs no new code — same client, query, decoder and
writer. Two conditions before any money is spent: **measure `svm::decode` rows/s on the
recorded fixtures first** (Starter implies 146,000 rows/s; if the decoder does 50,000 the
calendar is ~34 days whatever the tier), and **sweep BACKWARDS from the head**, because
recent months are what a chart needs first and an interrupted backward sweep still leaves
a contiguous window.

Old Faithful stays in the drawer for one thing only: everything before 2026-01-03, which
Envio cannot serve at any price. That is a second ingest path and ~250 TB, i.e. a
separate project, not a flag.

### 14.4 Cost of ownership

~12 GB/day and ~4.3 TB/year with slim side tables (~21 GB/day and 7.5 TB/year if side
tables are full row copies) — 26x the measured Base swap rate, and 2.5–10x all EVM chains
combined. Plan on 8 TB of NVMe, 64–128 GB of RAM and 16 cores for year one. Three levers
exist if that is too much, none of them taken: a TTL on raw swaps of about six months
while the candles are kept for ever (~2.2 TB steady state); side tables slim rather than
row copies; and writing `sol_transactions` only for transactions that contain a venue
instruction.

## 15. One process, many chains: fleet mode and the control panel (owner request 2026-09-19)

**Where the code stands.** `indexer run --chain N` is one chain per process. Nothing in the
chain pipeline is process-global (the entry points are `pipeline::run(config)` and
`pipeline::solana::run(config)`; leases, versions, epochs, caches and metrics are per run),
so several chains CAN share one process; what is missing is a supervisor, a way to stop one
chain without stopping the process, a status surface, and the panel.

**`indexer fleet`** - a new subcommand, `run` stays as it is.
- A supervisor owns one tokio task per chain; each task calls the SAME `run_with` the single
  chain command uses, so every safety property (lease + fencing per chain, tombstones, epochs,
  dedup tokens) is unchanged. Migrations run once at start, not per chain.
- One chain failing never takes the others down: the supervisor restarts it with exponential
  backoff (cap 5 min) and records the last error. A lease held by another process is a state
  ("running elsewhere"), not an error loop.
- Stop = the graceful path that ctrl-c takes today (flush, release the lease), per chain,
  through a cancellation handle instead of the process signal. The panel offers NO destructive
  action: no purge, no data removal, no schema change.
- Desired state lives in ClickHouse, table `fleet_chains` (migration 0007:
  `chain`, `desired` running|stopped, `settings` JSON of the per-chain `run` options,
  `_version`; ReplacingMergeTree, PARTITION BY tuple()). The supervisor's memory is the
  authority while it runs and the table is read once at start (no read-your-writes on
  ClickHouse). Adding a chain needs only its id: every `run` default applies
  (`--rpc auto`, DEX on, two-provider agreement).
- Shared budgets: one HyperSync token serves all chains, so the supervisor owns one rate
  budget per provider (EVM HyperSync; Solana 30 queries/min on the free tier) instead of one
  per chain. Memory: batch sizes are per chain; the fleet caps the sum (`--fleet-max-inflight-mb`).
- Live state per chain (in memory, fed by the pipeline through a small `StatusSink`):
  state (starting, backfilling, following, stopped, failed, running elsewhere), stored head,
  chain head, lag in blocks and seconds, blocks/s, last flush time and duration, reorg count
  and deepest reorg, token/DEX worker queue depths, last error with time. Chains indexed by
  OTHER processes show up read-only from `indexer_instances`.
- Metrics: one `/metrics` for the whole fleet, every series labelled with `chain`.

**Control panel** (`src/admin/`), served by the fleet process on `--admin-addr`
(default `127.0.0.1:8090`; off unless a password is set).
- One embedded HTML page (no build step, no CDN, no external request) + a small JSON API:
  `GET /api/chains`, `POST /api/chains` (add), `POST /api/chains/{id}/start|stop|restart`,
  `PATCH /api/chains/{id}` (settings, applied on the next start), `GET /api/chains/{id}/events`
  (recent errors, reorgs, restarts).
- Password: `ADMIN_PASSWORD` (env only, never a flag - flags leak into `ps`); kept as a salted
  hash in memory, compared in constant time. Login form -> random 256-bit session token in an
  `HttpOnly; SameSite=Strict` cookie (`Secure` when behind TLS), 12 h idle expiry, sessions in
  memory. Login attempts are rate limited per address (5 / minute, then backoff). State-changing
  requests must carry the session AND a same-origin `Origin`. Secrets (HyperSync token, RPC
  URLs with keys, database password) are never sent to the browser - settings show them
  redacted (`tokens::redact`).
- No TLS inside the process: binds to localhost by default; for remote access put it behind a
  reverse proxy with TLS or an SSH tunnel (README says how). Binding to a public address without
  `--admin-allow-remote` is refused.
- HTTP stack: `axum` (the hand-rolled metrics server stays for `run`); a security-facing
  surface with bodies, cookies and routing is not the place for a home-made parser.

**The rules that differ from the sketch above, and are the binding ones.**
`src/fleet/` and `src/admin/` are the implementation, `src/fleet/README.md`
and `src/admin/README.md` the reference. Items 6 to 10 are the five that an
independent security review of `src/admin` (2026-09-19, no blocker, five
majors, ten minors, all fixed) changed.

1. **No shared EVM query budget.** The Solana one is built (one
   `pipeline::solana::Budget` per process, handed to every Solana chain).
   The EVM path streams through `hypersync_client`'s own `stream()`, which
   issues and paces its requests internally: the process never sees "a query
   is about to be sent", so there is no seam to hold one back. Giving it a
   budget means replacing the streaming client with manually paged `get()`
   calls and re-proving ordering, rollback guards and throughput against
   them - a week of work on the hot path, not a day, and Envio publishes no
   per-minute EVM quota to respect. Left out on purpose; the memory half of
   "shared budgets" (`--fleet-max-inflight-mb`, split over the running
   chains) IS implemented for both families.
2. **`indexer fleet --chain <id>` (repeatable)** adds a chain the
   `fleet_chains` table does not list yet. A fresh database has an empty
   table and no way to reach the panel's "add" button otherwise; after the
   first start the panel is the place to add chains.
3. **The per-chain settings parse has clap's environment fallbacks
   removed.** A chain needs only its id and every `run` default applies, so
   nothing may be filled in behind the owner's back. With the fallbacks left
   in, clap would take anything not named on the generated command line from
   the process environment, and a `START_BLOCK` in a compose file would
   silently apply to every chain in the fleet - including chains added
   months later from the panel. `indexer run` keeps every environment
   fallback it has.
4. **No separate route for the settings vocabulary.** `GET /api/chains`
   returns the field list (name, kind, label, help, secret) next to the
   chains, so the page's form and the CLI's validator cannot drift and the
   panel needs one request instead of two.
5. **`Secure` on the session cookie is a flag** (`--admin-secure-cookie`),
   with `--admin-trust-forwarded-proto` as the opt-in for believing a
   proxy's `X-Forwarded-Proto`. The process cannot otherwise know it is
   behind TLS, and a header any client can set must not decide it.

6. **The panel's editable settings are an ALLOW-LIST, and endpoints are not
   on it.** Redaction is not enough on its own:
   `--hypersync-token` is process-wide and is attached to whatever
   url a chain is configured with, so a panel that could set
   `--hypersync-url` could send the token to any host, reach the indexer
   host's private network, and - since `verify_chain_id` merely warned when
   an endpoint failed to answer - feed the indexer fabricated blocks under a
   real chain's id. Endpoints and credentials are now process-only and shown
   read-only; the start block, end block and `--new-blocks-only` are out too
   because they move the section 16 coverage floor. What is left is
   behaviour while running. `configs::fleet::NOT_PANEL_EDITABLE` records the
   reason for every excluded flag and a test fails when a new flag is
   classified as neither.
7. **`Host` is validated before routing** (`--admin-host <name>`,
   repeatable; 421 otherwise). The same-origin check compared two headers
   the client sends, which said nothing about which server was addressed:
   classic DNS rebinding against a loopback-bound panel.
8. **The panel has its own accept loop** (`src/admin/server.rs`) with a
   connection cap and header-read / request / idle timeouts. `axum::serve`
   has none, and the sockets belong to the process that indexes every chain.
9. **`ADMIN_PASSWORD` has a 12 character minimum**, below which the panel
   does not start.
10. **The login throttle decays and is bounded**, and `X-Forwarded-For` is
    believed only from `--admin-trusted-proxy <ip>`. As written, an attacker
    who failed five times every quarter of an hour kept the OWNER out
    indefinitely - and behind a proxy every client shares one address.

Two additions worth recording: the pipeline's `StatusSink`
(`src/pipeline/status.rs`) is two methods called on a state CHANGE and on a
retried failure - everything else the panel shows is read from the
`metrics::Metrics` handle the chain already keeps; and `lease::acquire`
now fails with a typed `LeaseHeldElsewhere`, which is what lets the
supervisor treat "running elsewhere" as a state instead of matching on a
message.

## 16. Coverage floor: one consistent window, live data first (owner decision 2026-09-19)

The product promise is "gap-free and consistent from a known date to now, everything kept",
not "all of history". History is fetched only where consistency needs it.

- **Default start on EVM = one year back from the chain's first start here.** With no
  `--start-block` and no `--start-date`, the first `run`/`fleet` start of a chain resolves
  "now - 365 days" to a block (binary search over block headers by timestamp) and PERSISTS
  it as the chain's coverage floor. A year is not a round number picked for looks: it is
  what makes "all-time", "last 12 months" and every year-on-year figure a real answer
  rather than an artefact of when the indexer happened to be started. It is measured from
  the newest block the source HAS, or from now, whichever is earlier, so an archive that
  is behind cannot quietly give you less than a year.
- `--start-date YYYY-MM-DD` and `--start-block N` override the default **on the first
  start only**; `--new-blocks-only` means "start at the head" on either family. Note that
  `--start-block 0` is indistinguishable from "not given" and therefore means the default.
- **Solana default = the head on first launch** (live first). Envio serves history from
  2026-01-03 and the free tier is slow; going back is an explicit choice, never a default.
- **From then on the floor is a fact about the data, not a setting.** It does not roll
  forward; a restart, a different flag value and the control panel all keep it exactly
  where it is, loudly. Moving it EARLIER is an explicit `indexer backfill`; moving it
  LATER is refused on every path, because data is never dropped and a higher floor would
  be a claim the stored rows contradict.
- **The floor lives in its own table, `chain_coverage` (migration 0008)**, insert-only,
  first writer wins — NOT in columns on `chains`. The two answer different questions and
  have different writers: `chains` is a naming registry, user populated, never touched by
  a running indexer, read by views to know whether to print an id as hex or base58, and it
  ships no rows. The floor is written by the indexer at its own first start and is per
  DEPLOYMENT — the same chain id in two databases can honestly have two different floors.
  Putting it on `chains` would have made every row of a naming registry half empty and
  coupled "we know what to call this chain" to "we know what this database promises".
- `coverage_v` exposes, per chain, the floor and the contiguous stored head; `indexer
  verify` prints "gap-free from DATE (block N) to DATE (block M)" as its first line, or
  says exactly what is missing. The control panel shows the same line per chain.
- Readers must treat "all-time" numbers as "since the floor"; READMEs and view comments
  say so.
- **The one dataset that needs older data to be correct: prediction markets.** A market
  created before the floor has no question/outcomes and its open interest can go negative.
  Fix: a registry-only history pass (log-filtered by the trusted registry/exchange
  addresses) that stores market metadata and split/merge/redeem events but **no trades**
  outside the window, so volume means the same thing on both sides of the floor. Cheap: a
  handful of addresses.
- Launchpad tokens launched before the floor keep DEX data but have no launch attribution;
  documented, not fixed.

**The details that are easy to get wrong.** `src/coverage/` and
`src/predictions/history.rs` carry them.

- **"First writer wins" is enforced twice, not once.** The code reads the stored floor
  before it writes and refuses to write when one is there - that read is what produces the
  warning the owner needs - but a read cannot be trusted alone under section 2's
  no-read-your-writes rule. So `_version` is `MAX - coverage_from_block`, and the
  ReplacingMergeTree therefore keeps the row with the LOWEST block whatever order two
  inserts land in. "Never moves later" is a property of the engine, and the lease is not
  load bearing for it.
- **`--start-date` is REFUSED on Solana rather than resolved.** A slot carries no
  timestamp, so there is nothing to bisect; rounding a date to a slot by arithmetic would
  be a guess, and the floor it wrote could never be moved later. `--start-block <slot>`
  and `--new-blocks-only` (the default) are the two answers there. The head's floor is
  dated with `now`, which is what "the head" means to within a second.
- **The Solana coverage line is printed by `bin/indexer.rs`, not by
  `pipeline::solana_verify`.** Slots have no timestamps to date the covered head with, so
  the line needs nothing from the report; keeping it out of that file also kept this
  change out of another engineer's way.
- **The registry-only pass ships no deployment blocks, and needs none.** The obvious
  reading of "from their deployment block up to the floor" would be a constant table of
  addresses and blocks. It is a log filter over a handful of
  addresses, and a source that serves those filters skips the blocks before a contract
  existed without reading them and reports how far it got, so starting at block 0 costs
  the same and carries no risk of a wrong number in a README. `prediction_trusted` gains
  an optional `from_block` (migration 0023) for an operator who has verified one. A
  constant table of addresses would also have contradicted the module's own rule, which
  is that it ships no address list at all and the operator says what it believes.
- **Lowering the floor is a CHECK, not a claim, and it moves over blocks that
  are already stored.** `indexer backfill --start-block N` lowers the floor only
  after `verify` confirms that every block from `N` to the old floor is stored and
  gap-free; otherwise the floor stays and the command says how many are missing.
  Note what that means in practice: a backfill re-decodes logs this database
  already has, it does not fetch new ones, so this is the path for a deployment
  that indexed deeper before the floor existed. Streaming a range BELOW the floor
  that nobody has ever asked for is not implemented by any command yet, and is the
  one piece of section 16 that is not here.
- **The pass reads TRUSTED addresses only, which includes the questions.** A market's
  title comes from a NegRisk or UMA adapter, so an operator who wants titles back has to
  have those adapters in `prediction_trusted`. This is the module's existing trust model,
  not a new rule, and `src/predictions/README.md` says so where it matters.

- **Everything that reads a range starts at the floor.** With no explicit start, `indexer verify`, `indexer backfill`, the gap heal and
  the `--new-blocks-only` cursor all begin at the stored floor, never at 0: blocks below the floor are absent on purpose and are
  not a gap. The floor's own (partial) day IS cross-checked, because nothing is stored below the floor so both sides count the same
  rows. An explicit start below the floor is honoured and the report names the floor. `verify` exits 0 on a healthy floor-to-head
  database (found by the live release run, 2026-09-19).

## 17. Decisions log

Every question that was closed, with the date, what was decided, why, and what
was rejected. This section exists so that a decision does not have to be
re-derived from research that no longer exists: the research files (the
data-model proposals, perps, EVM launchpads, Solana, QuickNode, and the review
round 4 report) were folded into this document and removed on 2026-09-19.

### 17.1 Where the schema came from (2026-09-15 .. 2026-09-18)

Sections 1 to 9 are the resolution of a data-model audit of the 2.x codebase.
The verdicts, so the reasoning survives the proposals file:

| Proposal | Verdict | Why |
|---|---|---|
| Binary hashes, addresses and `UInt256` amounts instead of hex strings | **adopted** | section 1; plus the rule that `sum()` over a raw `UInt256` is banned in aggregates, because it wraps silently and hostile tokens emit amounts near 2^256 |
| Replace ~50 bloom filters with sort orders, using projections OR MV-fed side tables | **adopted as side tables; projections rejected** | tombstones must reach every read path, and a materialized view does that for free while a projection does not |
| Drop `Nullable`, dead and duplicated columns; enums as `Enum8` | **adopted, but `LowCardinality(String)` not `Enum8`** | forward compatibility beats the byte: a new enum value must not need a migration |
| `ZSTD(3)` rather than `ZSTD(9)`, `DoubleDelta` on timestamps | **adopted** | level 9 costs several times the CPU for a few percent, on every insert AND every merge |
| `PARTITION BY (chain, toYYYYMM(timestamp))` | **rejected** | 50 chains x 120 months is ~6,000 partitions per table. Month only; `chain` is the first sorting-key column and is what prunes reads |
| Fix the four wrong materialized views by making them `REFRESH`able | **bugs fixed, mechanism rejected** | the bugs (`status = '0x1'`, the day derived from the block NUMBER, `uniqExact` inside a Summing table, MVs firing before dedup) are fixed by the rules in section 1; aggregates stay incremental and are kept correct by bucket repair, not by re-running them on a timer |
| Repair reorgs with `DELETE FROM ... WHERE block_number >= ?`, `--confirmations` default ~12 | **adopted, both details changed** | repair is insert-only (section 2), because concurrent `DELETE`s are not reliable; `--confirmations` defaults to **0**, since reorgs are repaired either way and latency is worth more than avoided rollbacks |
| Fix the silent numeric narrowing (`UInt32` gas, `UInt16` indices) | **adopted** | `UInt64` for gas, nonce and block number, with no saturation anywhere |
| Document a `FINAL` reading convention | **adopted** | and `do_not_merge_across_partitions_select_final = 1` everywhere, so `FINAL` stays cheap |
| Rewrite `traces` with a better sorting key | **rejected, out of scope** | traces are removed entirely (section 9): the analytics modules need neither traces nor deployer data |
| Checkpoints instead of gap scans | **adopted, with one correction** | they are an index, not the resume cursor (section 3) |
| Take token resolution off the commit path | **adopted, and hardened** | into "without trusting one RPC" (section 4) |
| Trim the HyperSync field selection; numbered migrations; a Prometheus endpoint | **adopted** | sections 8, 6 and 7 |
| Build the derived datasets as a separate consumer, or in pure SQL | **rejected in practice** | decoding lives in-process as pure functions inside the feature modules, called from `transform` (section 12) |
| Arrow passthrough | **deferred** | named at the top of this document |

### 17.2 Perpetual futures: deferred (2026-09-19, owner)

**Decided.** Perps are out of scope. No `perp_*` tables, no decoders, no flag.

**Why.** Only **3.90% of 30-day perp volume ($26.0B of $666.7B) is readable from
EVM event logs on chains HyperSync serves**; counting every EVM chain, including
ones HyperSync does not serve, the ceiling is 5.20%. The market is concentrated
off EVM: Hyperliquid alone is 36.03%, and the top six (Hyperliquid, Aster,
Lighter, ApeX, edgeX, Variational) are 74.21% — **none of the six emits its
trades as EVM event logs**. They run their own L1s, zk rollups that publish only
account deltas, or off-chain matching with no per-trade event. Reaching that
3.90% would cost roughly **seven separate decoders**, because perps have almost
no "one ABI, many forks" effect: the only real fork families are GMX V1 (64
forks, nearly all dead, $89M combined) and GMX V2 (9 forks, one alive).
"Agnostic" for perps means a common OUTPUT table, not a common input ABI — the
opposite of the DEX module's economics. The ABIs also churn hard (Perpl renamed
its events twice in three months, changing `topic0` each time; Gains ships
`...BeforeV10...` / `...AfterV10...` variants), so it would be a signature
registry with several generations per family, maintained for ever.

**The candidate list, if this is ever picked up.** Best first: **Nado** on Ink
($9.48B/30d — one contract, one event, public source, funding and open interest
arrive as events), Avantis on Base ($2.77B), Perpl on Monad ($2.53B), SynFutures
V3 on Base ($2.41B), GMX V2 on Arbitrum and Avalanche ($2.34B — the richest
data, at the cost of decoding a nested key/value bag), Aark ($2.09B), Gains
(gTrade) ($1.25B), Katana Perps ($1.04B), Primit ($0.63B), Ostium ($0.51B and
falling fast), KiloEx ($0.41B), SYMMIO ($0.20–0.36B), LeverUp ($0.25B). A
further **$8.46B** (Orderly, RISEx, Reya, Derive) is readable in principle but
sits on chains HyperSync does not serve; Orderly's `ProcessValidatedFutures` is
the best dataset of any family and is blocked only by chain coverage.

**Rejected alternatives.** *Index Hyperliquid through its own S3 / REST data* —
it is 36% of the market and the only major off-chain venue with a public
historical dataset, but it is requester-pays AWS whose own docs disclaim
completeness, with three successive fill formats and rows that carry no block or
log coordinates and no reorg semantics. That is a second ingest path with a
`source` discriminator, i.e. a separate product, not a perp decoder. *API
adapters for Aster and Lighter* — Aster's market-wide trades carry no trader
address, and Lighter's full history is paywalled inside its app. *One generic
perp decoder* — there is no shared input ABI. *The GMX V1 fork family as a cheap
entry point* — 64 forks and the simplest ABI, but $0.09B of live volume, mostly
on a chain HyperSync does not serve.

**Two facts to keep if this is revisited:** `tx.from` is a keeper or sequencer at
essentially every venue and must never be used as the trader; and venues that
merely post their fills as logs are operator-reported, so their data is complete
only for as long as the operator keeps posting — any output table must say which
kind a row came from.

### 17.3 Block source: stay on HyperSync (2026-09-19, owner)

**Decided.** Envio HyperSync remains the block source for EVM and Solana.
QuickNode was costed as a replacement and rejected.

**Why.** The move is technically possible on every chain that matters; it is a
bad trade financially. Same data, same chains, first year: **Envio $70–$6,310
against QuickNode ~$45,763.** The gap is structural — HyperSync is priced per
QUERY and returns many blocks per query, QuickNode is priced per BLOCK and this
indexer asks for every block of every chain.

| Job | Envio HyperSync | QuickNode |
|---|---|---|
| Full history of Ethereum, Base, Arbitrum, BSC, Polygon (801M blocks) | $0 on the free tier (~142 days) or $480 for one month of Pro (~4.3 days) | ~$16,025 once, ~37 days at 500 RPS |
| Head of 50 chains, per month (87.7M blocks) | $0–$480 / month | ~$1,700 / month |
| Head poll at the current 1 s interval, 50 chains | $0 — `/height` is free and unmetered | +$1,296 / month on its own |
| Solana head, per month | $0 | ~$146 / month |
| Solana history back to 2026-01-03 | $70 once | ~$860 once |
| Solana history to genesis | **impossible at any price** | ~$6,726 once, plus 200–300 TB and 10–21 days |

QuickNode is also not faster (batching bills per sub-request and counts against
RPS, so 500 RPS is a hard 250 blocks/second, while HyperSync returned 5,018
Arbitrum blocks in a single request), serves ~25 mainnets fewer, and its Monad
is a ~40,000-block window rather than an archive. Plain JSON-RPC moves only
1.2–1.9x the bytes HyperSync does for identical content: bytes were never the
problem, requests and credits are.

**The one thing money cannot buy from Envio is Solana before 2026-01-03**, which
stays a separate decision (section 14.3).

**Rejected alternatives.** *QuickNode Streams* — costs exactly the same as
pulling over RPC, has no ClickHouse destination, no Solana backfill at all, and
a push model that fights the lease / epoch / commit-marker design. *QuickNode
Flat Rate RPS* — looks like the cheap backfill until the concurrency column: 6
concurrent in-flight requests at the measured ~180 ms each is 33 requests per
second, not 250. *Yellowstone gRPC for Solana* — byte metered at roughly 7x the
cost of `getBlock`, and it replays only about 20 minutes. *JSON-RPC batching to
cut cost* — a batch of 50 calls bills 50 credits and counts 50 against RPS.

**Parked, with the trigger written down: a generic `--source rpc`.** One new
file, `src/source/rpc.rs`, implementing the existing `BlockSource` and
`CanonicalChain` traits next to `evm.rs`, selected per chain so a fleet can run
HyperSync where it is served and RPC everywhere else. Two calls per block —
`eth_getBlockByNumber`, then `eth_getBlockReceipts` **by hash**, asserted, so a
load-balanced pool cannot silently stitch two different blocks together. Eight
to thirteen engineer days, plus about ten more for a Solana twin. It is parked
because it buys insurance and optionality only, and because the seam already
exists, so nothing decays by not building it. **Build it when** (a) a HyperSync
incident of any real length happens — today one stops every chain at once, and
with this the answer is a config change; (b) a chain that is wanted is not
served; or (c) deep Solana history is bought, which needs an RPC `getBlock` path
anyway. Two things to know before starting: `SourceResponse.data` already
deserializes from exactly the `0x…` hex encoding JSON-RPC speaks, so an RPC
source can fill it directly and `core::decode` never notices; and over RPC there
is no `rollback_guard`, so reorg detection falls back entirely to parent-hash
continuity, which is what it mostly is anyway. **And fix the head poll first if
anything is ever pointed at a metered provider**: a 1 s interval is free on Envio
and $1,296 a month on QuickNode across 50 chains.

### 17.4 Trust model: decode by signature, trust no address (2026-09-18)

**Decided, and it is the same rule in every analytics module.** Decoding is by
event signature and event SHAPE, with no address filter, so a byte-identical
fork on a chain nobody has heard of works on day one. The only address lists in
the codebase are **operator-populated registries that ship no rows**:
`dex_trusted_emitters`, `quote_tokens`, `prediction_trusted`,
`launchpad_trusted_emitters`, `launchpad_frontends`, `sol_dex_programs` and
`chains`. The migrations seed none of them; the module READMEs carry the
verified addresses as ready-to-run `INSERT`s, so the operator says what the
operator believes.

**Why.** A shipped registry of venue addresses would be wrong within weeks —
launchpad venues rise and die inside a month — and it would make this indexer's
correctness depend on a list nobody maintains. The price of the rule is that the
headline views are empty until an operator populates them, which is stated at
the top of every module README.

**On Solana the corroboration is structural rather than a step.** A Solana
program cannot forge an SPL balance change, so the generic movement layer over
the instruction subtree already is the proof the EVM side gets from
`corroborate.rs`. What Solana needs instead is the rule that decoding is PER
INSTRUCTION SUBTREE and never per transaction net balance: one real transaction
holds two opposite 6.2 SOL PumpSwap swaps on the same pool, netting to 0.03 SOL.

### 17.5 Review rounds (2026-09-18 .. 2026-09-19)

Four independent read-only review rounds and one security review of `src/admin`
were run before release. Every finding is either fixed or recorded as a
deliberate trade-off in the section it belongs to — the bounded validity rule,
the side-table repair, the checkpoint compaction, the month-split flush, the
lease fencing, the Solana pool-key rule and the two-row purge audit in section
2; the panel's allow-list, `Host` validation, own accept loop, password minimum
and decaying login throttle in section 15. The reports themselves were not kept:
a finding that is fixed is a rule, and the rule is here.
