# Data model & pipeline proposals

Status: **proposal — nothing here is implemented.** Issue #15 deliberately keeps every
non-DEX table schema unchanged so existing deployments keep working. Everything below
needs a migration (and in one case a partial re-index), so it needs sign-off first.

Findings come from an audit of `migrations/*.sql`, `src/db/**` and the ingestion flow as
of `main` @ `d49ca3b`. Items are ordered by priority inside each section.

| Pri | Item | Kind | Needs |
|---|---|---|---|
| P0 | [C1](#c1-traces-are-being-collapsed-by-merges-data-loss) `traces` rows collapse on merge | data loss | new table + re-index traces |
| P0 | [C2](#c2-materialized-views-compute-wrong-numbers) Materialized views compute wrong numbers | wrong data | drop/recreate MVs |
| P0 | [C3](#c3-reorgs-leave-orphan-rows-forever) Reorgs leave orphan rows forever | wrong data | code + `_version` column |
| P1 | [C4](#c4-silent-numeric-narrowing) Silent numeric narrowing | wrong data | `MODIFY COLUMN` (cheap) |
| P1 | [C5](#c5-readers-see-duplicates-until-merges-run) Readers see duplicates until merges run | read correctness | query convention |
| P1 | [S1](#s1-store-hashes-addresses-and-amounts-as-binary--integers) Binary hashes/addresses, `UInt256` amounts | storage, read, write | table rebuild |
| P1 | [S2](#s2-replace-the-50-bloom-filters-with-a-few-sort-orders-that-match-the-queries) Replace ~50 bloom filters with targeted sort orders | read + write | projections / side tables |
| P2 | [S3](#s3-nullable-lowcardinality-dead-columns) `Nullable`, `LowCardinality`, dead columns | storage, write | table rebuild (with S1) |
| P2 | [S4](#s4-codecs-and-partitioning) Codecs & partitioning | write CPU | with S1 |
| P2 | [F1–F7](#flow-optimizations) Flow optimizations | throughput, ops | code |

---

## Data consistency

### C1. `traces` are being collapsed by merges (data loss)

```sql
CREATE TABLE indexer.traces ( ... )
ENGINE = ReplacingMergeTree()
ORDER BY (chain, block_number)
```

`ReplacingMergeTree` deduplicates on the **sorting key**. Every trace of a block shares
`(chain, block_number)`, so whenever ClickHouse merges parts it keeps **one arbitrary
trace per block** and drops the rest. Fresh data looks fine, which is why this is easy to
miss; older, fully merged partitions will contain ~1 row per block. Check with:

```sql
SELECT block_number, count() FROM indexer.traces FINAL
WHERE chain = 1 GROUP BY block_number ORDER BY block_number LIMIT 20;
```

**Proposal.** Give traces a unique key. `transaction_position` is `Nullable` (reward
traces have none) and nullable key columns are a bad idea, so store a sentinel:

```sql
transaction_position UInt32,            -- 4294967295 for block/uncle rewards
trace_address        Array(UInt32),
...
ORDER BY (chain, block_number, transaction_position, trace_address)
```

The table has to be rebuilt, and rows already merged away are gone — **traces must be
re-indexed** for any range where merges have run. `contracts` derived from `create`
traces were computed before insert and are unaffected.

### C2. Materialized views compute wrong numbers

All in `migrations/indexes.sql`:

1. `mv_daily_tx_stats` counts `status = '0x1'` / `'0x0'`, but the indexer writes
   `'success'` / `'failure'`. Both counters are always 0.
2. `mv_daily_contract_deployments` derives the day from
   `toDate(fromUnixTimestamp(block_number))` — it treats the block *number* as a unix
   timestamp, so every deployment lands in early 1970. `contracts` has no timestamp column.
3. `uniqExact(...)` inside a `SummingMergeTree` view: each insert block writes its own
   distinct count and merges then **sum** them. An address active in 50 insert batches in
   one day counts 50 times. Distinct counts need `AggregatingMergeTree` + `uniqState`.
4. MVs fire per *insert*, before `ReplacingMergeTree` dedup. Anything inserted twice
   (gap healing of a partially stored block, a retried flush) is double counted forever.

**Proposal.** Drop these incremental views and replace them with
[refreshable materialized views](https://clickhouse.com/docs/materialized-view/refreshable-materialized-view)
(`REFRESH EVERY 10 MINUTE`) that aggregate over deduplicated data for a trailing window.
Daily stats don't need per-insert latency, this takes four extra writes off the hot
insert path, and it makes the numbers correct by construction. Add a `timestamp` column
to `contracts` (needed for 2 regardless).

### C3. Reorgs leave orphan rows forever

- `blocks` is keyed `(chain, number, hash)`, so both sides of a reorg are kept and
  nothing marks which one is canonical.
- Child tables include `transaction_hash` in their key. A reorged-out transaction's
  rows never collide with anything, so they are never replaced: orphaned transactions,
  logs and transfers stay queryable forever and inflate every aggregate.
- Dedup only happens *within* a partition. A replacement block whose timestamp falls in
  another month never meets the row it should replace.

After #15 the indexer *detects* reorgs (HyperSync `rollback_guard` parent-hash check) and
warns, but does not repair.

**Proposal**, two layers:

1. *Avoid most of it:* add `--confirmations N` (default e.g. 12; 0 for instant-finality
   chains). The writer only commits blocks `<= head - N`. For explorer-style "latest
   block" needs, an optional unconfirmed tail can live in a small separate table with a
   short TTL.
2. *Repair the rest:* on a parent-hash mismatch, walk back comparing stored
   `blocks.hash` with HyperSync until they agree (fork point), issue a lightweight
   `DELETE FROM <table> WHERE chain = ? AND block_number >= ?` on every table, and resume
   streaming from the fork point. Bounded by `N`, so deletes stay tiny.

Schema support: key `blocks` by `(chain, number)` and use
`ReplacingMergeTree(_version)` everywhere with `_version UInt64` = insert time in ms, so
"latest write wins" is explicit instead of accidental.

### C4. Silent numeric narrowing

| Column | Type today | Problem |
|---|---|---|
| `blocks.gas_limit`, `gas_used`, `transactions.gas`, `gas_used`, `cumulative_gas_used`, `traces.gas`, `gas_used` | `UInt32` | > 4.29B gas limits exist (several L2s/app-chains, most testnets with "unlimited" gas). Old code **panicked** here; #15 saturates instead, which is safe but lossy. |
| `blocks.transactions`, `transaction_index`, `log_index`, `transaction_position`, `subtraces` | `UInt16` | 65,535 cap. High-throughput chains (Monad, Sei, MegaETH-class) can exceed it per block for logs. `log_index` is part of the dedup key, so a wrap would make distinct logs *replace each other*. |
| `transactions.nonce` | `UInt32` | Fine in practice, wrong in principle (`uint64` in the protocol). |
| `blocks.size` | `UInt32` | Fine. |

**Proposal.** Widen gas columns and nonce to `UInt64`, index/count columns to `UInt32`.
With `Delta`/`T64` + `ZSTD` the on-disk cost is close to zero. Widening an integer is a
metadata-mostly `ALTER TABLE ... MODIFY COLUMN`, except for key columns (`log_index`,
`transaction_index`) which need the S1 rebuild.

### C5. Readers see duplicates until merges run

`ReplacingMergeTree` is eventually consistent. Until a merge happens, re-inserted rows
are visible twice. There is no documented read convention today.

**Proposal.** Document and enforce in the API layer: query with `FINAL`, and set
`do_not_merge_across_partitions_select_final = 1` on the tables so `FINAL` stays cheap
with monthly partitions. Combined with C3's `_version` this gives deterministic reads.

### Bugs found during the audit and fixed inside #15

Listed for the record; no decision needed.

- `withdrawals.validator_index` was filled with the *withdrawal* index (copy-paste).
  **Existing rows are wrong** and need a backfill from a re-index of post-Shapella blocks.
- ERC-1155 `TransferBatch` events were silently dropped ("skipped for now"). **Existing
  data is missing all batch transfers.**
- ERC-20 amount decoding panicked on `data` longer than 32 bytes — any contract could
  crash the indexer by emitting a malformed `Transfer`.
- Gas conversions panicked above `u32::MAX`.
- Token `type` was `"ERC20"` for every token with a name, including NFTs; tokens whose
  metadata call failed were refetched on every sighting.
- `logs.transaction_log_index` actually holds the *transaction index* (misnamed;
  unchanged in #15, rename proposed in S3).

---

## Schema: read/write performance

### S1. Store hashes, addresses and amounts as binary / integers

Everything is a hex `String` today: a 32-byte hash costs 66 bytes + length prefix, an
address 42 bytes, and they are compared bytewise as text. Amounts are **bare-hex strings
without `0x`** (`SerU256`), so `sum(amount)`, `amount > x` and `ORDER BY amount` are
impossible in SQL — every consumer has to pull rows out and do the math client side.

| Data | Today | Proposed |
|---|---|---|
| hashes, topics | `String` `'0x…'` | `FixedString(32)` |
| addresses | `String` `'0x…'` | `FixedString(20)` |
| `value`, `amount`, gas prices, `difficulty` | `String` bare hex | `UInt256` |
| `input`, `data`, `output`, `code`, `init` | `String` `'0x…'` hex | `String` raw bytes |
| `method` | `String` `'0x12345678'` | `FixedString(4)` |

Effects: roughly half the uncompressed volume on the widest tables (less to serialize,
send, compress and merge → faster writes), smaller primary/skip indexes, faster equality
filters, and amounts become first-class numbers (`sum`, `quantile`, balances via MVs).
The API formats on the way out: `concat('0x', lower(hex(hash)))`.

Client note: `clickhouse-rs` has no native `u256`; `UInt256` is 32 little-endian bytes
in RowBinary, which is a ~10-line `serde` adapter replacing `SerU256`.

Migration without touching the chain: create `*_v3` tables, backfill per partition with
`INSERT INTO t_v3 SELECT unhex(substring(hash, 3)), reinterpretAsUInt256(reverse(unhex(leftPad(amount, 64, '0')))), ...`,
then `EXCHANGE TABLES`. Only `traces` (C1) needs a real re-index.

### S2. Replace the ~50 bloom filters with a few sort orders that match the queries

Every table is sorted by `(chain, block_number, …)`. That is ideal for ingestion and
range scans and wrong for almost everything an API asks: *tx by hash, txs of an address,
logs of a contract + topic0, transfers of a wallet, holders of a token.* The current
answer is a `bloom_filter` skip index on nearly every column (~50 of them), which:

- costs CPU and IO on **every insert and every merge**, per index;
- barely helps where it matters: with `GRANULARITY 4` (32k rows) any active address or
  popular `topic0` (`Transfer`!) is present in nearly every granule block, so nothing is
  skipped, while rare values still require reading every index granule of every part;
- duplicates what the engine already has: the `minmax` on `timestamp` repeats the
  partition key's min/max and the primary key's monotonic `block_number`.

**Proposal.** Keep bloom filters only for true point lookups on unique values, and serve
each real access pattern with a sort order:

| Access pattern | Mechanism |
|---|---|
| tx / block by hash | keep `bloom_filter GRANULARITY 1` on `hash` **or** a slim lookup table `tx_lookup (chain, hash) → (block_number, transaction_index)` written by an MV; the second is O(log n) regardless of table size |
| `eth_getLogs`-style: contract + topic0 + block range | projection on `logs` `ORDER BY (chain, address, topic0, block_number, log_index)` |
| wallet history / balances | MV-fed table `erc20_transfers_by_account` with **two rows per transfer** (`account`, `direction`, signed amount), `ORDER BY (chain, account, token_address, block_number, log_index)`; balances become a `SummingMergeTree` over it once S1 makes amounts numeric |
| token activity / holders | projection on transfers `ORDER BY (chain, token_address, block_number, log_index)` |
| traces of a tx | projection `ORDER BY (chain, transaction_hash, trace_address)` |
| txs from/to an address | MV-fed `transactions_by_address` (two rows per tx), same shape as transfers |

Drop: bloom filters on `topic1..3`, `from`/`to` everywhere (replaced above), `set()`
indexes on `status`/`action_type`/`call_type` (low cardinality — they match every
granule), all `minmax(timestamp)`, and `tokenbf_v1` on `tokens` (tiny table).

Net effect on writes: ~50 index builds per flush become ~6 projections/MVs, each of
which actually accelerates a query. Projections can be added table by table and
materialized in the background, so this is independent of S1 (but cheaper after it).

### S3. `Nullable`, `LowCardinality`, dead columns

- **`Nullable` is not free**: a separate null-mask stream per column on disk and a
  branch in every function. `topic0..3`, `transaction_hash`, `from`, `to`, etc. should
  be non-null with a default (zero bytes / empty), or `topics Array(FixedString(32))`.
  Keep `Nullable` only where NULL ≠ default matters (`base_fee_per_gas` pre-London).
- **Enums as free-form `String`**: `status`, `transaction_type`, `action_type`,
  `call_type`, `reward_type`, `tokens.type` → `Enum8` (or `LowCardinality(String)` if
  forward compatibility matters more).
- **Dead / duplicated columns**: `log_type` (always NULL) and `removed` (always false)
  on four tables; `address` and `token_address` are the same value on all three
  transfer tables; `logs.transaction_log_index` is really the transaction index →
  rename to `transaction_index`; `is_uncle` is always false after #15.
- `blocks.logs_bloom` is 514 chars per block and useless for SQL filtering. Keep only if
  something downstream reconstructs headers; otherwise drop.
- `transactions.base_fee_per_gas` duplicates `blocks`; cheap after compression, fine to
  keep for join-free fee math.

### S4. Codecs and partitioning

- `CODEC(ZSTD(9))` on `input`/`data`/`output`/`extra_data`: level 9 costs several times
  the CPU of level 3 for a few percent of ratio on hex text, paid on insert *and* on
  every merge. After S1 (raw bytes, half the input) use `ZSTD(3)`.
- `timestamp`: `DoubleDelta, ZSTD` beats `Delta` for near-constant block times.
  `log_index`/`transaction_index`: `Delta, ZSTD` (already) or `T64`.
- `contracts` and `traces` have **no `PARTITION BY`**, unlike every other table: dedup
  and `FINAL` run over the whole table and data can't be dropped by range. Add
  `timestamp` and partition monthly like the rest.
- Multi-chain: `PARTITION BY toYYYYMM(timestamp)` mixes chains in each partition. With a
  handful of chains, `PARTITION BY (chain, toYYYYMM(timestamp))` makes per-chain
  operations (`DROP PARTITION` to re-index one chain, per-chain TTL, moving a chain to
  cold storage) trivial. Not recommended beyond ~10–15 chains per cluster (part count).

---

## Flow optimizations

Already delivered by #15 (for context): HyperSync streaming instead of 2–3 RPC calls per
block; bounded channel between stream and writer (backpressure instead of unbounded
memory); size/time-based flushes with async inserts; retry with backoff instead of
`panic!` on insert errors; SQL gap detection instead of loading every indexed block
number into a `HashSet`; Redis/Dragonfly token metadata cache with two-phase commit and
negative caching.

Proposed next:

- **F1. Arrow passthrough (largest remaining win).** HyperSync can stream Apache Arrow
  record batches (`stream_arrow`) and ClickHouse ingests `FORMAT ArrowStream` natively.
  For `blocks`, `transactions`, `logs` and `traces` that removes the per-row
  decode → Rust struct → hex `String` → RowBinary path entirely; only transfer decoding
  needs row access, and it can read the log batch columns directly. Becomes natural once
  S1 lands (HyperSync's binary columns map 1:1 to `FixedString`).
- **F2. Checkpoint table instead of gap scans.** `indexer.checkpoints (chain,
  from_block, to_block, committed_at)` written last in each flush. Resume becomes a
  lookup rather than a window function over the whole `blocks` table, and "what ranges
  are complete" no longer depends on `blocks` rows being merged/deduplicated. Keep the
  gap query as a `--verify` repair mode.
- **F3. Take token resolution off the commit path.** `tokens` is independent of block
  commit, yet a slow RPC multicall currently delays the batch it rides in. Feed token
  addresses to a side task with its own queue and insert loop; the block pipeline never
  waits for an RPC.
- **F4. Trim the field selection.** Don't request what we don't store (or stop storing
  what nobody reads: `logs_bloom`, `sha3_uncles`, roots). HyperSync bills bandwidth per
  selected column.
- **F5. Versioned migrations.** `migrations/` only runs through the ClickHouse image's
  `docker-entrypoint-initdb.d` on an empty volume; there is no way to evolve an existing
  database and `indexes.sql` mixes required DDL with commented-out advice. Move to
  numbered files + a `schema_migrations` table + `indexer migrate`, run on startup.
  Prerequisite for shipping any of C1–S4 safely.
- **F6. Observability.** Prometheus endpoint: head lag (blocks/seconds), rows/s per
  table, flush latency and retries, channel fill (who is the bottleneck: stream or
  ClickHouse), token cache hit rate, reorgs detected. `/healthz` for compose/k8s. Today
  the only signal is an `info!` line per flush.
- **F7. Derived datasets as separate consumers.** Per the note in #15, DEX trades,
  balances and similar should be built *from* `logs` (ClickHouse MVs, or a second small
  service tailing the table), not inside the ingester. S1 + S2 make this practical in
  pure SQL for Uniswap-V2/V3-style events.

## Suggested order

1. **F5** (migration tooling) — everything else depends on it.
2. **C1 + C2** — stop losing traces, stop publishing wrong stats. Small, urgent.
3. **C4** non-key widenings (cheap `MODIFY COLUMN`).
4. **S1 + S3 + S4** as one `v3` table generation (single backfill), including C4 key
   columns, C3's `_version`, and `traces`/`contracts` partitioning.
5. **C3** reorg handling + `--confirmations`, **C5** read convention.
6. **S2** projections/side tables, driven by the API's real query mix.
7. **F1–F4, F6, F7** as throughput and ops needs dictate.
