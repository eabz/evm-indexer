# DEX analytics (`src/dex`, migrations `0010`-`0012`)

Chain agnostic, DEX agnostic swap / liquidity / pool indexing, enabled with
`--dex`. Design: `docs/design.md` §5.

**Decoding is by event family, never by router / factory registry.** A log is a
swap when its `topic0` AND its shape (topic count, data length, value ranges of
the narrow integer types, zero padding of addresses) are the ones the family
emits. A Uniswap V2 fork on a chain nobody has heard of works on day one.
`protocol` is therefore the FAMILY, not a brand.

Decoding happens in Rust (pure, no I/O); every analytic lives in ClickHouse.

## Families

| protocol | pools from | swaps | liquidity |
|---|---|---|---|
| `uniswap_v2` | `PairCreated` | `Swap` (also Solidly V1 forks) | `Sync`, `Mint`, `Burn` |
| `solidly` | Solidly V1 `PairCreated` (stable flag), Velodrome V2 / Aerodrome `PoolCreated` | Velodrome V2 / Aerodrome `Swap` | `Sync(uint256,uint256)`, `Burn(address,address,..)` |
| `uniswap_v3` | `PoolCreated`, Slipstream `PoolCreated`, Algebra `Pool` / `CustomPool` | `Swap`, PancakeSwap V3 `Swap`, Algebra Integral `Swap` (Algebra V1 emits the V3 signature) | `Mint`, `Burn` |
| `uniswap_v4` | `Initialize` (PoolManager) | `Swap` | `ModifyLiquidity` |
| `balancer_v2` | `PoolRegistered` + `TokensRegistered` (Vault) | `Swap` | - |
| `curve` | RPC only (no creation event) | `TokenExchange` (int128, uint256 and NG payloads), `TokenExchangeUnderlying` | - |

Every `topic0` is asserted against `keccak256(signature)` in
`events.rs`; every family is tested against real mainnet logs in `fixtures.rs`.

Where families share an event (V2 `Mint`, and the V2 `Swap` that Solidly V1
forks emit) the row says `uniswap_v2`; `dex_pools.protocol` tells them apart
(`solidly` from the factory event, or from `stable()` when resolved over RPC).

## Conventions

* **Signed amounts are pool relative: positive = INTO the pool**, negative = out
  (the Uniswap V3 convention). V2 / Solidly `in - out` pairs are netted into it.
  **Uniswap V4 reports caller relative deltas and is negated** (checked against
  the ERC-20 transfers of a real transaction). Mints positive, burns negative.
  V3 `Burn` amounts are the amounts owed, not yet collected.
* Two token families fill `amount0` / `amount1`. Multi asset families fill
  `amount_in` / `amount_out` plus `token_in` / `token_out` (Balancer: carried by
  the event) or `coin_in` / `coin_out` / `underlying` (Curve: indices into
  `dex_pools.tokens` / `underlying_tokens`). `dex_swaps_v` unifies both.
* `pool_id` is 32 bytes: pool address left padded, or the native `bytes32` id
  (V4, Balancer). `emitter` is the contract that emitted the event and is part
  of a pool's identity: `dex_pools` is keyed `(chain, pool_id, emitter)`, so a
  forked PoolManager / Vault can not overwrite the original's pools.
* **`tx_from` / `tx_to` are the sender and the target of the TRANSACTION**, filled
  on swaps AND liquidity rows by `DexRows::attach_transactions` from the
  transactions of the same batch. `dex_liquidity.tx_from` is who seeded (or
  pulled) a pool's liquidity - the event `sender` is usually a router, never use
  it for attribution. `dex_swaps.trader` = `tx_from` when attached, else the
  event's recipient, else its sender. `tx_to` is the contract the user called:
  router / aggregator attribution without a registry.
* **`dex_pools` holds one row per creation EVENT** (positional key `(chain,
  pool_id, emitter, created_block, log_index)`, like every block scoped table)
  plus at most one row of the RPC resolver (`created_block = 0`). Several live
  rows of one pool can exist - a forged `PairCreated` costs one transaction - so
  **read pools through `dex_pool_current_v`**: event rows before resolver rows,
  then the earliest position. The first creation event wins; versions are the
  plain flush versions, there is nothing special for a writer to respect.
* Hashes / addresses are raw bytes (`FixedString`): format with
  `concat('0x', lower(hex(x)))`, compare with `unhex('...')`. Query base tables
  with `FINAL`.

## Reorgs: insert-only (docs/design.md §2)

The indexer never issues DELETE / ALTER DELETE / DROP PARTITION (ClickHouse
loses one of two concurrent DELETEs, and 50+ indexers share the database).

* **Base tables** (`dex::BASE_TABLES`: `dex_swaps`, `dex_liquidity`, `dex_pools`)
  are `ReplacingMergeTree(_version, is_deleted)`. `purge_range` INSERTs
  tombstones with `dex::tombstone_sql(table, chain, from, to, version)`;
  `FINAL` hides the rows. A re-streamed row at the same position carries a
  newer version and is alive again; positions the canonical block does not
  have stay dead.
