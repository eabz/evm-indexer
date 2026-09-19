# `svm` — Solana

Solana DEX data, in the same database and the same analytics tables as every
EVM chain. Owns `migrations/0040`–`0049` and the `sol_*` tables.

## Read this first: what these tables are NOT

**This is an analytics-only, program-filtered pipeline.** It is not a Solana
block explorer backend, and the tables here are a deliberate subset of the
chain.

Solana produces ~150M non-vote transactions a day — roughly 30× the row rate
the EVM pipeline is tuned for — and essentially all of the value is
concentrated in a couple of dozen programs. So the indexer asks HyperSync for
DEX and launchpad programs only, and `sol_slots` / `sol_transactions` hold
only the slots and transactions those programs appear in.

Consequences, stated plainly because a partial table presented as a complete
one is worse than no table at all:

| Something you might expect | Why it is not here |
|---|---|
| every SPL transfer of a mint | we only see transfers inside matched transactions, so the answer would be partial and therefore wrong |
| a wallet's history or balance | same reason; a Solana address page could only ever show its DEX activity |
| a transaction count, or daily chain stats | `sol_transactions` is the MATCHED transactions. `count()` over it is not a transaction count, and there is deliberately no `daily_*_stats` for Solana |
| token names and symbols | they live in account state (the Metaplex metadata PDA, or the Token-2022 metadata extension) and HyperSync serves no account reads. Phase 1 stores decimals only |

`decimals`, on the other hand, arrive **free** on every `account_activity`
token row, so unlike EVM a Solana swap can be valued with no RPC call at all.

## Tables

| Table | What it is |
|---|---|
| `sol_slots` | the commit marker, Solana's `blocks`. `block_number` holds the SLOT |
| `sol_transactions` | matched transactions, slim: fee payer, success, fee, compute units |
| `sol_tokens` | mints seen traded, with decimals and their token program |
| `sol_dex_swaps` | swaps, already in the chain-neutral `dex_swaps` shape — see *Merging*, below |

Storage follows the shared rules (docs/design.md §1, §2): binary columns and
no hex strings anywhere, `ReplacingMergeTree(_version, is_deleted)` with
`epoch`, month-only partitions on base tables, and **nothing is ever
deleted** — a rollback INSERTs tombstones. Query everything with `FINAL`.

### Skipped slots are normal

A slot with no block is not a gap and not a reorg; Solana just produces no
block for it. Continuity is the **`parent_slot` / `parent_blockhash` chain**,
never `slot + 1`. Any gap check that assumes every integer has a row will
produce endless false gaps on this chain. This is the one place the EVM reorg
detector genuinely needs a variant.

### Position key

`(chain, block_number, tx_index, ordinal)`, where `ordinal` packs the
instruction tree path 12 bits per level, left aligned (Solana's CPI stack
height limit is 5). A parent sorts before its children, siblings sort in
execution order, and the value is unique inside a transaction — and it is
computable from ONE row, which matters because a program-filtered stream
never sees the sibling instructions a flat rank would need.

## How decoding works

Two layers, **both always on**. This is the opposite balance from EVM, where
an event ABI carries the semantics for free.

### 1. The movement layer — generic, and it is the primary decoder

A swap is an instruction of a registered venue program whose **own subtree**
moves exactly two mints across one common counterparty.

It runs **per instruction subtree and never on a transaction's net balance**.
That is not a style choice. `Qxpfmre4JbRctxg1…` (a recorded fixture) is one
transaction holding two PumpSwap swaps on the *same pool* in *opposite
directions*: ~6.2 SOL moves each way and the transaction's net vault delta is
~0.03 SOL. A decoder reading transaction-level balances reports one 0.03 SOL
trade instead of two 6.2 SOL trades.

