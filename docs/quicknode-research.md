# Moving from HyperSync (Envio) to QuickNode — research

Analyst: `quicknode-research`, 2026-09-19. Read-only on code: this file is the
only thing written.

**How to read every number in here.** Each figure carries a tag:

| Tag | Means |
|---|---|
| **[P]** | Published by the vendor. The URL and the date I read it are given. |
| **[M]** | Measured by me today, 2026-09-19, against a live endpoint. The probe is described. |
| **[C]** | Computed from published or measured numbers. The arithmetic is shown. |
| **[E]** | Estimate. My judgement, with the reasoning stated. Treat as the weakest. |

Prices move. Everything marked [P] was on the vendor's page on 2026-09-19 and
should be re-checked before money is committed.

---

# PART ONE — THE ONE-PAGE ANSWER

**Can we move from HyperSync to QuickNode? Technically yes, on every chain we
care about. Financially it is a bad trade for the EVM chains, and a good trade
for one specific Solana problem.**

### What it would cost in money

| Job | Envio HyperSync today | QuickNode |
|---|---|---|
| Load the full history of Ethereum, Base, Arbitrum, BSC and Polygon | **$0** (free tier, ~5 months) or **$480 for one month** of the Pro tier | **~$16,000** of API credits, one off **[C]** |
| Follow the head of 50 chains, every month | **$0 to $480/month** | **~$1,700/month** **[C]** |
| Follow the Solana head, every month | **$0** | **~$150/month** on top of a plan **[C]** |
| Load Solana history back to 2026-01-03 | **$70**, once | **~$860**, once **[C]** |
| Load Solana history *before* 2026-01-03 | **Impossible at any price** | **~$6,700**, once **[C]** |

First-year total, same data: **Envio roughly $70–$6,000. QuickNode roughly
$36,000–$43,000.** [C] The gap is not a negotiating position; it is the
difference between a product that is priced per *query* and one priced per
*block*, and we ask for every block of every chain.

### What it would cost in time

**8 to 13 engineer-days** for a JSON-RPC source on the EVM side, and **8 to 12
more** for Solana. [E] The codebase is unusually ready for this: there is
already a one-method seam (`BlockSource`) that the sync loop talks through, and
a fake implementation of it in the tests. Nothing in the schema, the reorg core,
the writer, the decoder modules or fleet mode has to change.

### What we would lose

1. **Money**, as above — by a factor of roughly 30 on the backfill.
2. **Backfill speed on the cheap chains.** HyperSync hands back up to 5,000
   Arbitrum blocks in one request [M]; RPC needs two requests per block, always.
3. **A free head signal.** Envio's `/height` costs nothing and is not even
   metered [M]. On QuickNode every head poll is 20 credits; polling 50 chains
   once a second would cost **$1,296/month on its own** [C].
4. **Chains.** HyperSync serves ~25 mainnets QuickNode does not, including
   Moonbeam, opBNB, Polygon zkEVM, Aurora, Etherlink, Zircuit and Rootstock [P].

### What we would gain

1. **Solana before 2026-01-03.** QuickNode's Solana mainnet is a full archive
   back to genesis [P], and the owner's own endpoint served a block from October
   2021 today [measured by the coordinator]. Envio's Solana history starts in
   January 2026 and will not start earlier. This is the one thing money cannot
   buy from Envio.
2. **Independence.** A JSON-RPC source is not a QuickNode source. The same code
   works against Alchemy, dRPC, Ankr, Infura or our own reth node. Today a
   HyperSync outage stops every chain at once.
3. **Chains HyperSync lacks** — Story, and a long tail of non-EVM chains we do
   not index.

### Recommendation

**Do not replace HyperSync. Add a provider-agnostic JSON-RPC source next to it,
and buy QuickNode only for deep Solana history if the owner wants it.**

Concretely:

- **Build `--source rpc`** (8–13 days). It is the insurance policy, the way onto
  chains HyperSync does not serve, and the thing that makes "provider" a
  configuration line instead of an architecture.
- **Keep HyperSync as the default** for EVM backfill and head following. It is
  free or nearly free, and for our unfiltered query it is still 1.2–1.9x fewer
  bytes on the wire [M].
- **Do not use QuickNode Streams.** It is the most lock-in for the least
  benefit: it costs exactly the same as pulling over RPC [C], has no ClickHouse
  destination [P], cannot back-fill Solana at all [P], and its push model fights
  our lease/epoch/commit-marker design.
- **For Solana, decide separately.** Envio's $70 gets 8.5 months. QuickNode's
  ~$6,700 gets everything back to 2021 — but also ~260 TB to download [E],
  because QuickNode cannot filter Solana blocks server-side.

The rest of this document is the evidence.

---

# PART TWO — THE EVIDENCE

## 1. Where the seam is, and what today's code assumes

Read: `src/source/evm.rs`, `src/source/solana.rs`, `src/pipeline/mod.rs`,
`src/pipeline/solana.rs`, `src/core/decode.rs`, `src/svm/mod.rs`,
`src/pipeline/sync_tests.rs`, `docs/design.md` §2/§8/§14/§15,
`docs/solana-research.md` §11.

```rust
// src/pipeline/mod.rs
pub trait BlockSource {
    async fn head(&self) -> Result<u64>;
    async fn stream(&self, range: BlockRange)
        -> Result<Receiver<Result<SourceResponse>>>;
}
```

That is the whole EVM contract, plus `CanonicalChain::headers(from, to)` in
`src/reorg/mod.rs` for the fork-point search. `pipeline::run_with<S: BlockSource>`
already takes the source as a type parameter, and `sync_tests.rs` already has a
complete fake (`MockSource`) driving the real sync loop. Solana's contract is
smaller still: `SlotSource { head, fetch(from, to) }`.

**One thing binds us to HyperSync more than it should.** `SourceResponse.data`
is `ResponseRows { blocks: Vec<Vec<hypersync_client::simple_types::Block>>, … }`
and `core::decode` has four `from_hypersync` constructors. The row models are
ours; the *input* types are the vendor's. **That is cheaper to work around than
it looks:** in `hypersync-format 0.7.1`, `Quantity` implements `From<Vec<u8>>`,
`From<&[u8]>`, `From<u64>` and a serde `Deserialize` that parses `"0x…"` hex;
`Data` the same; `Hash`/`Address` are `FixedSizeData<N>` from `[u8; N]`. **The
HyperSync input types deserialize from exactly the encoding JSON-RPC speaks**,
and `src/source/evm.rs`'s own tests already build them by hand. An RPC source can
fill `simple_types::{Block, Transaction, Log}` directly and `core::decode` never
notices. Neutral types are therefore optional, not a prerequisite — both options
are costed in §8.2.