* **Side tables** (`dex::SIDE_TABLES`) are never touched: their materialized
  views pass `_version`, `is_deleted` and `epoch` through, a tombstone on the
  base table tombstones them.
* **`dex_pools`** is purged by `created_block`, event rows only
  (`dex::purge_filter`). Resolver rows describe chain STATE, not a block: no
  purge touches them, and when a pool's creation event is reorged out the pool
  falls back to its resolver row if there is one (if one is ever tombstoned the
  backfill resolves the pool again and the newer row wins).
* **Aggregates** carry `epoch` (the chain's purge generation, stamped on every
  row by `DexRows::set_epoch` / `PoolWorker::set_epoch`) as the last key
  column, their views only aggregate live rows. A purge records `(chain, epoch,
  from_ts)` in `reorgs` and runs every `DEX_DERIVED` `rebuild_sql` under the
  new epoch. The `*_v` views apply the validity rule - epoch `e` counts in
  bucket `b` iff `e >=` the largest epoch among the chain's reorgs with
  `from_ts <= b` - BEFORE merging states:

```sql
-- dex_epoch_floor_v: reorgs as a step function (running max over from_ts)
FROM dex_candles_1h AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= f.epoch_floor          -- no reorg at or before b: floor 0
GROUP BY chain, pool_id, emitter, bucket
```

Never read an aggregate table directly, only its `*_v` view.

## Tables (`0010_dex_tables.sql`)

Base tables are partitioned by month only (chain is the first key column); side
tables and `dex_pools` by chain. Always read with `FINAL`.

| table | key | purged by |
|---|---|---|
| `dex_pools` | `(chain, pool_id, emitter, created_block, log_index)` | tombstones on `created_block`, event rows only |
| `dex_swaps` | `(chain, block_number, log_index)` | tombstones on `block_number` |
| `dex_liquidity` | `(chain, block_number, log_index)` | tombstones on `block_number` |
| `dex_swaps_by_pool` (MV) | `(chain, pool_id, block_number, log_index)` | follows `dex_swaps` |
| `dex_swaps_by_trader` (MV) | `(chain, trader, block_number, log_index)` | follows `dex_swaps` |
| `dex_pools_by_token` (MV) | `(chain, token, pool_id, emitter, block_number, log_index)` | follows `dex_pools` |
| `dex_pool_current_v` (view) | THE row of every pool | - |
| `quote_tokens` | `(chain, token)` | user data, never purged |

## Aggregates (`0011_dex_aggregates.sql`)

`AggregatingMergeTree` (key ends in `epoch`, partitioned by month) + materialized
view + finalizing `*_v` view, each declared in `dex::DEX_DERIVED` with a
`rebuild_sql` that repeats the view's `SELECT` (placeholders `{chain}`,
`{from_ts}` = unix seconds of the first bucket, `{epoch}` = the new epoch;
`dex::derived::render_rebuild`). Buckets are computed from the unix time, always
UTC. `dex_pool_stats_1d_v` finalizes swaps / traders per pool and day.

| table | bucket | content |
|---|---|---|
| `dex_candles_1m` / `_1h` / `_1d` | 60 / 3600 / 86400 | per pool open / high / low / close (`argMin` / `argMax` on `(block_number, log_index)`), volume0, volume1, swaps, unique traders |
| `dex_pool_volume_1d` | 86400 | per pool and LEG: volume in / out, swaps, traders. Legs: `side` 0/1 (token0/token1), `token` (Balancer), `coin` / `ucoin` (Curve index) |
| `dex_protocol_stats_1d` | 86400 | per family swaps, unique traders, unique pools |

**Price** = token1 per token0 in RAW units: `(sqrt_price_x96 / 2^96)^2` when the
event has it (price AFTER the swap), else `|amount1| / |amount0|` (execution
price, fee included). Multi asset pools have no candles.

**Precision.** Prices and volumes are `Float64` computed inside SQL from the
`UInt256` / `Int256` columns: ~15.9 significant digits, relative error < 1e-15.
Volumes are Float64 on purpose: `sum()` over 256 bit integers wraps silently and
spam tokens emit amounts near 2^256 (tested with `2^256 - 1`). Exact amounts
stay in `dex_swaps`.

## Analyst views (`0012_dex_views.sql`)

Tokens and pools are joined at QUERY time, so results improve as the background
resolvers fill `tokens` / `dex_pools`. Unknown stays `NULL`, never 0.

