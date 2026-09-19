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
  becomes `transaction_index`. `contracts` and `traces` get `timestamp`.
- Every table: `ENGINE = ReplacingMergeTree(_version)`, `_version UInt64` = unix ms taken
  once per flush, `PARTITION BY (chain, toYYYYMM(timestamp))`,
  `SETTINGS do_not_merge_across_partitions_select_final = 1`.
- Sorting keys are positional, never hash based, so a re-inserted block replaces itself:

| Table | ORDER BY |
|---|---|
| blocks | (chain, number) |
| transactions | (chain, block_number, transaction_index) |
| logs, erc20/721/1155_transfers | (chain, block_number, log_index) |
| traces | (chain, block_number, transaction_position, trace_address) — `transaction_position = 4294967295` for reward traces |
| withdrawals | (chain, block_number, withdrawal_index) |
| contracts | (chain, block_number, contract_address) |
| tokens | (chain, address) — not block scoped |

- Codecs: `ZSTD(3)` on large byte columns (not 9); `Delta`/`DoubleDelta` + `ZSTD` on
  monotonic integers.
- Read convention (C5): consumers query base tables with `FINAL`. Document in README.

### Read-path tables (S2)

**No projections** (they complicate lightweight deletes on ReplacingMergeTree). No
bloom-filter zoo. Each access pattern gets an MV-fed side table, itself
`ReplacingMergeTree(_version)`, carrying `(chain, block_number)` so it participates in
rollback like any other table:

| Table | Fed from | ORDER BY |
|---|---|---|
| `tx_lookup` | transactions | (chain, hash) → block_number, transaction_index |
| `block_lookup` | blocks | (chain, hash) → number |
| `transactions_by_address` (2 rows/tx: from, to) | transactions | (chain, address, block_number, transaction_index, direction) |
| `logs_by_address` (slim: keys + topic0) | logs | (chain, address, topic0, block_number, log_index) |
| `erc20_transfers_by_account` (2 rows/transfer, signed direction) | erc20_transfers | (chain, account, token_address, block_number, log_index, direction) |
| `nft_transfers_by_account` | erc721 + erc1155 | same shape |
| `traces_by_tx` | traces | (chain, transaction_hash, trace_address) |

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
    /// `INSERT INTO <name> SELECT ... FROM <base> FINAL WHERE chain = {chain}
    ///  AND timestamp >= {from_ts} GROUP BY ...` — must produce exactly what the MV produces.
    pub rebuild_sql: &'static str,
}
pub const CORE_DERIVED: &[DerivedTable] = &[ /* daily block/tx/transfer/contract stats */ ];
```

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

`purge_range(chain, from, to)`:
   1. `min_ts` = min `timestamp` of stored blocks in range (before deleting anything).
   2. Lightweight `DELETE FROM t WHERE chain = ? AND block_number >= ? [AND < ?]` on every
      **block-scoped table, children and side tables first, `blocks` LAST**, each
      awaited synchronously (`lightweight_deletes_sync = 2`). Mirror image of the insert
      order: while the old `blocks` row exists the reorg is re-detected after a crash and
      the purge re-runs; it is idempotent. No intent log needed.
   3. **Bucket repair** for every `DerivedTable`: delete buckets `>= bucket(min_ts)` for
      the chain, run `rebuild_sql` from that bucket start over the surviving base rows.
      Re-streamed blocks then flow through the MVs incrementally as usual.
   4. Append a row to `reorgs` (chain, detected_at, fork_block, old_head, old_hash,
      new_hash, depth, blocks_purged) — audit + metric.
   5. Evict anything cached from the purged range (pending token / pool discoveries).

The list of block-scoped tables is code, not convention:
`db::BLOCK_SCOPED_TABLES` + `dex::BLOCK_SCOPED_TABLES`, children before `blocks`. A unit
test asserts every table in the migrations that has a `block_number` column is listed.

**Gap healing uses the same primitive.** A gap range may hold orphan children from a
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
as `indexer verify`. `purge_range` deletes/truncates overlapping checkpoints first.

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
enabled by `--dex`, pure function `decode(&[DatabaseLog]) -> DexRows` inside transform.

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

DEX analytics need neither traces nor deployer data: pools come from factory events,
tokens from transfers, liquidity providers from `dex_liquidity.tx_from` (the event
`sender` is usually a router, never use it for attribution). Therefore:

- `--traces` stays off by default; `full` traces are for explorer/forensic deployments.
- `contracts` always holds **directly deployed** contracts (receipt `contractAddress`,
  free). Factory-created contracts only appear when `--traces` is on. Document as such.
- Not built, possible later: a `creates`-only trace mode (`TraceFilter::and_type(["create"])`)
  would complete `contracts` without storing the trace firehose.