The Solana side is already clean: `SvmSlotBatch` / `SvmTransaction` /
`SvmInstruction` / `SvmAccountActivity` / `SvmLog` are **our own types with no
HyperSync in them**. Only `src/source/solana.rs` (Arrow → those structs) is
vendor-specific.

---

## 2. What QuickNode actually sells

### 2.1 Core JSON-RPC

| Fact | Value | Source |
|---|---|---|
| Credit cost, `eth_getBlockByNumber` / `ByHash` / `getBlockReceipts` / `getTransactionReceipt` / `getLogs` / `blockNumber` | **20 credits each**, identically, on Ethereum, Base, Arbitrum, BSC and Polygon | [P] quicknode.com/api-credits/{eth,base,arb,bsc,matic} |
| Cost depends on response size? | **No.** A 400-transaction `eth_getBlockReceipts` costs the same 20 as `eth_blockNumber` | [P] same |
| Chain base rates | 10 (Bitcoin family, Flow, Stellar) · **20** (Ethereum, Base, Arbitrum, Optimism, Polygon, BNB, Avalanche, Linea, Sonic, Unichain, Berachain, HyperEVM, Ink, Blast, Mantle, …) · **30** (Solana, Monad, zkSync, Scroll, Tron, Gnosis, Abstract, MegaETH) · 40 (Celo, Fantom, XRP, Sui, TON) · 50 (NEAR) | [P] quicknode.com/api-credits |
| Trace/debug multiplier | 2x chain base (40 credits on our chains). **We removed traces** (`docs/design.md` §9), so this is pure savings for us: the trace-free design is what keeps us on the 20-credit tier | [P] |
| Not billed | HTTP 4xx/5xx, API errors, and **`null` results** | [P] support.quicknode.com "How are my requests billed" |
| Rate limit | **15 / 50 / 125 / 250 / 500 requests per second** on Free / Build / Accelerate / Scale / Business | [P] quicknode.com/pricing |
| **Batching** | **A batch of 50 calls counts as 50 against RPS and 50 against credits.** QuickNode actively discourages batching and recommends concurrent individual requests instead | [P] support.quicknode.com "429 Errors Explained"; quicknode.com/guides/…/guide-to-efficient-rpc-requests |
| `eth_getLogs` range | **5 blocks free tier, 10,000 blocks paid.** Over → HTTP 413 / `-32602` | [P] quicknode.com/docs/ethereum/eth_getLogs |
| `eth_getBlockReceipts` limits | **None published** | [P] (absence) |
| Archive | **Included on every plan, no surcharge**, no pruning on Ethereum/Base/Arbitrum/BSC/Polygon mainnets *and* their testnets | [P] quicknode.com/pricing; docs/platform/supported-chains-node-types |
| Uptime SLA | 99.99% is marketed; **contractual only on Enterprise**. The "Response time (SLA)" row on the pricing page is a *support-ticket* SLA, not uptime | [P] + [E] |

Plans, verbatim from the pricing page on 2026-09-19 [P]:

| Plan | $/month | Credits included | RPS | Extra credits |
|---|---|---|---|---|
| Free trial | 0 | 10 M | 15 | — |
| Build | 49 | 80 M | 50 | $0.62 / 1 M |
| Accelerate | 249 | 450 M | 125 | $0.56 / 1 M |
| Scale | 499 | 950 M | 250 | $0.53 / 1 M |
| Business | 999 | 2 B | 500 | $0.50 / 1 M |
| Business+ T1/T2/T3 | 1,499 / 1,999 / 2,999 | 3.2 B / 4.5 B / 7 B | 500 | $0.50 / 1 M |

There is also **Flat Rate RPS** [P] (`docs/platform/billing/flat-rate-rps`):
unmetered, no credits at all, for Ethereum/Base/Optimism/Arbitrum/BSC/Polygon
and Solana — **75 RPS / 2 concurrent / $799**, **150 / 4 / $1,499**, **250 / 6 /
$1,949** per month (Solana x1.5). This looks like the cheap way to backfill until
you read the concurrency column: *both* limits apply at once, and 6 concurrent
in-flight requests at the ~180 ms per call the coordinator measured on the
owner's endpoint is **33 requests/second, not 250** [C]. Unless "concurrent
connections" means TCP sockets rather than in-flight requests — **unverified,
open question 4 in §11** — Flat Rate is a trap for bulk work.

### 2.2 Streams

Streams is QuickNode's push pipeline. All [P],
`quicknode.com/docs/streams/*`, read 2026-09-19.

- **Datasets (EVM):** `block`, `transactions`, `logs`, `receipts`,
  `block_with_receipts`, four trace variants. The payloads are *the raw RPC
  results, unmodified* — `block_with_receipts` is literally
  `{block: eth_getBlockByNumber, receipts: eth_getBlockReceipts}`. Streams gives
  us **exactly the data our own RPC source would fetch**, no more.
  **Solana: two datasets only** — `block` and `programs_with_logs`.
- **Destinations: five — webhook, S3, Azure Blob, PostgreSQL, Kafka. No
  ClickHouse**, no Snowflake, no BigQuery (QuickNode's own `llms.txt` still
  advertises the last two; those pages 404). Functions has been removed.
- **Pricing is not published as prose**, only as a calculator. Its embedded data
  blob (`docs/assets/js/2d5a9170.2e1bb990.js`) gives the rule: **credits/block =
  chain base x dataset factor**; factor 1 for `block`/`logs`/`receipts`/
  `transactions`, **2 for `block_with_receipts`**, 2 per trace dataset, 4 for the
  composites. On our five chains that is **40 credits per block — identical to
  fetching the same two things ourselves over RPC** (2 x 20) [C]. QuickNode's own
  example: "24,227,108 blocks x 20 = 484,542,160 credits".
- **Filters do not reduce credits**: "calculated based on the number of blocks
  processed, regardless of filtering". A filter saves bandwidth, never money.