| view | what |
|---|---|
| `dex_token_info_v` | symbol / decimals / quote kind per token (`tokens`, then `quote_tokens`) |
| `dex_pools_v` | pools with symbols and decimals |
| `dex_swaps_v` | every swap as token_in / token_out / amount_in / amount_out (+ `_adj`), whatever the family |
| `dex_swaps_usd_v` | the same plus `amount_usd` |
| `dex_pool_prices_1m_v` / `_1h_v` / `_1d_v` | decimals adjusted candles |
| `dex_native_price_1h_v` / `_1d_v` | USD price of the native coin |
| `dex_pool_token_volume_1d_v` | daily volume per pool and token (legs resolved) |
| `dex_pool_volume_usd_1d_v` | daily USD volume per pool |
| `dex_protocol_volume_usd_1d_v` | daily USD volume, swaps, traders, pools per family |
| `dex_token_volume_1d_v` | daily raw / adjusted volume per token (USD for quote tokens) |
| `dex_token_volume_usd_1d_v` | daily USD volume of ANY token (swap level: filter chain + time) |
| `dex_top_pools_v` | pools by USD volume of the trailing 30 days |

These views join `tokens FINAL` and `dex_pool_current_v` whole: always filter by
`chain` (and a time range for the swap level ones).

### USD

`quote_tokens (chain, token, kind, decimals, symbol)` is user populated; the
indexer ships no chain specific data and never writes it.

* `kind = 'stable'`: worth 1 USD. `kind = 'native'`: the wrapped native coin (and
  its pseudo addresses), priced by `dex_native_price_*_v` = stable volume /
  native volume over every two token pool pairing a native with a stable, per
  bucket, last known bucket.
* A swap is valued on a stable side if it has one, else on a native side, else
  `amount_usd` is `NULL`.
* `decimals` is only a fallback for addresses without a `tokens` row and is
  `NULL` by default: a quote token without decimals anywhere is unpriceable.

```sql
INSERT INTO quote_tokens (chain, token, kind) VALUES
  (1, unhex('A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'), 'stable'),  -- USDC
  (1, unhex('dAC17F958D2ee523a2206206994597C13D831ec7'), 'stable'),  -- USDT
  (1, unhex('C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2'), 'native');  -- WETH
-- pseudo addresses of the native coin have no contract: give the decimals
INSERT INTO quote_tokens (chain, token, kind, decimals, symbol) VALUES
  (1, unhex('0000000000000000000000000000000000000000'), 'native', 18, 'ETH'),  -- Uniswap V4
  (1, unhex('EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE'), 'native', 18, 'ETH');  -- Curve
```

Retire a row by inserting it again with `kind = ''`. Nothing about USD is
materialized: the views pick every change up immediately, for all of history.

## Pool resolver (`worker.rs`, `resolve.rs`)

Pools first seen mid-history (partial sync) and all Curve pools are resolved in
the background, never on the commit path: `token0()` / `token1()` / `factory()`
/ `fee()` / `tickSpacing()` / `stable()`, Curve `coins(i)` (uint256 then int128
index), `underlying_coins(i)` and `base_pool()` for metapools. V4 and Balancer
pools are described by their events and are never called.

* `discover` is sync, bounded, drop-on-full. The DB driven backfill
  (`MISSING_POOLS_SQL`) finds whatever was dropped, so an RPC outage heals.
* Definitive failures (revert, garbage) are stored as `source = 'unresolved'`
  rows: never asked again. Transient errors and addresses without code (the
  node may lag) are never cached persistently.

## Known gaps

* **Forged events.** Anyone can deploy a contract that emits swap shaped events
  with absurd amounts, or a "factory" that announces a pool BEFORE the real one
  exists. First-event-wins and `emitter` keyed pools close the cheap attacks
  (one remains: if a reorg drops the real creation and re-creates it AFTER a
  forged one, the forgery is the first event);
  analytics that need more should restrict to factories they trust
  (`dex_pools.factory`) or cross check against `erc20_transfers`.
* **Aggregator attribution** is only `tx_to`. Multi hop routes are separate
  swaps (volume counts every hop); there is no route reconstruction and no
  solver / intent (CoW, UniswapX) attribution.
* **Curve**: tokens need RPC; `TokenExchangeUnderlying` of lending pools maps
  to `underlying_coins`, of metapools to `[coin0, base pool coins...]`; pools
  with other layouts stay unresolved for that index. `AddLiquidity` /
  `RemoveLiquidity*` (one signature per coin count) are not decoded.
* **Balancer**: `PoolBalanceChanged` (joins / exits) is not decoded; tokens
  registered in a later block than the pool lose against the first row;
  Balancer V3 is a different family.
* **Uniswap V4**: hooks are stored but not interpreted (custom curves / dynamic
  fees may make `sqrt_price_x96` meaningless); `ModifyLiquidity` has no token
  amounts; `Donate` is not decoded; pools whose `Initialize` was not indexed can
  not be resolved over RPC.
* **V3 `Collect`**, fee growth and TVL are not tracked (`Sync` gives V2 style
  reserves only).
* Not covered at all: order books, RFQ / PMM (0x, Hashflow, Native), Maverick,
  Trader Joe Liquidity Book, DODO, KyberSwap Elastic, WOOFi, GMX style perps.
* USD: one price per chain for the native coin, stables assumed at 1.00 (no
  depeg handling), multi asset pool volume is an underestimate when neither
  side is a quote token, no pricing of tokens through routes of pools.
* The resolver makes individual `eth_call`s (2 to ~20 per pool); batching through
  Multicall3 is a possible optimization.
