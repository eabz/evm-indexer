# Solana: DEX + launchpad data for this indexer - research

STATUS: WORK IN PROGRESS (skeleton + raw notes; sections are filled as evidence lands).
Analyst: solana-research. All retrievals 2026-09-19 01:20-03:00 UTC unless stated.

## RAW NOTES (to be folded into the sections below)

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
TODO

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
| `chain UInt64` | EIP-155 id | unchanged; Solana = one reserved constant + a `chains` registry table (`chain`, `name`, `family` = `evm` \| `svm`) | views need the family to format ids: `concat('0x', lower(hex(substring(x, 13))))` vs `base58Encode(x)` (ClickHouse built-in) |

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
TODO
## 2. How swaps appear on Solana, per venue
TODO
## 3. Is a DEX-agnostic decoder possible?
TODO
## 4. What Solana HyperSync actually serves
TODO
## 5. Data volume reality check
TODO
## 6. Architecture for this project
TODO
## 7. Phasing and effort
TODO
## 8. Alternatives
TODO
## 9. Recommendation
TODO
## Appendix
TODO