- **Backfill** works from genesis on most EVM networks but is **not supported on
  Solana at all** ("Solana networks, for example, do not currently support
  backfill"), nor on 15 other networks incl. Ink, Sei, MegaETH, Flow.
  **Throughput is not published** — the only claim anywhere is "7x Faster
  Backfills". Delivery is strictly serial ("will not proceed to the next block
  or batch until receiving confirmation"), so one stream is one in-flight batch
  and the rate is `batch_size / ack_latency` [E].
- **Ordering** is strictly sequential and guaranteed. **Delivery** is claimed
  "exactly-once" in the FAQ but described as re-delivering on reorg and on
  restart elsewhere — treat as at-least-once keyed on block number [E]. That
  suits us: our writer is insert-only, so a duplicate block is a no-op.
- **Tip latency is not published**; QuickNode's own table rates Streams
  "Standard" vs gRPC "Sub-second" and says Streams is "not designed for
  sub-100ms latency use cases".
- **Limits:** 1 active stream on Free, 10 on Build, 100 above; **Solana capped at
  5 streams** below Enterprise. One filter per stream, in sandboxed Go or JS with
  no imports and no network.
- **The console API key is not the endpoint key.** The coordinator confirmed
  today that the owner's endpoint key returns 403 against the Streams REST API;
  a pilot needs a new key created in the dashboard.

### 2.3 Solana

| | Envio HyperSync Solana | QuickNode Solana |
|---|---|---|
| History | starts ~slot 391M / 403M, i.e. **2026-01-03** [P, and `docs/solana-research.md` §11.3] | **genesis, archive, no pruning** [P]; the coordinator served slot 100,000,000 (Oct 2021) from the owner's endpoint today |
| Server-side filtering | **yes** — 26 programs, instruction/log/activity selections | **no** for `getBlock`; Streams `programs_with_logs` filters but **cannot backfill** |
| Cost | flat 1,000 budget units per query, 30 queries/min free [M, `docs/solana-research.md` §11.1] | **30 credits per `getBlock`**, any size [P] |
| Live stream | `/height/sse`, free and unmetered [M] | Yellowstone gRPC — **included from Scale ($499) up**, $499/mo add-on below [P] |
| gRPC billing | n/a | **10 credits per 0.1 MB delivered, rising to 15 on 2026-10-01** [P] — byte-metered, unlike JSON-RPC |
| gRPC historical replay | n/a | **3,000 slots, ~20 minutes** [P]. Useful for a disconnect, useless for backfill. (Helius offers 24 h for comparison [P].) |
| Fields in `getBlock` meta | — | `innerInstructions`, `logMessages`, `pre/postTokenBalances`, `pre/postBalances`, `rewards`, `err`, `fee` [P]; `computeUnitsConsumed` is not in QuickNode's documented field list but the coordinator observed it live |
| Gotcha | — | `maxSupportedTransactionVersion` must be **>= 1** on recent blocks; with 0 the call fails `-32015` [measured by the coordinator today] |

### 2.4 Chain coverage

QuickNode: **"80 chains, 139 networks"** [P] (`quicknode.com/chains`).
HyperSync: ~85 rows including testnets [P]
(`docs.envio.dev/docs/HyperSync/hypersync-supported-networks`).

**On HyperSync but not QuickNode** (mainnets): **Moonbeam, opBNB, Polygon
zkEVM**, Aurora, Boba, Chiliz, Citrea, Etherlink, Harmony, Injective EVM (1776),
Lukso, Manta, Merlin, Metall2, Plume, Rootstock, Shimmer EVM, Sophon, Stable,
Superseed, Swell, XDC, Zeta, Zircuit, Ab — **~25 chains**, a real cost if "50+
chains in one database" is the target.

**On QuickNode but not HyperSync** (EVM): **Story / DATA Network (1514)**, 0G,
B3, Fluent, Gravity, Hemi, Immutable zkEVM, peaq, Vana, X Layer, XRPL EVM, Japan
Open Chain — plus every non-EVM chain we do not index.

**The newer chains the owner asked about: both serve all of them** [P], with two
caveats on the QuickNode side. **Monad — "over 40,000 recent blocks available",
not an archive**, which is a blocker: we index from block 0 and Monad is at
106.2 M blocks [M]. **HyperEVM** — the `/evm` endpoint is recent-only; the
archive lives on a separate `/nanoreth` endpoint. Sonic, Berachain, Unichain,
Ink, Abstract, Katana and Plasma are full archives on both. Story is QuickNode
only. I confirmed HyperSync's free `/height` answers for chains 1, 8453, 42161,
56, 137, 10, 43114, 59144, 130, 146, 80094, 999 and 143 today [M].

---

## 3. The money, with the arithmetic

### 3.1 What one block costs us, on each side

Both numbers are the *same data*: every block, every transaction with its
receipt, every log. We ask HyperSync for no filter at all
(`TransactionFilter::all()`, `LogFilter::all()`, `include_all_blocks()`), so
HyperSync's headline advantage — server-side filtering — **buys us almost
nothing on EVM**. What it buys is packaging and price.

**Measured today [M].** One HyperSync query per chain with the exact field
selection from `src/source/evm.rs`, and one `eth_getBlockByNumber(full)` +
`eth_getBlockReceipts` pair per chain against a public node, all gzipped:

| Chain | Height today | HyperSync blocks per query | HyperSync MB/block (gzip) | RPC MB/block (gzip) | RPC / HyperSync |
|---|---|---|---|---|---|
| Ethereum | 26,012,618 | 84 | 0.162 | 0.227 | 1.40x |
| Base | 51,522,156 | 16 | 0.077 | 0.142 | 1.85x |
| Arbitrum | 506,824,908 | **5,018** | 0.0024 | 0.0040 | 1.69x |
| BSC | 122,825,670 | 20 | 0.055 | 0.097 | 1.76x |
| Polygon | 94,085,748 | 291 | 0.097 | 0.112 | 1.15x |

(Blocks-per-query is measured on *today's* blocks, the heaviest the chains have
ever had; historical blocks are smaller, so HyperSync will do better than this
over a real backfill. Both sides are gzipped; HyperSync's Arrow encoding would
cut its side further — measured at 43% of JSON on Solana [`docs/solana-research.md`
§11.1] — but the Arrow endpoint returned nothing for my EVM probe, so I have not
verified it here.)

**Reading:** over the wire, plain JSON-RPC moves **1.2 to 1.9x** the bytes
HyperSync does for identical content [M]. That is a much smaller penalty than
the marketing on either side suggests, and it is the honest answer to "is RPC
too slow to be serious". Bytes are not the problem. Requests and credits are.

### 3.2 (a) Full-history backfill of Ethereum, Base, Arbitrum, BSC, Polygon

Heights measured today [M]. Total = **801,271,100 blocks** [C].

**QuickNode over RPC.** Two calls per block — `eth_getBlockByNumber(n, true)`
and `eth_getBlockReceipts(n)`. We need no third call: receipts carry the logs,
so `eth_getLogs` is unnecessary, and **we removed traces** (`docs/design.md` §9),
which keeps us off the 2x multiplier entirely.

```
801,271,100 blocks x 2 calls          = 1,602,542,200 calls          [C]
1,602,542,200 calls x 20 credits      = 32,050,844,000 credits       [C]
32,050.8 M credits x $0.50 / 1M       = $16,025    (Business rate)   [C]
                   x $0.53 / 1M       = $16,987    (Scale rate)      [C]
                   x $0.62 / 1M       = $19,872    (Build rate)      [C]
```

**QuickNode over Streams.** `block_with_receipts` = 2x chain base = **40 credits
per block** on all five chains [P].

```
801,271,100 blocks x 40 credits       = 32,050,844,000 credits       [C]
```

**Identical to the nanocredit.** Streams is not a discount; it is a delivery
mechanism at the same price.

**Envio HyperSync, same job.** Each chain endpoint has its own 30-query/minute
free budget — measured across two endpoints in `docs/solana-research.md` §11.1
and consistent with what I saw today [M]. Using the measured blocks-per-query:

| Chain | Queries needed [C] | Free (30/min) | Starter $70 (100/min) | Pro $480 (1000/min) |
|---|---|---|---|---|
| Ethereum | 309,674 | 7.2 d | 2.2 d | 0.2 d |
| Base | 3,220,135 | 74.5 d | 22.4 d | 2.2 d |
| Arbitrum | 101,001 | 2.3 d | 0.7 d | 0.1 d |
| BSC | 6,141,284 | **142.2 d** | 42.6 d | 4.3 d |
| Polygon | 323,319 | 7.5 d | 2.2 d | 0.2 d |

Because the budgets are per endpoint, the chains run in parallel and the
calendar time is the **worst chain, not the sum**: ~142 days free, ~43 days on
Starter, **~4.3 days on Pro** [C]. (Conservative: today's heavy blocks give the
fewest blocks per query.)

> **The headline.** Same five chains, same data: **$0–$480 on Envio against
> ~$16,000 on QuickNode.** A factor of 33 at Envio's most expensive tier, and
> unbounded at its free one.

Bytes, for capacity planning, at today's block sizes [C from M]: **~25 TB over
HyperSync, ~38 TB over RPC.** History is lighter than the tip, so [E] the real
figures are likely 40–60% of those. Either way the download is not the obstacle;
ClickHouse storage for 801 M blocks is a separate conversation and is unchanged
by the provider choice.

### 3.3 (b) Following the head of 50 chains for a month

Block rates measured over a 220-second window today [M]:

| Chain | blocks/day | Chain | blocks/day |
|---|---|---|---|
| Ethereum | 6,676 | Avalanche | 65,193 |
| Base | 43,593 | Linea | 11,782 |
| Arbitrum | 341,673 | Unichain | 87,185 |
| BSC | 192,436 | Sonic | 53,018 |
| Polygon | 57,731 | Berachain | 43,593 |
| Optimism | 43,593 | HyperEVM | 89,149 |
| | | Monad | 289,047 |

Sum of those thirteen = 1,324,669 blocks/day. For the other 37 chains of a
50-chain fleet I assume a typical 2-second chain, 43,200 blocks/day [E].

```
1,324,669 + 37 x 43,200 = 2,923,069 blocks/day                        [C]
x 30 days               = 87,692,070 blocks/month                     [C]
x 40 credits (2 calls, or Streams block_with_receipts)
                        = 3,507,682,800 credits/month                 [C]
```

That is more than Business (2 B) and more than Business+ T1 (3.2 B), so the bill
is **$1,499 + 308 M overage x $0.50/1M = ~$1,653/month**, or on Business
**$999 + 1.5 B x $0.50/1M = ~$1,753/month** [C]. Call it **~$1,700/month**.

**Plus the head poll, which is the trap.** `src/pipeline/mod.rs` sets
`HEAD_POLL_INTERVAL = 1 s`. On QuickNode that is `eth_blockNumber` at 20 credits:

```
50 chains x 86,400 s x 30 days x 20 credits = 2,592,000,000 credits/month
                                            = $1,296/month              [C]
```

**Head polling would cost as much as the data.** The fix is easy — poll at the
chain's block time instead of 1 s (at 12 s: 216 M credits, **$108/month** [C]),
or use a WebSocket `newHeads` subscription — but it has to be a deliberate
change, and it is exactly the kind of thing that silently triples a bill. Note
too that `null` results are not billed [P], so a head poll that finds nothing new
is free only if it *returns* null; `eth_blockNumber` always returns a number and
always bills.

**Envio, same job: $0.** `/height` carries no rate-limit headers and needs no
token [M]. Data queries fit inside the free 30/min per endpoint at the cadence a
head follower needs. Pro ($480/month) buys headroom, not necessity.

### 3.4 (c) Solana

Slot rate 324,538/day, measured in `docs/solana-research.md` §11.2 [M there].

**Following the head.**
```
324,538 slots/day x 30 credits x 30 days = 292,084,200 credits/month   [C]
                                          = ~$146/month at $0.50/1M    [C]
```
Comfortable inside a Scale plan; Yellowstone gRPC is included from Scale up, but
gRPC is byte-metered (10 credits/0.1 MB, **15 from 2026-10-01** [P]) so at the
measured ~1.4 MB/slot on the wire it would cost ~210 credits/slot — **7x more
than `getBlock`** [C]. **For us, gRPC is the wrong tool**: it is a low-latency
product and we are 14–20 s behind the chain by design.

Envio, same job: **$0** [M].

**Backfill from 2026-01-03 (57,334,753 slots).**
```
57,334,753 x 30 credits = 1,720,042,590 credits = ~$860               [C]
```
Envio, same job: **$70** (one month of Starter, ~13 days —
`docs/solana-research.md` §11.3.2) [P].

**Backfill deeper than Envio can go — the whole point.** Head ~448.4 M slots.
```
448,400,000 x 30 credits = 13,452,000,000 credits = ~$6,726           [C]
```
Time: at Scale's 250 RPS, 448.4 M calls = **20.8 days**; at Business's 500 RPS,
**10.4 days** [C]. Bandwidth is the real question. I measured a live Solana
`getBlock` today (slot 448,456,105, public RPC): **4.58 MB raw / 1.44 MB gzipped
on the wire** with `encoding: base64`, and 8.18 MB / 1.70 MB with `jsonParsed`
[M]. Older slots are far lighter (the chain was a fraction of its current size
before 2024), so [E] the full-history download is on the order of **200–300 TB**,
against Envio's 28.6 TB for its 8.5 months (which is *filtered* to our 26
programs; QuickNode's `getBlock` is not, and there is no Solana Streams backfill
to filter it) [P].

