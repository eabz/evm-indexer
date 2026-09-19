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

PumpSwap and the pump.fun bonding curve. It adds the pool account the venue
itself names, exact fee splits and pool/curve state — and it checks the
movement layer on every swap it touches. A row is marked `decoded` only when
the venue's own numbers match what the movement layer worked out
independently; a contradiction is counted, not stored.

Dispatch is on the **event**, never the instruction discriminator. Both
programs have grown instruction variants that carry real volume
(`buy_exact_quote_in`, `buy_v2`, `sell_v2`, `buy_exact_sol_in`) with
different account layouts, and every one emits the same Anchor event. Nothing
in the decoders depends on an account meta index.

Only the **fixed prefix** of each event is read. Both structs hold a Borsh
`String` (`ix_name`) in the middle, and pump.fun's also a
`Vec<Shareholder>`; every field after those has a variable offset.

### Routers are attribution, never volume

40% of Solana DEX volume is routed. A fill under an aggregator gets
`route_ordinal` and `route_program` set and is still credited to the venue
that executed it. A Jupiter three-hop becomes three swaps, one per venue —
there is a recorded fixture for exactly that.

`trader` is the transaction's **fee payer**, never the instruction's signer,
which is usually a router PDA or a bot.

## Two things the research got wrong

Both were found in recorded live data, and both are fixed here.

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
- **Responses are capped** at roughly 700 rows unless `max_num_instructions`
  is set.
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

## Adding a venue

One line in `programs::VENUES` plus a name. The generic layer needs nothing
else — that is the whole point of the two-layer design, and the Jupiter
fixture test proves it by decoding three venues that phase 1 does not stream.
A per-program decoder is then optional, and only buys exact fees and pool
state.
