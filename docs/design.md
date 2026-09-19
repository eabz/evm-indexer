# Design (binding for all engineers)

Context: **no data is loaded anywhere.** This is a clean schema; no backward
compatibility, no backfill, no 2.x migration path. This document turns
`docs/data-model-proposals.md` into decisions. If something here is wrong or
impossible, message `lead` on tirith — do not silently deviate.

Deferred (NOT in scope): F1 Arrow passthrough.

## 1. Schema rules (S1, S3, S4, C1, C4, C5)

| Data | ClickHouse type | Rust side |
|---|---|---|
| hashes, topics | `FixedString(32)` | alloy `B256`, serialized as 32 raw bytes |
| addresses | `FixedString(20)` | alloy `Address`, 20 raw bytes |
| wei amounts, gas prices, difficulty, token amounts/ids | `UInt256` | alloy `U256`, 32 bytes **little-endian** |
| signed amounts (DEX) | `Int256` | alloy `I256`, 32 bytes little-endian two's complement |
| calldata, log data, code, output | `String` (raw bytes, not hex) | `Bytes` |
| 4-byte selector | `FixedString(4)` (zeros when input < 4 bytes) | |
| block number, gas*, nonce, size | `UInt64` | no saturation anywhere |
| tx index, log index, trace position, counts | `UInt32` | |
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
  creations, trace fields that don't apply to the action type). Log topics are four
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
- Read convention (C5): consumers query base tables with `FINAL`. Document in README.

### Read-path tables (S2)

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

### Aggregates (C2 and DEX)

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

## 2. Reorgs and the `purge_range` primitive (C3)

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