For comparison [P]: Old Faithful is free and covers genesis onward but needs a
second ingest path and ~250 TB (`docs/solana-research.md` §11.3.1 already costed
this at 113 TB for the Envio-era window); Triton One charges "$10.00 per million
queries" for archive reads, i.e. **~$4,484** for 448.4 M `getBlock` calls [C] —
cheaper than QuickNode but a new vendor.

### 3.5 First-year totals

| | Envio | QuickNode |
|---|---|---|
| EVM backfill (5 chains) | $0–$480 | ~$16,025 |
| EVM head, 50 chains, 12 months | $0–$5,760 | ~$20,400 (+$1,300/mo if head polling is left at 1 s) |
| Solana head, 12 months | $0 | ~$1,752 |
| Solana history since 2026-01-03 | $70 | ~$860 |
| Solana history to genesis | not possible | ~$6,726 |
| **Total** | **$70 – $6,310** | **~$45,763** |

[C] throughout. QuickNode's figure would shrink with an Enterprise contract we
have not priced; Envio's would not, because it is already close to zero.

---

## 4. Speed

**Backfill.** Both sides are limited by different things:

| | Binding limit | 5-chain full history |
|---|---|---|
| HyperSync free | 30 queries/min per endpoint | ~142 days (worst chain: BSC) [C] |
| HyperSync Pro $480 | 1,000 queries/min | **~4.3 days** [C] |
| QuickNode RPC, Scale (250 RPS) | requests/second | 1.60 B calls / 250 = **74 days** [C] |
| QuickNode RPC, Business (500 RPS) | requests/second | **37 days** [C] |
| QuickNode Streams | not published | unknown; "7x faster backfills" is the only claim [P] |