This works on Solana in a way it could not on EVM: only the SPL Token program
can change an SPL balance, so the transfer instruction *is* the movement.
The corroboration step `src/dex/corroborate.rs` performs for EVM swaps is
structurally satisfied here, and `verified_in` / `verified_out` are always
populated. A "swap" that moved no tokens never becomes a row — which is also
what keeps prop-AMM **quote updates** out, by construction rather than by a
rule. (Counting instructions instead would overstate those venues about
fivefold.)

What gets classified as what:

| Shape inside the subtree | Result |
|---|---|
| one mint in, a different mint out, one counterparty | a swap |
| a further transfer of a leg's mint to a non-pool account | that leg's fee |
| two mints moving the SAME way across the counterparty | liquidity add/remove — kept OUT of the swap table |
| no token movement at all | a quote update or an unrelated call, dropped |
| anything else | unclassified: counted, dropped, never guessed |

Every rejection is counted in `Diagnostics`, so coverage is measurable
instead of assumed.

### 2. The per-program layer — enriches, and CROSS-CHECKS

Eight venues have one (`events.rs` for the pump.fun pair, `venues.rs` for the
rest). It adds the pool account the venue itself names, exact fee splits and
pool state — and it checks the movement layer on every swap it touches. A row
is marked `decoded` only when the venue's own numbers match what the movement
layer worked out independently; a contradiction is counted, not stored.

**Where each venue publishes its event is the thing to know**, and it is not
uniform:

| Venue | Mechanism | Where the event is |
|---|---|---|
| PumpSwap, pump.fun, Meteora DLMM, Meteora DAMM v2 | Anchor `emit_cpi!` | a self-CPI INSTRUCTION |
| Raydium CPMM, Raydium CLMM, Orca Whirlpools | Anchor `emit!` | a `Program data:` LOG LINE |
| Raydium AMM v4 | bare `msg!` | a `Program log: ray_log:` LOG LINE |
| BisonFi | — | nothing at all |

A log is weaker evidence than an instruction — validators truncate log lines,
and `has_dropped_log_messages` says when they did — so a log-sourced event may
only ever CONFIRM a row that real token transfers already proved. It can never
create one.

Dispatch is on the **event**, never the instruction discriminator, wherever a
program has grown variants: PumpSwap and pump.fun have `buy_exact_quote_in`,
`buy_v2`, `sell_v2`, `buy_exact_sol_in` with different account layouts and one
shared event. The single place an account meta index is used at all is
Raydium AMM v4, whose `ray_log` is seven bare integers and names no accounts;
there the pool is meta 1, which holds for all four swap tags.

Only the **fixed prefix** of an event with a variable-length field is read.
Both pump.fun structs hold a Borsh `String` (`ix_name`) in the middle, and
pump.fun's also a `Vec<Shareholder>`; every field after those has a variable
offset.

Two venues also get a second, independent opinion on "was this a trade?": the
instruction's own discriminator. `Venue::instruction_kind` classifies it as
`Swap` / `Liquidity` / `Admin` / `Unknown` from the venue's published
instruction names, and a disagreement with the shape of the token movement is
counted in `Diagnostics::kind_disagreed`. Live, that counter is zero.

### Routers are attribution, never volume

40% of Solana DEX volume is routed. A fill under an aggregator gets
`route_ordinal` and `route_program` set and is still credited to the venue
that executed it. A Jupiter three-hop becomes three swaps, one per venue —
there is a recorded fixture for exactly that.

`trader` is the transaction's **fee payer**, never the instruction's signer,
which is usually a router PDA or a bot.

### Event layouts are verified by LENGTH

Phase 1 was burned by a stale on-chain IDL that truncated an event by 41
bytes. Every layout here therefore states the exact byte count it implies, and
a parser requires that length **exactly** rather than as a minimum. That is
not pedantry:

- Raydium's CPMM and CLMM **both named their event `SwapEvent`**, so the
  8-byte discriminator is identical and only the length (170 vs 221) tells
  them apart. With a `>=` check a CLMM event parses cleanly as a CPMM one and
  returns plausible nonsense. It did, until the length test caught it.
