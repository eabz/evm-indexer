# DEX analytics (`src/dex`, migrations `0010`-`0012`)

Chain agnostic, DEX agnostic swap / liquidity / pool indexing, enabled with
`--dex`. Design: `docs/design.md` §5 and §13.

**The `dex_*` tables are chain neutral**: they are meant to hold Solana (and any
other family) beside the EVM chains, so every id is 32 bytes and every position is
`(chain, block_number, tx_index, ordinal)`. See "Identity and position" below
before writing a query - an EVM address is *padded*, not stored as 20 bytes.

**Decoding is by event family, never by router / factory registry.** A log is a
swap when its `topic0` AND its shape (topic count, data length, value ranges of
the narrow integer types, zero padding of addresses) are the ones the family
emits. A Uniswap V2 fork on a chain nobody has heard of works on day one.
`protocol` is therefore the FAMILY, not a brand.

Decoding happens in Rust (pure, no I/O); every analytic lives in ClickHouse.

**Being registry free has a price: an event is a claim of whoever emitted it.**
The rule of this module is *a wrong number is worse than a missing one* - see
"What is proven" below before trusting any number.

## Families

| protocol | pools from | swaps | liquidity |
|---|---|---|---|
| `uniswap_v2` | `PairCreated` | `Swap` (also Solidly V1 forks) | `Sync`, `Mint`, `Burn` |
| `solidly` | Solidly V1 `PairCreated` (stable flag), Velodrome V2 / Aerodrome `PoolCreated` | Velodrome V2 / Aerodrome `Swap` | `Sync(uint256,uint256)`, `Burn(address,address,..)` |
| `uniswap_v3` | `PoolCreated`, Slipstream `PoolCreated`, Algebra `Pool` / `CustomPool` | `Swap`, PancakeSwap V3 `Swap`, Algebra Integral `Swap` (Algebra V1 emits the V3 signature) | `Mint`, `Burn` |
| `uniswap_v4` | `Initialize` (PoolManager; the id must be `keccak256(PoolKey)`) | `Swap` | `ModifyLiquidity` |
| `balancer_v2` | `PoolRegistered` + `TokensRegistered` (Vault) | `Swap` (joins / exits against the pool's own BPT are dropped) | - |
| `curve` | RPC only (no creation event) | `TokenExchange` (int128, uint256 and NG payloads), `TokenExchangeUnderlying` | - |

Every `topic0` is asserted against `keccak256(signature)` in `events.rs`; every
family is tested against real mainnet logs AND the real ERC-20 transfers of the
same transactions in `fixtures.rs`.

Families that share an event (V2 `Mint`, and the V2 `Swap` that Solidly V1 forks
- Velodrome V1, Thena, Ramses, Equalizer - emit) say `uniswap_v2` on the ROW. The
per protocol views attribute by the POOL (its creation event or its own
getters), falling back to the row's value when the pool is not trusted.

## What is proven

### Swap legs: corroboration against ERC-20 transfers (`corroborate.rs`)

`decode` receives every log of a batch (whole blocks, hence whole transactions).
A swap leg is **verified** when the token ITSELF reports the movement:

* an ERC-20 `Transfer` (3 topics, 32 data bytes) in the SAME transaction,
* of EXACTLY the leg's amount (no tolerance),
* `to` the emitter of the swap (in leg) / `from` it (out leg),
* emitted by a contract other than the emitter, and - when the event names the
  token (Balancer) - by that token,
* for pools that are their own contract only transfers BEFORE the swap event
  count (they move the tokens, then emit); the singletons (Balancer Vault,
  Uniswap V4 PoolManager) settle after their events, any position counts,
* transfers of two different tokens matching the same leg => unverified,
* one transfer verifies one leg (a contract can not emit a thousand swaps over
  one real transfer).

The proven token's address is stored in `dex_swaps.verified_in` /
`verified_out` (zero bytes = unverified). **Every USD number is built on
verified legs only.** Bonus: token identity and valuation of a verified swap
need no pool metadata and no RPC - a pool first seen mid-history (and every
Curve pool) is valued immediately.

Bluntly, per family:

| family | what gets verified | what never does |
|---|---|---|
| V2 / Solidly | both legs of ordinary swaps (checked on real transactions) | fee-on-transfer legs whose `Transfer` amount differs from what the pair accounts (the OTHER leg still verifies, and values the swap); a token with both an in and an out amount in one swap; same-sign swaps |
| V3 / Algebra / Pancake | both legs | fee-on-transfer legs (V3 does not support them anyway) |
| Curve | both legs of plain pools, incl. which coins they are | native coin legs (no `Transfer`); legs a pool wraps / unwraps internally |
| Balancer V2 | single swaps: both legs | batch swaps: only the first in and the last out reach the Vault, middle hops are unverified; swaps paid from internal balance |
| Uniswap V4 | a leg whose currency is settled by ONE transfer of exactly that amount | anything netted: the PoolManager settles once per currency and transaction, so multi hop / multi swap transactions mostly do not verify (in the real fixture: 1 of 4 legs); native ETH legs |

**What verification does NOT prove: that the movement was a trade at a market
price.** The owner of a fake pool can move real tokens through it, and flash
loans make that free. Wash VOLUME is therefore possible (as it is on real
pools), wash PRICES are dampened, not excluded (see the native price below).
Volume headlines are upper bounds on real activity, not audited figures.

### Singleton emitters: `dex_trusted_emitters`

V4 and Balancer pools can not be asked anything over RPC, and any contract can
emit their events. Their swaps are valued ONLY when the emitter is listed by the
operator (no seed rows ship; addresses are raw bytes):

```sql
-- emitter is a 32 byte id: an EVM address is left padded with 12 zero bytes.
INSERT INTO dex_trusted_emitters (chain, emitter, protocol) VALUES
  (1, unhex(concat(repeat('00', 12), 'BA12222222228d8Ba445958a75a0704d566BF2C8')), 'balancer_v2'),  -- Vault (same on most chains)
  (1, unhex(concat(repeat('00', 12), '000000000004444c5dc75cB358380D2e3dE08A90')), 'uniswap_v4');   -- PoolManager, Ethereum
```

Retire a row with `protocol = ''`. `price_source = 1` on any row of a chain
restricts the native price of that chain to the listed emitters (pool addresses
for the contract families) - the one defence against wash priced fake pools that
does not depend on counting pools.

### Pool metadata: `dex_pool_current_v`

`dex_pools` holds CLAIMS: one row per creation event (anyone can emit
`PairCreated(tokenA, tokenB, anyAddress)`, and V2 / V3 addresses are predictable,
so even BEFORE the real creation) plus at most one row of the RPC resolver. Read
pools only through `dex_pool_current_v`, which adds a `status`:

| status | meaning | trusted |
|---|---|---|
| `verified` | the pool answered `token0()` / `token1()` / `coins(i)` itself. That answer WINS over every event; factory / fee / created_block come from the first event naming the same tokens | 1 |
| `event` | singleton family: only the singleton emits for its own `(pool_id, emitter)` key (whether the emitter is the real one: `dex_trusted_emitters`) | 1 |
| `unverified` | creation event(s) naming one token set, pool not asked yet | 0 |
| `contested` | creation events naming different token sets | 0 |

Metadata dependent views (`dex_pools_v` symbols / decimals, `dex_pool_prices_*_v`,
token resolution of UNVERIFIED legs in `dex_swaps_v`, protocol attribution) use a
pool only when `trusted = 1` and yield NULL / nothing otherwise. The background
worker asks every contract pool that trades - contested ones and the most active
first. Without `--rpc` contract pools are never trusted: USD numbers still work
(verified legs), decimals adjusted pool prices do not.

## Conventions

* **Signed amounts are pool relative: positive = INTO the pool**, negative = out
  (the Uniswap V3 convention). V2 / Solidly `in - out` pairs are netted into it.
  **Uniswap V4 reports caller relative deltas and is negated** (checked against
  the ERC-20 transfers of a real transaction). Mints positive, burns negative.
  V3 `Burn` amounts are the amounts owed, not yet collected. `-2^255` is refused
  everywhere (it has no absolute value).
* `amount_in` / `amount_out` are filled for EVERY family (two token families:
  the positive / negative side; both zero when the signs do not describe a
  swap). Balancer adds the `token_in` / `token_out` its event names (a claim),
  Curve `coin_in` / `coin_out` / `underlying` (indices into `dex_pools.tokens` /
  `underlying_tokens`). V2 / Solidly swaps carry `reserve0` / `reserve1`: the
  `Sync` the pair emits right before its `Swap`.
* `pool_id` is 32 bytes: pool address left padded, or the native `bytes32` id
  (V4, Balancer). `emitter` is the contract that emitted the event and is part
  of a pool's identity everywhere.
* **Position is `(chain, block_number, tx_index, ordinal)`** in every table.
  On EVM `tx_index` is the transaction's index in the block and `ordinal` the
  log index; `block_number` keeps its name and holds the slot on Solana.
  `tx_id` is the raw transaction id (32 bytes on EVM, 64 on Solana) and is
  never part of a sorting key.
* **`tx_from` / `tx_to` are the sender and the target of the TRANSACTION**, filled
  on swaps AND liquidity rows by `DexRows::attach_transactions` from the
  transactions of the same batch. `dex_liquidity.tx_from` is who seeded (or
  pulled) a pool's liquidity - the event `sender` is usually a router, never use
  it for attribution. `dex_swaps.trader` = `tx_from` when attached, else the
  event's recipient, else its sender. `tx_to` is the contract the user called:
  router / aggregator attribution without a registry.
* Ids are raw bytes (`FixedString(32)`), never hex. Query base tables with
  `FINAL`. How to format and compare them: next section.

## Identity and position: 32 byte ids (`docs/design.md` §13)

Every identity column - `pool_id`, `emitter`, `factory`, `token0` / `token1` /
`tokens`, `hooks`, `sender`, `recipient`, `owner`, `tx_from`, `tx_to`, `trader`,
`token_in` / `token_out`, `verified_in` / `verified_out`, `quote_tokens.token`,
`dex_trusted_emitters.emitter` - is `FixedString(32)`:

| family | encoding |
|---|---|
| EVM | 12 zero bytes + the 20 address bytes |
| Solana | the 32 raw pubkey bytes |

"Unknown" is the 32 zero bytes, `toFixedString('', 32)`.

**Comparing.** Pad the address:

```sql
WHERE token = unhex(concat(repeat('00', 12), 'A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'))
```

**Printing.** The bytes do not say which family they belong to, so register the
chain once in `chains` (migration `0006`) and read it through `chains_v`:

```sql
INSERT INTO chains (chain, name, family) VALUES
  (1, 'ethereum', 'evm'),
  (8453, 'base', 'evm'),
  (1399811149, 'solana', 'svm');   -- family is 'evm' or 'svm'
```

Then use THE one expression, everywhere (`id` is any identity column):

```sql
SELECT if(c.family = 'svm',
          base58Encode(substring(s.trader, 1, 32)),
          concat('0x', lower(hex(substring(s.trader, 13))))) AS trader
FROM dex_swaps AS s FINAL
LEFT JOIN chains_v AS c ON c.chain = s.chain
WHERE s.chain = 1
```

`substring()` is **not** decoration. Turning a `FixedString` into a `String` -
`toString(id)`, `CAST(id AS String)`, and the implicit conversion
`base58Encode(id)` performs - **trims trailing zero bytes**, so
`base58Encode(id)` silently encodes a shortened pubkey. `substring(id, 1, 32)`
and `concat(id, '')` keep every byte.

**A `pool_id` is not an address**, even on EVM (a Uniswap V4 or Balancer id is a
native 32 byte value), so `dex_pools_v.pool` prints all 32 bytes and must never
go through the `'evm'` branch.

**The seam with the EVM-only tables.** `tokens` (and the transfer tables) stay
EVM shaped, `address FixedString(20)`. `dex_token_info_v` therefore pads the
address UP to 32 bytes; it never truncates the analytics side, so a Solana token
finds no `tokens` row instead of matching one that happens to share its last 20
bytes.

In Rust nobody hand rolls the padding: `crate::utils::format` has `id32` /
`address_of_id32`, the `SerId32` / `SerVecId32` serializers, and `tx_id` /
`tx_hash_of` / `SerTxId`. Reading an id whose 12 leading bytes are not zero is
an error, never a truncation.

## Reorgs: insert-only (docs/design.md §2)

The indexer never issues DELETE / ALTER DELETE / DROP PARTITION.

* **Base tables** (`dex::BASE_TABLES`: `dex_swaps`, `dex_liquidity`, `dex_pools`)
  are `ReplacingMergeTree(_version, is_deleted)`. `purge_range` INSERTs
  tombstones with `dex::tombstone_sql(table, chain, from, to, version)`;
  `FINAL` hides the rows. A re-streamed row at the same position carries a
  newer version and is alive again; positions the canonical block does not
  have stay dead.
* **Side tables** (`dex::SIDE_TABLES`) are never touched: their materialized
  views pass `_version`, `is_deleted` and `epoch` through.
* **`dex_pools`** is purged by `created_block`, event rows only
  (`dex::purge_filter`). Resolver rows are chain STATE, no purge touches them.
* **Aggregates** carry `epoch` as the last key column, their views only
  aggregate live rows. A purge records `(chain, epoch, from_ts)` in `reorgs` and
  runs `dex::derived::rebuild_statements(table, chain, from_ts, to_ts, epoch)`
  for every `DEX_DERIVED` table - **one INSERT per month**: a single INSERT over
  more than 100 monthly partitions (a gap heal deep in history) is refused by
  ClickHouse. The `*_v` views apply the validity rule BEFORE merging states:

```sql
FROM dex_candles_1h AS a
ASOF LEFT JOIN dex_epoch_floor_v AS f ON f.chain = a.chain AND f.from_ts <= a.bucket
WHERE a.epoch >= ifNull(f.epoch_floor, 0)   -- ifNull: join_use_nulls = 1 profiles
GROUP BY chain, pool_id, emitter, bucket
```

Never read an aggregate table directly, only its `*_v` view.

## Tables (`0010_dex_tables.sql`)

Base tables are partitioned by month only (chain is the first key column); side
tables and `dex_pools` by chain. Always read with `FINAL`.

| table | key | purged by |
|---|---|---|
| `dex_pools` | `(chain, pool_id, emitter, created_block, tx_index, ordinal)` | tombstones on `created_block`, event rows only |
| `dex_swaps` | `(chain, block_number, tx_index, ordinal)` | tombstones on `block_number` |
| `dex_liquidity` | `(chain, block_number, tx_index, ordinal)` | tombstones on `block_number` |
| `dex_swaps_by_pool` (MV) | `(chain, pool_id, block_number, tx_index, ordinal)` | follows `dex_swaps` |
| `dex_swaps_by_trader` (MV) | `(chain, trader, block_number, tx_index, ordinal)` | follows `dex_swaps` |
| `dex_pools_by_token` (MV) | `(chain, token, pool_id, emitter, block_number, tx_index, ordinal)` | follows `dex_pools` (a claim index: join `dex_pool_current_v`) |
| `dex_pool_current_v` (view) | what is known about every pool, and how well | - |
| `quote_tokens`, `dex_trusted_emitters` | user data | never purged |

## Aggregates (`0011_dex_aggregates.sql`)

`AggregatingMergeTree` (key ends in `epoch`, partitioned by month) + materialized
view + finalizing `*_v` view, each declared in `dex::DEX_DERIVED` with a
`rebuild_sql` that repeats the view's `SELECT` (unit tested).

| table | bucket | content |
|---|---|---|
| `dex_candles_1m` / `_1h` / `_1d` | 60 / 3600 / 86400 | per pool (two token families) two price series, volume0, volume1, swaps, unique traders |
| `dex_pool_volume_1h` | 3600 | per pool and VERIFIED `(token_in, token_out)`: volume in / out, swaps, traders. The base of every USD number, of per pool / protocol activity and of the resolver's work list |

A candle's `open` / `close` is the price at the smallest / largest POSITION of
the bucket: the `argMinIf` / `argMaxIf` states order by the
`(block_number, tx_index, ordinal)` tuple of §13, not by a `(block, log index)`
pair.

Candle prices are token1 per token0 in RAW units, NULL when the bucket has no
swap that defines them:

* `open/high/low/close` - TRADE prices `|amount1| / |amount0|` of swaps with
  opposite signs and at least 1000 raw units on both sides (a dust swap of 10
  for 1 is not a price). Fee included.
* `pool_open ... pool_close` - the POOL price after each swap: `(sqrt_price_x96
  / 2^96)^2` (V3 / V4 / Algebra), `reserve1 / reserve0` (V2 / Solidly). Exact for
  concentrated liquidity and constant product pools, WRONG for Solidly stable
  pools - which is why it is a separate series. `dex_pool_prices_*_v` picks the
  pool series unless the (trusted) pool is `stable`.

**Precision.** Prices and volumes are `Float64` computed inside SQL from the
`UInt256` / `Int256` columns: ~15.9 significant digits. Volumes are Float64 on
purpose: `sum()` over 256 bit integers wraps silently and spam tokens emit
amounts near 2^256 (tested with `2^256 - 1`). Exact amounts stay in `dex_swaps`.

## Analyst views (`0012_dex_views.sql`)

| view | what |
|---|---|
| `dex_token_info_v` | symbol / decimals / quote kind per token. A `tokens` row with no name, no symbol and 0 decimals means UNKNOWN decimals (NULL), not 0 |
| `dex_pools_v` | pools with status, symbols and decimals (trusted pools only) |
| `dex_swaps_v` | every swap as token_in / token_out / amount_in / amount_out (+ `_adj`), with `token_*_verified` |
| `dex_swaps_usd_v` | the same plus `native_price`, `amount_usd` |
| `dex_native_price_1h_v` | USD price of the native coin per complete hour |
| `dex_pool_volume_usd_1h_v` | the valuation rule on the hourly aggregate |
| `dex_pool_volume_usd_1d_v` | daily USD volume, swaps, priced_swaps, traders per pool |
| `dex_protocol_stats_1d_v`, `dex_protocol_volume_usd_1d_v` | per protocol (attributed by pool) |
| `dex_token_volume_1d_v` | per token, verified legs only. `volume_usd` is the FULL value of the swaps the token took part in: summing it over tokens counts every swap twice |
| `dex_pool_prices_1m_v` / `_1h_v` / `_1d_v` | decimals adjusted candles of trusted pools |
| `dex_top_pools_v` | pools by USD volume of the trailing 30 days |

These views join `tokens FINAL` and `dex_pool_current_v` whole: always filter by
`chain` (and a time range for the swap level ones).

### USD: ONE valuation rule

A swap is valued once, by its best VERIFIED leg: a `stable` quote token (in
before out, worth 1) before a `native` one (in before out, at the native price),
else NULL - never 0. `dex_pool_volume_usd_1h_v` applies the same rule to the
hourly aggregate (all swaps of a row share their verified tokens and their hour,
so the row's value IS the sum of its swaps' values) and every rollup is a sum of
those. `priced_swaps / swaps` says how much of the activity could be valued.

**Native price** (`dex_native_price_1h_v`): only swaps with BOTH legs verified,
one native and one stable, of contract pools or trusted singleton emitters. Per
pool the hour's volume weighted price; pools with less than 1000 stable units of
volume in the hour do not vote; the price is the MEDIAN over the pools. It is
applied only to LATER hours (no look ahead into the running hour) and for at most
24 hours, then it is NULL. A median can be outvoted by many wash traded fake
pools: set `price_source = 1` rows to pin the price to pools you trust.

`quote_tokens (chain, token, kind, decimals, symbol)` is user populated; the
indexer ships no chain specific data and never writes it:

```sql
-- token is a 32 byte id: an EVM address is left padded with 12 zero bytes.
INSERT INTO quote_tokens (chain, token, kind) VALUES
  (1, unhex(concat(repeat('00', 12), 'A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48')), 'stable'),  -- USDC
  (1, unhex(concat(repeat('00', 12), 'dAC17F958D2ee523a2206206994597C13D831ec7')), 'stable'),  -- USDT
  (1, unhex(concat(repeat('00', 12), 'C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2')), 'native');  -- WETH
-- pseudo addresses of the native coin have no contract (and no Transfer, so
-- their legs never verify): they only matter for display
INSERT INTO quote_tokens (chain, token, kind, decimals, symbol) VALUES
  (1, unhex(concat(repeat('00', 12), '0000000000000000000000000000000000000000')), 'native', 18, 'ETH'),
  (1, unhex(concat(repeat('00', 12), 'EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE')), 'native', 18, 'ETH');
-- on Solana a token id is the 32 pubkey bytes, so no padding:
-- (1399811149, base58Decode('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'), 'stable')
```

`decimals` is only a fallback for addresses without a `tokens` row and is NULL by
default: a quote token without decimals anywhere is unpriceable. Retire a row by
inserting it again with `kind = ''`. Nothing about USD is materialized: the views
pick every change up immediately, for all of history.

## Pool resolver (`worker.rs`, `resolve.rs`)

Never on the commit path. Asks every contract pool that trades or is announced:
`token0()` / `token1()` / `factory()` / `fee()` / `tickSpacing()` / `stable()`,
Curve `coins(i)` (uint256 then int128 index), `underlying_coins(i)` and
`base_pool()` for metapools. V4 and Balancer pools are never called.

* `discover` is sync, bounded, drop-on-full. The backfill (`MISSING_POOLS_SQL`)
  is driven by the small hourly aggregate, in a stable order: contested pools
  first, then by number of swaps, then by id.
* EVERY verdict is persisted, so dead emitters leave the list instead of filling
  its pages: `rpc` (answered), `unresolved` (reverts / garbage: not a pool, never
  asked again), `no_answer` (no code, or 3 transient failures while the RPC
  answered for others: asked again after 1 h x 2^attempts, at most 30 days).
  A general RPC outage persists nothing (circuit breaker).

## Known gaps

* **Wash trading.** See "What is proven": verified volume is real token
  movement, not necessarily real trading. No registry free system can tell.
* **Coverage of verification** is partial for V4 (netted settlement), Balancer
  batch swaps, native coin legs and fee-on-transfer legs: those swaps are in the
  data with NULL USD. `priced_swaps / swaps` quantifies it.
* **Aggregator attribution** is only `tx_to`. Multi hop routes are separate
  swaps (volume counts every hop); no route reconstruction, no solver / intent
  (CoW, UniswapX) attribution.
* **Curve**: `AddLiquidity` / `RemoveLiquidity*` are not decoded; pool metadata
  needs RPC (swap valuation does not).
* **Balancer**: `PoolBalanceChanged` (joins / exits) is not decoded; tokens
  registered in a later block than the pool lose against the first row; Balancer
  V3 is a different family.
* **Uniswap V4**: hooks are stored but not interpreted; `ModifyLiquidity` has no
  token amounts; `Donate` is not decoded.
* **V3 `Collect`**, fee growth and TVL are not tracked (`Sync` gives V2 style
  reserves only).
* Not covered at all: order books, RFQ / PMM (0x, Hashflow, Native), Maverick,
  Trader Joe Liquidity Book, DODO, KyberSwap Elastic, WOOFi, GMX style perps.
* USD: one price per chain for the native coin, stables assumed at 1.00 (no
  depeg handling), no pricing of tokens through routes of pools.
* The resolver makes individual `eth_call`s (2 to ~20 per pool); batching through
  Multicall3 is a possible optimization. `MISSING_POOLS_SQL` aggregates the whole
  hourly table of a chain per run (small, but not bounded in time).