**Batching does not help.** QuickNode counts a 50-call batch as 50 requests
against RPS *and* 50 against credits, and advises against batching on latency
grounds [P]. So the RPS ceiling is a hard blocks-per-second ceiling: 250 RPS is
**125 blocks/second**, 500 RPS is **250 blocks/second** [C]. HyperSync on the
same Ethereum data delivered 84 blocks in one request [M]; at Pro's 1,000
requests/minute that is **117 blocks/second on Ethereum and 4,182 on Arbitrum**
[C] — and it costs $480 a month flat instead of $16,000 once.

**This is the sharpest technical finding in the document: QuickNode is not
faster, it is slower, and it is ~30x more expensive.** The "thousands of blocks
per second" in `docs/design.md` is real for light chains (Arbitrum, measured at
5,018 blocks per single request today) and not reachable over per-block RPC at
any plan QuickNode publishes.

**At the tip**, neither matters. Both are far inside one block time on every
chain we index, and the coordinator measured ~180 ms per call on the owner's
QuickNode endpoint today.

---

## 5. Data completeness

Our field selection (`src/source/evm.rs`) is 21 block fields, 20 transaction +
receipt fields, 10 log fields. Mapping each to JSON-RPC:

| Our fields | Over JSON-RPC |
|---|---|
| number, hash, parent_hash, nonce, sha3_uncles, transactions_root, state_root, receipts_root, miner, difficulty, extra_data, size, gas_limit, gas_used, timestamp, uncles, base_fee_per_gas, withdrawals_root, withdrawals, mix_hash | `eth_getBlockByNumber` — all present, same names modulo camelCase |
| from, gas, gas_price, hash, input, nonce, to, transaction_index, value, max_priority_fee_per_gas, max_fee_per_gas, access_list, type | the block's `transactions[]` with `fullTransactions = true` |
| cumulative_gas_used, effective_gas_price, gas_used, contract_address, status | `eth_getBlockReceipts`, joined to the transaction by `transactionIndex` |
| log_index, transaction_index, transaction_hash, block_number, address, data, topic0..3 | the receipts' `logs[]`. **No separate `eth_getLogs` is needed**, which also sidesteps the 10,000-block range cap [P] |
| **total_difficulty** | **the one real gap.** Geth dropped `totalDifficulty` from the block response in v1.14 and most providers omit it post-merge; archive nodes usually still serve it pre-merge. Needs a decision: store 0 after the merge, or accept a hole. [E] |

**Everything we store today exists over plain JSON-RPC, with that one asterisk.**
It is only true because we removed traces: a trace table would put us on
QuickNode's 2x multiplier and on the ~23 chains that support `trace_block` [P].

**What HyperSync gives that RPC does not:** the `rollback_guard`
(`first_block_number` + `first_parent_hash`, used in `src/pipeline/mod.rs` and
`src/reorg/guard.rs` as independent evidence of a rollback), columnar Arrow
transport, and server-side filtering we do not currently use. **What RPC gives
that HyperSync does not:** `eth_call` (we already use RPC for that,
`src/tokens/`), chains HyperSync lacks, and any provider we like.

**Solana.** The decoder wants `SvmAccountActivity { account, mint, pre_owner,
post_owner, decimals, pre/post_token_balance, pre/post_balance, is_signer,
is_fee_payer, token_program }` plus inner instructions and log lines. Over
`getBlock` [P]: `pre/postTokenBalances` carry `accountIndex`, `mint`, `owner`,
`programId` and `uiTokenAmount.{amount, decimals}` (mint, owners, decimals,
balances, token program — all present); `pre/postBalances` give the lamport
sides; `innerInstructions` gives the instruction tree the per-subtree decoder
needs; `logMessages` gives the `Program log:` / `Program data:` lines Raydium and
Orca events live in; `rewards` exists but defaults to `false`. `is_signer` is not
served but is derivable from `message.header.numRequiredSignatures` and the
account-key ordering (`is_fee_payer` = key 0) — our type already models it as
`Option<bool>` because a source may not know.