- Both layouts had **grown** since their last published snapshot — CPMM by
  five creator-fee fields (89 → 170), CLMM by two trade-fee fields
  (205 → 221). The length is what proved the new ones are live.
- Meteora DLMM emits **two** events per swap, and the second is not the first
  with fields appended: `Swap2Evt` reorders them, putting `swap_for_y` and
  `fee_bps` *before* the amounts.

A future version that appends a field will show up as "event not found" — the
row keeps `movement` confidence and the live agreement rate drops — rather
than as a wrong number. That is the right way round.

## Three things the research got wrong

All three were found in live data, and all three are fixed here.

**1. "Both legs share one counterparty" does not identify the pool.**
docs/solana-research.md §3.1 proposes that rule, but a swap is *locally
symmetric*: the taker also receives one mint and sends the other, so the rule
returns two candidates whenever the taker uses one owner for both legs. On
the recorded Jupiter route the router's proxy is a counterparty of all three
hops, and picking it would have inverted every trade's direction. Resolved in
three steps, each with its own reason: a pool takes part in only its own hop
while a router proxy spans the whole route; and a pool authority is a Program
Derived Address, hence off the ed25519 curve (`pda.rs`). Anything still
ambiguous is refused.

**2. A pump.fun SOL leg has no instruction at all.** The curve program
decrements its own account's lamports, so there is no System transfer child
and no WSOL account — a decoder that only reads transfer instructions sees
one mint move and reports nothing. The leg is recovered from the native side
of `account_activity`. There *both* sides turn out to be PDAs (the "user" in
the recorded fixture is a bot's vault), so the movement layer proposes both
readings and the venue's own event picks the right one.

**3. A Token-2022 transfer fee shifts the INPUT leg too, not just the
output.** §3.2 records the output case — the vault sent 32,982,064,366 units
and the user received 32,652,243,722 — and phase 1 modelled it as
`amount_out_gross` against `amount_out`. The input leg has the mirror image
and nothing modelled it, because pump.fun's pools are classic SPL. Live,
Raydium CPMM's agreement rate with the movement layer sat at **52.8%** until
this was understood, and in every disagreeing case the gap was *exactly*
`input_transfer_fee`, to the unit: the movement layer reads the
`transferChecked` instruction, which is what the taker **sent**, while the
event reports what the pool **credited**. Neither is wrong. Each leg now
tolerates exactly the transfer fee the event itself declares for it, and
nothing more.

The same asymmetry decides how the live cross-check is written: a transfer
fee comes out of what the RECEIVER is credited, never out of what the sender
is debited, so every equality asserted against the public RPC is on a
SENDER's side and needs no tolerance at all.

## Streaming

`src/source/solana.rs`. Three things the EVM source does not have to deal
with, all measured live:

- **A matched instruction does not return its children.** Filtering on a
  venue instruction returns that row, its transaction, and all of the
  transaction's log and `account_activity` rows — but not the child SPL
  transfers the movement layer needs. So the SPL Token, Token-2022 and System
  transfer instructions are extra objects in the **same** `instruction_calls`
  array. Objects in one array are OR-ed; *different arrays are INTERSECTED*,
  so this union cannot be expressed any other way.
- **Responses are capped, by FIVE independent row limits** — blocks,
  transactions, instructions, logs and account activity — and each one stops
  the whole response. Raising one raises nothing, because the lowest unset cap
  binds first. Measured on the production query shape: with only
  `max_num_instructions` raised a request returned **one slot**; with all five
  raised, **66**. At 30 queries a minute one slot per query is 0.5 slots/s
  against a chain producing 3.76, so the pipeline could not have followed its
  own head. The client's `StreamConfig` is the same trap one layer down: its
  500 KB response ceiling against ~0.5 MB *per slot* converges the auto-tuner
  back on one slot, so both byte thresholds are raised too.
- **The `log` table has to be selected**, match-all. Raydium's three programs
  and Orca publish their swap events there and nowhere else. It must be
  match-all rather than filtered to those programs, because selections in
  different arrays are INTERSECTED and a `program_id` filter would drop every
  transaction that carries no such log.
- **There is no live-tail mode and no reorg handling.** At the head the
  client errors with "server made no progress at slot N", so the range is
  always bounded and the head follower is ours, exactly as for EVM.

A range below the served history returns empty with `next_slot` **not**
advancing. A resume loop must treat that as a stop condition, not spin.

## Tests

```sh
cargo test svm                                   # unit, on recorded fixtures

TEST_DATABASE_URL=http://default@127.0.0.1:8123/x_test \
  cargo test svm::integration -- --ignored       # real ClickHouse

ENVIO_API_TOKEN=... \
  cargo test svm::live -- --ignored --nocapture  # live mainnet + public RPC
```

`fixtures/recorded.json` holds five real mainnet transactions exactly as the
server returned them, including all three the research calls out: the
two-opposite-swaps netting case, the Jupiter three-hop, and a prop-AMM quote
update that must decode to nothing.

`fixtures/phase2.json` holds four more, all carrying the log table: a route
across two different phase 2 venues, an Orca liquidity instruction that must
decode to **no** swap, a Token-2022 transfer-fee mint, and a
concentrated-liquidity swap that moves the price across a tick. Each one
records, in the file, why it was captured.

The live tests do not compare HyperSync against HyperSync: they recompute
each swap from the public RPC's own `getTransaction` metadata and require the
amounts, mints and trader to be identical.

## Merging into `dex_swaps` — TODO(merge)

`sol_dex_swaps` is temporary. Its columns **are** the chain-neutral
`dex_swaps` columns of docs/design.md §13, in order, so once the
`dex-neutral` work lands the whole migration is:

```sql
INSERT INTO dex_swaps (<columns>) SELECT <columns> FROM sol_dex_swaps;
```

with no renames and no casts. `SvmSwap::DEX_SWAP_COLUMNS` holds that list in
Rust and an integration test compares it against the live table, so a column
added on either side breaks a test instead of being discovered later.

## Venues, and adding one

Streamed today, with their 30-day volume share
(docs/solana-research.md §1.1) — **~54% of Solana DEX volume**:

| Venue | Share | Event | Live agreement with the movement layer |
|---|---|---|---|
| PumpSwap | 23.1% | self-CPI | 100% |
| Orca Whirlpools | 10.0% | log line | 100% |
| Raydium AMM v4 + CPMM + CLMM | 9.5% | log line | 100% / 100% / 100% |
| Meteora DLMM | 8.0% | self-CPI | 100% |
| pump.fun curve | 3.2% | self-CPI | 98.2% |
| Meteora DAMM v2 | 0.4% | self-CPI | 99.3% |

Measured over 150 arbitrary recent slots and 14,418 swaps; 99.8% of all rows
were confirmed by their venue's own event. 15 swaps per venue, 120 in total,
were then recomputed from the public Solana RPC's validator metadata and
matched exactly.

**Adding one is still one line in `programs::VENUES` plus a name.** The
generic layer needs nothing else — that is the whole point of the two-layer
design, and the Jupiter fixture test proves it by decoding venues that are
not streamed. A per-program decoder is then optional, and only buys exact
fees and pool state.

### What is still out of reach, and why

The prop / "dark" AMMs — BisonFi, HumidiFi, Tessera, Scorch, QuantumAMM,
GoonFi, AlphaQ, Deriverse, SolFi V2 and the rest, together **~32% of the
chain** (§1.3) — publish no IDL and, for most of them, no event at all. They
need no new decoding work: the movement layer already handles them, and
`BisonFi` is registered in `Venue::ALL` and decodes correctly in the Jupiter
fixture test. What they need is a judgement about promoting a program to a
venue, because the generic rule identifies token movement perfectly and "this
was a trade on a market" only probabilistically (§3.2, last row). Until
`sol_dex_programs` exists as a curated registry, adding them is a decision
about false positives, not a decoding problem.
