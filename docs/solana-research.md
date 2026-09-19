# Solana: DEX + launchpad data for this indexer - research

STATUS: COMPLETE. Analyst: solana-research.
Retrievals 2026-09-18 and 2026-09-19 01:20-04:44 UTC; every timestamp is stated
inline. Read the executive summary, then section 0 if you are deciding table
shapes this week.

## RAW NOTES (the measurements the sections below are built from; kept for re-checking)

### Market size (DefiLlama, retrieved 2026-09-19 01:22-01:23 UTC)
- `https://api.llama.fi/overview/dexs?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` -> all chains: 24h $11.21B, 7d $64.55B, 30d $302.19B.
- Per-chain endpoint `.../overview/dexs/<chain>`: solana 24h $3.09B (27.5%), 7d $15.51B (24.0%), 30d $78.77B (26.1%) - RANK 1.
  ethereum 30d $42.76B (14.2%), robinhood $41.23B (13.6%), bsc $37.27B (12.3%), base $29.07B (9.6%), hyperliquid $12.19B, polygon $7.13B, arbitrum $6.48B, near $5.18B, monad $4.73B, avalanche $3.17B.
- Solana chain total = categories Dexs ($75.57B) + Launchpad ($3.22B); trading apps / bots (fomo $7.6B, Axiom $2.3B, ...) are listed but NOT in the total (double counting).
- Top Solana venues 30d (share of $78.77B): PumpSwap 18,200M 23.1% | BisonFi 9,322M 11.8% | Orca 7,891M 10.0% | Raydium (AMM v4+CPMM+CLMM combined) 7,475M 9.5% | Meteora DLMM 6,327M 8.0% | Manifest 4,309M 5.5% | Tessera V 3,207M 4.1% | Scorch 2,985M 3.8% | HumidiFi 2,784M 3.5% | pump.fun curve 2,530M 3.2% | QuantumAMM 1,891M 2.4% | Jupiterz (RFQ) 1,742M 2.2% | GoonFi 1,738M 2.2% | AlphaQ 1,691M 2.1% | Deriverse 1,430M 1.8% | SolFi V2 1,011M 1.3% | PancakeSwap v3 739M 0.9% | Aquifer 692M 0.9% | Byreal 592M 0.8% | Jupiter Lend DEX 586M 0.7% | Meteora DAMM v2 300M | Meteora DBC 300M | Archer 250M | StonkFun 242M | Whalestreet 155M | LaunchLab 149M | Quay 99M | Meteora DAMM v1 53M.
- Cumulative: top 5 = 62.5%, top 9 = 79.3%, top 14 = 91.5%, top 17 = 95.6%.
- DefiLlama PumpSwap volume is FILTERED (pools with TVL >= $5k and >= 50 traders, "filters out wash trading pools"); raw on-chain volume is larger.

### Program ids + how Dune's open-source spellbook (duneanalytics/spellbook, dbt_subprojects/solana/models/_sector/dex) derives trades
- Generic macro `solana_amm_base_trades`: swap = instruction of program P (filtered by 1-byte or 8-byte discriminator) + the SPL transfers at inner_instruction_index +1 and +2. Used for: BisonFi `BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi` (d1 02/07), HumidiFi `9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp`, Scorch `SCoRcH8c2dpjvcJD6FiPbCSQyQgu3PcUAWj2Xxx3mqn`, GoonFi `goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j` + v2 `goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE`, Tessera `TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH` (d1 10/11), ZeroFi `ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY`, SolFi v2 `SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF`, Aquifer `AQU1FRd7papthgdrwPTTq5JacJh8YtwEXaBfKU3bTz45`, AlphaQ `ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA`, Obric `obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y`, Manifest `MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms` (d1 0d/04), Whalestreet `FW6zUqn4iKRaeopwwhwsquTY6ABWLLgjxtrC3VPnaWBf`, Byreal `REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2`. QuantumAMM `QuaNtZsgYRe5Z9Bk4LZ4cTD9tbkVoyCNf1R2BN9bBDv`, Quay `QUayE6nexQWYNZAEqfN8FxoNwQDSu3CAzT2qq9J1ArG` same pattern in DefiLlama adapters.
- IDL-decoded: PumpSwap `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` (buy/sell calls), Raydium AMM v4 `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`, CPMM `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`, CLMM `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`, Orca Whirlpool `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`, Meteora DLMM `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` (evt swap), Meteora DBC `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`, pump.fun `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`, Phoenix, Lifinity, Jupiterz (order_engine fill).
- Routers seen: Jupiter v6 `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`, DFlow `DF1ow4ts...`, `proVF4pM...`, GMGN `GMGNreQc...`, Ave `AveaiuA1...`, `term9YPb...`, `MAyhSmzX...`.