Two caveats. **`has_dropped_log_messages` does not exist over RPC**, and
`docs/design.md` §14 makes a correctness decision on it (a truncated transaction
is never enriched from its logs). The only signal is a trailing `"Log truncated"`
line [E — Agave behaviour, undocumented by QuickNode, confirm before shipping].
And **address-lookup tables**: QuickNode recommends `jsonParsed` because it
"includes all transaction account keys (including those from Lookup Tables)" [P];
with `base64` we resolve ALTs ourselves. `jsonParsed` costs 1.18x the compressed
bytes [M] — cheap insurance.

**Net: a QuickNode Solana source can produce every field `SvmSlotBatch` needs
except `dropped_logs`, which becomes a string check.**

---

## 6. Reorgs

Our design (`docs/design.md` §2, `src/reorg/`): `--confirmations N` →
parent-hash continuity inside the stream (+ `rollback_guard`) → fork-point search
in windows of 8, 16, 32 … via `CanonicalChain::headers` → `purge_range` =
tombstones + a new epoch → resume from the fork point. **Insert-only, no DELETE.**

**Over polling RPC.** The mapping is direct and, in one respect, better:

| Layer | Over RPC |
|---|---|
| `--confirmations N` | unchanged: `head = eth_blockNumber - N` |
| Parent-hash continuity | unchanged: the block we fetch carries `parentHash` |
| `rollback_guard` | **gone.** `SourceResponse.rollback_guard` becomes `None`. The pipeline already handles `None` (it is an `Option`), and `src/reorg/tests.rs` has a case for the guard being the *only* evidence — that path simply never fires. Detection falls back entirely to parent-hash continuity, which is what it mostly is anyway. |
| Fork-point search | `CanonicalChain::headers(from, to)` becomes N x `eth_getBlockByNumber(n, false)`. At windows of 8/16/32/…/512 and 20 credits a header, the deepest possible search is 1,016 calls = 20,320 credits = **$0.01** [C]. Non-issue. |
| `purge_range`, epochs, tombstones | **completely unchanged.** They never touch the source. |

**The two real risks, and the fix for both.**

1. **Load-balanced nodes disagree about the head.** QuickNode routes each request
   to a pool and documents head-lag as expected: `-32000 "Header not found /
   Block not found"`, with the advice "the node you're hitting is not in sync
   yet. Use a retry mechanism" [P]. They also sell a *Smart RPC Load Balancer*
   add-on that routes "to active RPC endpoints with the highest block height" —
   i.e. QuickNode concedes a bare endpoint can serve a stale head and go
   **backwards** between two calls [P]. So the sync loop must treat a receding
   head as normal, not as a reorg: `head = max(head, previous)` plus a staleness
   alarm. `null` and 4xx/5xx are not billed [P], so the retry loop costs RPS only.

2. **`eth_getBlockByNumber` and `eth_getBlockReceipts` can hit different nodes
   and therefore different blocks** — a silent corruption that cannot happen over
   HyperSync. **Fix: fetch atomically by hash. I verified today that
   `eth_getBlockReceipts` accepts a block hash** (probed against a public
   Ethereum node: `getBlockByNumber` → `result.hash` → `getBlockReceipts(hash)`
   returned the matching receipts) [M]:

   ```
   1. eth_getBlockByNumber(n, true)   -> block, hash H
   2. eth_getBlockReceipts(H)         -> receipts of exactly that block
   3. assert every receipt.blockHash == H and count == block.transactions.len()
   ```

   Step 3 is two lines and turns corruption into a retry. Step 1 is the only
   place a stale node bites, and that just means re-fetching the height.

**Over Streams** reorgs are handled for us and documented properly [P]: detection
is parent-hash mismatch; `fix_block_reorgs: 1` (paid plans) re-delivers corrected
blocks; every batch carries `reorgs` (the orphaned block) and `blocks_reorged`,
in the body and as `Batch-Reorgs` / `Batch-Blocks-Reorged` headers;
`keep_distance_from_tip` is exactly our `--confirmations`; and a
`batch_start_range` that is not sequentially increasing is the rollback signal.
That maps onto `purge_range` cleanly. But the same page also says "independently
verify the continuity of block hashes … if you detect a mismatch, pause your
processes" — so we would keep our own detector anyway, and Streams' reorg
machinery becomes a second opinion we do not need.

---

## 7. Lock-in and resilience

Three options, judged on what happens when the vendor has a bad day.

**A. Generic RPC source + HyperSync for fast backfill (recommended).** One new
file implementing two traits. Works against QuickNode, Alchemy, dRPC, Ankr,
Infura, Chainstack or our own reth/erigon node, switchable per chain by a flag.
HyperSync stays the default because it is free and faster. Cost: 8–13
engineer-days and one more path to keep tested. **Lock-in: none.**

**B. QuickNode Streams.** Their sandboxed filter language, their five
destinations (none of them ClickHouse), a webhook receiver for us to write and
operate, their pause behaviour, their unpublished throughput — and it costs the
same as (A) in credits [C]. We would be running a service that receives raw
`eth_getBlockByNumber` + `eth_getBlockReceipts` payloads, i.e. exactly what (A)
fetches, and then still do all our own decoding, reorg handling and writing.
**The only thing Streams saves is the fetch loop. Lock-in: high.**

**C. Replace HyperSync entirely.** ~$46,000 in year one [C], ~25 chains lost [P],
slower on backfill [C], every chain behind one vendor with no contractual uptime
SLA below Enterprise [P]. **No.**

**Resilience is the strongest single reason to spend the days on (A).** Today a
HyperSync incident stops all 50 chains at once; with `--source rpc` per chain in
fleet mode, the answer is a config change rather than an outage.

---

## 8. What would have to be built — EVM

### 8.1 New code

**`src/source/rpc.rs`** — one file, implementing `BlockSource` and
`CanonicalChain`, next to `evm.rs`. Structure:

```rust
pub struct Source {
    http: reqwest::Client,        // already a dependency
    endpoints: Vec<Url>,          // comma separated, failover, like --rpc
    chain_id: u64,
    concurrency: usize,           // in-flight requests
    rps: RateLimiter,             // token bucket, per endpoint
}

impl BlockSource for Source {
    async fn head(&self) -> Result<u64>;                 // eth_blockNumber - never regress
    async fn stream(&self, range) -> Receiver<Result<SourceResponse>>;
}
impl CanonicalChain for Source {
    fn headers(&self, from, to) -> Vec<BlockHeader>;     // getBlockByNumber(n, false)
}
```

