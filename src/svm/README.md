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
height limit is 5), and the HOP sub-index of a multi-fill instruction in the
four bits left over at the bottom. A parent sorts before its children,
siblings sort in execution order, hop 0 sorts before hop 1 of the same
instruction, and the value is unique inside a transaction — and it is
computable from ONE row, which matters because a program-filtered stream
never sees the sibling instructions a flat rank would need.

The sub-index exists because Orca's `two_hop_swap` and Raydium CLMM's
`swap_router_base_in` execute TWO fills, on two different pools, from one
instruction. They share an `instruction_address`, so without it the second
row replaces the first in a `ReplacingMergeTree`.

### The pool key

`pool_id` is the venue's own pool account and **never a vault authority**.
Five of the ten streamed venues — Raydium AMM v4 and CPMM, Meteora DAMM v2
and DBC, Raydium LaunchLab — own every pool's two vaults with ONE
program-wide PDA, which is also the "common counterparty" the movement layer
finds. Storing it would key the whole venue into a single candle series.
So for those five the pool comes from the instruction's own account metas
(an index verified against the pool each venue's event names), and a fill
whose pool cannot be named at all is written with 32 zero bytes and left out
of the pool-keyed aggregates — the trade still counts, the series does not
exist.

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
create one. **And when that flag is set, no log of the transaction is read at
all**: a line that survived cannot be told from one that did not, and Orca's
two `Traded` lines are selected by position. Those rows keep `movement`
confidence and are counted in `Diagnostics::dropped_logs`, which is a
different fact from "this venue emits no event" and must not be mistaken for
it.

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
there is a recorded fixture for exactly that. The list is every Jupiter
version, DFlow, both OKX routers, Titan, Photon, Trojan, Ave, Terminal and
GMGN; registering one changes attribution only, because the fill is
attributed to the venue whether the router above it is known or not.

`trader` is the account the **venue's own event** names as the user, and the
transaction's fee payer only when no event names one. It is never the
instruction's signer. The fee payer alone was wrong often enough to matter:
on the recorded pump.fun curve sell it is a bot, and
`sol_dex_candles_*.traders` is `uniqState(trader)`.

### `fee_amount` is in ONE mint, and the row says which

A swap can pay a fee in lamports and another in the token. `fee_mint` names
the mint `fee_amount` is in, and only fees of that mint are summed — the
column used to add every non-pool transfer of the subtree together whatever
its unit.

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

`fixtures/round4.json` holds the two transactions the review round 4
addendum names, recorded by slot and signature: a Raydium v4 fill whose
vaults are owned by the one program-wide authority next to a PumpSwap sell
whose taker account is opened and closed inside the transaction, and a
LaunchLab sell with a real 1% Token-2022 transfer fee.

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
| pump.fun curve | 3.2% | self-CPI | **100%** (was 98.2%) |
| Meteora DAMM v2 | 0.4% | self-CPI | 98.7% |
| Meteora DBC | 0.3% | self-CPI | 99.8% |
| Raydium LaunchLab | 0.2% | self-CPI | 100% |

Measured over 150 arbitrary recent slots and ~14,000 swaps; 99.8% of all rows
were confirmed by their venue's own event. 15 swaps per venue, 120 in total,
were then recomputed from the public Solana RPC's validator metadata and
matched exactly.

The two new entries are LAUNCHPADS whose curve is also the market until
migration, exactly like pump.fun's. They are registered as venues for that
reason and their launches, trades, graduations and fee sweeps go into the
shared `launchpad_*` tables as well — see *Launchpads*, below.

**Adding one is still one line in `programs::VENUES` plus a name.** The
generic layer needs nothing else — that is the whole point of the two-layer
design, and the Jupiter fixture test proves it by decoding venues that are
not streamed. A per-program decoder is then optional, and only buys exact
fees and pool state.

### The prop AMMs, and `sol_dex_programs`

The prop / "dark" AMMs — BisonFi, HumidiFi, Tessera, Scorch, QuantumAMM,
GoonFi, AlphaQ, Deriverse, SolFi V2 and the rest, together **~32% of the
chain** (§1.3) — publish no IDL and, for most of them, no event at all. They
need no new decoding work: the movement layer already handles them, and
`BisonFi` is registered in `Venue::ALL` and decodes correctly in the Jupiter
fixture test.

What they need is a **judgement**, because the generic rule identifies token
movement perfectly and "this was a trade on a market" only probabilistically
(§3.2, last row): staking, lending and NFT sales also move two mints across
one counterparty. Promoting a program to a venue is a false-positive
decision, and a false positive here is a fabricated market on somebody's
screen.

`sol_dex_programs` (migration `0042`) is where that judgement lives —
operator data, with a stated confidence and a stated source, revisable
without a release, exactly like `quote_tokens` and `dex_trusted_emitters`.
**Migrations seed nothing**; the rows below are ready to run.

Venues with a per-program decoder in this module. Every id round-trips
through base58 in `programs.rs`'s own unit test, so a typo here is a test
failure rather than a filter that silently matches nothing.

```sql
INSERT INTO sol_dex_programs (program_id, name, kind, confidence, source) VALUES
  (toFixedString(base58Decode('pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA'), 32), 'pumpswap',          'venue', 100, 'public IDL; decoder in svm/events.rs'),
  (toFixedString(base58Decode('6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P'), 32), 'pump_fun',          'venue', 100, 'public IDL; decoder in svm/events.rs'),
  (toFixedString(base58Decode('675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8'), 32), 'raydium_amm_v4',    'venue', 100, 'raydium-io/raydium-amm'),
  (toFixedString(base58Decode('CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C'), 32), 'raydium_cpmm',      'venue', 100, 'raydium-io/raydium-cp-swap'),
  (toFixedString(base58Decode('CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK'), 32), 'raydium_clmm',      'venue', 100, 'raydium-io/raydium-clmm'),
  (toFixedString(base58Decode('whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc'), 32), 'orca_whirlpool',    'venue', 100, 'orca-so/whirlpools'),
  (toFixedString(base58Decode('LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo'), 32), 'meteora_dlmm',      'venue', 100, 'MeteoraAg/dlmm-sdk'),
  (toFixedString(base58Decode('cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG'), 32), 'meteora_damm_v2',   'venue', 100, 'MeteoraAg/damm-v2'),
  (toFixedString(base58Decode('dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN'), 32), 'meteora_dbc',       'venue', 100, 'MeteoraAg/dynamic-bonding-curve v0.2.1 SOURCE - its on-chain IDL is STALE'),
  (toFixedString(base58Decode('LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj'), 32), 'raydium_launchlab', 'venue', 100, 'the deployed program on-chain IDL 0.2.0 - raydium-io/raydium-idl is STALE');
```

Prop AMMs: real markets, no IDL, decoded by the movement layer alone.
Confidence below 100 because "this program is a market" rests on observation
rather than on a published format — which is exactly what the column is for.

```sql
INSERT INTO sol_dex_programs (program_id, name, kind, confidence, source) VALUES
  (toFixedString(base58Decode('BiSoNHVpsVZW2F7rx2eQ59yQwKxzU5NvBcmKshCSUypi'), 32), 'bisonfi',   'prop_amm', 90, 'docs/solana-research.md appendix B; decodes in the Jupiter fixture test'),
  (toFixedString(base58Decode('9H6tua7jkLhdm3w8BvgpTn5LZNU7g4ZynDmCiNN3q6Rp'), 32), 'humidifi',  'prop_amm', 80, 'docs/solana-research.md section 2; obfuscated data, 2 SPL transfers a swap'),
  (toFixedString(base58Decode('TessVdML9pBGgG9yGks7o4HewRaXVAMuoVj4x83GLQH'), 32), 'tessera_v', 'prop_amm', 80, 'docs/solana-research.md section 2; free-text S-00 log only');
```

Routers. **Attribution only**: 40% of Solana DEX volume is routed, so
counting a router's instruction as a trade would double count almost half
the chain.

```sql
INSERT INTO sol_dex_programs (program_id, name, kind, confidence, source) VALUES
  (toFixedString(base58Decode('JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4'), 32), 'jupiter_v6', 'router', 100, 'docs/solana-research.md section 1.2'),
  (toFixedString(base58Decode('FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9'), 32), 'axiom',      'router',  90, 'docs/launchpads-research.md section 2.1');
```

A row sets the `protocol` NAME a swap of that program is stored under. It
does **not** add the program to the streaming query — that is still one line
in `programs::VENUES`, because a streamed program costs bandwidth on every
slot for ever and that is a different decision from naming one. An unlisted
program keeps its built-in name, so the table can only ever ADD knowledge:
`the_program_registry_renames_a_venue_and_nothing_else` asserts both halves.

## Launchpads

pump.fun, Meteora DBC and Raydium LaunchLab write into the **same**
`launchpad_tokens` / `launchpad_trades` / `launchpad_graduations` /
`launchpad_creator_fees` as every EVM chain (docs/design.md §11). One UI
screen shows a launch, its curve trades, its graduation and then its
PumpSwap candles continuously, because the graduation row's `pool_id` IS
the `sol_dex_swaps.pool_id` the DEX decoder writes.

Nothing in `src/launchpads/**` changed shape to make that work. Those tables
have been `FixedString(32)` with a raw `tx_id String` and a
`(chain, block_number, tx_index, ordinal)` position key since migration
`0030`, so a pubkey, a 64-byte signature and a packed instruction path fit
as they are.

| Family | Program | Launch | Trades | Graduation | Fees |
|---|---|---|---|---|---|
| `pumpfun` | `6EF8rrec…F6P` | `CreateEvent` | `TradeEvent` | `CompletePumpAmmMigrationEvent`, **names the pool** | `CollectCreatorFeeEvent` |
| `meteora_dbc` | `dbcij3LW…aqN` | `EvtInitializePool` | `EvtSwap2`, **carries curve progress** | `EvtCurveComplete`, names no pool | `EvtClaim*TradingFee`, `Evt*WithdrawSurplus` |
| `raydium_launchlab` | `LanMV9sA…3uj` | `PoolCreateEvent` | `TradeEvent` | `pool_status = Migrate` on the trade | — |

### Trust on Solana is STRUCTURAL, not a registry

On EVM anyone can deploy a contract that emits `TokenLaunched`, which is why
`launchpad_trusted_emitters` exists. On Solana the emitter is the **program
id**, which the runtime stamps on every instruction and which cannot be
forged. So:

* `launchpad_tokens.emitter` is the **program**, and an operator lists
  exactly three of them;
* `launchpad_tokens.curve` — and `launchpad_trades.emitter` — is the **curve
  account**: the bonding curve, the DBC virtual pool, the LaunchLab pool
  state. `launchpad_trusted_curves_v` is the listed singletons UNION every
  curve a listed emitter announced, so it works unchanged and a trade joins
  its token exactly as it does on EVM.

```sql
INSERT INTO launchpad_trusted_emitters (chain, emitter, family, label) VALUES
  (1399811149, toFixedString(base58Decode('6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P'), 32), 'pumpfun',           'pump.fun bonding curve'),
  (1399811149, toFixedString(base58Decode('dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN'), 32), 'meteora_dbc',       'Meteora Dynamic Bonding Curve'),
  (1399811149, toFixedString(base58Decode('LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj'), 32), 'raydium_launchlab', 'Raydium LaunchLab');
```

Three rows, and every real curve on the chain follows from them.
`the_trust_views_accept_solana_rows` asserts both directions: with none of
them listed a headline view yields **nothing** — missing numbers, never
wrong ones — and with all three the token page returns its launch.

And there is something EVM has no equivalent of. Every curve account is a
**Program Derived Address of the launch's own fields**, so the decoder
re-derives it and compares:

| Family | Seeds |
|---|---|
| pump.fun | `["bonding-curve", mint]` |
| Raydium LaunchLab | `["pool", base_mint, quote_mint]` |
| Meteora DBC | `["pool", config, max(mints), min(mints)]` |

A launch whose curve does not re-derive is **refused and counted**
(`LaunchpadDiagnostics::curve_not_derived`), never written. That is also
what makes reading LaunchLab's mint out of an account META safe: the event
names neither mint, so the index is proposed and then *proved*. Live over
400 slots the counter is 0.

One thing that is NOT program derived: a **Meteora DBC config**, because
`create_config` takes it as a signer keypair. Nothing may use the off-curve
test to decide whether an account belongs to a venue — that rule holds for
pools and curves only, and a unit test pins it.

### Front ends are attribution, never venues

bags.fm, StonkFun, BONK.fun / LetsBonk, Jupiter Studio and the rest are not
programs. They are **configurations** of one of these three
(docs/launchpads-research.md §4.2), and the only thing on chain that names
one is the config account's fee claimer.

* A DBC launch names its `config`, a LaunchLab launch its `platform_config`
  — **not** the `config` field of `PoolCreateEvent`, which is the GLOBAL
  config and identical for every launch. Using that one would attribute the
  whole venue to a single front end.
* Both go into `launchpad_tokens.launch_config_id`, as 32 big-endian bytes
  in the `UInt256` column the schema shares with EVM. Read it back with
  `reverse(reinterpretAsFixedString(launch_config_id))` — the `reverse()`
  is not decoration, because `reinterpretAsFixedString` writes the
  integer's little-endian memory.
* `sol_launchpad_configs` maps a DBC config to its `fee_claimer`, filled
  from the venue's own `EvtCreateConfig(V2)`. An operator gives that
  address a NAME in `launchpad_frontends`, and
  `sol_launchpad_attribution_v` is the join.

A launch with no listed front end still reads perfectly: attribution is
never a precondition, and front-end volume PARTITIONS a venue's rather than
adding to it.

### The pump.fun disagreement, and what it turned out to be

Phase 2 reported 98.2% agreement on the pump.fun curve and left the rest
unexplained. Measured per INSTRUCTION rather than per row — the interesting
cases produced no row at all and so never reached the statistic — it was
**two** things, and neither was fee accounting:

1. **Non-SOL quote mints.** pump.fun now runs curves quoted in USDC, BONK
   and other pump tokens. `TradeEvent.sol_amount` is then **0** and the real
   leg is `quote_mint` / `quote_amount`, which sit behind two
   variable-length Borsh fields (`ix_name: String`, `shareholders:
   Vec<Shareholder>`, 34 bytes an element) — exactly the fields the decoder
   refused to parse past. 4.3–11.5% of curve instructions.
2. **Both sides of the trade are PDAs.** When a bot VAULT buys, the taker is
   off the ed25519 curve too, so `resolve_pool`'s curve test cannot break a
   swap's local symmetry and the row was dropped `unclassified`. 9.7–13.3%.

Both are fixed. `PumpFunTrade` walks the tail and `quote_leg()` returns WSOL
and `sol_amount` on a SOL curve or the tail's own mint and amount otherwise;
the tail is OPTIONAL, because pump.fun appends fields and an older event is
simply shorter. The two-sided path now proposes both readings and lets the
venue's event pick, as the native-leg path always did — and still refuses
the row when no event settles it, counting it in the new
`Diagnostics::ambiguous_pool`.

**Result, live: 100.00% agreement, zero disagreements.** Per curve
instruction over 150 slots: 911 agreed, 4 dropped (0.32%, all ambiguous
native legs), 351 genuinely not trades (`extend_account`,
`close_user_volume_accumulator`, `claim_cashback_v2`, `create_v2`).

### What in the launchpads module assumed EVM

Everything found, and what happened to it:

| Assumption | Where | Status |
|---|---|---|
| Identity columns are 20-byte addresses padded by `SerId32` | `launchpads/models.rs` row structs | **Rust only.** The COLUMNS were always `FixedString(32)`. `svm/launchpads.rs` has twin structs with `Pubkey` ids and identical column NAMES; `the_solana_rows_have_the_evm_columns` asserts the two lists are equal |
| `Family` is a closed enum of EVM decoders | `launchpads/models.rs` | Left alone. `SolFamily` is its Solana counterpart and a test keeps the two vocabularies disjoint, so no EVM test moves |
| `tx_id` is a 32-byte hash, read with `tx_hash_of` | `LaunchpadRows::attach_transactions` | Not used on Solana: the fee payer is in the transaction the decoder already has, so nothing needs a second lookup |
| Holders come from `erc20_transfers` | `launchpad_token_holders_v` (0032) | **The one real gap.** That view pads a 20-byte address up to 32, so a pubkey finds no row — the right failure, but no answer. `sol_token_balances` + `sol_launchpad_token_holders_v` (0042) answer it from `account_activity` post balances, which are validator metadata, so a balance is READ rather than accumulated |
| A `pool_kind` of `pool_address` means "strip 12 bytes" | `launchpad_graduations` | Solana rows always say `pool_id`: a pubkey is a native 32-byte id and a reader must never strip anything |
| `launch_config_id` is a number | `launchpad_tokens` | Holds a 32-byte account big-endian. Documented above, pinned by a ClickHouse test |
| Printing an id as `0x…` | every view | Already handled: the views take the format from `chains.family`, and this module registers `(1399811149, 'solana', 'svm')` |

The only change made inside `src/launchpads/**` is that `decode::sanitize`
is now `pub`, so the Solana decoder applies the same hostile-text rule to
the same columns instead of growing a second copy. No EVM behaviour moves
and its tests are untouched.

### What is still missing, and why

* **A DBC or LaunchLab graduation names no destination pool.** Both
  programs' migration instructions emit *nothing at all* — verified against
  DBC's source at the deployed commit and LaunchLab's own on-chain IDL. The
  row is written with `pool_id` zero, which the schema already means as "not
  known", rather than guessing at an account meta index. pump.fun's
  `CompletePumpAmmMigrationEvent` does name its pool, so the join is proven
  there.
* **DBC launches carry no name, symbol or URI.** They live in a Metaplex
  metadata account, and this pipeline serves no account reads. Empty, never
  guessed.
* **A DBC trade's `trader` is the fee payer.** Its event names no user at
  all, so the row puts the fee payer in both `trader` and `caller` and says
  so. pump.fun names the beneficiary and is exact.
* **pump.fun's graduation threshold** is a curve parameter in the program,
  not a field of any event, so `graduation_threshold` is 0 for that family.
  LaunchLab states its own; DBC's is in the config.
* **Nothing is wired to the pipeline yet** — same as the rest of this
  module (docs/solana-research.md §11.5). `svm::decode` produces the rows;
  `svm::SHARED_BASE_TABLES` and `launchpads::INSERT_ORDER` say how to write
  them.