### Real transactions fetched from https://api.mainnet-beta.solana.com (getTransaction jsonParsed), 2026-09-19 ~01:25-01:35 UTC
- PumpSwap, slot 448258071, sig `Qxpfmre4JbRctxg1JKJ7vk5Rze6XGpX36bqkm34dgRe5vvCWt4mkRhDe6Zu999TomKnJ14DetevDDURGsgPLaMb`: ONE tx, TWO signers, TWO PumpSwap swaps on the same pool in opposite directions (ix #3 Sell disc 33e685a4017f83ad, ix #6 Buy disc 66063d1201daebea). Each: 3-4 transferChecked children (user->vault, vault->user, protocol fee, creator fee) + self-CPI Anchor event (`e445a52e51cb9a1d` prefix). Net vault delta over the tx is ~0.03 SOL while 6.2 SOL traded each way -> tx-level pre/post balance deltas CANNOT recover the trades; instruction-level transfers can.
- Jupiter 3-hop, sig `66R3Ucvpa4KtxFoUCgnkZY9DYv7BptgrCyMamt6Dzz2WhGskmUphS446gnStzH6fnroPZv52FvZmQXGsxRYp2H94`: SOL -> USDC on BisonFi (no event, 2 transfers at depth+1) -> token on Meteora DLMM (2 transfers + 2 self-CPI events) -> Token-2022 token on Raydium CPMM (`Program data:` log event disc 40c6cde8260871e2); Jupiter SwapEvent self-CPI; Jupiter fee transfer 7075 lamports-WSOL. Token-2022 transfer fee visible: vault sent 32,982,064,366, user received 32,652,243,722.
- Prop AMM direct txs are QUOTE UPDATES, not swaps: BisonFi sig `5URSqMNt5q221ipYVVSMoNrF4Vb8XDtFtzinVSQfNCA8ve8croiTyw4bAiwvGDMUMwLevwxT8dWo8VjJJR9d2tDv` (379 CU, no token movement); HumidiFi sig `GJ4CUabz7S6Hi3youX7BmPxWXBYr9e4LWNKiMuNS8HQpNQZCcf836UxvhotqrtNdKXdUhtKH3PwzpvkHQYn3aEm` (503 CU). 8 of 8 sampled direct HumidiFi txs were updates; HumidiFi instruction data looks obfuscated (no stable discriminator).
- Event mechanisms observed: Orca Whirlpool `Program data:` (disc e1ca49af932ba096, 121 bytes); Raydium AMM v4 `Program log: ray_log: <base64>`; Raydium CLMM + CPMM `Program data:` disc 40c6cde8260871e2; Manifest `Program data:` fill logs; Tessera only `Program log: S-00: ...`; BisonFi/HumidiFi nothing; pump.fun + PumpSwap + DLMM + Jupiter self-CPI events.
- Jupiter sample (22 successful route instructions): hops 1:4, 2:10, 3:5, 4:3. Venues incl. unknown-to-me programs `riptK81h...`, `ghosty4Z...`, `BSwp6bEB...`.

### Envio Solana HyperSync (docs.envio.dev/docs/HyperSync/solana and /solana-query, retrieved 01:37 UTC; crates 0.2.0)
- Filters: instruction_calls {executing_account, d1/d2/d4/d8, a0..a9, is_inner, tx_success}; transactions {fee_payer, transaction_id, success}; logs {program_id, kind}; account_activity {kind, account, mint, owner, program_id, flags}. AND inside a selection, OR across selections in one array, **different arrays are INTERSECTED**.
- "Join behavior: ... There is currently a single default join mode; finer control (matched rows only, or all rows of matched transactions) is planned."  -> whether sibling/child instructions of a matched instruction come back is NOT documented. MUST PROBE.
- History: "around slot 403,000,000 as of September 2026", backfill "prioritized by demand". Slot 403,000,000 block time = 1772170820 = 2026-02-27 05:40 UTC (public RPC getBlockTime).
- /height vs public RPC getSlot (3 samples 01:38 UTC): Envio head was 33-44 slots behind processed/confirmed and 4-12 slots behind `finalized` -> the server appears to serve at about finalized. Not documented.
- `/query` without token -> 401. `/height` open.
- Client 0.2.0 (2026-08-12), repo 0 stars, 1 open issue, last push 2026-08-12. stream_arrow has no live-tail mode (errors "server made no progress" at head) and no reorg handling; we must write the head follower.
- Chain facts: slot time now ~0.267 s (216,000 slots in 57,669 s, slots 448,044,000-448,260,000); was ~0.40 s until ~slot 430M. getRecentPerformanceSamples: ~96k-114k non-vote tx/min (~1,600-1,900 TPS non-vote), ~250k-265k total/min. Transaction version 1 now exists (public RPC demands maxSupportedTransactionVersion:1).

### Live probes of solana.hypersync.xyz with the project token (2026-09-19 04:40-04:44 UTC, ~12 requests, all `x-ratelimit-cost: 0`)
- JOIN SEMANTICS (slot 448258095, filter = BisonFi inner instruction): response = the ONE matched instruction row `[5,1]` (NOT its child SPL transfers `[5,1,0]`, `[5,1,1]`), + the parent transaction, + ALL log rows of that transaction (10, each with `instruction_address`), + ALL account_activity rows of that transaction (12, incl. unchanged token accounts, with mint/owner/decimals/pre/post).
- HISTORY START: query from slot 0 is served from slot **391,000,000** (block_time 1767425822 = 2026-01-03 07:37 UTC) - deeper than the documented ~403M (2026-02-27): backfill is moving.
- Row caps: without `max_num_instructions` a response stopped after ~700 rows (1-2 slots). With `max_num_instructions: 200000`: 107 slots, 33,886 instruction rows, 5.99 MB JSON in 2.7 s.
- DEX/launchpad program rows (26 programs, successful txs), 6 windows x 30 slots spread over 24h: 288-415 rows/slot (avg 346), 142-186 distinct txs/slot (avg 168). Per slot: PumpSwap 66.9 outer + 9.8 inner + 76.7 self-CPI events; Meteora DLMM 4.5/7.4/20.0; DAMM v2 13.6/2.1/15.7; pump.fun 3.8/8.1/11.8; DBC 4.6/0/9.3; prop AMMs OUTER (= quote updates): Quantum 15.5, Tessera 13.3, HumidiFi 13.1, BisonFi 6.7; prop AMMs INNER (= swaps): BisonFi 3.9, Tessera 1.9, HumidiFi 1.8, Quantum 1.7; Orca 6.3, Raydium CLMM 5.6, CPMM 4.6, v4 3.0, Manifest 4.2.
- All SPL Token + Token-2022 transfer instructions (d1 03/0c), one 100-slot window: 494 rows/slot (91% inner).
- Base (public RPC `https://mainnet.base.org`, eth_getLogs, 6 x 50 blocks over 24h, topic0 in {V2, V3, V4, Solidly, Pancake-V3 Swap}): 45.2 swap logs/block = ~2.0M/day.

## Executive summary

- **Solana is the biggest DEX chain there is.** $78.8B in 30 days = 26% of all
  DEX volume on every chain, rank 1, more than Ethereum ($42.8B) and Base
  ($29.1B) put together. 40% of it is routed through aggregators like Jupiter,
  which are not venues and must never be added to venue volume.
- **Skipping it would blind the launchpad module exactly where it matters.**
  pump.fun tokens graduate into Solana AMM pools; without Solana DEX data the
  chart stops at the moment the token starts trading for real.
- **It is feasible with what we already have.** Envio runs a Solana HyperSync
  with the same query API, the same API token (all my probes billed 0), the same
  reorg guard. I probed it live: it serves the instruction tree position, the
  per-transaction token balances with decimals, and 8.5 months of history -
  deeper than its own docs claim.
- **Decoding is harder than EVM in one way and easier in another.** Harder:
  there is no shared event ABI, and about a third of the volume comes from
  "dark" prop AMMs that publish nothing. Easier: a Solana program cannot fake a
  token balance change, so the forgery check we had to build for EVM swaps is
  built into the data instead.
- **So the decoder has two layers, both always on:** a generic one that reads
  actual token movement inside each instruction (works on 100% of venues, gives
  price, volume and the real trader) and per-program decoders for the ~8 venues
  that publish their format (~60% of volume, adds exact fees and pool depth).
- **One warning found in real data:** you cannot read a Solana trade from a
  transaction's net balance change. I found a single transaction holding two
  opposite 6.2 SOL swaps on the same pool that net to 0.03 SOL. The decoder must
  work per instruction, not per transaction.
- **Volume is large but manageable if we filter.** Measured: ~52M swaps a day
  (~26x Base), ~8.6 GB/day compressed, ~3 TB a year. That is fine. Indexing
  Solana the way we index an EVM chain is not: Solana would be an
  **analytics-only** pipeline, with no wallet history and no chain-wide
  transfers, and the schema must say so rather than serve half-truths.
- **Architecture: one binary, one database.** `indexer run --chain solana` adds
  another data source and a Solana core module, and writes into the *same*
  `dex_*` and `launchpad_*` tables as EVM. Not a separate project - a separate
  project makes the one screen you asked for (curve -> graduation -> AMM candles
  on one chart) impossible.
- **THE TIME-SENSITIVE DECISION, and it is independent of whether we ever build
  Solana:** the shared analytics tables must use 32-byte identity columns and a
  `(chain, block_or_slot, tx_index, ordinal)` position key **now**, while no data
  is loaded and the DEX and launchpad tables are being written this week. Cost
  now: about a day per module. Cost later: these columns sit in the sort key of
  every table and materialized view, ClickHouse cannot change a sort-key column,
  so it becomes a full rebuild across 50+ chains - or two parallel sets of tables
  forever. Standing cost of doing it now if Solana never happens: a few
  compressed zero bytes. *(Answer already sent to the launchpads engineer.)*
- **Effort: ~7-9 weeks** to a continuous pump.fun -> PumpSwap chart with USD
  prices and history, delivering something visible from week 4.
- **Main risks:** the Solana client is 5 weeks old and its own changelog says it
  was dropping up to 99% of rows on dense ranges until this release; history is
  only 8.5 months (genesis depth would need the free Old Faithful archive as a
  one-off backfill); and a third of the volume will always be "we know the price
  and size, not the pool's internals".

## 0. Time-sensitive decision: shared table identity + position key

**Answer: yes, change the chain-neutral analytics tables now** (sent to `launchpads` and
`lead` on tirith 2026-09-19 04:39 UTC). Scope: tables of the analytics DATA MODULES
(`dex_*`, `launchpad_*`, and `prediction_*` where they carry addresses). NOT the EVM
`core` tables (`blocks`, `transactions`, `logs`, transfers): those stay EVM-shaped, and
Solana gets its own small `sol_*` core tables (section 6).

| Topic | Today (EVM only) | Chain-neutral rule | Why |
|---|---|---|---|
| Identity columns (pool, token, trader, creator, emitter, factory, recipient, tx_from, tx_to) | `FixedString(20)` (but `pool_id` is already `FixedString(32)`, left-padded) | `FixedString(32)`; EVM address = 12 zero bytes + 20 address bytes; Solana pubkey = 32 raw bytes | a Solana pubkey is 32 bytes (verified: every program id / account in section 2). The zero prefix compresses to almost nothing. One rule for every id column instead of "20 here, 32 there" |
| Transaction id | `transaction_hash FixedString(32)` | `tx_id String` (raw bytes: 32 on EVM, 64 on Solana) | a Solana signature is 64 bytes (`Signature([u8; 64])` in `hypersync-solana-net-types`). It is never a sorting-key column in analytics tables, so `String` costs nothing |
| Position key | `(chain, block_number, log_index)` | `(chain, block_number, tx_index, ordinal)` | Solana has no block-global log index. Position = slot + transaction + place in the instruction tree |
| `block_number` | block | keep the NAME; holds the slot on Solana | purge / tombstone / checkpoint code keys on this column name (`db::block_number_column`); renaming buys nothing |
| `tx_index UInt32` | (not in the key) | EVM: `transaction_index`; Solana: Envio's `transaction_index` (dense rank over non-vote transactions of the slot, documented as uniform across ingest sources) | |
| `ordinal UInt64` | `log_index UInt32` | EVM: `log_index`. Solana: the instruction tree path packed into one integer (below) | must be computable from ONE row, because a program-filtered stream never sees the sibling instructions needed to compute a flat rank |
| Amounts | `UInt256` / `Int256` | unchanged | SPL amounts are u64 |
| `chain UInt64` | EIP-155 id | unchanged; Solana = one reserved constant + a `chains` registry table (`chain`, `name`, `family` = `evm` \| `svm`) — constant proposed in section 6.6 (no standard exists) | views need the family to format ids: `concat('0x', lower(hex(substring(x, 13))))` vs `base58Encode(x)` (ClickHouse built-in) |

Solana `ordinal`: Envio serves `instruction_address` as the full tree path
(`[2]` = third top-level instruction, `[2, 0]` = its first child; doc comment in
`hypersync-client-solana-0.2.0/src/simple_types.rs`: "depth AND parentage AND sibling
order"). Solana's CPI stack height limit is 5, so a path has at most 5 elements. Pack
`(index + 1)` of each level into 12 bits, left-aligned in a `UInt64` (absent levels = 0):
a parent sorts before its children, siblings sort in execution order, and the value is
unique inside a transaction. (12 bits = 4,095 per level is far above what a transaction
can hold; assert and fail loudly rather than truncate.)

**Cost now vs later.** Now: no data is loaded, the DEX module is being revised this
week and the launchpad tables are being written today. The change is DDL + a `pad32()`
helper in the row models + `substring(x, 13, 20)` where an analytics view joins the
EVM-only `tokens` / transfer tables + the candle `argMin/argMax` tuple becoming
`(block_number, tx_index, ordinal)`. Roughly a day per module including the integration
tests. Later: identity columns and the position columns are part of `ORDER BY` of base
tables and MV-fed side tables (`dex_swaps_by_pool`, by trader, by token). ClickHouse
cannot alter the type of a sorting-key column, so it means new tables, `INSERT ..
SELECT` of every chain's history, and a rebuild of every MV and aggregate - for 50+
chains - or living with two parallel table families forever (every view, candle and
cookbook query duplicated). This holds **even if Solana is never built**: the cost of
being ready is a few compressed zero bytes.

What this decision does NOT require now: any Solana code, any change to `core` tables,
or a chain-neutral `tokens` table (analytics views can read a `token_registry_v` union
of `tokens` and a future `sol_tokens`; decide when Solana starts).

DDL sketch: section 6.4.

## 1. How big

Source: DefiLlama free API, retrieved 2026-09-19 01:22-01:23 UTC.
`https://api.llama.fi/overview/dexs?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true`
and `https://api.llama.fi/overview/dexs/<chain>`, plus
`https://api.llama.fi/overview/aggregators/solana` for routers.

**Solana is the number 1 DEX chain, by every window.**

| Window | All chains | Solana | Solana share | Next chain |
|---|---|---|---|---|
| 24h | $11.21B | $3.09B | 27.5% | - |
| 7d | $64.55B | $15.51B | 24.0% | - |
| 30d | $302.19B | $78.77B | 26.1% | ethereum $42.76B (14.2%) |

30d by chain after Solana: ethereum $42.76B, robinhood $41.23B, bsc $37.27B,
base $29.07B, hyperliquid $12.19B, polygon $7.13B, arbitrum $6.48B, near $5.18B,
monad $4.73B, avalanche $3.17B. **Solana alone is bigger than Ethereum + Base
combined, and ~2.7x Base.** The chain total is DefiLlama categories `Dexs`
($75.57B) + `Launchpad` ($3.22B); trading apps / bots (fomo $7.6B, Axiom $2.3B,
...) are listed but excluded from the total to avoid double counting - the same
"front ends are not venues" rule design.md section 11 already states for EVM.

### 1.1 Venues (30d, share of the $78.77B Solana total)

| # | Venue | 30d | Share | Cum. | Decoding class |
|---|---|---|---|---|---|
| 1 | PumpSwap | $18,200M | 23.1% | 23.1% | public IDL + self-CPI event |
| 2 | BisonFi | $9,322M | 11.8% | 34.9% | prop AMM, no event |
| 3 | Orca Whirlpools | $7,891M | 10.0% | 44.9% | public IDL + `Program data:` |
| 4 | Raydium (v4 + CPMM + CLMM) | $7,475M | 9.5% | 54.4% | public IDL + logs |
| 5 | Meteora DLMM | $6,327M | 8.0% | 62.5% | public IDL + self-CPI event |
| 6 | Manifest | $4,309M | 5.5% | 67.9% | public source + `Program data:` |
| 7 | Tessera V | $3,207M | 4.1% | 72.0% | prop AMM, `S-00:` text log only |
| 8 | Scorch | $2,985M | 3.8% | 75.8% | prop AMM, no event |
| 9 | HumidiFi | $2,784M | 3.5% | 79.3% | prop AMM, obfuscated data |
| 10 | pump.fun (bonding curve) | $2,530M | 3.2% | 82.5% | public IDL + self-CPI event |
| 11 | QuantumAMM | $1,891M | 2.4% | 84.9% | prop AMM |
| 12 | Jupiterz (RFQ) | $1,742M | 2.2% | 87.1% | order_engine fill |
| 13 | GoonFi | $1,738M | 2.2% | 89.3% | prop AMM |
| 14 | AlphaQ | $1,691M | 2.1% | 91.5% | prop AMM |
| 15 | Deriverse | $1,430M | 1.8% | 93.3% | prop AMM |
| 16 | SolFi V2 | $1,011M | 1.3% | 94.6% | prop AMM |
| 17 | PancakeSwap v3 | $739M | 0.9% | 95.6% | public IDL |
| 18 | Aquifer | $692M | 0.9% | | prop AMM |
| 19 | Byreal | $592M | 0.8% | | prop AMM |
| 20 | Jupiter Lend DEX | $586M | 0.7% | | |

Then Meteora DAMM v2 $300M, Meteora DBC $300M, Archer $250M, StonkFun $242M,
Whalestreet $155M, LaunchLab $149M, Quay $99M, Meteora DAMM v1 $53M.

**Coverage arithmetic (the number that drives the plan): top 5 = 62.5%,
top 9 = 79.3%, top 14 = 91.5%, top 17 = 95.6%.**

### 1.2 Routers are not venues

`https://api.llama.fi/overview/aggregators/solana` 30d = $31.41B, i.e. **40% of
all Solana DEX volume is routed**, and must never be added to venue volume:
Jupiter Aggregator $16,599M, DFlow $9,303M, OKX Swap $3,995M, Titan $736M,
Binance Wallet $209M, LiquidMesh $103M, Defi App $102M, LI.FI $88M, 0x $81M.
Router program ids seen in real transactions: Jupiter v6
`JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4`, DFlow `DF1ow4ts...`,
`proVF4pM...`, GMGN `GMGNreQc...`, Ave `AveaiuA1...`, `term9YPb...`, `MAyhSmzX...`.
Consequence for attribution: **the signer of a Solana swap is very often a
router's PDA or a bot, not the trader** - the same lesson design.md section 9
records for EVM (`the event sender is usually a router, never use it for
attribution`). On Solana the reliable trader is the transaction's `fee_payer`.

### 1.3 How opaque is the opaque part

Summing the venues I classified as prop / "dark" AMMs above (BisonFi, Tessera,
Scorch, HumidiFi, QuantumAMM, GoonFi, AlphaQ, Deriverse, SolFi V2, Aquifer,
Byreal, ZeroFi, Quay): **~$25.6B of 30d volume, about 32% of the chain.** None
of them publishes an IDL; several emit no event at all (section 2). This is the
single biggest difference from EVM, where a Uniswap fork on an unknown chain
decodes on day one from its event ABI. It is also why the balance-delta path in
section 3 is not a "fallback" on Solana - for a third of the volume it is the
only path.

Caveat stated by the source: DefiLlama's PumpSwap figure is filtered (pools with
TVL >= $5k and >= 50 traders, "filters out wash trading pools"), so real
on-chain PumpSwap volume is larger than $18.2B, and its share is understated.
## 2. How swaps appear on Solana, per venue

Evidence: ~80 real transactions fetched from `https://api.mainnet-beta.solana.com`
(`getTransaction`, `encoding:"jsonParsed"`, `maxSupportedTransactionVersion:1`)
on 2026-09-19 01:25-01:35 UTC, plus the open-source Dune spellbook
(`duneanalytics/spellbook`, `dbt_subprojects/solana/models/_sector/dex`) for
program ids and the shape each venue's decoder uses. Raw JSON is kept in the
scratch dir; signatures are quoted so anything here is re-checkable.

Solana has no EVM-style log table. The three mechanisms are:
- **(a) Anchor events** - either a `Program data: <base64>` log line, or an
  `emit_cpi!` **self-CPI**: the program invokes *itself* with an instruction whose
  data starts with `e445a52e51cb9a1d` followed by the event discriminator. The
  self-CPI form is the robust one, because validators truncate log lines
  (HyperSync even exposes `transaction.has_dropped_log_messages`) but never drop
  an instruction.
- **(b) decode the swap instruction** - 8-byte Anchor discriminator (or a 1-byte
  tag on non-Anchor programs) + argument bytes + the account metas at fixed
  positions (`a0`-`a9` in HyperSync are exactly these).
- **(c) token movement** - the SPL Token / Token-2022 transfer instructions that
  are *children of the swap instruction*, and/or the pre/post token balances of
  the pool's vault accounts.

| Venue | Program id | (a) event | (b) instruction | (c) transfers | Evidence |
|---|---|---|---|---|---|
| PumpSwap | `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` | self-CPI `e445a52e...` | public IDL, Buy `66063d1201daebea` / Sell `33e685a4017f83ad` | 3-4 `transferChecked` children | `Qxpfmre4JbRctxg1JKJ7vk5Rze6XGpX36bqkm34dgRe5vvCWt4mkRhDe6Zu999TomKnJ14DetevDDURGsgPLaMb` |
| pump.fun curve | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | self-CPI | public IDL | Token-2022 children | `5w7k63vqBomjaE9U...`, `eUj32vmGVJuxr4ir...` |
| Orca Whirlpools | `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` | `Program data:` disc `e1ca49af932ba096`, 121 bytes | public IDL, swapV2 `2b04ed0b1ac91e62` | yes | `2a3W52uooRoMtvYW...`, `211ktuxWTvAdfoX2...` |
| Raydium AMM v4 | `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8` | `Program log: ray_log: <base64>` (not `Program data:`) | 1-byte tag, public source | yes | `2G6Ytdh9UkREMXXv...` |
| Raydium CLMM / CPMM | `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` / `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` | `Program data:` disc `40c6cde8260871e2` | public IDL, CPMM swap `8fbe5adac41e33de` | yes, incl. Token-2022 | `2G6Ytdh9UkREMXXv...`, HyperSync probe pL |
| Meteora DLMM | `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` | self-CPI, 2 events/swap | public IDL | yes | `2j1uv7uM3Boc7UHb...`, `66R3Ucvpa4Kt...` |
| Meteora DAMM v2 | `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG` | self-CPI, ~1/instruction | public IDL | yes | `4XJqjL7cDn1HwNum...` |
| Meteora DBC (curve) | `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN` | self-CPI | public IDL | yes | HyperSync probe pW |
| LaunchLab (Raydium) | `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj` | self-CPI | public IDL | SPL + Token-2022 | `4thxJz2jxUd1AarK...` |
| Manifest | `MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms` | `Program data:` fill logs | open source, 1-byte tag | yes | `211ktuxWTvAdfoX2...` |
| **BisonFi** | `BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi` | **none** | 1-byte tag 02/07, no IDL | 2 SPL transfers at inner idx +1,+2 | `211ktuxWTvAdfoX2...` |
| **HumidiFi** | `9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp` | **none** | **data looks obfuscated, no stable discriminator** | 2 SPL transfers | `2gU8T1FMrwjejLpU...` |
| **Tessera V** | `TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH` | only `Program log: S-00: ...` free text | 1-byte tag 10/11 | 2 SPL transfers | `41m9QyrXCR4t8X3v...`, `4XJqjL7cDn1HwNum...` |
| Scorch / GoonFi / AlphaQ / QuantumAMM / SolFi V2 / ZeroFi / Quay / Deriverse / Byreal / Aquifer / Obric | see appendix | none / unverified | no IDL | 2 SPL transfers each | `4bttiwkq74rNcUxW...`, `4VfG8i9VwpxYtjUF...`, `2tQtHnnMsumEaYv2...`, `5XS4d2AEB4kS6xtG...`, `4sdBryRdZfDDs3Ds...`, `2gG2aCSN166r8BZw...`, `3xcX5HgS5Ls1nYNe...` |

**The one uniform fact, measured on every venue above: a swap instruction always
has at least two SPL Token / Token-2022 transfer instructions as direct
children.** I walked the `innerInstructions` stack-height tree of every fetched
transaction and attributed each transfer to its nearest AMM ancestor; all 20
distinct venues that appeared had >= 2 transfer children per swap. Nothing else
is uniform: events are absent for a third of the volume, and only ~half the
venues publish an IDL.

### 2.1 Normalised fields, and where each comes from

| Field | From an event / IDL decode | From transfers only |
|---|---|---|
| pool | account meta at a known index | the common *owner* of the two vault token accounts |
| token_in / token_out (mints) | in the event or the vault accounts | `mint` on the `account_activity` row of each vault |
| amount_in / amount_out | exact, pre-fee and post-fee separable | post-fee only (what actually moved) |
| trader | event field, or `fee_payer` | `fee_payer` (router-proof) |
| fee | explicit (protocol / creator / LP split) | only if fee goes to a separate account in the same subtree |
| price | derivable | derivable (amount ratio) |
| liquidity / tick / sqrt_price | **only** from the event (Orca, CLMM, DLMM) | not available |

So: **prices, volumes and trader attribution are recoverable for 100% of
venues; pool-state series (liquidity, tick, active bin) only for the ~60% that
emit events.** That is an acceptable split for a display-first module - candles
and volume everywhere, depth charts only where the venue tells us.

### 2.2 Two findings that shape the whole design

**(1) Transaction-level pre/post balances are NOT enough.** In
`Qxpfmre4JbRctxg1...` one transaction contains two PumpSwap swaps on the *same
pool* in *opposite directions* (instruction #3 Sell, instruction #6 Buy), signed
by two different signers. ~6.2 SOL moves each way; the net vault delta over the
transaction is ~0.03 SOL. A decoder that reads only the transaction's pre/post
token balances - which is what most "balance diff" approaches do - would report
a 0.03 SOL trade instead of two 6.2 SOL trades. **Balance deltas are ground
truth only at instruction-subtree granularity, not at transaction granularity.**

**(2) A direct transaction to a prop AMM is usually not a trade.** Sampling
transactions that call BisonFi/HumidiFi directly: `5URSqMNt5q221ipYVVSMoNrF4Vb8XDtFtzinVSQfNCA8ve8croiTyw4bAiwvGDMUMwLevwxT8dWo8VjJJR9d2tDv`
(BisonFi, 379 compute units, zero token movement) and
`GJ4CUabz7S6Hi3youX7BmPxWXBYr9e4LWNKiMuNS8HQpNQZCcf836UxvhotqrtNdKXdUhtKH3PwzpvkHQYn3aEm`
(HumidiFi, 503 CU) are **quote / price updates**, not swaps; 8 of 8 sampled
direct HumidiFi transactions were updates. Their actual swaps arrive as *inner*
instructions under Jupiter/DFlow. The HyperSync probe confirms this at scale
(section 5): prop AMMs show ~49 outer rows/slot of quote updates against ~10
inner rows/slot of real swaps. **A decoder that counts "instructions of program
P" as trades would overstate prop-AMM activity by ~5x.**

### 2.3 A real multi-hop route, end to end

`66R3Ucvpa4KtxFoUCgnkZY9DYv7BptgrCyMamt6Dzz2WhGskmUphS446gnStzH6fnroPZv52FvZmQXGsxRYp2H94`
is one Jupiter v6 transaction with three hops: SOL -> USDC on **BisonFi** (no
event, 2 transfers at depth+1), USDC -> token on **Meteora DLMM** (2 transfers +
2 self-CPI events), then -> a Token-2022 token on **Raydium CPMM**
(`Program data:` event, disc `40c6cde8260871e2`). Jupiter emits its own
`SwapEvent` self-CPI and takes a 7,075-lamport wSOL fee. The Token-2022 transfer
fee is visible in the raw amounts: the vault sent 32,982,064,366 units, the user
received 32,652,243,722 - **a 1% transfer fee that no event mentions**. Across a
22-route sample, hop counts were 1:4, 2:10, 3:5, 4:3, and the routes touched
programs I could not identify at all (`riptK81h...`, `ghosty4Z...`, `BSwp6bEB...`).
This transaction is the acceptance test for any Solana decoder we write.
## 3. Is a DEX-agnostic decoder possible?

Short answer: **yes, and it has to be the primary decoder, not a fallback - but
it must run on the instruction subtree, never on the transaction.**

### 3.1 The rule that works

> A swap is an instruction `I` of program `P` (any program) such that, within
> `I`'s own subtree and not inside a nested non-token program call, exactly two
> mints move: some token accounts of mint A gain and some of mint B lose (from
> `I`'s caller's perspective), and the counterparty accounts of both legs share
> one owner (the pool authority / PDA).

Everything that makes this work on Solana and not on EVM:

- **SPL amounts cannot be forged.** On EVM a `Swap` event is just a program
  writing bytes; that is precisely why `src/dex/corroborate.rs` now refuses to
  value a swap unless a real ERC-20 `Transfer` in the same transaction backs the
  leg. On Solana there is no such gap: the SPL Token program is the only code
  that can change an SPL balance, the transfer instruction *is* the movement,
  and HyperSync's `account_activity` pre/post balances come from validator
  metadata, not from the program. **A Solana "swap" that moves no tokens is not
  a swap and simply never enters the table.** The EVM corroboration step
  disappears on Solana - not because we relax the rule, but because the rule is
  structurally satisfied by the data source.
- **Position is exact.** HyperSync serves `instruction_address` as the full tree
  path, so "children of `I`" is a prefix test, not a heuristic.
- **It covers the opaque third.** BisonFi, HumidiFi, Tessera, Scorch and friends
  publish nothing, but their transfers are ordinary SPL transfers with an
  ordinary pool owner.

### 3.2 Where it breaks, and what each break costs

| Break | What goes wrong | Mitigation |
|---|---|---|
| **Transaction-level netting** | two opposite swaps on one pool net to ~0 (`Qxpfmre4JbRctxg1...`, section 2.2) | never aggregate above the subtree; this is a hard rule, not a tuning knob |
| **Multi-hop in one tx** | Jupiter 3-hop looks like one SOL -> Token-2022 trade at tx level | subtree rule gives 3 separate swaps naturally; also emit one `route` row keyed on the router instruction so a UI can show "1 user trade, 3 venue fills" |
| **Split routes** | the same pair filled on 2-4 venues in one route | same: N swaps + 1 route row; do not de-duplicate |
| **Fee accounts in the subtree** | a third transfer (protocol/creator/Jupiter fee) makes it look like 3 mints move | fee legs are same-mint as one of the two legs, or go to a non-vault account; classify the two largest same-mint groups as the legs, the remainder as fee |
| **Token-2022 transfer fee / hooks** | amount sent != amount received (measured 1% in `66R3Ucvpa4Kt...`) | store BOTH `amount_out_sent` (vault side) and `amount_out_received` (user side); a single `amount_out` column would silently be wrong for every Token-2022 pair |
| **Wrapped SOL open/close** | a WSOL account is created, funded, closed in the same tx; `pre_token_balance` is null on open | HyperSync gives `token_state` (`opened` / `closed` / `persisted`) - use it instead of inferring from null |
| **Native SOL legs** | some venues move lamports, not WSOL, so there is no SPL transfer | use the `native` side of `account_activity` (pre/post lamports) for that leg; unlike EVM native legs, the data IS there |
| **Liquidity add / remove** | two mints move into the pool together - looks like a swap with both legs the same sign | sign test: a swap has one leg in and one leg out relative to the pool; same-sign = liquidity, tag it as such and keep it out of `dex_swaps` |
| **Flash loans / MEV bundles / arb** | one tx borrows, swaps 4x, repays | subtree rule handles it; each leg is a real fill. These are real volume, not noise, but mark `is_arb` when the fee payer ends in the same mint it started |
| **Prop AMMs with many vaults** | a venue with N vaults per pool breaks "two vault accounts, one owner" | the owner test is on the *authority*, not the account, and held on every venue sampled; unresolved cases go to a `dex_swaps_unclassified` table rather than being guessed |
| **Quote-update instructions** | 5x overstatement if you count instructions (section 2.2) | require token movement; a quote update has none, so it is filtered by construction |
| **An unknown program that is not a DEX** | staking, lending, NFT sales also move two mints | require the *pool-side* accounts to be reused across many transactions and many counterparties before promoting a program to a venue; ship a `sol_dex_programs` registry (user-populated, like `quote_tokens`) and keep unregistered matches in the unclassified table |

The last row is the honest limit: **the generic rule identifies token movement
perfectly and "this was a trade on a market" only probabilistically.** On EVM the
event ABI carries that semantic for free; on Solana it does not exist.

### 3.3 The design that follows

**Two layers, both always on, not a primary + fallback:**

1. **Movement layer (generic, 100% coverage).** Subtree rule over every
   instruction of a registered program. Produces pool, mints, amounts, fee
   payer, price. Confidence `movement`.
2. **Decoder layer (per program, the venues that publish).** Anchor
   discriminator + IDL, or the documented log/self-CPI event. Adds exact
   pre-fee amounts, the fee split, the venue's own idea of the trader, and pool
   state (liquidity / tick / active bin). Confidence `decoded`.

A swap row carries both; the decoder layer *enriches*, the movement layer
*guarantees the row exists*. Where both are present and the amounts disagree by
more than the Token-2022 fee, flag it - that disagreement is a bug report, and
it is exactly the check the EVM module gets from corroboration.

**How many decoders for 90% / 95%?** From section 1.1: **5 programs cover 62.5%,
9 cover 79.3%, 14 cover 91.5%, 17 cover 95.6%.** But 5 of the top 9 and 9 of the
top 17 are prop AMMs with no IDL, so per-program *decoders* cannot reach 90% at
all. The realistic split is: **decoders for ~8 public programs (PumpSwap,
pump.fun, Orca, Raydium v4/CPMM/CLMM, Meteora DLMM/DAMM v2/DBC, Manifest,
LaunchLab) ~= 60% of volume with full fidelity, and the movement layer for the
other ~40% with price/volume fidelity but no pool state.** That is the opposite
ratio from EVM, and it is the reason the movement layer cannot be the
"lower-confidence fallback" it would be on EVM.
## 4. What Solana HyperSync actually serves

Sources: `https://docs.envio.dev/docs/HyperSync/solana` and `/solana-query`
(retrieved 2026-09-19 01:37 UTC); crate sources `hypersync-client-solana` 0.2.0,
`hypersync-solana-net-types` 0.2.0, `hypersync-solana-schema` 0.2.0 downloaded
from crates.io and read (not built); and **12 live probes of
`https://solana.hypersync.xyz` with the project token, 2026-09-19 04:40-04:44
UTC**.

### 4.1 Tables and fields (all verified in the crate's `field_selection.rs`)

| Table | Fields |
|---|---|
| `block` | slot, blockhash, parent_slot, parent_blockhash, block_time, block_height |
| `transaction` | slot, transaction_index, transaction_id, signatures, fee_payer, success, err, fee, compute_units_consumed, account_keys, recent_blockhash, version, loaded_addresses_writable, loaded_addresses_readonly, has_dropped_log_messages |
| `instruction_call` | slot, transaction_index, **instruction_address**, executing_account, executing_account_index, account_arguments, account_index_arguments, data, d1, d2, d4, d8, a0-a9, is_inner, tx_success, error, compute_units_consumed |
| `log` | slot, transaction_index, instruction_address, program_id, kind (`invoke`/`success`/`failed`/`consumed`/`log`/`data`/`other`), message |
| `account_activity` | slot, transaction_index, transaction_id, account_index, account, pre_balance, post_balance, is_signer, is_writable, is_fee_payer, from_lookup_table, mint, pre_owner, post_owner, token_decimals, pre_token_balance, post_token_balance, pre_program_id, post_program_id, **token_state** |
| `reward` | slot, pubkey, lamports, post_balance, reward_type, commission |

Everything section 2 and 3 need is here. Three fields matter more than the rest:

- **`instruction_address`** is the full tree path (`[2]` = third top-level
  instruction, `[2,0]` = its first child). The crate's own doc comment
  (`hypersync-client-solana-0.2.0/src/simple_types.rs:100-105`) says it carries
  "depth AND parentage AND sibling order" and that `stack_height()` is just its
  length. This is what makes the section 3 subtree rule and the section 0
  `ordinal` packing possible.
- **`token_decimals` on every token row** - so unlike EVM, **we do not need an
  RPC call to know a mint's decimals for a swap we just saw.** Only name/symbol
  need external resolution (section 6.5).
- **`token_state`** (`not_a_token` / `opened` / `closed` / `persisted`) solves
  the wrapped-SOL open/close case from section 3.2 without inference.

### 4.2 Filter expressiveness, and the trap

`instruction_calls`: `executing_account`, `d1`/`d2`/`d4`/`d8`, `a0`-`a9`
(account pubkey at a given meta position), `is_inner`, `tx_success`.
`transactions`: `fee_payer`, `transaction_id`, `transaction_index`, `success`.
`logs`: `program_id`, `kind`. `account_activity`: `kind`, `account`,
`transaction_id`, `mint`, `owner` (matches pre or post), `program_id`, and the
header flags.

**AND inside one selection object, OR across objects in the same array, and
different arrays are INTERSECTED.** The docs spell out the trap with a measured
example on slot 437500000: an instruction filter alone returns 44 rows, a
`fee_payer` filter alone 16, both together 2. A selection that matches nothing
zeroes every table with no error. Unknown top-level keys are rejected loudly.
Practical consequence for us: **"all swaps OR all launchpad trades" is one
`instruction_calls` array with many objects; it can never be expressed by mixing
arrays.**

### 4.3 The join question - PROBED, and the answer is good enough

The docs only say "there is currently a single default join mode; finer control
(matched rows only, or all rows of matched transactions) is planned", which left
the most important question for us open. I probed it (slot 448258095, filter =
BisonFi *inner* instructions, all five tables in `field_selection`):

- Returned: the ONE matched instruction row `[5,1]`.
- **Not returned: its child SPL transfers `[5,1,0]` and `[5,1,1]`.**
- Returned: the parent transaction row.
- Returned: **ALL 10 log rows of that transaction**, each with its own
  `instruction_address`.
- Returned: **ALL 12 `account_activity` rows of that transaction**, including
  accounts that did not change, with mint / owner / decimals / pre / post.

So: **sibling and child *instructions* do not come back, but the full
`account_activity` and `log` of the matched transaction do.**

What that means for the design:
- The **decoder layer** (events) works today: the self-CPI event is itself an
  instruction of the same program, so a `d8` filter on `e445a52e51cb9a1d` +
  `executing_account` catches it, and `Program data:` events arrive via the log
  table for free.
- The **movement layer** does *not* get its child transfer instructions - but it
  gets every `account_activity` row of the transaction, which carries the
  vault-level pre/post per mint. That is enough for single-swap transactions but
  **not** for the two-opposite-swaps case (section 2.2) or for multi-hop.
- Therefore the movement layer needs the SPL transfer instructions too, which
  means **adding `Tokenkeg...` + `Token-2022` with `d1 in (03, 0c)` as extra
  selection objects in the same array** - a union, not an intersection. Cost
  measured: 494 transfer instruction rows/slot for the whole chain (section 5),
  ~1.4x the DEX-program row volume. Acceptable, and it is the single decision
  that makes the generic decoder possible.

### 4.4 History depth - deeper than documented

Documented: "mainnet is around slot 403,000,000 as of September 2026". I probed
from slot 0: **the server serves from slot 391,000,000**, whose `block_time` is
1767425822 = **2026-01-03 07:37 UTC**, i.e. ~8.5 months of history, not the
~7 stated. (Slot 403,000,000 `block_time` = 1772170820 = 2026-02-27 05:40 UTC.)
Backfill is clearly moving; the docs say depth is "prioritized by demand" and
invite requests on Discord. A range entirely below the earliest slot returns
empty with `next_slot` not advancing, rather than erroring - a resume loop must
treat "next_slot did not increase" as a stop condition, not spin.

### 4.5 Finality, the rollback guard, and what we must build

- `rollback_guard` has the same shape as the EVM one, with Solana naming:
  `slot_number` / `blockhash` (last slot of the server's head window) and
  `first_slot_number` / `first_previous_blockhash`. Observed live in the probe
  output. It describes the server's window, **not** the response's slots, and the
  docs warn not to compare one page's guard against another's.
- **Commitment is not documented.** I measured it: three samples of
  `GET /height` against public-RPC `getSlot` at 01:38 UTC showed Envio's head
  33-44 slots behind `processed`/`confirmed` and **4-12 slots behind
  `finalized`** - so it serves at about finalized, which for us is the good
  answer (finalized Solana slots do not roll back in practice). **This is a
  question to confirm with Envio, not an assumption to build on.**
- `stream_arrow` **has no live-tail mode**: at head it errors "server made no
  progress at slot {cur}" (`hypersync-client-solana-0.2.0/src/stream.rs:66`),
  and the client does no reorg handling. **We write the head follower**, exactly
  as we already do for EVM.

### 4.6 Throughput, limits, maturity

- Row caps: without `max_num_instructions` a response stopped after ~700 rows
  (1-2 slots). With `max_num_instructions: 200000` a single request returned
  **107 slots, 33,886 instruction rows, 5.99 MB of JSON in 2.7 s**. Arrow
  (`POST /query/arrow`) is offered as the smaller/faster path; the client
  auto-tunes `batch_size` against `response_bytes_ceiling`/`floor`
  (`config.rs`).
- **Rate limit / cost: every one of my ~12 probe responses carried
  `x-ratelimit-cost: 0`.** So the existing project token covers Solana
  HyperSync, on the same `x-ratelimit-*` surface the EVM client already speaks
  (`Client::get_with_rate_limit`, `RateLimitInfo`, `proactive_rate_limit_sleep`,
  added in 0.2.0). No separate plan or token appears to be needed - **confirm
  with Envio before depending on it.**
- Maturity: client 0.2.0 released 2026-08-12, repo at 0 stars, 1 open issue,
  last push the same day. The CHANGELOG's own `Fixed` entry for 0.2.0 is
  sobering: *"`stream_arrow` / `collect_arrow` silently dropped the tail of any
  chunk the server truncated ... losing up to 99% of rows on dense ranges
  (HOS-1834)"*. Dense ranges is exactly our workload. 0.2.0-rc.4 renamed tables
  and columns wholesale (`instructions` -> `instruction_calls`, `program_id` ->
  `executing_account`, `owner` split into `pre_owner`/`post_owner`) and the docs
  list more breaking-ish work as "coming next" (IDL-aware decoding, Node/Python
  clients, wider JSON-RPC facade). **Treat the wire format as not yet frozen and
  pin the crate version.**
- `POST /query` requires the bearer token (401 without); `GET /height` and
  `/height/sse` are open. `/height/sse` is the natural head follower.

### 4.7 What I could not determine - questions for Envio

1. What commitment level is `/height` and the served data at - finalized, or
   confirmed with a rollback window? My measurement says ~finalized; please
   confirm, and state the maximum rollback depth the `rollback_guard` window
   covers.
2. Is there a way to request **all instruction rows of a matched transaction**
   (the "planned" join mode)? If not, is including SPL Token + Token-2022
   transfer instructions as extra selections the intended pattern, and is it
   billed as one query?
3. How is a Solana query **priced/weighted** against our existing plan? Every
   probe showed `x-ratelimit-cost: 0` - is that a beta grace period?
4. **How far back will history go, and by when?** We would use 391M today; a
   full-history product needs genesis or at least 2 years. Is there a paid
   backfill-on-demand path?
5. What is the **head lag SLA** and the throughput ceiling for a sustained
   ~110M rows/day consumer (section 5)?
6. Is the 0.2.0 wire format frozen? What is the deprecation policy for another
   rename wave like 0.2.0-rc.4?
7. Does `instruction_call.error` / `compute_units_consumed` get populated for
   the whole served range, or only SQD-ingested ranges (the docs say RPC and
   Firehose ranges leave them null)? Same question for log `kind` values
   `invoke`/`success`/`consumed`, which the docs say are missing on some ranges.
8. Is `transaction_index` stable across a re-ingest of the same slot from a
   different source? Our position key depends on it.
## 5. Data volume reality check

All figures below are **measured**, not modelled: 6 windows x 30 slots spread
over 24h (slots 447934000, 447988000, 448042000, 448096000, 448150000,
448204000), filter = the 26 DEX/launchpad program ids in section 2,
`tx_success: true`, via `https://solana.hypersync.xyz/query` on 2026-09-19
04:40-04:44 UTC. Assumptions are stated so the owner can re-derive them.

### 5.1 Chain baseline

- **Slot time ~0.267 s** (216,000 slots in 57,669 s, slots 448,044,000 ->
  448,260,000, public RPC `getBlockTime`) - it was ~0.40 s until ~slot 430M.
  **=> 323,600 slots/day.** This is the assumption every row/day number below
  multiplies by, and the one most likely to change.
- `getRecentPerformanceSamples`: ~96k-114k **non-vote** transactions/min =
  ~1,600-1,900 TPS = **138-164M non-vote transactions/day**; ~250k-265k
  total/min including votes.

### 5.2 Measured DEX + launchpad volume

| Quantity | Per slot | Per day |
|---|---|---|
| Matched instruction rows (26 programs, incl. self-CPI event rows) | 346 | **112M** |
| ... of which real swap instructions | 161 | **52M** |
| ... of which prop-AMM quote updates (not trades) | 50 | 16M |
| ... of which Anchor self-CPI event rows | 135 | 44M |
| Distinct transactions touching those programs | 168 | 54M (~35% of all non-vote txs) |
| **All** SPL Token + Token-2022 transfer instructions, whole chain (`d1` in 03/0c, separate 100-slot probe) | 494 (91% inner) | **160M** |

Per venue, swaps/day: PumpSwap 24.8M, Meteora DAMM v2 5.1M, Meteora DLMM 3.9M,
pump.fun 3.9M, Orca 2.0M, Raydium CLMM 1.8M, Meteora DBC 1.5M, Raydium CPMM
1.5M, Manifest 1.4M, BisonFi 1.3M, Raydium v4 1.0M, then a long tail.

Two cross-checks and one disagreement:
- PumpSwap emits exactly one self-CPI event per swap instruction: 13,801 event
  rows vs 13,813 swap instruction rows over 180 slots. That confirms the
  instruction count really is a swap count for that program.
- 24.8M PumpSwap swaps/day against DefiLlama's $607M/day = **$24 average trade**
  - small, but consistent with memecoin micro-trading, and DefiLlama's PumpSwap
  number is filtered (section 1.3), so the real average is higher.
- **Sources disagree on Meteora DAMM v2**: 5.1M swaps/day measured on chain
  against DefiLlama's $300M/30d ($10M/day) implies a $2 average trade. Either
  DefiLlama's DAMM v2 coverage is partial, or some DAMM v2 instructions I
  counted are not swaps. Both are shown; do not build a number on that row.

### 5.3 Storage in ClickHouse

Assumptions: a `dex_swaps` row under the section 0 rules is ~300 raw bytes
(4 x 32-byte pubkeys, 2 x 32-byte amounts, 64-byte tx id, keys, enums). Pubkeys
are random, so ZSTD(3) buys little on them; **assume 60-120 compressed bytes/row,
80 as the working number.**

| Stream | Rows/day | Compressed/day | Per year |
|---|---|---|---|
| `dex_swaps` + `launchpad_trades` (52M swaps) | 52M | **~4.2 GB** (range 2.6-6.2) | ~1.5 TB |
| \+ slim `sol_transactions` for matched txs (54M, 64-byte sig) | 54M | ~2.7 GB | ~1.0 TB |
| \+ per-pool candles 1m/1h/1d + side tables (rule of thumb: ~40% of base) | | ~1.7 GB | ~0.6 TB |
| **Program-filtered Solana pipeline, total** | | **~8.6 GB/day** | **~3.1 TB/yr** |
| If we also stored every SPL transfer ("all transfers" parity with EVM) | 160M | +~10 GB/day | +3.6 TB/yr |
| If we stored every non-vote transaction | 150M | +~8 GB/day | +2.9 TB/yr |

**Comparison with an EVM chain.** Measured the same day on Base (public RPC
`https://mainnet.base.org`, `eth_getLogs`, 6 x 50 blocks over 24h, topic0 in
{V2, V3, V4, Solidly, Pancake-V3 `Swap`}): **45.2 swap logs/block = ~2.0M
swaps/day**, ~160 MB/day at the same 80 bytes/row.

> **Solana is ~26x Base in swap rows per day, and the program-filtered Solana
> pipeline alone would write roughly as much per day as a large handful of EVM
> chains put together.**

### 5.4 Ingest bandwidth

The measured JSON density is 177 bytes/row (5.99 MB for 33,886 rows). At 112M
rows/day that is **~20 GB/day of JSON at head** (~230 KB/s - trivial), but
backfilling the **available** history (slot 391M -> 448.3M = 57.3M slots = ~177
days) is **~3.5 TB over the wire in JSON**, materially less in Arrow. Plan the
first backfill in Arrow and in slot ranges, not as one stream.

### 5.5 Verdict: program-filtered, and it is not close

**"Index everything" is not viable and should not be attempted.** Not because of
disk (3 TB/yr is affordable) but because:
1. 150M+ transactions/day is ~30x the row rate the EVM pipeline is being tuned
   for, and it would be spent almost entirely on vote-adjacent bot spam.
2. HyperSync is program-filtered *server-side*; an unfiltered stream is exactly
   the "heavy, keep the range small" case the docs warn about.
3. The value is concentrated: 26 program ids cover >95% of DEX volume.

**What we lose by filtering:** the generic EVM features do NOT carry over.
- *All ERC-20 transfers* -> no equivalent. We would store SPL transfers only
  inside matched transactions. Chain-wide "every transfer of token X" would be
  wrong/partial and must not be offered.
- *Wallet history / `transactions_by_address`* -> no equivalent. A Solana wallet
  page could only show its DEX and launchpad activity. HyperSync can answer
  "everything address W touched" as an ad-hoc query, but that is a live API
  call, not an indexed table.
- *Blocks / transactions tables* -> only for matched transactions, so counts and
  daily stats over them would be a subset, not the chain. **Either do not create
  `daily_block_stats` for Solana, or name it so nobody mistakes it for the
  chain's activity.** (Mislabelling partial data is the exact failure design.md
  section 9 avoided by refusing a contract-deployment aggregate.)
- *Token metadata for arbitrary mints* -> only mints we saw traded.

**This asymmetry is the strongest argument for the architecture in section 6:
Solana is an analytics-only pipeline, not a general chain indexer.**
## 6. Architecture for this project

### 6.1 Recommendation: option (i), one binary, with a source seam

**Build the Solana pipeline inside this binary and this schema**
(`indexer run --chain solana`), writing chain-specific `sol_*` core tables plus
**the same** `dex_*` / `launchpad_*` analytics tables the EVM modules write.
Not a workspace split yet, not a sister project.

Why, in the vocabulary of design.md section 12:

> A Solana pipeline is **another `source`**, **another chain-specific core data
> module** (`svm/` beside `core/`), and **the same analytics data modules**
> (`dex/`, `launchpads/`) with a second decoder each.

- **The reason a sister project is wrong is the product.** A pump.fun token
  graduates into a PumpSwap or Raydium pool. The owner's screen is "curve trades
  -> graduation -> AMM candles, continuous on one chart". That is one query over
  one `dex_swaps` + one `launchpad_trades`, which is only possible if both
  chains write the same tables in the same database. Two projects means two
  databases and a join in the UI - which is exactly the thing design.md section
  10 forbids ("one cheap query against a view, no client-side joins").
- **The reason a workspace split is premature is that the shared layer already
  is shared.** `db/`, `reorg/`, `metrics/`, the migrator, `DerivedTable`, the
  validity rule and the writer key on `chain` + `block_number` and know nothing
  about EVM. They need no abstraction, only a second caller. Splitting crates
  before the Solana module exists would force us to guess the trait boundaries.
  Revisit the split when Solana compiles and build times or dependency bleed
  actually hurt - it is a mechanical `git mv` at that point, same as the
  section-12 refactor.
- **The reason it is not "just another chain" is section 5.5:** Solana is an
  *analytics-only* pipeline. It gets no `transactions_by_address`, no chain-wide
  transfers, no block stats. The binary must make that explicit rather than
  quietly serving partial tables.

### 6.2 What is reusable, what needs an abstraction, what is new

| Component | Verdict |
|---|---|
| `db/` (client, insert path, `format.rs` serializers, `tombstone_sql`, ranges, checkpoints, the `DerivedTable` type) | **as-is** - infrastructure, dataset-agnostic by design |
| `reorg/` fork-point search + `purge_range` orchestration | **as-is** - already trait-based with no ClickHouse; slots substitute for block numbers without a code change |
| Tombstones + epochs + the validity rule + `epoch_floor_v` | **as-is**. Epochs are per chain; Solana is a chain. The only requirement is that Solana rows also carry `epoch`, `_version`, `is_deleted` |
| `metrics/` | **as-is** (add slot-lag gauges) |
| Migrator | **as-is**; Solana core tables take a new reserved range (propose `0040-0049`) |
| `dex/derived.rs`, candles, `*_v` views | **as-is once the position key is `(chain, block_number, tx_index, ordinal)`** (section 0). This is the whole time-sensitive decision |
| `configs/` | small change: `--chain` selects the pipeline; Solana-only flags (`--sol-programs`) |
| **`source/`** | **needs the abstraction.** Today it is a typed EVM HyperSync wrapper. Introduce a `ChainSource` trait: `stream(range) -> impl Stream<Item = SourceBatch>`, `head()`, `headers(range)` for fork-point search, `rollback_guard()`. Two impls: `source/evm.rs`, `source/solana.rs` |
| **`pipeline/transform.rs`** | **needs the seam** the section-12 refactor is already creating: transform keeps orchestration, decoding lives in the data modules. Solana adds a second decode entry point per module |
| **Reorg *detector*** | **needs a variant.** Parent-hash continuity still works, but on Solana the predecessor is `block.parent_slot` / `parent_blockhash`, **not `slot - 1`** - skipped slots are normal (see 6.3) |
| **Gap / checkpoint contiguity** | **needs a variant** for the same reason: "slot N has no `blocks` row" is not a gap on Solana. Contiguity must be defined over *requested slot ranges* (the `next_slot` cursor), verified by the `parent_slot` chain, never by "every integer has a row" |
| `tokens/` | **needs a second resolver** (6.5). The worker shape (off the commit path, bounded, drop-on-full, DB-driven backfill) is reused verbatim |
| **`svm/`** (new DATA MODULE) | `sol_blocks`, `sol_transactions` (slim, matched txs only), `sol_tokens`; models/decode/derived/README like every other module |
| **`dex/decode_svm.rs`, `launchpads/decode_svm.rs`** (new) | the movement layer + the per-program decoders of section 3.3, producing the **existing** row structs |

### 6.3 The frictions, one by one

| Friction | Resolution |
|---|---|
| **32-byte pubkeys vs `FixedString(20)`** | analytics identity columns become `FixedString(32)`, EVM left-padded with 12 zero bytes - exactly what `dex_pools.pool_id` already does for V4 ids. Section 0 |
| **Signature is 64 bytes** | `tx_id String` (raw bytes) in analytics tables; `sol_transactions.signature FixedString(64)` in the chain-specific table. Verified: `Signature([u8; 64])` in `hypersync-solana-net-types-0.2.0/src/query.rs` |
| **Slot vs block number** | keep the column **name** `block_number`; it holds the slot. Every purge / tombstone / checkpoint statement is written against that name (`db::block_number_column`); renaming buys nothing and costs everything |
| **Skipped slots** | a slot with no block is normal. Continuity = `block.parent_slot` + `parent_blockhash` chain, not integer succession. **This is the one place the EVM reorg detector would produce false gaps** and must be varied |
| **Position inside a tx** | `(tx_index, ordinal)` where `ordinal` packs `instruction_address` 12 bits per level (CPI depth <= 5). Section 0. Crucially computable from ONE row, which matters because a program-filtered stream never sees the siblings |
| **`chain UInt64`** | see 6.6 |
| **Pool = several accounts** | `dex_pools.pool_id` = the pool **state** account; the vaults go in a new `sol_pool_vaults` (or a `vaults Array(FixedString(32))` column on `dex_pools`, which already carries a `tokens Array`). The movement layer *learns* vaults from observed swaps rather than being told them |
| **Commitment vs our reorg detection** | HyperSync appears to serve at ~finalized (4-12 slots behind, section 4.5), so reorgs should be rare to absent. **Keep the full tombstone+epoch machinery anyway** - it costs nothing when unused, and `rollback_guard` is served in the same shape as EVM. Set `--confirmations 0` and rely on the guard + parent-hash chain, same as EVM |
| **No `eth_call`** | 6.5 |

### 6.4 DDL sketch (the chain-neutral shape)

```sql
-- migration 0010-0019 (dex), amended before any data exists
CREATE TABLE dex_swaps (
  chain          UInt64,
  block_number   UInt64,                 -- EVM block, Solana slot
  tx_index       UInt32,
  ordinal        UInt64,                 -- EVM log_index; Solana packed instruction path
  timestamp      DateTime CODEC(DoubleDelta, ZSTD),
  tx_id          String,                 -- 32 bytes EVM hash, 64 bytes Solana signature
  pool_id        FixedString(32),
  protocol       LowCardinality(String), -- 'uniswap_v3' | 'pumpswap' | 'orca_whirlpool' | ...
  venue_program  FixedString(32),        -- EVM: pool/emitter; Solana: executing_account
  trader         FixedString(32),        -- EVM: tx from; Solana: fee_payer. NEVER the router
  sender         FixedString(32),        -- what the venue itself reports (often a router)
  recipient      FixedString(32),
  token_in       FixedString(32),
  token_out      FixedString(32),
  amount_in      UInt256,
  amount_out     UInt256,                -- what the taker RECEIVED
  amount_out_gross UInt256,              -- what the pool SENT (Token-2022 fee != 0)
  amount0        Int256,                 -- pool-relative, signed; EVM families keep these
  amount1        Int256,
  fee_amount     UInt256,
  sqrt_price_x96 UInt256,                -- 0 when the venue emits no state
  liquidity      UInt256,
  tick           Int32,
  confidence     LowCardinality(String), -- 'decoded' | 'movement'
  route_ordinal  UInt64 DEFAULT 0,       -- ordinal of the router instruction; 0 = direct
  epoch          UInt32,
  _version       UInt64,
  is_deleted     UInt8 DEFAULT 0
) ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

CREATE TABLE launchpad_trades (
  chain UInt64, block_number UInt64, tx_index UInt32, ordinal UInt64,
  timestamp DateTime CODEC(DoubleDelta, ZSTD),
  tx_id String,
  token FixedString(32),          -- EVM address left-padded / Solana mint
  venue LowCardinality(String),   -- 'pons_v2' | 'pump_fun' | 'meteora_dbc' | 'launchlab' | ...
  curve FixedString(32),          -- bonding-curve account/contract
  trader FixedString(32),         -- tx from / fee_payer
  side LowCardinality(String),    -- 'buy' | 'sell'
  token_amount UInt256,
  quote_token FixedString(32),    -- EVM: WETH/USDC; Solana: WSOL mint
  quote_amount UInt256,
  price_num UInt256, price_den UInt256,
  fee_amount UInt256, creator_fee UInt256,
  curve_progress UInt32,          -- basis points, 10000 = graduated
  confidence LowCardinality(String),
  epoch UInt32, _version UInt64, is_deleted UInt8 DEFAULT 0
) ENGINE = ReplacingMergeTree(_version, is_deleted)
PARTITION BY toYYYYMM(timestamp)
ORDER BY (chain, block_number, tx_index, ordinal)
SETTINGS do_not_merge_across_partitions_select_final = 1;

-- new, tiny, and the thing that makes views chain-aware
CREATE TABLE chains (
  chain UInt64, name LowCardinality(String),
  family LowCardinality(String),        -- 'evm' | 'svm'
  id_source LowCardinality(String),     -- 'eip155' | 'internal'
  _version UInt64, is_deleted UInt8 DEFAULT 0
) ENGINE = ReplacingMergeTree(_version, is_deleted) ORDER BY chain;
```

Formatting in views (design.md's "no hex strings in storage" rule survives):
`if(family = 'evm', concat('0x', lower(hex(substring(x, 13)))), base58Encode(x))`
- ClickHouse has `base58Encode` built in, so nothing new is needed on the read
path. Side tables (`dex_swaps_by_pool`, `_by_trader`, `_by_token`) change the
same way; candle `argMinState/argMaxState` keys become the
`(block_number, tx_index, ordinal)` tuple.

### 6.5 Token metadata without `eth_call`

- **decimals are free.** HyperSync serves `token_decimals` on every
  `account_activity` token row (section 4.1). This removes the single biggest
  reason design.md section 4 needs an RPC at all. **A Solana swap can be valued
  without any external call**, which is strictly better than EVM.
- **name / symbol / uri** live in account *state*: the Metaplex Token Metadata
  PDA (`["metadata", metadata_program, mint]`, program
  `metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s`) for SPL Token mints, or the
  Token-2022 `TokenMetadata` extension inside the mint account itself.
  HyperSync serves no account reads, so this needs an RPC
  `getMultipleAccounts` (batched, 100 per call) - the same shape as the existing
  `TokenWorker`: off the commit path, bounded, drop-on-full, DB-driven backfill
  from mints present in `dex_swaps` with no `sol_tokens` row. `--rpc auto` has no
  Solana equivalent, so the Solana pipeline needs an explicit `--sol-rpc`
  (default: the public `https://api.mainnet-beta.solana.com`, documented as
  best-effort and rate-limited).
- **Open question worth asking Envio:** their partial JSON-RPC facade
  (`POST /` / `POST /rpc`) might already serve `getAccountInfo` /
  `getMultipleAccounts`, which would remove the external RPC entirely. Added to
  the section 4.7 list.
- `sol_tokens` stays a separate table from `tokens` (different key space,
  different resolver). Analytics views read a `token_registry_v` union of the
  two. Do **not** merge them now.

### 6.6 What `chain` value Solana gets - decide explicitly, it is not obvious

There is **no standard integer chain id for Solana.** What exists (all checked
2026-09-18):

| Convention | Value | Source |
|---|---|---|
| CAIP-2 (the actual standard) | `solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp` - a **string** | `github.com/ChainAgnostic/namespaces/blob/main/solana/caip2.md` |
| Hyperlane domain id | **1399811149** | `docs.hyperlane.xyz/docs/reference/domains`; the page itself warns domain ids are not guaranteed to match EVM chain ids |
| Wormhole chain id | 1 (**collides with Ethereum mainnet**) | `wormhole.com/docs/reference/supported-networks/` |
| SLIP-44 coin type | 501 | `slip-0044.md` |
| SPL token-list cluster id | 101 | `solana-labs/token-list` `ENV.MainnetBeta = 101` |
| chainid.network (EIP-155 registry) | no Solana entry | `chainid.network/chains.json` |

**Recommendation: `chain = 1399811149`, with `id_source = 'internal'` in the
`chains` table and a one-line comment saying it is the Hyperlane domain id,
adopted because it is (a) already used by real bridge infrastructure for exactly
this purpose, (b) far outside any plausible EIP-155 allocation, so it cannot
collide with a future EVM chain.** Do not use 1, 101 or 501: 1 is Ethereum and
101/501 are small enough to collide. The `chains` table is what makes this
reversible - nothing else in the schema should hard-code the constant.
## 7. Phasing and effort

Estimates are for one engineer working the way this project has been working.
Every phase names what is **validated against real data**, because the lesson
from `docs/perps-research.md` is that memory of this market is stale.

| Phase | Weeks | Delivered | Validated against | What the UI gains |
|---|---|---|---|---|
| **0. Shared-table shape** (do now, before Solana is approved) | ~1 day per analytics module | `FixedString(32)` identity columns, `tx_id String`, `(chain, block_number, tx_index, ordinal)` key, `chains` table | existing EVM integration tests, unchanged behaviour | nothing - it is insurance |
| **1. Source seam + Solana ingest** | 2 | `ChainSource` trait, `source/solana.rs`, `svm/` module (`sol_blocks`, slim `sol_transactions`), slot-aware checkpoints/gaps, `parent_slot` continuity detector, `/height/sse` head follower | 24h of live slots streamed and resumed; a forced `purge_range` on a synthetic fork leaves candles correct | nothing visible yet |
| **2. Movement layer + pump.fun / PumpSwap** | 2 | generic subtree decoder (section 3.1) + 2 per-program decoders; Solana rows in `dex_swaps` and `launchpad_trades` | the two fixtures from section 2: `Qxpfmre4JbRctxg1...` (two opposite swaps, one tx) and `66R3Ucvpa4Kt...` (3-hop, Token-2022 fee) | **pump.fun launch feed, curve trades, PumpSwap candles** - ~26% of Solana DEX volume on day one |
| **3. The public decoders** | 2 | Orca, Raydium v4/CPMM/CLMM, Meteora DLMM/DAMM v2/DBC, LaunchLab, Manifest | per-venue fixture transactions; `decoded` vs `movement` amounts must agree within the Token-2022 fee | **graduation join** (`launchpad_graduations` -> `dex_pools`): curve -> AMM on ONE continuous chart. ~60% decoded, ~95% covered |
| **4. Metadata, USD, history** | 1-2 | `sol_tokens` resolver (Metaplex / Token-2022 extension), `quote_tokens` seeded with WSOL + USDC, backfill of the ~177 available days, `indexer verify` for slots | spot-check 50 mints against an explorer; backfilled candles must match live-streamed candles on an overlap window | names/symbols, USD volume, history on every chart |
| **5. The opaque third** | ongoing | `sol_dex_programs` registry, `dex_swaps_unclassified` triage, arb/MEV tagging | manual review of the top unclassified programs by volume | prop-AMM venues appear by name instead of "unknown" |

**~7-9 weeks to a continuous pump.fun -> PumpSwap chart with USD and history**,
of which phase 0 is the only part that must happen this week.

### 7.1 Prerequisites

Blocking on Envio (section 4.7): the **commitment level** (1), the **join mode /
transfer-union pattern and how it is billed** (2), and **pricing** (3). Phase 2
cannot be sized honestly without (2) - if all instructions of a matched
transaction become requestable, the movement layer gets ~1.4x cheaper and
simpler. Nothing blocks phase 0 or phase 1.

### 7.2 Risks

| Risk | Severity | Note |
|---|---|---|
| **Early-stage client** | high | 0.2.0 is 5 weeks old, 0 stars, 1 open issue; its own changelog says `stream_arrow` was "losing up to 99% of rows on dense ranges" until this release, and dense ranges are our workload. **Pin the version, and make phase 1's acceptance test a row-count reconciliation against public RPC for a sample of slots** |
| **History depth** | high | 8.5 months today (slot 391M, 2026-01-03). Anything older needs Old Faithful (section 8) or an Envio backfill commitment. A "since launch" pump.fun view is not possible on HyperSync alone |
| **Wire format not frozen** | medium | 0.2.0-rc.4 renamed tables and columns wholesale; IDL-aware decoding is "coming next" |
| **Prop-AMM opacity** | medium | ~32% of volume; we get price/volume but no pool state, and venue attribution depends on a registry we maintain |
| **Data volume** | medium | ~8.6 GB/day compressed, ~26x Base in rows. Budget disk before phase 2, not after |
| **Slot time** | medium | 0.267 s today, was 0.40 s before ~slot 430M. Every rows/day figure in section 5 scales inversely with it |
| **Someone treats Solana tables as chain-complete** | medium | section 5.5. Guard it in the schema (names) and the README, not in tribal knowledge |

## 8. Alternatives

Retrieved 2026-09-18; every row has a URL in the appendix.

| Source | Shape | History depth | Cost class | Fits a self-hosted ClickHouse indexer? |
|---|---|---|---|---|
| **Envio Solana HyperSync** | filtered columnar query API (JSON / Arrow) | **8.5 months** (slot 391M), backfill moving | covered by our existing token; every probe `x-ratelimit-cost: 0` | **Yes - this is the one that matches how the project already works** |
| Triton "Dragon's Mouth" (Yellowstone gRPC) | live gRPC stream | replay buffer only; window not published, discoverable via `SubscribeReplayInfo` | not published | No - live tail only, no bulk history |
| Helius LaserStream | live gRPC (Yellowstone-compatible) | **hard cap ~24h / ~216,000 slots** | mainnet needs Business $499/mo or Professional $999/mo | No - tail only. Would be a *complement* to a bulk source |
| Helius enhanced / parsed APIs | JSON-RPC + REST, per address | "archival" on all tiers, **depth not stated** | credit metered; archival calls 10 credits each | No - per-address pagination, no bulk export |
| Bitquery | GraphQL / WS / Kafka / gRPC, **plus Parquet-to-S3 and a managed ClickHouse replica** | self-service Solana is a **real-time window**: trades 30d, transfers and balances **8h**, instructions 3 months. Full history is Enterprise only | Personal $39/mo … Scale $239/mo; Solana archive packs sold separately ($210/mo OHLCV, $400/mo transfers+txns); Enterprise custom | Partly - only Enterprise, and Solana instruction history is S3-export-only |
| Dune | SQL warehouse + Datashare (Snowflake/BigQuery/Databricks/**S3 Iceberg**) | "full Solana blockchain"; start date not stated | credits; base plan prices not published on a readable page (free tier 2,500 credits/mo, PAYG $5/100 credits) | Indirectly, via Datashare/Iceberg. Not a streaming indexer |
| Flipside | — | — | — | **Gone.** `flipsidecrypto.xyz` 301s to an unrelated product; Dune publishes a "Flipside migration guide" referencing the shutdown |
| Google BigQuery public datasets | SQL warehouse | **no Solana dataset** - the `bigquery-public-data.crypto_*` family is BTC/BCH/DASH/DOGE/ETH/ETC/LTC/ZEC | n/a | No |
| **Old Faithful** (`rpcpool/yellowstone-faithful`) | **CAR file archive** + JSON-RPC/gRPC servers over it; Anza's **Jetstreamer** streams it "to any geyser plugin, **ClickHouse**, or your own plugin" | **genesis to ~1 epoch behind tip** (CAR built from the end-of-epoch snapshot, online 10-20h after epoch end) | free / self-host; Triton hosts a copy at `files.old-faithful.net`. **Total size not published** ("100s of GB" per epoch) | **Yes - the only free genesis-depth bulk path.** Repo self-describes as "RFC stage … not intended for production use", AGPL-3.0 |

**Reading of the table.** HyperSync is the right primary source: it is the same
API, the same token, the same rate-limit surface and the same `rollback_guard`
the EVM pipeline already speaks, and its server-side program filter is exactly
the thing section 5.5 says we must have. Its one real weakness is **history
depth**, and the mitigation is not a different vendor but **Old Faithful +
Jetstreamer as a one-off deep backfill** if and when the owner wants pre-2026
history - it writes to ClickHouse by design, and it is free. Everything else on
the list is either a live tail (no history), a warehouse (not an indexer), or
gone.
## 9. Recommendation

Three separable yes/no decisions. The first is urgent; the other two are not.

**A. Change the chain-neutral analytics tables to 32-byte identity columns and
the `(chain, block_number, tx_index, ordinal)` position key - this week.**
Cost: about a day per analytics module while the DEX module is being revised and
the launchpad tables are being written anyway. Cost if deferred: these columns
are in the `ORDER BY` of base tables and every MV-fed side table, ClickHouse
cannot alter a sorting-key column, so it becomes new tables + `INSERT..SELECT`
of every chain's history + a rebuild of every MV and aggregate, across 50+
chains - or two parallel table families forever. **This is worth doing even if
the answer to B is no**; the standing cost is a few compressed zero bytes.
*(Already sent to `launchpads` and `lead`, 2026-09-19 04:39 UTC.)*

**B. Add Solana DEX + launchpad data.** Recommended: **yes.**
- Solana is the **largest DEX chain**: $78.8B/30d, 26.1% of all DEX volume,
  bigger than Ethereum and Base combined.
- The EVM launchpad module is already approved, and **pump.fun tokens graduate
  into Solana AMM pools**. Without Solana DEX the launchpad module goes blind at
  the exact moment a token starts trading for real - which is the moment that
  matters to a trader.
- It is **feasible with the infrastructure we have**: same vendor, same API
  token, same `rollback_guard`, same tombstone/epoch machinery, and - verified by
  live probe - the position and balance data the decoders need.
- The decoding story is *better* than EVM in one important way: SPL balance
  changes cannot be forged by a program, so the corroboration step
  `src/dex/corroborate.rs` performs on EVM is structurally built into the data.
  It is *worse* in another: a third of the volume publishes no IDL and no event,
  so per-program decoders top out around 60% and the generic movement decoder
  must be a first-class path, not a fallback.

**C. Build it as a second pipeline in this binary** (`indexer run --chain
solana`): another `source`, a new `svm/` core data module, and the **same**
`dex/` and `launchpads/` analytics modules with a second decoder each. Not a
workspace split (premature - the shared layer is already chain-agnostic), not a
sister project (it would make the one screen the owner asked for impossible).
**~7-9 weeks** to a continuous pump.fun -> PumpSwap chart with USD and history.

**What the owner should say no to:** "index Solana the way we index an EVM
chain". All transfers, wallet history and chain-wide block stats are not
affordable and would be partial if attempted (section 5.5). Solana is an
**analytics-only pipeline**, and the schema and README must say so.

## Appendix

### A. Endpoints and retrieval times

| What | URL | Retrieved (UTC) |
|---|---|---|
| All-chain DEX overview | `https://api.llama.fi/overview/dexs?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` | 2026-09-19 01:22 |
| Per-chain DEX | `https://api.llama.fi/overview/dexs/<chain>` | 2026-09-19 01:22-01:23 |
| Solana aggregators | `https://api.llama.fi/overview/aggregators/solana` | 2026-09-19 01:23 |
| Real transactions | `https://api.mainnet-beta.solana.com` `getTransaction` (`jsonParsed`, `maxSupportedTransactionVersion:1`) | 2026-09-19 01:25-01:35 |
| Chain rates | same RPC, `getRecentPerformanceSamples`, `getBlockTime`, `getSlot` | 2026-09-19 01:38 |
| Envio docs | `https://docs.envio.dev/docs/HyperSync/solana`, `.../solana-query` | 2026-09-19 01:37 |
| Crate sources | `https://crates.io/api/v1/crates/{hypersync-client-solana,hypersync-solana-net-types,hypersync-solana-schema}/0.2.0/download` | 2026-09-19 |
| **Live HyperSync probes** (~12 requests, project token, read-only) | `https://solana.hypersync.xyz/query`, `/height` | 2026-09-19 04:40-04:44 |
| Base swap-log baseline | `https://mainnet.base.org` `eth_getLogs` | 2026-09-19 04:44 |
| Dune spellbook (program ids, decoder shapes) | `github.com/duneanalytics/spellbook`, `dbt_subprojects/solana/models/_sector/dex` | 2026-09-19 01:30 |
| Alternatives (section 8) | Triton `docs.triton.one/project-yellowstone/dragons-mouth-grpc-subscriptions`; Helius `helius.dev/docs/laserstream`, `/docs/laserstream/historical-replay`, `/pricing`, `/docs`; Bitquery `bitquery.io/blockchains/solana-blockchain-api`, `/pricing`; Dune `docs.dune.com/data-catalog/solana/overview`, `/learning/how-tos/pricing-faqs.md`, `/learning/flipside-migration-guide.md`; Flipside `flipsidecrypto.xyz` (301); BigQuery `cloud.google.com/blog/products/data-analytics/introducing-six-new-cryptocurrencies-in-bigquery-public-datasets-and-how-to-analyze-them`; Old Faithful `github.com/rpcpool/yellowstone-faithful`, `docs.old-faithful.net`, `/usage/jetstreamer.md` | 2026-09-18 |
| Chain-id conventions | `github.com/ChainAgnostic/namespaces/blob/main/solana/caip2.md`; `docs.hyperlane.xyz/docs/reference/domains`; `wormhole.com/docs/reference/supported-networks/`; `slip-0044.md`; `solana-labs/token-list`; `chainid.network/chains.json` | 2026-09-18 |

The `ENVIO_API_TOKEN` was read from the git-ignored `.env` into a shell variable
inside each probe command; it is not printed, stored or reproduced anywhere in
this document or the scratch files.

### B. Program ids referenced

PumpSwap `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA` ·
pump.fun `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` ·
Orca Whirlpool `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` ·
Raydium AMM v4 `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8` ·
Raydium CPMM `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C` ·
Raydium CLMM `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` ·
Meteora DLMM `LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo` ·
Meteora DAMM v2 `cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG` ·
Meteora DBC `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN` ·
LaunchLab `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj` ·
Manifest `MNFSTqtC93rEfYHB6hF82sKdZpUDFWkViLByLd1k1Ms` ·
BisonFi `BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi` ·
HumidiFi `9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp` ·
Tessera `TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH` ·
Scorch `SCoRcH8c2dpjvcJD6FiPbCSQyQgu3PcUAWj2Xxx3mqn` ·
QuantumAMM `QuaNtZsgYRe5Z9Bk4LZ4cTD9tbkVoyCNf1R2BN9bBDv` ·
GoonFi `goonERTdGsjnkZqWuVjs73BZ3Pb9qoCUdBUL17BnS5j` / v2 `goonuddtQRrWqqn5nFyczVKaie28f3kDkHWkHtURSLE` ·
AlphaQ `ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA` ·
Deriverse `DRVSpZ2YUYYKgZP8XtLhAGtT1zYSCKzeHfb4DgRnrgqD` ·
SolFi V2 `SV2EYYJyRz2YhfXwXnhNAevDEui5Q6yrfyo13WtupPF` ·
Aquifer `AQU1FRd7papthgdrwPTTq5JacJh8YtwEXaBfKU3bTz45` ·
ZeroFi `ZERor4xhbUycZ6gb9ntrhqscUcZmAbQDjEAtCf4hbZY` ·
Obric `obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y` ·
Byreal `REALQqNEomY6cQGZJUGwywTBD2UmDT32rZcNnfxQ5N2` ·
Quay `QUayE6nexQWYNZAEqfN8FxoNwQDSu3CAzT2qq9J1ArG` ·
Whalestreet `FW6zUqn4iKRaeopwwhwsquTY6ABWLLgjxtrC3VPnaWBf` ·
Jupiter v6 (router) `JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4` ·
Metaplex Token Metadata `metaqbxxUerdq28cj1RbAWkYQm3ybzjb6a8bt518x1s`

### C. Unverified - do not build on these without checking

1. **Commitment level** of HyperSync Solana data. Measured ~finalized (4-12
   slots behind `finalized`, 3 samples); not documented, not confirmed.
2. **`x-ratelimit-cost: 0`** on all 12 probes. Could be a beta grace period.
3. **Meteora DAMM v2 swap count** (5.1M/day measured) vs DefiLlama volume
   ($10M/day) imply a $2 average trade. One of the two is wrong.
4. **Slot time 0.267 s** is from a single 216,000-slot measurement; every
   rows/day figure scales inversely with it.
5. Prop-AMM discriminators (Scorch, GoonFi, AlphaQ, Quantum, SolFi V2, ZeroFi,
   Quay, Deriverse, Byreal, Aquifer, Obric) are taken from the Dune spellbook,
   **not** verified against a fetched transaction of each.
6. **Whether prop AMMs will stay decodable.** HumidiFi's instruction data
   already looks deliberately obfuscated. The movement layer is immune to that;
   any discriminator-based rule is not.
7. Programs seen in real Jupiter routes that I could not identify at all:
   `riptK81h...`, `ghosty4Z...`, `BSwp6bEB...`.
8. **Old Faithful total archive size** is not published anywhere I could find
   ("100s of GB" per epoch is the only hint). Size it before planning a deep
   backfill.
9. **Dune's Solana history start date** and its paid plan prices are not stated
   on any page that renders without JavaScript.
10. Solana **token-2022 transfer hooks** (arbitrary program calls on transfer)
    were not exercised by any fetched transaction; their effect on the subtree
    rule is reasoned, not measured.

### D. Questions to send Envio

See section 4.7 (eight questions), plus: does the JSON-RPC facade
(`POST /` / `POST /rpc`) serve `getAccountInfo` / `getMultipleAccounts`? If it
does, Solana token metadata needs no external RPC at all.

## 11. Phase 2 plan: head following, history backfill, cost

STATUS: COMPLETE. Analyst: `solana-plan`, 2026-09-19.
Basis: 14 live requests to `solana.hypersync.xyz` / `1.hypersync.xyz` between
07:05:39 and 07:09:37 UTC (every one recorded, with its headers, in appendix E),
3 free `/height` + public-RPC samples at 07:10:03-07:10:13 UTC, and the Envio
and Old Faithful documentation retrieved 2026-09-19 (URLs in appendix F).
Section 1-10 above is unchanged; where this section contradicts it, this
section is the later measurement and wins.

### The ten lines: what the owner has to decide

1. **The free Envio token is 30 queries a minute, per endpoint.** Measured, not
   guessed: `x-ratelimit-limit: 30000, 30000;w=60` with a flat
   `x-ratelimit-cost: 1000` on every single query. `GET /height` carries no
   rate-limit headers at all and needs no token - it is free.
2. **That is enough to follow the Solana head forever.** Keeping up needs 7 to
   15 of those 30 queries a minute. **No paid tier is required for live data**,
   now or later.
3. **It is not enough to load the history.** The 8.5 months Envio has is 57.3
   million slots; at the ~35 slots a query the server will actually return that
   is 1.64 million queries, which is **48 days of the entire free budget**.
4. **DECISION 1 - pay Envio $70 for one month, or wait.** "Starter" ($70/month,
   100 queries a minute) does the backfill in **~13 days** while still
   following the head. Free does it in 48 days with the head paused, or ~11
   weeks if you follow the head at the same time. $70 once, then back to free.
5. **DECISION 2 - how deep is the history.** Envio starts at 2026-01-03 and
   nothing will change that. Older than that exists only in the free Old
   Faithful archive, which means pulling **~113 TB** and writing a second
   ingest path. Recommendation: take Envio's 8.5 months, and treat "since
   pump.fun launched" as a separate project that is not worth it yet.
6. **DECISION 3 - hardware.** Solana alone writes **~12 GB a day compressed,
   ~4.3 TB a year** - **26x the measured Base swap rate**, and 2.5 to 10 times
   all the EVM chains put together.
   Budget an **8 TB NVMe** and 64-128 GB of RAM for year one, plus ~220 Mbit/s
   of sustained download for the fortnight the backfill runs.
7. **Reorgs can be switched off, carefully.** Envio serves Solana at (just
   behind) `finalized` - re-measured today, 5 to 19 slots behind it. So the
   Solana pipeline needs **no fork-point search and no confirmations**, but it
   must keep the tombstone/epoch machinery for gap heals and keep the
   parent-hash check as a tripwire that stops the indexer loudly if the
   assumption ever breaks.
8. **A bug found while measuring, and it is the first thing to fix.** The query
   the merged code sends returns **one slot per request**, because it raises
   only `max_num_instructions` and a different, unset cap binds first. Raising
   all the caps turns 1 slot per query into 35. An hour's work, and nothing
   else in this plan is possible without it.
9. **Freshness will be ~14-20 seconds behind the live chain**, of which 10-13
   seconds is Envio's own lag and no amount of polling changes it. Measured
   against Solana's own `finalized`, we would be 3-9 seconds behind.
10. **The engineering is ~12 working days in eleven tasks**, five of which can
    run in parallel (section 11.5).

### 11.1 The rate limit: what it is, and it is not what section 4.6 says

Section 4.6 records `x-ratelimit-cost: 0` on all twelve probes of 2026-09-19
04:40-04:44 UTC. The phase-1 engineer then observed cost 1000. I re-measured at
07:05-07:09 UTC on the same token: **cost is 1000 on every query, and it was
1000 on the very first one of the session**. Either the grace period ended in
those two hours, or the earlier probes read the header from a client that did
not surface it. Either way, **the section 4.6 conclusion "no separate plan
appears to be needed" is dead, and the section 8 table row saying probes billed
0 must be read as superseded.**

What the headers say (`https://solana.hypersync.xyz/query`, all 9 metered
Solana requests identical):

```
x-ratelimit-cost: 1000
x-ratelimit-limit: 30000, 30000;w=60
x-ratelimit-remaining: 29000        (then 28000, 27000, ...)
x-ratelimit-reset: 39               (seconds until the window resets)
```

| Question | Answer, and the evidence |
|---|---|
| Which headers come back | `x-ratelimit-cost`, `-limit`, `-remaining`, `-reset`. Nothing else; no `retry-after` was seen (no 429 was provoked) |
| Window length | **60 seconds**, stated in the header itself: `30000;w=60` is the RFC "RateLimit" quota-policy form, `w` = window in seconds. Confirmed by watching `x-ratelimit-reset` count down 39 → 38 → 37 over three back-to-back requests and reset to a fresh 30000 budget in the next minute |
| Budget and therefore queries | 30,000 budget units / 1,000 per query = **30 queries per 60 s** |
| Does cost depend on the query | **No - it is flat.** A query returning **378 bytes** (p06: one slot, a filter that matches nothing, one field) and a query returning **46.2 MB** (p08: 40 slots, 110,931 rows across four tables) both cost exactly 1000. Rows returned, slots scanned, number of selections and number of field-selection columns all make no difference |
| Is `/height` free | **Yes, and it is not even metered.** `GET /height` with and without a token returned **no `x-ratelimit-*` headers at all** (p01, p02). The docs confirm it needs no token. `GET /height/sse` is the same endpoint as a stream |
| Arrow vs JSON | Same cost. `POST /query/arrow` billed 1000 (p12) and returned **19.9 MB against JSON's 46.2 MB for the identical query - 43%** |
| The EVM endpoint | **Identical, and on a separate counter.** `https://1.hypersync.xyz` returns the same `30000;w=60` with flat cost 1000 (p13: 214 bytes, p14: 40.1 MB, both 1000). Crucially, a Solana query had just taken the Solana counter to 29,000 when the first EVM query reported its own fresh 29,000 with a different `reset` offset - so **each chain endpoint has its own 30,000/60 s pool. Adding Solana does not eat the EVM chains' budget.** (Caveat: this is two data points; it could in principle be per edge node rather than per chain. Re-check if it ever matters financially.) |

Envio's own documentation (retrieved 2026-09-19, `docs.envio.dev/docs/HyperSync/
stream-config-tuning`) describes exactly this contract - `remaining` "counts
budget units, not requests - divide it by `cost` for the number of requests you
have left" - but **publishes no numbers**: the free tier is described only as
"Fair-use based rate limiting", and neither the window length nor the cost
formula appears anywhere. The runtime headers are the only source of truth, and
the client must read them rather than hard-code 30.

**The paid tiers** (`https://envio.dev/pricing/hypersync`, retrieved
2026-09-19):

| Plan | Price | Stated limit | Queries/day | Slots/day at 35 slots/query |
|---|---|---|---|---|
| Free | $0 | "fair-use" - **measured 30/min** | 43,200 | 1.51M |
| Starter | **$70/month** | "100 requests per minute", burst to 250 | 144,000 | 5.04M |
| Pro | **$480/month** | "1,000 requests per minute", burst to 2,500 | 1,440,000 | 50.4M |
| Custom | "talk to us" | custom, SLA, support | | |

The page also states "Rate limits cap how fast you can query, never how much
per month" and "All plans include data for every supported chain. There are no
strict data volume limits." Yearly billing is "2 months free". Nothing on the
page mentions Solana pricing separately; Solana is labelled "Beta" in the
product navigation.

### 11.1.1 The measurement that changes the plan: 35 slots per query, not 107

Section 4.6 records "107 slots, 33,886 instruction rows, 5.99 MB in 2.7 s" with
`max_num_instructions: 200000`. That was an instruction-only query. **The query
the merged code actually sends** (`src/source/solana.rs::build_query`: 20+ venue
programs OR the SPL/Token-2022/System transfer union, plus
`account_activity: [{}]` and `include_all_blocks: true`) behaves completely
differently:

| Probe | Query | `max_num_*` set | Slots returned | Bytes | Wall |
|---|---|---|---|---|---|
| p05 | the production shape, slots 448,300,000+ | only `instructions: 200000` (**what the code does today**) | **1** | 1.25 MB | 1.8 s |
| p08 | the same query | `blocks`, `transactions`, `instructions`, `account_activity` all raised to 10^6 | **40** | 46.2 MB | 7.8 s |
| p15 | the same query, slots 448,320,000+ | same | **30** | 26.8 MB | 5.7 s |
| p11 | headers only (instruction/activity selections that match nothing) | same | **10,000** (the whole requested range) | 3.6 MB | 12.1 s |

Three findings, in order of importance:

1. **The merged source gets one slot per query.** `max_num_instructions` is
   raised but `max_num_account_activity` and `max_num_transactions` are not,
   and one of them binds first. At 30 queries a minute that is 0.5 slots/s
   against a chain producing 3.76 - **the pipeline cannot even follow the head
   as written.** This is the wall phase 1 hit and it is a one-line fix.
2. **With every cap raised, the binding limit becomes the server's own
   execution budget, and it lands at 30-40 slots.** Not bytes (26.8 MB and
   46.2 MB both stopped) and not rows; the Envio EVM docs state a "5-second
   query execution limit" and the two wall times (5.7 s, 7.8 s, transfer
   included) fit that. **Use 35 slots per query as the planning number, and
   never assume it: the client must follow `next_slot` and not its own
   arithmetic.**
3. **Header-only queries are ~300x cheaper per slot** (10,000 slots in one
   query). That is what makes the `parent_slot` / `block_height` verification
   sweep in section 11.4 affordable: a full re-verification of 8.5 months of
   headers is 5,734 queries, i.e. **3.2 hours of the free budget**, against 48
   days for the data.

There is also a **client-side trap** waiting in the same place. The Solana
`StreamConfig` defaults (documented at `docs.envio.dev/docs/HyperSync/
solana-client`) are `response_bytes_ceiling: 500_000` and
`response_bytes_floor: 250_000`, and the client auto-tunes `batch_size` to keep
responses inside that band. Measured Arrow density is **0.50 MB per slot**, so
the auto-tuner will converge on a batch of **one slot** and quietly re-create
the same problem after the query caps are fixed. Both ceilings must be raised
to the tens of megabytes in `StreamConfig`, with a test that asserts it.

### 11.2 Head following: cadence, latency, and whether we must pay

**The chain's own rate, re-measured from my own probes and not taken from
section 5.1.** Slot 448,300,000 has `block_time` 1789792277 (p04); the
`rollback_guard` in the same response puts slot 448,334,809 at timestamp
1789801544. That is 34,809 slots in 9,267 s = **0.2662 s/slot = 3.756 slots/s =
324,538 slots/day.** Independently, the public RPC's `processed` slot advanced
40 slots in the 10 s between my first and third `/height` sample (4.0 slots/s).
Every number below scales inversely with this and it has moved before (it was
0.40 s until ~slot 430M).

**The minimum.** Keeping up needs 324,538 slots/day. At the measured 35 slots
per response that is 9,272 queries/day = **6.4 queries/minute against a free
budget of 30.** So the answer to "is a paid tier required for live following"
is **no, with 4.7x of headroom**, and that headroom is what pays for retries,
gap heals and the header sweeps.

**But at the head you cannot batch 35 slots** - they have not happened yet. The
cadence, not the cap, sets the query rate:

| Poll interval | Slots per query | Queries/min | % of free budget | Instruction rows per response | Arrow per response | End-to-end lag behind the live chain |
|---|---|---|---|---|---|---|
| 2 s | 7.5 | 30 | 100% | 7,600 | 3.7 MB | 13-18 s |
| **4 s** | **15.0** | **15** | **50%** | **15,200** | **7.5 MB** | **14-20 s** |
| 6 s | 22.5 | 10 | 33% | 22,700 | 11.2 MB | 16-22 s |
| 8 s | 30.0 | 7.5 | 25% | 30,300 | 15.0 MB | 18-24 s |
| 10 s | 37.6 | 6 | 20% | — | — | the server truncates at ~35, so this costs 2 queries and buys nothing |

Row counts use the measured per-slot rates of the production selection (below);
the `~1.4x` child-SPL-transfer ride-along section 4.3 identified is already
inside them, because the transfer union is part of the selection that was
measured.

**Recommendation: a 4-second cadence, 15 queries a minute, half the free budget
left over.** Cheaper cadences buy nothing because of the next paragraph.

**Where the latency actually goes.** Three `/height` samples against the public
RPC at 07:10:03-07:10:13 UTC:

| Sample | Envio head | RPC `finalized` | RPC `processed` | Behind finalized | Behind processed |
|---|---|---|---|---|---|
| 07:10:03 | 448,335,737 | 448,335,756 | 448,335,786 | 19 slots | 49 slots |
| 07:10:08 | 448,335,763 | 448,335,776 | 448,335,807 | 13 slots | 44 slots |
| 07:10:13 | 448,335,790 | 448,335,795 | 448,335,826 | 5 slots | 36 slots |

At 0.2662 s/slot: **Envio is 9.6-13.0 s behind the live chain and 1.3-5.1 s
behind `finalized`.** (Section 4.5 measured 33-44 and 4-12 slots at 01:38 UTC;
the two sessions agree.) The budget, at a 4 s cadence:

```
  9.6 - 13.0 s   Envio's own ingest lag          <- we cannot change this
  2.0 s          average half of the poll interval
  1.5 - 3.5 s    query + transfer of a 15-slot response
  0.5 - 1.5 s    decode + writer flush (tip_interval)
  -------------
  13.6 - 20.0 s  from a Solana transaction executing to it being queryable
   3.3 -  9.1 s  from that transaction being FINALIZED to it being queryable
```

**The second number is the honest one.** "18 seconds behind the chain" is
mostly "Solana's finality plus Envio's ingest", and a sub-second Solana feed is
a different product (Yellowstone gRPC / Helius LaserStream, section 8) with no
history behind it. For candles, volume and a launch feed, 3-9 seconds behind
finality is fine.

**Two free things the follower must use.**
- `GET /height/sse` is an unauthenticated, **unmetered** server-sent-events
  stream of the head slot (confirmed: `/height` carries no `x-ratelimit-*`
  headers at all). The follower should take its head signal from there and
  never spend a metered query discovering that nothing happened. On a quiet
  endpoint that turns the poll loop into "wake on SSE, query only when
  `head > committed`".
- Header-only queries cost the same 1000 but return 10,000 slots (p11), so the
  `parent_slot` / `block_height` verification of section 11.4 is essentially
  free.

**Bandwidth at the head:** 324,538 slots/day x 0.499 MB/slot Arrow =
**162 GB/day = 1.87 MB/s = 15 Mbit/s** sustained, or 375 GB/day in JSON. Use
Arrow (`POST /query/arrow`, 43% of JSON, same cost).

**Row rates the decoder must chew at the head**, measured over the 70 slots of
p08 + p15 with the real production selection:

| Table | Rows/slot | Rows/day | Rows/s at the head |
|---|---|---|---|
| `instruction_calls` (venues + SPL/T22/System transfers) | 1,011 | 328M | 3,800 |
| `account_activity` | 1,286 | 418M | 4,830 |
| `transactions` | 210 | 68M | 790 |
| `blocks` | 1 | 0.32M | 3.8 |
| **total source rows** | **2,508** | **814M** | **9,420** |

Section 5.2 modelled 346 matched instruction rows/slot for the DEX programs and
494 SPL transfer rows/slot separately; my 1,011 is those two plus the System
transfers and the extra venues the merged registry streams, so the two
measurements agree. **9,400 source rows a second is not a problem.** 146,000 a
second, which is what a Starter-tier backfill implies, might be - see 11.3.

### 11.3 History backfill: 8.5 months, three ways

**The size of the job.** Envio serves from slot **391,000,000** (2026-01-03
07:37 UTC, section 4.4); the head was **448,334,753** at 07:05:39 UTC today.
That is **57,334,753 slots**, 8 months and 16 days, **132.7 Solana epochs**.

At the measured 35 slots per response: **1,638,136 queries**, and **28.6 TB** of
Arrow over the wire (66 TB as JSON).

| Path | Queries/day | Calendar time for 57.3M slots | Money | Sustained download |
|---|---|---|---|---|
| Envio **free**, head follower paused, one forward sweep | 43,200 | **48.3 days** | $0 | 55 Mbit/s |
| Envio **free**, head followed at 4 s in parallel | 21,600 for the sweep | **75.8 days** | $0 | 28 Mbit/s |
| Envio **Starter**, one forward sweep | 144,000 | **12.2 days** | **$70 once** | 220 Mbit/s |
| Envio **Starter**, head followed at 4 s in parallel | 122,400 for the sweep | **13.4 days** | **$70 once** | 200 Mbit/s |
| Envio **Pro** | 1,440,000 | 1.2 days *by budget* — not achievable, see below | $480/month | 2.2 Gbit/s |
| **Old Faithful + Jetstreamer** | n/a | 10.5 days at a saturated 1 Gbps; 8.4 h at 30 Gbps | $0 (egress free today) | **113 TB total** |

The "one forward sweep" rows account for the head moving while you sweep (net
drain = 1.51M − 0.32M slots/day on free). The parallel rows do not, because a
separate follower is already handling the new slots.

**Why Pro is not worth considering.** 1,000 queries/minute is 16.7 queries a
second; each takes 5-8 s server-side, so it needs 100-130 concurrent requests in
flight, and 50.4M slots/day is **1.46 million source rows a second** to decode
and insert. That is not a single-node number. **Pro's money buys throughput this
project cannot consume.**

**The honest caveat on Starter, and it should be tested before the money is
spent.** Starter implies 5.04M slots/day = **146,000 source rows a second**
through the decoder. Nobody has measured what `svm::decode` sustains. If it
does 50,000 rows/s, the real rate is 1.7M slots/day = **34 days regardless of
tier**, and Starter is wasted. **Task S0: run the recorded fixtures through
`svm::decode` in a loop and measure rows/s before buying anything.** Size the
tier to the decoder, not to the API.

#### 11.3.1 Old Faithful / Jetstreamer, with the numbers their docs give

Sources retrieved 2026-09-19: `github.com/rpcpool/yellowstone-faithful`,
`docs.old-faithful.net`, `github.com/anza-xyz/jetstreamer` (note: the repo is
Anza's; `github.com/rpcpool/jetstreamer` is a 404), and the project's
auto-generated CAR report at
`raw.githubusercontent.com/rpcpool/yellowstone-faithful/gha-report/docs/CAR-REPORT.md`.

- **Depth: genesis to the epoch before the current one.** The only free path to
  anything older than 2026-01-03. CARs land "within 10-20h after epoch end"
  (target "within 4 epochs").
- **Size per epoch, measured in their own report:** recent epochs run
  **586 GB (epoch 966), 604 GB (979), 715 GB (1000), 1,215 GB (1019), 1,059 GB
  (1023), 917 GB (1036)** of CAR, plus 40-123 GB of indexes each. Our 8.5 months
  is **epochs 905-1037**, mean ~849 GB → **~113 TB** of CAR to pull. The total
  archive size is **not published anywhere**; extrapolating their report over
  1,000+ epochs puts it in the hundreds of TB.
- **No server-side filtering.** Jetstreamer selects by epoch or slot range only
  (`900-950`, `358560000:367631999`, `--reverse`); the plugin filters locally.
  **Narrowing to our 26 programs saves zero bandwidth** - all 113 TB crosses the
  wire. This is the single fact that decides it.
- **Throughput:** "over 2.7M TPS to a local Jetstreamer plugin or geyser
  plugin", achieved on "64 core CPU, 30 Gbps+ network". Default
  `JETSTREAMER_NETWORK_CAPACITY_MB=1000` assumes ~8 Gbps. Memory default is
  `min(4 GiB, 15% of RAM)`. Requires Clang 16 exactly. No epochs/hour or MB/s
  figure is published; the only calendar claim is "a full ingest within days".
- **Cost:** "This archive is currently completely free to use." Hosted in
  **Amsterdam** ("download using servers nearby for best throughput"). The
  repo still says "RFC stage … not intended for production use". Triton's
  *hosted* archive RPC is separate and is paid ($10 per million queries, $10/mo
  minimum) - that is not the bulk path.
- **It writes to ClickHouse by design** (built-in sinks: any geyser plugin,
  ClickHouse, or a custom plugin), but not to *our* schema, so it is still a
  second ingest path: a Jetstreamer plugin that feeds `svm::decode`.
- Caveats from their feature table that matter if this is ever built: epochs
  0-156 are "incompatible with modern Geyser plugins", epochs 0-449 report
  compute units as 0, and "Old Faithful does not contain account updates".
  Transactions arrive "in their already-executed state as they originally
  appeared to Geyser", which is exactly what the movement layer needs.

#### 11.3.2 Recommendation

**Buy one month of Envio Starter ($70), run the backfill in ~13 days, then go
back to free.** Reasons, in order:

1. For the *same* 8.5 months, Envio moves **28.6 TB** and Old Faithful moves
   **113 TB** - a 4x difference, entirely because Envio filters server-side and
   Jetstreamer cannot.
2. Envio needs **no new code**: the same client, the same query, the same
   decoder, the same writer. Old Faithful needs a second ingest path and a
   10 Gbps-class machine just to match Envio's calendar time.
3. $70 is less than a fortnight of the electricity the alternative burns.
4. Free is a legitimate answer if the owner does not mind waiting: 48 days with
   the head paused, and the tip can be switched on the moment the sweep
   finishes. It is the difference between "history in October" and "history in
   late November".

**Do not build the hybrid now.** Old Faithful's only unique value is
**pre-2026-01-03**, which Envio cannot serve at any price. Keep it in the drawer
for a future "since pump.fun launched" project and size it then; the archive is
not going anywhere and it is getting cheaper to consume, not dearer.

**Date-based estimate.** The backfill cannot start until tasks S1 and S10 of
section 11.5 land. Taking ~2 weeks of engineering from a start on 2026-09-22,
the driver is ready around **2026-10-06**:

| Plan | Sweep | Complete | Covering |
|---|---|---|---|
| Starter, $70 | 13.4 days | **~2026-10-20** | 2026-01-03 → live |
| Free, head paused | 48.3 days | ~2026-11-23 | 2026-01-03 → live |
| Free, head followed | 75.8 days | ~2026-12-21 | 2026-01-03 → live |

All three stretch if `svm::decode` turns out to be the bottleneck (task S0).
Sweep **backwards from the head**, not forwards from 391M: the recent months are
the ones a chart needs first, and an interrupted backward sweep still leaves a
contiguous, useful window.

### 11.4 Commitment, reorgs and contiguity

#### 11.4.1 What commitment Envio serves, and what follows

**Not documented.** I re-read `docs.envio.dev/docs/HyperSync/solana`,
`/solana-query`, `/solana-client` and `/solana-curl-examples` on 2026-09-19:
the words `confirmed`, `finalized` and `commitment` do not appear as a
guarantee anywhere. The closest the docs come is the `rollback_guard` advice to
"re-sync from a finalized slot", which presupposes you work finality out
yourself. **This remains question 1 for Envio (section 4.7) and it is now the
only question on that list that blocks a design decision.**

**What measurement says, across two independent sessions:**

| Session | Samples | Behind RPC `finalized` | Behind RPC `processed` |
|---|---|---|---|
| 2026-09-19 01:38 UTC (section 4.5) | 3 | 4-12 slots | 33-44 slots |
| 2026-09-19 07:10 UTC (this section) | 3 | 5-19 slots | 36-49 slots |

**Six of six samples put Envio's head behind `finalized`, never ahead of it.**
`finalized` on Solana means rooted by a supermajority of stake; a rooted slot is
not abandoned by the normal fork-choice rule at all - abandoning one requires a
cluster restart from a snapshot, which has happened (September 2021, February
2024) and whose documented recovery procedure restarts from the last
*optimistically confirmed* slot, i.e. at or above the last rooted one.

#### 11.4.2 The proposal: no fork search, keep the tombstones

**Yes, the Solana pipeline should skip reorg handling - specifically, it should
skip the fork-point search - and no, it should not lose the machinery.**

| Layer (`src/reorg/README.md`) | EVM | Solana | Why |
|---|---|---|---|
| 1. `--confirmations N` | tunable, default 0 | **0, and reject any other value with a message** | staying behind a head that is already behind `finalized` buys nothing and costs freshness twice |
| 2. Detection (parent hash + `rollback_guard`) | on | **on** — and add the `block_height` chain (11.4.3) | it costs one comparison per slot and it is the tripwire that tells us the finality assumption broke. Deleting it would mean discovering a problem as a wrong chart |
| 3. Fork-point search (`fork.rs`, k = 8, 16, 32 …) | on | **off** | there is no fork to find. A mismatch on finalized data is not a fork, so walking backwards looking for agreement is answering the wrong question, and it costs metered queries to do it |
| 4. Rollback = `purge_range` + resume | on | **on, unchanged** | it is the only repair routine there is, and gap healing needs it whatever finality does |
| Tombstones + epochs + `epoch_floor_v` | on | **on, unchanged** | they cost nothing when unused and everything if absent when needed. `SvmRows::set_epoch` / `set_version` already do their half |

**What a mismatch should do instead of a fork search.** On a parent-hash,
`block_height` or `rollback_guard` mismatch at slot S:

1. WARN loudly with both hashes and a dedicated `reason = 'parent_mismatch'`
   row in `reorgs` - this is an event that is *not supposed to happen*, and it
   must be visible rather than silently repaired.
2. `purge_range(chain, S, ∞)` and re-stream from S. The fork point **is** S,
   because everything below it was finalized when we stored it.
3. If the re-streamed S mismatches again against S-1, the same rule fires one
   slot lower. That converges, one slot per pass, and `--max-reorg-depth`
   (default 512) is still the fuse: reaching it is fatal with a clear message,
   exactly as today. A finalized chain that disagrees with us 512 slots deep is
   a wrong endpoint or a re-ingest bug, not a reorg.

**Residual risk, stated plainly.**

| Risk | Likelihood | What it would look like | Mitigation in this design |
|---|---|---|---|
| Envio's commitment is not what six samples say, or changes | **medium** - it is undocumented, the product is labelled Beta, and the wire format already changed once in `0.2.0-rc.4` | the parent/height tripwire fires | detection stays on; a purge-and-restream repairs it; the `reorgs` table records it |
| Envio re-ingests a slot from a different source and `transaction_index` changes | **medium** - section 4.7 question 8 is still unanswered | the position key `(chain, slot, tx_index, ordinal)` points at a different instruction; **no hash check catches this**, because the block itself is identical | this is the one hole. Mitigate by storing `sol_slots.blockhash` (already) *and* by making `indexer verify` re-fetch a random sample of stored slots and compare row counts per slot. A cheap nightly 100-slot sample is 3 metered queries |
| A Solana cluster restart discards rooted slots | **low** - twice in five years, and the recovery targets a slot at or above the last rooted one | the tripwire fires, possibly over a wide range | `--max-reorg-depth` turns it into a loud stop rather than a silent half-purge; the operator re-runs with a larger value |
| We set `--confirmations 0` and Envio later starts serving `confirmed` | low | frequent tripwire firing | the metric `solana_parent_mismatch_total` should page. If it ever becomes routine, the answer is `--confirmations 32`, not re-adding the fork search |

#### 11.4.3 Contiguity: how a gap is detected when skipped slots are normal

**First, a measurement that reframes the problem.** The 10,000-slot header
sweep (p11, slots 448,300,000-448,309,999) returned **10,000 block rows, with
zero missing slot integers, zero `block_height` breaks and zero parent-chain
breaks.** Solana's skip rate in this region is below 0.01%. That does **not**
license an integer-gap check - the skip rate has been percent-scale
historically and one leader outage brings it back - but it does mean skipped
slots are an exception to handle correctly, not a constant background hum.

**The structural fact that makes this easy.** With `include_all_blocks: true`
the server returns a block row for **every slot it has** inside the window it
served. So inside one response's `[from_slot, next_slot)` a missing integer is
a genuinely skipped slot, full stop. **A gap can therefore only exist *between*
served windows - it is a property of the cursor, not of the block rows.** That
single sentence is the whole Solana contiguity design.

**Three witnesses, in increasing strength. Use all three; they are cheap.**

1. **The cursor (`next_slot`), and it is the primary one.** Checkpoint windows
   must tile `[start_slot, head)` with no hole and no overlap. This is pure
   `from`/`to` arithmetic - `db::ranges::contiguous_until` already does exactly
   it for EVM - and it works unchanged provided `to_block` is written as the
   server's `next_slot`, **not** as `max(slot) + 1`. That is the one semantic
   change, and getting it wrong is what would produce endless false gaps.
2. **`block_height`.** Solana's `block_height` counts *produced blocks*, so it
   increments by exactly 1 per block **regardless of how many slots were
   skipped**. For consecutive stored slots P < S with nothing stored between:
   `S.block_height == P.block_height + 1` **proves no block was lost**, and it
   is immune to skipped slots in a way `parent_slot` arithmetic is not.
   Verified over all 10,000 rows of p11 and all 40 of p08: not one break.
   `BlockField::BlockHeight` is already selected in `src/source/solana.rs`, so
   this costs nothing but the comparison. **This is the Solana replacement for
   the EVM "every integer has a `blocks` row" gap query.**
3. **`parent_slot` + `parent_blockhash`.** `S.parent_slot == P.slot` and
   `S.parent_blockhash == P.blockhash`. Witness 2 counts; this one *identifies*.
   It is what catches a wrong block rather than a missing one, and it is the
   tripwire of 11.4.2.

**Do not use Envio's `next_slot` as a substitute for witness 2 or 3.** It tells
you what the server *served*, which is what you asked for plus a truncation; it
cannot tell you the server served the right thing.

#### 11.4.4 What the checkpoint row must contain

`checkpoints` today is `(chain, from_block, to_block, _version)`. Since no data
is loaded anywhere (design.md's opening line), extending it in migration `0004`
is free, and three of the four additions help the EVM side too.

| Column | Meaning on Solana | Meaning on EVM | Why |
|---|---|---|---|
| `from_block` | first slot of the served window, inclusive | unchanged | |
| `to_block` | **the server's `next_slot`**, exclusive | unchanged | the cursor, not `max(slot)+1`. This is the change that makes witness 1 work |
| `blocks_present UInt32` *(new)* | how many `sol_slots` rows landed in the window | how many `blocks` rows landed | `to_block - from_block - blocks_present` = the skipped slots, and on EVM it is always 0. It is what lets `verify` say "5 skipped" instead of "5 missing" |
| `last_block UInt64` *(new)* | highest slot with a row in the window | highest block | the anchor the next window's parent check compares against |
| `last_hash FixedString(32)` *(new)* | its `blockhash` | its `hash` | **removes a `blocks FINAL` read on every resume**, on both families |
| `last_height UInt64` *(new)* | its `block_height` | = `last_block` | carries witness 2 across a restart and across a window boundary |
| `epoch`, `_version`, `is_deleted` | unchanged | unchanged | `purge_range` already tombstones overlapping checkpoints |

#### 11.4.5 What `indexer verify` must check for Solana

`src/pipeline/verify.rs` runs three checks today (gaps, orphan children,
checkpoints). Check 1 as written - "blocks without a live `blocks` row" - would
report every skipped slot as a gap forever, so it needs a family switch (or
Solana needs its own entry point; a switch on `chains.family` is smaller).

| # | Check | Notes |
|---|---|---|
| 1 | **Cursor tiling.** Live checkpoints tile `[start_slot, head]` with no hole and no overlap | replaces the EVM integer-gap query. It is the only thing that can detect "we never asked for these slots" |
| 2 | **Height chain.** For consecutive stored slots with nothing between: `S.block_height == P.block_height + 1`; across a window boundary, against the previous checkpoint's `last_height` | detects a *lost produced block* without false-positiving on skipped slots. This is the strong one |
| 3 | **Parent chain.** `S.parent_slot == P.slot` and `S.parent_blockhash == P.blockhash` for the same pairs | detects a *wrong* block. Redundant with 2 for the missing case, which is the point |
| 4 | **Orphan children.** Every live `sol_transactions` / `sol_dex_swaps` row's slot has a live `sol_slots` row | **carries over from EVM unchanged.** It is the gap-heal invariant and it is the check that matters most in practice, because a flush that dies before its `sol_slots` insert is far more likely than anything in 11.4.2 |
| 5 | **Epoch floor.** Every live row's `epoch >= epoch_floor_v` for the chain | unchanged |
| 6 | **Sample re-fetch** *(new, optional, nightly)* | re-query 100 random stored slots and compare per-slot row counts against what is stored. 3 metered queries. This is the **only** defence against the `transaction_index` re-ingest risk of 11.4.2, which no hash can catch |

Checks 1-5 are read-only over ClickHouse and cost no Envio budget. Check 6 costs
3 queries a night out of 43,200.

### 11.5 Integration shape: the task list for `indexer run --chain solana`

Phase 1 merged `src/svm/` (tables, decoders, fixtures, live tests) and
`src/source/solana.rs`. **Nothing is wired to the pipeline**: `SolanaSource` is
referenced only by `src/svm/live_tests.rs`. Everything below is the wiring, and
it is deliberately a list of small, separable jobs rather than "build the
Solana pipeline".

Effort tags: **S** = half a day or less, **M** = one to two days, **L** = three
to five days.

| # | Task | Effort | Depends on | Can run in parallel with |
|---|---|---|---|---|
| **S0** | **Measure `svm::decode` throughput** on the recorded fixtures: rows/s, single core and with the pipeline's real channel. It decides which Envio tier is worth buying (11.3) | S | — | everything |
| **S1** | **Fix the query caps.** Raise `max_num_blocks` / `max_num_transactions` / `max_num_account_activity` alongside the existing `max_num_instructions` in `build_query`; raise `StreamConfig::response_bytes_ceiling` / `_floor` well above the measured 0.5 MB/slot Arrow. Add a live test asserting a response covers **more than 10 slots** | S | — | everything |
| **S2** | **Rate-limit governor.** A per-endpoint token bucket seeded from `x-ratelimit-limit` / `-cost` / `-remaining` / `-reset` (never hard-code 30), with priorities: head follower > gap heal > verify > backfill. Use `get_with_rate_limit` and `proactive_rate_limit_sleep`. Note the client asymmetry Envio documents: the Solana `*_with_rate_limit` methods retry a 429, the EVM ones do not | M | — | S3, S4, S7, S8 |
| **S3** | **The `BlockSource` seam.** `SourceResponse.data` is `ResponseRows` (EVM). Make it an enum `ChainRows { Evm(ResponseRows), Svm(SvmRows) }` and match once in `Writer::flush` and once in `ClickhouseSink` — see the note below | M | S1 (for testing) | S2, S4, S7, S8 |
| **S4** | **Slot-aware checkpoints and `Progress`.** Write `to_block = next_slot`; add `blocks_present`, `last_block`, `last_hash`, `last_height` to migration `0004` (11.4.4); make `missing_ranges` / `contiguous_until` cursor-based rather than "every integer has a row" | M | — | S2, S3, S7, S8 |
| **S5** | **Solana reorg variant.** A `CanonicalChain` over `SolanaSource::headers`; parent-hash **and** `block_height` continuity; **no** fork-point search — a mismatch purges from the mismatching slot up (11.4.2). Reject `--confirmations != 0` | M | S4 | S6, S9 |
| **S6** | **Writer + commit marker.** `sol_slots` written **last**, `svm::BASE_TABLES` order for the purge, epoch/version stamping (`SvmRows::set_epoch` / `set_version` already exist), and an `svm` entry alongside `ModuleSpec` so `purge_range` and `verify` can enumerate its tables | M | S3 | S5, S9 |
| **S7** | **CLI and chain registration.** `--chain` accepts a **name or a number** via a `clap` value parser (try `u64`, else a tiny const table containing `solana → 1399811149`); `CHAIN_ID` keeps working; run `svm::REGISTER_CHAIN_SQL` once after migrations (idempotent — `chains` is a `ReplacingMergeTree` keyed on `chain`); reject EVM-only flags with a message that names the Solana one; add `--sol-rpc` (default the public endpoint, documented as best-effort) | S | — | everything |
| **S8** | **Metrics.** See the note below — the important part is that lag is reported in **seconds as well as slots** | S | — | everything |
| **S9** | **`indexer verify` for Solana.** A switch on `chains.family`, then the six checks of 11.4.5 | M | S4 | S5, S6 |
| **S10** | **Backfill driver.** A **backward** sweep from the head to slot 391,000,000 in cursor-following windows, resumable from `checkpoints`, Arrow, budget-aware through S2, stopping when `next_slot` stops advancing (the documented below-history condition) | M | S1, S2, S4 | — |
| **S11** | **Lease: verify, do not rewrite.** `src/pipeline/lease.rs` keys `indexer_instances` on `chain` and has no EVM in it; a Solana process with `chain = 1399811149` gets the one-process-per-chain guarantee for free. The job is a test that proves it, not a change | S | — | everything |

**Total ~12.75 days.** Order, with waves that run in parallel:

```
wave 0  S0  S1  S7  S11        (day 1 - S1 unblocks every measurement)
wave 1  S2  S3  S4  S8
wave 2  S5  S6  S9
wave 3  S10
then    the phase-1 acceptance test from section 7: 24 h of live slots
        streamed and resumed, plus a forced purge_range on a synthetic
        mismatch, with candles correct afterwards
```

**The one architectural decision, in S3.** Three ways to carry Solana rows
through the sync loop:

- *(a)* an associated type on `BlockSource` — the "right" abstraction, but it
  makes `Writer`, `ClickhouseSink`, `ModuleRows` and `DecodeState` generic and
  costs three or four times the diff for no change in behaviour;
- *(b)* **an enum `ChainRows { Evm(..), Svm(..) }` on `SourceResponse`,
  matched once in the writer** — small, explicit, and leaves exactly one sync
  loop, one lease and one reorg guard;
- *(c)* a parallel `run_svm_with` — duplicates the crash-tested sync loop,
  which is the single thing in this codebase you least want two of.

**Take (b).** Section 6.1 already argues that the shared layer is shared and the
trait boundaries should not be guessed at before the second caller exists; (b)
is the smallest change that makes the second caller exist, and (a) remains a
mechanical refactor afterwards if a third family ever appears.

**Metrics (S8), specifically.** Lag must be published **in seconds, not only in
heights**: a Solana slot is 0.27 s and an Ethereum block is 12 s, so one
`indexer_block_lag` panel comparing them is meaningless and would be read wrong
on the first bad day.

| Metric | Source |
|---|---|
| `indexer_head_block{chain}` | Solana: `GET /height`, which is **free and unmetered** — or `/height/sse` |
| `indexer_committed_block{chain}` | the last committed `sol_slots` |
| `indexer_block_lag{chain}` | head − committed, in slots on Solana |
| **`indexer_seconds_lag{chain}`** | `now() − block_time` of the last committed slot. **This is the number the owner should look at**, and it is the only one comparable across families |
| `hypersync_ratelimit_remaining{endpoint}` / `_cost{endpoint}` | straight from the response headers |
| `hypersync_queries_total{endpoint,purpose}` | `purpose` in `head` / `backfill` / `headers` / `verify` — so 30 queries a minute can be seen being spent |
| `solana_parent_mismatch_total{chain}` | should be **0 forever**; page on it (11.4.2) |
| `solana_skipped_slots_total{chain}` | `to_block − from_block − blocks_present`, from the checkpoint. Measured 0 in 10,000 slots today; worth watching because every rows/day figure assumes it |

**What needs no change at all**, and it is worth writing down so nobody
"abstracts" it: `db/` (client, `format.rs`, `tombstone_sql`, ranges,
`DerivedTable`), `reorg/purge.rs` (slots substitute for block numbers with no
code change), the tombstone/epoch/validity machinery, `epoch_floor_v`, the
migrator, and `lease.rs`. The chain-neutral analytics tables of section 13 are
already the right shape because decision A was taken.

### 11.6 Cost of ownership at steady state

Row rates are the measured ones (11.2); bytes-per-row use section 5.3's
assumption of **80 compressed bytes for a swap row** (range 60-120; Solana
pubkeys are random and ZSTD buys little on them).

| Stream | Rows/day | Compressed/day | Per year |
|---|---|---|---|
| `sol_dex_swaps` (52M swaps — section 5.2) | 52M | 4.2 GB | 1.5 TB |
| `sol_transactions` (210 matched tx/slot measured) | 68M | 3.4 GB | 1.2 TB |
| `sol_slots`, `sol_tokens` | 0.33M | <0.01 GB | negligible |
| candles 1m/1h/1d per pool | ~7M | 0.4 GB | 0.16 TB |
| **subtotal, base tables + candles** | | **~8.0 GB** | **~2.9 TB** |
| \+ side tables **slim** (`_by_pool` / `_by_trader` / `_by_token` as key + position, ~25 B) | 156M | +3.9 GB | +1.4 TB |
| \+ side tables **as full row copies** (what the EVM design does today) | 156M | **+12.6 GB** | **+4.6 TB** |
| **Solana total, slim side tables** | | **~12 GB/day** | **~4.3 TB/yr** |
| **Solana total, wide side tables** | | **~21 GB/day** | **~7.5 TB/yr** |

**Against the EVM chains.** The only EVM swap rate this project has measured is
Base: **2.0M swaps/day** (section 5.3, `eth_getLogs` over 6 x 50 blocks).
Solana at 52M/day is **26x Base**. A realistic EVM portfolio lands somewhere
between 5M and 20M swaps/day across every chain, so **Solana alone is 2.5x to
10x all the EVM chains put together**, and it is the only chain where the
schema decisions below change the hardware bill.

**A single-node ClickHouse over 12 months.**

| | Slim side tables | Wide side tables |
|---|---|---|
| Solana live data after 12 months | 4.3 TB | 7.5 TB |
| \+ EVM (5-20M swaps/day) | 0.4-1.7 TB | 0.7-3.0 TB |
| **live compressed total** | **4.7-6.0 TB** | **8.2-10.5 TB** |
| largest monthly partition (Solana base) | ~360 GB | ~630 GB |
| merge headroom (a merge needs the partition's size free again) | ~700 GB | ~1.3 TB |
| **recommended disk for year 1** | **8 TB NVMe** | **16 TB NVMe** |

- **RAM: 64 GB minimum, 128 GB comfortable.** `do_not_merge_across_partitions_
  select_final = 1` keeps a `FINAL` read inside one month, so the working set is
  per-partition rather than per-table; the mark cache and the candle aggregate
  states are what want the rest.
- **CPU: sized by the backfill, not the head.** Steady state is 9,400 source
  rows/s to decode, which is nothing; a Starter-tier backfill is **146,000
  rows/s**, which is the real number. 16 cores minimum.
- **Write rate is not the problem.** 52M swaps/day is 600 rows/s into
  `dex_swaps`. What matters is the *flush cadence*: every flush is a synchronous
  ClickHouse part and 50+ chains share the server, so `tip_interval` should be
  seconds, not milliseconds.
- **Network:** 15 Mbit/s at the head, ~220 Mbit/s for the fortnight of the
  backfill (11.3).

#### The three levers that actually matter

**1. Keep raw swaps for N months; keep candles forever.** This is the biggest
lever and it is a product decision, not an engineering one. A
`TTL timestamp + INTERVAL 6 MONTH` on `sol_dex_swaps` / `dex_swaps` /
`sol_transactions`, with the 1m/1h/1d candle tables and `launchpad_*` keeping
everything, turns a linear 4.3 TB/year into a **steady state around 2.2 TB**.
Nothing on a chart is lost; what goes away is "show me every individual fill of
this pool last March". Two conditions before committing: set
`ttl_only_drop_parts = 1` so a TTL drops whole parts instead of rewriting them,
and confirm that a part-drop cannot race the tombstone/epoch logic the way
design.md section 2 forbids `ALTER DELETE` from doing. **A TTL is a merge-time
part drop, not a `DELETE`, so it is compatible in principle — verify it before
depending on it.**

**2. Side tables must be slim or projections, never full row copies.** Three
wide copies of `dex_swaps` cost **12.6 GB/day — three times the base table** and
more than everything else in the budget put together. Store the lookup key plus
`(chain, block_number, tx_index, ordinal)` and join back, or use a ClickHouse
`PROJECTION`. At EVM row rates the wide form was affordable and nobody noticed;
at Solana rates it is the single largest line.

**3. Only write `sol_transactions` for transactions that contain a venue
instruction.** The SPL/Token-2022 transfer selections match transfers
chain-wide, so the matched-transaction set is the measured **210/slot** where
the venue programs alone are ~168/slot (section 5.2) — a 25% inflation whose
extra rows contain no venue instruction and therefore no analytic value. The
transfer rows are still needed **in flight** for the movement layer; they simply
should not land on disk. It saves ~0.7 GB/day, and more importantly it keeps
`count()` over `sol_transactions` meaning *"transactions that touched a venue"*
rather than *"transactions that moved a token"* — which is exactly the
partial-table-presented-as-complete trap section 5.5 and `src/svm/README.md`
both warn about.

### Appendix E. Every request made for section 11

14 HTTP requests, 2026-09-19 07:05:39 - 07:09:37 UTC, plus 3 free `/height` +
public-RPC lag samples at 07:10:03 - 07:10:13 UTC (tabulated in 11.2). The
`ENVIO_API_TOKEN` was read from the git-ignored `.env` into a shell variable
inside each command; it is not printed, stored or reproduced anywhere in this
document, in the scratch files, or in the repository.

| # | Time (UTC) | Endpoint | Request | `cost` | `limit` | `remaining` | `reset` | Result |
|---|---|---|---|---|---|---|---|---|
| p01 | 07:05:39 | `solana.hypersync.xyz` | GET /height, **no token** | **(absent)** | **(absent)** | **(absent)** | **(absent)** | `448334753` |
| p02 | 07:05:39 | `solana.hypersync.xyz` | GET /height, with token | **(absent)** | **(absent)** | **(absent)** | **(absent)** | `448334747` |
| p03 | 07:05:40 | `1.hypersync.xyz` | GET /height, with token | **(absent)** | **(absent)** | **(absent)** | **(absent)** | `{"height":26009954}` |
| p04 | 07:05:59 | `solana.hypersync.xyz` | POST /query - 1 slot, `block` field selection only, no `max_num_*` | `1000` | `30000, 30000;w=60` | `29000` | `1` | 1 slot, 1.27 MB |
| p05 | 07:07:21 | `solana.hypersync.xyz` | POST /query - **the production query as merged**: 20 venues + SPL/T22/System transfer union + `account_activity:[{}]`, 10,000-slot range, only `max_num_instructions: 200000` | `1000` | `30000, 30000;w=60` | `29000` | `39` | **1 slot**, 1.25 MB, 1.8 s |
| p06 | 07:07:22 | `solana.hypersync.xyz` | POST /query - featherweight: 1 slot, a filter that matches nothing, one field | `1000` | `30000, 30000;w=60` | `28000` | `38` | 0 rows, 378 B |
| p07 | 07:07:23 | `solana.hypersync.xyz` | POST /query - 100,000-slot header sweep, `max_num_blocks: 100000` | `1000` | `30000, 30000;w=60` | `27000` | `37` | 1 slot, 2.81 MB |
| p08 | 07:08:08 | `solana.hypersync.xyz` | POST /query - **the production query with every `max_num_*` raised to 10^6**, 10,000-slot range | `1000` | `30000, 30000;w=60` | `29000` | `52` | **40 slots**, 46.18 MB, 7.8 s; 8,848 tx / 44,212 instr / 57,791 activity |
| p11 | 07:08:26 | `solana.hypersync.xyz` | POST /query - headers only (instruction + activity selections that match nothing), 10,000-slot range | `1000` | `30000, 30000;w=60` | `28000` | `44` | **10,000 slots** of headers, 3.59 MB, 12.1 s; 0 skipped slots, 0 height breaks, 0 parent breaks |
| p09 | 07:08:28 | `solana.hypersync.xyz` | POST /query - header sweep with `max_num_account_activity: 0` (bad probe: `next_slot` did not advance) | `1000` | `30000, 30000;w=60` | `27000` | `32` | `next_slot` did not advance; 378 B |
| p12 | 07:09:21 | `solana.hypersync.xyz` | POST **/query/arrow** - byte-for-byte the same query as p08 | `1000` | `30000, 30000;w=60` | `29000` | `39` | 40 slots, **19.94 MB Arrow** (43% of p08's JSON) |
| p13 | 07:09:27 | `1.hypersync.xyz` | POST /query - EVM, 1 block, 2 fields | `1000` | `30000, 30000;w=60` | `29000` | `33` | 1 block, 214 B |
| p14 | 07:09:27 | `1.hypersync.xyz` | POST /query - EVM, 10,000 blocks, all logs + all transactions, caps raised | `1000` | `30000, 30000;w=60` | `28000` | `33` | 66 blocks, 40.06 MB, 90,742 rows |
| p15 | 07:09:32 | `solana.hypersync.xyz` | POST /query - the p08 query at a different slot range (448,320,000) | `1000` | `30000, 30000;w=60` | `28000` | `28` | **30 slots**, 26.85 MB, 5.7 s; 5,851 tx / 26,588 instr / 32,263 activity |

Reading the table:

- **`cost` is 1000 on all 11 metered requests**, from a 378-byte response to a
  46 MB one, on both the Solana and the EVM endpoint, on `/query` and on
  `/query/arrow`. It is flat.
- **`/height` carries no rate-limit headers at all**, with or without a token.
  It is free.
- **`remaining` decrements by exactly 1000** within a window (p04→p07:
  29000 → 28000 → 27000; p12→p15 on Solana: 29000 → 28000; p13→p14 on EVM:
  29000 → 28000) and returns to 30000 when `reset` elapses.
- **`w=60`** in `x-ratelimit-limit` is the window length in seconds, confirmed
  by `reset` counting 39 → 38 → 37 across three back-to-back requests.
- **The two endpoints have independent counters.** p12 left Solana at 29000;
  one second later p13 on the EVM endpoint reported its own 29000 with a
  different `reset` offset (33 vs 39). Two data points, so treat "per chain
  endpoint" as likely rather than proven.
- **p05 against p08 is the bug**: the same query, the same slot range, the only
  difference being which `max_num_*` caps are set. 1 slot versus 40.
- **p08 against p15** shows that with all caps raised the stop is the server's
  own execution budget: 40 slots / 46 MB / 7.8 s in one place, 30 slots /
  27 MB / 5.7 s in another.

### Appendix F. Sources retrieved for section 11

| What | URL | Retrieved (UTC) |
|---|---|---|
| Rate-limit header contract, `concurrency`, the EVM/Solana 429 asymmetry | `docs.envio.dev/docs/HyperSync/stream-config-tuning` | 2026-09-19 |
| API tokens, the "Credits" notion, the token requirement | `docs.envio.dev/docs/HyperSync/api-tokens` | 2026-09-19 |
| **Paid tiers and prices** (Free / Starter $70 / Pro $480 / Custom) | `envio.dev/pricing/hypersync` | 2026-09-19 |
| Solana history depth (slot 403,000,000 documented), `/height` open, Beta status | `docs.envio.dev/docs/HyperSync/solana` | 2026-09-19 |
| `max_num_*` caps, pagination to head, `rollback_guard` rules | `docs.envio.dev/docs/HyperSync/solana-query` | 2026-09-19 |
| `StreamConfig` defaults (`response_bytes_ceiling` 500,000 etc.) | `docs.envio.dev/docs/HyperSync/solana-client` | 2026-09-19 |
| `/height` and `/height/sse` need no token | `docs.envio.dev/docs/HyperSync/solana-curl-examples` | 2026-09-19 |
| EVM "5-second query execution limit" | `docs.envio.dev/docs/HyperSync/hypersync-query`, `/hypersync-usage` | 2026-09-19 |
| Old Faithful: what it is, CAR format, free archive, Amsterdam hosting, "RFC stage" | `github.com/rpcpool/yellowstone-faithful`, `docs.old-faithful.net` | 2026-09-19 |
| **CAR size per epoch** (586 GB / 604 / 715 / 1215 / 1059 / 917 GB for epochs 966-1036) | `raw.githubusercontent.com/rpcpool/yellowstone-faithful/gha-report/docs/CAR-REPORT.md` | 2026-09-19 |
| Jetstreamer: 2.7M TPS on 64 cores / 30 Gbps+, epoch and slot ranges, ClickHouse sink, no wire-level filter, Clang 16 | `github.com/anza-xyz/jetstreamer`, `docs.rs/jetstreamer/0.7.0` | 2026-09-19 |
| Triton's *hosted* archive RPC pricing ($10 per million queries) - not the bulk path | `docs.triton.one/chains/solana/old-faithful-historical-archive-1` | 2026-09-19 |
| Live probes (appendix E) | `solana.hypersync.xyz/query`, `/query/arrow`, `/height`; `1.hypersync.xyz/query`, `/height` | 2026-09-19 07:05-07:10 |
| Head-lag comparison | `api.mainnet-beta.solana.com` `getSlot` at `processed` and `finalized` | 2026-09-19 07:10 |

Not found, and stated as not found rather than guessed: Envio's free-tier rpm
(published only as "fair-use"), the rate-limit **window length** (nowhere in the
docs - the `w=60` in the header is the only source), the `cost` formula, the
`max_num_*` default values, Envio's Solana **commitment level**, the total size
of the Old Faithful archive, and any published Jetstreamer epochs/hour or MB/s
figure.

### Appendix G. What section 11 supersedes

| Where | What it says | Replace with |
|---|---|---|
| §4.6, §8 table, §9 bullet 3, appendix C item 2 | "every probe `x-ratelimit-cost: 0`", "no separate plan appears to be needed" | cost is a flat **1000**, budget **30,000 per 60 s** = **30 queries/min**, free (§11.1) |
| §4.6 | "107 slots, 33,886 instruction rows, 5.99 MB in 2.7 s" | true for an instruction-only query; **the production query returns 30-40 slots with all caps raised and 1 slot with only `max_num_instructions` set** (§11.1.1) |
| §5.4 | "~20 GB/day of JSON at head", "~3.5 TB over the wire" for the backfill | **375 GB/day JSON / 162 GB/day Arrow** at head; **66 TB JSON / 28.6 TB Arrow** for the backfill (§11.2, §11.3) |
| §7.1 | pricing is "blocking on Envio" | answered by measurement; the remaining blocker is the **commitment level** (§11.4.1) |
| §8 | Old Faithful "total size not published (100s of GB per epoch)" | **586-1,215 GB per epoch measured** in their own report; our 8.5 months = **133 epochs ≈ 113 TB** (§11.3.1) |
| §6.3 "Commitment vs our reorg detection" | "Set `--confirmations 0` and rely on the guard + parent-hash chain, same as EVM" | same, **plus drop the fork-point search** and add the `block_height` witness (§11.4) |