`stream` runs a bounded window of concurrent per-block fetches, reorders into
block order and emits one `SourceResponse` per chunk with `next_block` set — the
same contract HyperSync honours, which is what the sync loop and
`transform::transform_with` already assume (a response must contain *every* block
of the range it covers, or it is an error). Atomic fetch per block is the
three-step sequence of §6. Fallback for a chain without `eth_getBlockReceipts`:
`eth_getLogs` over the chunk (<= 10,000 blocks [P]) plus
`eth_getTransactionReceipt` per transaction — far more expensive, so opt-in and
loud.

### 8.2 Is `SourceResponse` tied to HyperSync, and what would a neutral type cost?

Yes, and less than it looks.

**Option 1 — reuse the HyperSync input types (recommended first step, ~1 day).**
Deserialize the RPC JSON straight into `hypersync_client::simple_types::{Block,
Transaction, Log}`: their field types already parse `"0x…"` hex, so a shim struct
with `#[serde(rename_all = "camelCase")]` gets most of the way in one pass, and
receipt fields merge onto the transaction by index. `core::decode` and
`pipeline::transform` are untouched, every existing test keeps its meaning, and
`selection_is_exactly_what_the_rows_need` keeps guarding the field set. *Cost:
the crate name `hypersync_client` appears in a file that has nothing to do with
HyperSync. Ugly, honest, reversible.*

**Option 2 — neutral types (~3–4 extra days).** Define
`source::{RawBlock, RawTransaction, RawLog}` in `src/source/mod.rs` and rename
`from_hypersync` to `from_raw` across `src/core/decode.rs` (four functions, ~90
call sites with tests). Mechanical, but it touches `core/decode.rs`,
`core/models/*`, `pipeline/{transform,sync_tests,acceptance}.rs`,
`db/integration_tests.rs` and `svm/fixtures.rs`. **Do it second**, once
`--source rpc` has earned its place; doing it first front-loads risk into the
part that is already correct.

### 8.3 Config

```
--source hypersync|rpc     (env SOURCE, default hypersync)
--source-url <list>        (comma separated, failover; distinct from --rpc,
                            which is the eth_call pool for token metadata)
--source-rps <n>           request budget      --source-concurrency <n>
```

`Config` gains `source: SourceKind` and `source_url: Option<String>`;
`pipeline::run` picks the constructor and hands the same `Runtime` to the
unchanged `run_with<S: BlockSource>`. In fleet mode (`docs/design.md` §15) the
per-chain `settings` JSON in `fleet_chains` already carries the `run` options, so
`source` is one more key — **per chain**, which is exactly the granularity we
want: HyperSync for the chains it serves, RPC for Story and for anything
HyperSync drops.

### 8.4 Concurrency and rate-limit budget

Two calls per block, 20 credits each. At the ~180 ms per call the coordinator
measured, saturating the plan RPS needs 9 concurrent requests on Build (50 RPS =
25 blocks/s), 45 on Scale (250 RPS = 125 blocks/s) and 90 on Business (500 RPS =
250 blocks/s) [C]. The limiter must be **account-wide, not per chain** —
QuickNode's RPS is a plan figure while a plan allows 10–50 endpoints, and nothing
published says the budget multiplies [P + E]; that matches the fleet's existing
"one budget per provider" rule. Back off on `429` / `-32007` / `-32008`
exponentially, with no `Retry-After` header to lean on [P].

### 8.5 Tests

`MockSource` in `src/pipeline/sync_tests.rs` is **unchanged** — it implements the
trait, not HyperSync, so the sync loop's whole suite is already source-agnostic.
This is the single biggest reason the work is small. New tests in
`src/source/rpc.rs`: recorded-fixture cases per chain family (Ethereum
post-Cancun, Arbitrum, Optimism, pre-London, pre-Byzantium) asserting the rows
are **byte-identical to the rows HyperSync produces for the same block** — cheap
to write, because both paths end in the same `DatabaseBlock`/`Transaction`/`Log`;
a receipt/transaction count mismatch is an error, not a silent row; `head()`
never regresses. Keep `selection_is_exactly_what_the_rows_need` as the
field-drift guard.

### 8.6 What does not change

Schema and migrations · `purge_range`, tombstones, epochs, `epoch_floor_v` ·
checkpoints and gap healing · the lease and fencing · `core`, `dex`,
`predictions`, `launchpads`, `svm` decode modules · the writer and its flush
rules · `indexer verify` · fleet mode and the control panel · `src/tokens`
(already plain JSON-RPC).

### 8.7 Effort

| Task | Days [E] |
|---|---|
| `src/source/rpc.rs`: client, failover, limiter, concurrent window, ordering | 3 |
| JSON-RPC → `simple_types` mapping, receipt merge, hash-atomic fetch | 2 |
| `CanonicalChain::headers` over RPC | 0.5 |
| Config, fleet wiring, `--source` | 1 |
| Fixture tests incl. HyperSync-vs-RPC row equality | 2 |
| Head handling (non-monotonic head, `null`, `-32000` retry) | 1 |
| Docs, README, metrics labels | 1 |
| **Subtotal** | **10.5** |
| Optional: neutral `RawBlock` types (§8.2 option 2) | +3.5 |

**8–13 engineer-days**, depending on whether the neutral-type refactor is
included.

---

## 9. What would have to be built — Solana

`SlotSource { head, fetch(from, to) -> SlotPage }` and `SlotPage { next_slot,
batches: Vec<SvmSlotBatch>, budget }`. **`SvmSlotBatch` is already ours** — no
HyperSync types anywhere in `src/svm/`. So a QuickNode Solana source is:

**`src/source/solana_rpc.rs`:**
```
head()  = getSlot({commitment: "finalized"})
fetch() = getBlock(slot, {encoding: "jsonParsed" | "base64",
                          transactionDetails: "full",
                          maxSupportedTransactionVersion: 1,
                          rewards: false})
          for each slot in [from, to), concurrently
       -> SvmSlotBatch { slot, blockhash, parent_slot, parent_blockhash,
                         block_height, timestamp, transactions }
```

`SvmTransaction` maps straightforwardly: `signature` /`fee_payer` from
`transaction.signatures[0]` and `message.accountKeys[0]`; `success` / `fee` /
`compute_units` from `meta`; `instructions` with their `path` from
`message.instructions` plus `meta.innerInstructions`; `activity` from the token
and lamport balance arrays as in §5; `logs` from `meta.logMessages`;
`dropped_logs` from a trailing `"Log truncated"` [E].

**Two things are harder than on Envio.**

