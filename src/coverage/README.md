# The coverage floor

> **What this is** — the date each chain's data begins at. It is chosen once, on that chain's first start, and never moves by accident afterwards.
> **What tables** — `chain_coverage` (migration 0008) and the `coverage_v` view; nothing else writes them.
> **Where the queries are** — `SELECT * FROM coverage_v`, or run `indexer verify`, which prints the same sentence as its first line. The control panel shows it per chain.
> **Why you care** — "all-time" in any dashboard built on this database means "since the floor", and the floor differs per chain.
> **Binding design** — `docs/design.md` section 16.

*Binding design: `docs/design.md` section 16.*

## The promise

This indexer does not claim to hold all of history. It claims something
narrower and much more useful:

> **gap-free and consistent from a known date to now, everything kept.**

The **coverage floor** is that known date. Every chain has one, it is
written down next to the chain's data, and `indexer verify`, the fleet's
status line and the control panel all print the same sentence about it:

```
Coverage: gap-free from 2024-09-19 (block 20779400) to 2025-09-19 (block 23400512).
```

## Where it comes from

On a chain's **first** start, and only then:

| What was given | Floor |
|---|---|
| nothing, on an EVM chain | a year back, resolved to a block |
| nothing, on Solana | the head |
| `--start-block N` | block `N` |
| `--start-date YYYY-MM-DD` | the first block at or after midnight UTC of that day |
| `--new-blocks-only` | the head, on either family |

`--start-block` and `--start-date` are mutually exclusive, at the command
line and through the environment.

**One case beats all of them: a database that already has blocks.** A
deployment that was indexing before floors existed holds, say, three years
of Ethereum. Computing "a year back" for it would make it promise less than
it is sitting on and - worse - would stop the gap heal from ever looking
below that line again. So such a chain's floor is its oldest stored block,
with the reason `existing`, and `indexer verify` is what says whether the
window below is actually gap-free.

A **year** is the default because it is what makes "all-time", "last 12
months" and every year-on-year comparison a real answer rather than an
artefact of the day somebody happened to start the indexer. It is measured
from the newest block the source HAS, or from now, whichever is earlier: a
year is a promise about the data, and an archive that is behind would
otherwise quietly give less than a year - or, if it is more than a year
behind, nothing at all.

**Solana starts at the head.** Envio serves Solana history from 2026-01-03
and the free tier is slow, so a year of it is not something anybody should
discover by accident. `--start-date` is refused there rather than rounded:
a slot carries no timestamp, so there is nothing to resolve a date against,
and a guess would become a floor that can never be moved later.

## Where it goes, and why it stays

The floor is written to `chain_coverage` (migration `0008`) the first time a
chain is indexed. After that it is a **fact about the data**, not a setting:

* A restart keeps it.
* A different `--start-block` or `--start-date` keeps it, and logs a warning
  saying so and what to run instead.
* The control panel cannot touch it: `start-block`, `start-date`,
  `end-block` and `new-blocks-only` are all on the list of options a web
  page may never change, and a test walks every option of `indexer run` to
  keep it that way.
* Moving it **later** is refused everywhere. Data is never dropped, so a
  higher floor would be a claim the stored rows contradict.
* Moving it **earlier** is `indexer backfill --start-block N` (or
  `--start-date D`). It lowers the floor only after `verify` says every
  block from `N` to the old floor is stored and gap-free; otherwise the
  floor stays and the command says how many are missing. Note that a
  backfill re-decodes stored logs rather than fetching new ones, so this
  moves the floor over blocks this database already has.

Two independent mechanisms enforce "never later", because one of them would
not be enough. The code reads the stored floor before it writes, which is
what produces the warning; and the row's `_version` is `MAX - block`, so the
ReplacingMergeTree keeps the row with the **lowest** block whatever order
the inserts land in. ClickHouse has no read-your-writes (design section 2),
so the read alone could not survive two processes starting at once.

## What "all-time" means to a reader

**Since the floor.** Every total, every "all-time volume", every leaderboard
covers the window this chain actually has. `coverage_v` is where a dashboard
should read the window from, and `indexer verify` prints it.

Two consequences worth stating to users of the data:

* A **launchpad token launched before the floor** keeps its DEX data but has
  no launch attribution: the launch event is below the window. Documented,
  not fixed.
* A **prediction market created before the floor** would have no question
  and no outcomes, and its open interest could go negative. That one IS
  fixed, by the registry-only history pass in `src/predictions`
  (`indexer backfill --module predictions --registry-only`).

## The files

| File | What it answers |
|---|---|
| `date.rs` | What day is `2024-03-01` in unix seconds, and what day is this timestamp? UTC calendar arithmetic, written out rather than pulled in as a crate. |
| `resolve.rs` | Which block is the first one at or after that moment? A binary search over block timestamps through the existing `CanonicalChain` seam - about `log2(height)` header requests, once in a chain's life. |
| `store.rs` | What is this chain's floor, may I change it, and how is it printed? |
| `mod.rs` | Given the flags and what is stored, what happens. A pure function with the database and the source on either side of it. |

This module owns one table and no chain data. It is a concept module like
`src/reorg/`, not a layer: nothing here decodes, stores or aggregates a
dataset, and it must never grow a `models.rs` of chain rows.