`purge_range(chain, from, to)` — idempotent, crash-safe, lock-free:
   1. `new_epoch` = chain's max epoch + 1. `from_ts` = start of day (UTC) of the minimum
      `timestamp` among live rows in range across block-scoped tables (rows, not
      `blocks`: a gap range may hold orphan children and no block).
   2. Tombstone every block-scoped table in range, **children and side-less bases first,
      `blocks` LAST** (mirror of the insert order: while the old `blocks` row is alive a
      crash is followed by re-detection and a full re-run; re-running is harmless).
      Side tables are NOT tombstoned directly — their MVs do it.
   3. Insert the `reorgs` row (chain, epoch, from_ts, detected_at, fork_block, old_head,
      old_hash, new_hash, depth, rows_tombstoned, reason `reorg` | `gap_heal`).
   4. Bucket repair for every `DerivedTable` at `new_epoch`.
   5. Tombstone `blocks`, then overlapping `checkpoints`; adopt `new_epoch` in the writer;
      evict cached discoveries from the range.
   Two subtleties (found by the schema engineer):
   - `from_ts` is computed over ALL row versions, **without `FINAL`** (tombstoned rows
     included): after a crash mid-purge the early part of the range is already dead, and
     a minimum over live rows would move forward and leave the first bucket stale forever.
   - Because `blocks` is tombstoned last, the rebuild of any aggregate sourced from
     `blocks` still sees the orphaned blocks. Such `rebuild_sql` takes
     `{purge_from}`/`{purge_to}` and excludes that block range; child-sourced aggregates
     (transactions, transfers, swaps, trades) do not need it.
   Accepted trade-offs: (1) the `reorgs` row lands before the rebuild, so readers
   briefly UNDER-count the repaired buckets (the opposite order would double count;
   neither is atomic across aggregates, and under-counting for a moment is the safe
   side). (2) The rule is open ended (`from_ts` only), so a rebuild re-aggregates the
   chain from `from_ts` to now: trivial for tip reorgs (today's bucket), expensive only
   for a purge deep in history, which needs a crash mid-flush during a backfill of old
   blocks. Kept for simplicity; bound it with a `to_ts` if it ever hurts.
   A crash anywhere re-runs the whole thing under a newer epoch; the validity rule makes
   the abandoned partial epoch invisible.

**Corrections found by the reorg-core proof (implemented in `src/reorg/`, binding for the pipeline):**
   - Checkpoints are tombstoned FIRST, not last: with "after blocks" a crash leaves a
     checkpoint claiming dead blocks (the crash matrix fails). This supersedes step 5's
     wording and section 3.
   - Gap heal is only crash safe if `has_orphan_children` counts tombstoned rows too (no
     `FINAL`): once orphans are tombstoned nothing else marks the unfinished heal.
   - `from_ts` includes `blocks` rows: a reorged range of EMPTY blocks has no child row,
     yet `daily_block_stats` needs repair.
   - The writer adopts the new epoch right after the `reorgs` row is written and re-reads
     it after any failed purge; otherwise a surviving process writes rows the validity
     rule hides.
   - No read-your-writes also bites: the epoch read (keep the epoch in memory; back-to-back
     purges must never reuse one), the detector seed read (a stale "not stored" would skip
     the parent check for ever: read it several times, and lookup ERRORS are errors, never
     "not stored"), `min_timestamp`, and `missing_ranges` after a barrier.
   - **Epochs and the validity rule are per CHAIN, not per module.** Anything that writes
     a `reorgs` row - including `indexer backfill --module X` - must rebuild EVERY derived
     table of every module for the affected buckets, or it silently zeroes the others.
     Two indexer processes on the same chain are unsupported (refuse at startup), and so
     are two `indexer backfill` runs of the same module: the lease has a ROLE and only
     excludes processes of the same role, so a backfill is refused by another backfill
     while still being allowed next to a live `indexer run`.
   - A MODULE purge settles only its OWN tables. Its repair window is the timestamp span
     of the module's rows, which can be far narrower than its block range, so
     `has_orphan_children` must not treat a completed `reason = 'redecode'` row as having
     settled anybody else's tombstones.
   - `timestamp_span` reports the smallest timestamp ABOVE ZERO. A `timestamp` of 0 is a
     MISSING block time, not a block time of 1970; taking it as the start of the repair
     window arms the validity rule on every day since the epoch and hides a whole chain's
     aggregates until a rebuild of fifty years finishes. `Purger` clamps a 0 it still
     gets to the last day of the range, loudly.

**Changes from the hardening round (implemented, binding):**
   - Purge order is now: checkpoints, children, `reorgs` row (armed), rebuild, `blocks`,
     **verify + repair side tables**, **mark the `reorgs` row completed**, caches. Side
     tables normally follow through their MVs, but a lost view push would leave live
     orphans for ever, so the purge checks each side table for the range and tombstones
     it directly (`tombstone_sql_where`) until zero.
   - `reorgs` gains `to_block`, `tombstone_version`, `completed`; two rows per purge
     (armed, completed), deliberately not collapsed. This is what tells the debris of a
     FINISHED purge from the leftovers of one that died: a tombstoned orphan heals only
     when its `_version` is newer than what a completed purge of that block wrote, so a
     restart after a rollback that shortened the chain is a no-op. Audit queries use
     `WHERE completed = 1`. `epoch_floor_v`'s columns are unchanged.
   - Fencing: the writer asks the lease before every flush and every purge and refuses
     to write when the lease is lost or its own heartbeat is older than the ttl; a
     process whose heartbeats lapsed stops for ANY other live instance.
   - `indexer backfill` re-reads the chain's epoch before each chunk and stops loudly if
     the live indexer purged meanwhile. `indexer verify` cross-checks aggregates against
     base tables per complete UTC day (a doubled aggregate is INCONSISTENT), over the
     complete days of the GAP-FREE PARTS of the range so that a backfill in progress does
     not switch the check off; a pending gap heal is INCONSISTENT too (the aggregates
     still count rows that the next start removes), and a range where no complete day
     could be compared reads "CONSISTENT, NOT FULLY CHECKED", never plain CONSISTENT.
   - Checkpoints are an index, deliberately NOT the resume cursor: resuming from them
     would skip the one inspection that finds orphan children below the cursor.
   - ClickHouse 25.12 landmines: in `SELECT * REPLACE (x AS c) ... WHERE c = ..` the WHERE
     sees the REPLACED value; and `SELECT * REPLACE` with `LIMIT` silently returns no
     rows. Generated tombstones use positional column lists for this reason.

**Changes from hardening round 2 (implemented, binding):**
   - The validity rule is now BOUNDED: a contribution with epoch `e` in bucket `b` counts
     iff `e >= max(r.epoch)` over the chain's `reorgs` rows with `r.from_ts <= b AND
     b < r.to_ts` (`to_ts` = start of the day after the newest purged row). A repair
     covers exactly `[from_ts, to_ts)`. `epoch_floor_v` is a per-day step function with
     explicit segment ends, so every consumer's `ASOF LEFT JOIN ... WHERE a.epoch >=
     ifNull(f.epoch_floor, 0)` stays byte-identical (measured: 0.28 s on 10k reorgs x 1M
     aggregate rows; the array formulation needed 59 GiB). This supersedes accepted
     trade-off (2) above. `ReorgStore`: `min_timestamp` -> `timestamp_span` (both ends,
     no `FINAL`); `rebuild_derived` gains `to_ts`; `DerivedTable::rebuild_slice`.
   - A flush spanning more than 90 monthly partitions is split by month, oldest part
     first, each part complete in itself (`blocks` last, own dedup tokens, own checkpoint).
   - `checkpoints` are compacted (insert-only cover + tombstones, lease-fenced, bounded).
   - Every module's rebuild SQL excludes the purged block range itself
     (`{purge_from}`/`{purge_to}`): a rebuild never depends on seeing tombstones.
   - Test harnesses must re-issue tombstones until a count says 0 and re-read after an
     insert: ClickHouse 25.12 misses ~3% of reads issued right after an acknowledged INSERT.
   - The sink's queue of flush spans that raced another process's purge is drained
     NON-DESTRUCTIVELY: a span leaves it only after its purge succeeded, so a transient
     error does not lose it (nothing else asks for those blocks again - their rows are
     stored, so no gap query reports them). It no longer has to survive in memory either:
     every start re-derives the same spans from the database (`stale_flush_ranges`) as
     the live base rows inside a purge's `[from_ts, to_ts)` whose `epoch` is below that
     purge's and whose `_version` is above its `tombstone_version`, i.e. rows written
     after the rebuild had read its input. Conservative and self-terminating: the rows
     come back stamped with the newest epoch, which no `reorgs` row is above.

**No read-your-writes (ClickHouse 25.12, observed on the macOS build).** Right after an
`INSERT` returns, the next query can miss the new part for a few milliseconds when
several writers are active (44-137 misses per 3,200 in the schema engineer's repro; it
heals on the next try). So: (1) before purging, make sure the last flush is visible;
(2) a tombstone `INSERT .. SELECT` can miss freshly flushed rows: re-issue
`tombstone_sql` until `live_rows_sql` returns 0 (idempotent, lock-free), bounded, fatal
if it never converges; (3) a rebuild never depends on seeing tombstones (it excludes the
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

## 3. Checkpoints (F2)

`checkpoints (chain, from_block, to_block, _version)` — one row per contiguous committed
range per flush, written after `blocks`. Resume = max contiguous `to_block` from
`start_block`. The gap query over `blocks` remains as the first-pass verifier/repair and
as `indexer verify`. `purge_range` tombstones overlapping checkpoints (insert-only, like everything else).

## 4. Token metadata without trusting one RPC (F3)

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

## 6. Migrations (F5)

`migrations/NNNN_name.sql`, embedded in the binary at compile time, applied in order at
startup and via `indexer migrate`; `schema_migrations (version, name, checksum,
applied_at)`; refuse to start if an applied migration's checksum changed. Statement
splitting must survive `;` inside strings/comments. No more
`docker-entrypoint-initdb.d`. The database name comes from the URL (no hard-coded
`indexer.` prefix in DDL). Reserved numbers: `0001` core tables, `0002` read-path side
tables, `0003` core aggregates, `0004` checkpoints + reorgs, `0010`–`0019` DEX.

## 7. Observability (F6)

`--metrics-addr` (default off): Prometheus text endpoint + `/healthz` + `/readyz`.
Metrics: head, indexed height, lag (blocks, seconds), rows/s per table, flush latency
histogram, flush retries, channel fill, token queue depth / cache hit rate / rpc breaker
state, reorgs total + last depth, purge duration. Module `src/metrics/` with a cheap
clonable handle; no metrics crate lock-in leaking into other modules.

## 8. Field selection (F4)

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

Everywhere else in this document, references to traces / `traces_by_tx` / a `contracts`
table are superseded by this section.

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

Basis: `docs/launchpads-research.md` (73% of 30d launchpad fees are on EVM chains
HyperSync serves; two verified event families cover ~86% of that). Module
`src/launchpads/`, ON by default (`--no-launchpads`), same shape and storage rules as
`src/dex/` and `src/predictions/`. Migrations `0030`–`0039`.

- **Families first:** `pons_v2` and `flap_portal` (verified source; same ABI on several
  chains). Then **launch attribution only** for venues that launch straight into
  Uniswap V3/V4 pools the spot decoders already capture (Pons V1, Clanker, NOXA, ...):
  one launch event each, no curve decoder.
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
- **Front ends are not venues.** fomo, GMGN, Axiom etc. have no contracts of their own;
  attribution is by fee-recipient/router address in a user-populated
  `launchpad_frontends` table. Never add front-end volume to venue volume.
- Forgery rules from the DEX review apply: curve trades are valued only when
  corroborated by the token/quote ERC-20 (or native value) movement in the same tx.

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
```

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

Binding spec: `docs/solana-research.md` section 0 (fold its table into this section at
the final docs cleanup). Applies to the analytics DATA MODULES only - `dex_*`,
`launchpad_*`, `prediction_*` - NOT to the EVM `core` tables.

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

Basis: `docs/solana-research.md` (sections 3, 4, 6, 7 are the working spec). One binary,
one database: `indexer run --chain solana`. **Analytics-only, program-filtered** - no
wallet history, no chain-wide transfers, and the schema/README must say so.

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
- Chain id for Solana: 1399811149 (no standard exists; recorded in `chains`).
- Resume on Solana is the CHECKPOINT TILING and not a gap query over the commit marker
  (a slot with no row is usually a skipped slot, not a gap), but section 2's rule still
  binds: **the data decides.** Before a hole in the tiling is streamed again, it is
  purged whenever it still holds anything - orphan children, or live `sol_slots` rows
  whose checkpoint insert never landed. The tiling is read with no `LIMIT` and compacted
  after a covered pass, exactly as on the EVM path.
- Order: (1) source + `svm` core + generic movement decoder + PumpSwap and pump.fun
  curve decoders, validated live with the owner's token; (2) Raydium / Orca / Meteora
  per-program decoders; (3) launchpads on Solana (pump.fun, Meteora DBC, LaunchLab) into
  `launchpad_*`; (4) history backfill strategy (Envio serves from 2026-01-03).

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

Order of work: after the review round 4 core fixes merge (both touch `src/pipeline/mod.rs`).