1. **Log-to-instruction attribution.** HyperSync serves an
   `instruction_address` per log row, which is what makes `SvmLog.path`
   trustworthy. Over `getBlock` the log lines are one flat array and the path has
   to be reconstructed by tracking `invoke [n]` / `success` / `failed` markers.
   This is well-trodden but it is real work and it must be tested against the
   recorded fixtures in `src/svm/fixtures.rs`, where the Raydium and Orca
   decoders depend on it.
2. **No server-side filtering.** Envio returns only our 26 programs' rows; every
   `getBlock` returns the whole slot. Bandwidth goes from 0.5 MB/slot Arrow to
   ~1.4 MB/slot gzipped [M], and the decoder has to walk ~5x the instructions.
   `docs/solana-research.md` §11.3 already flags `svm::decode` throughput as the
   unmeasured risk (task S0); this makes measuring it mandatory, not optional.

**Yellowstone gRPC is the wrong tool for us.** It replays only ~3,000 slots /
20 minutes [P], is byte-metered at 7x the cost of `getBlock` for the same slot
[C], and buys sub-second latency in a pipeline that is deliberately 14–20 s
behind. Use it only if a future product needs the tip.

**Effort [E]:**

| Task | Days |
|---|---|
| `src/source/solana_rpc.rs`: getSlot/getBlock, concurrency, retries | 2 |
| `getBlock` → `SvmSlotBatch` mapping | 2 |
| Log attribution (`invoke`/`success` walker) + fixture tests | 3 |
| Fixture equivalence: Envio-sourced vs RPC-sourced `SvmRows` for the same slots | 2 |
| `--source` wiring, budget accounting (credits, not queries/min) | 1 |
| **Total** | **10** |

Plus task S0 from `docs/solana-research.md` (measure `svm::decode` rows/s) —
which was already required.

---

## 10. Recommendation

1. **Keep Envio HyperSync as the default EVM source.** It is free-to-cheap,
   faster on backfill, fewer bytes on the wire, and serves ~25 chains QuickNode
   does not. Nothing in the research argues for replacing it.
2. **Build `src/source/rpc.rs` behind `--source hypersync|rpc`, per chain
   (8–13 days).** Buy insurance against a HyperSync outage, unlock chains
   HyperSync does not serve (Story today; whatever launches next), and make the
   provider a config line. Ship it with option 1 of §8.2; do the neutral-type
   refactor later if it earns its keep.
3. **Do not build against QuickNode Streams.** Same price as RPC, no ClickHouse
   destination, no Solana backfill, a sandboxed filter language, and a push model
   that duplicates machinery we already have and trust.
4. **Solana: run the head on Envio (free) and decide the history separately.**
   Envio's $70 for 8.5 months is still the right first move
   (`docs/solana-research.md` §11.3.2 stands). If the owner wants pump.fun's
   whole life, QuickNode at ~$6,700 and ~200–300 TB is the *simplest* path (same
   `getBlock` adapter as the head follower), Triton at ~$4,484 is cheaper, and
   Old Faithful at $0 is cheapest and needs a second ingest path.
5. **If anything is ever run against QuickNode, fix the head poll first.**
   `HEAD_POLL_INTERVAL = 1 s` is free on Envio and $1,296/month on QuickNode for
   50 chains [C].

---

## 11. Decisions only the owner can make

1. **Budget.** Is ~$16,000 once plus ~$1,700/month a number worth discussing at
   all? If no, everything below the EVM recommendation is moot and the work is
   purely the resilience play.
2. **Which chains.** Do we need the ~25 mainnets HyperSync has and QuickNode
   lacks (Moonbeam, opBNB, Polygon zkEVM, Zircuit, Rootstock, Etherlink …)? Do
   we need Story, which only QuickNode has? Do we need Monad from block 0 — if
   so, QuickNode cannot serve it (40,000-block window [P]) and HyperSync can.
3. **How much Solana history.** 8.5 months for $70, or since genesis for
   ~$6,700 plus a quarter of a petabyte and roughly three weeks of downloading?
   This is the one question where QuickNode offers something Envio cannot.
4. **Whether to spend 8–13 days on insurance.** A JSON-RPC source has no
   immediate revenue; it removes a single point of failure and a vendor's veto
   over which chains we index.
5. **Which QuickNode plan the owner actually has.** Credits and RPS — and
   therefore every dollar figure in §3 — depend on it. The measurements the
   coordinator took today prove the endpoint is multi-chain and archive-capable,
   but not which tier it sits on.

---

## 12. Open questions I could not verify

1. **Whether QuickNode's RPS is per account or per endpoint.** Nothing published
   says; I assumed account-wide, the conservative reading. If it is per endpoint,
   the backfill times in §4 divide by the endpoint count and the *speed* picture
   (not the price) changes materially.
2. **Flat Rate RPS concurrency semantics.** "250 RPS / 6 concurrent" — if
   "concurrent" counts TCP connections rather than in-flight requests, Flat Rate
   at $1,949/month becomes an interesting unmetered backfill option (~74 days for
   the five chains) and should be re-costed. If it counts in-flight requests, it
   is unusable for bulk. **Maximum JSON-RPC batch size** is also unpublished —
   moot for cost, since batches bill per sub-request [P].
3. **Streams backfill throughput.** Not published in any form; "7x faster" is the
   only claim. A one-week pilot would settle it, but needs a dashboard-created
   Streams key, which the owner's endpoint key is not.
4. **Streams' credit multipliers are not documentation.** The §2.2 table comes
   from the pricing calculator's embedded data blob. Self-consistent and matching
   QuickNode's own worked example, but not a quotable price list.
5. **Whether `totalDifficulty` is served by QuickNode's archive nodes** for
   pre-merge blocks. We store the column; one probe at block 15,000,000 settles
   it.
6. **How a truncated Solana log set appears over `getBlock`.** I assert a
   trailing `"Log truncated"` line; that is Agave behaviour, not QuickNode
   documentation, and `docs/design.md` §14 makes a correctness decision on it.
7. **Whether Envio's paid tiers are per endpoint** the way the free tier measured
   per endpoint. §3.2's calendar times assume yes. If a paid token's limit is
   account-wide, Pro's 4.3 days becomes ~20 — still far ahead of QuickNode.
8. **`hypersync-client` 1.4's Arrow endpoint for EVM.** My `POST /query/arrow`
   probe returned zero bytes, so the 43% size saving measured on Solana is
   unconfirmed on EVM and the §3.1 HyperSync byte figures are the JSON ones.
