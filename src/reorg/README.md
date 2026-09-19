# How the indexer handles reorgs

Written for the person who runs the indexer, not for the person who
programs it. The code next to this file (`src/reorg/`) is the "brain": it
decides what to do. It contains no database code and no HyperSync code, so
every decision below is tested in memory, thousands of times, including
"the process dies exactly here".

## What a reorg is

A blockchain does not grow in a straight line. For a few seconds (on some
chains, minutes) two different versions of the newest blocks can exist.
The network then settles on one of them and the other one is thrown away.
That is a **reorganization**, a *reorg*. Its **depth** is how many of the
newest blocks were replaced: usually 1 or 2, rarely more.

For an indexer this is a problem: if we already stored block 100 and the
network later decides that a *different* block 100 is the real one, then
everything we stored for the old block 100 (its transactions, logs, token
transfers, swaps, and its share of every daily total) is now wrong.

Every block names its parent by hash, like a chain of fingerprints. That is
how a reorg is noticed: the new block 101 says "my parent has fingerprint
B", and what we stored as block 100 has fingerprint A.

## The four layers of defence

1. **Stay behind the head (`--confirmations N`).** The indexer simply does
   not store the newest N blocks. A reorg of depth N or less then never
   touches anything stored: nothing to notice, nothing to repair. This is
   the cheapest protection and the only one that costs *freshness*: data is
   N blocks late. Default 0.
2. **Detection.** Every block that is about to be stored is checked against
   the block below it: the one streamed just before, or, at the start of a
   run, the one in the database. HyperSync's own "rollback guard" is checked
   the same way. If the database cannot be asked, the pass fails and is
   retried; the check is never skipped.
3. **Finding the fork point.** Once a mismatch is seen, the indexer asks
   HyperSync for the real recent blocks and compares them with the stored
   ones, going backwards 8 blocks, then 16, then 32 ..., until it finds a
   height where both agree. The block above it is the **fork point**:
   everything from there up has to go. This search gives up after
   `--max-reorg-depth` blocks (default 512) with a clear error instead of
   deleting half the database.
4. **Rollback.** Everything from the fork point up is removed, the daily /
   hourly totals are repaired, and the indexer streams the real blocks from
   the fork point on. There is one routine for this, `purge_range`; it is
   also used to clean up after a crash (see "gap healing").

## What happens, step by step

Nothing is ever deleted from ClickHouse. (Deletes running at the same time
can silently lose one another, and 50+ indexers share the database.) Rows
are *crossed out* instead: a copy of the row marked "deleted" is inserted
(a **tombstone**), and readers, who always read the latest version of a row,
no longer see it.

Totals cannot be crossed out row by row, so they work with generations:
every row carries the chain's **epoch**, a counter that goes up by one with
every rollback. A rollback says "for this chain, from day D on, only count
what belongs to epoch 7 or newer", recomputes those days under epoch 7, and
from then on new rows are stamped 7. Older numbers for those days are still
on disk but no longer counted; days before D are untouched.

The order of a rollback:

0. The writer hands in everything it still holds in memory and confirms it
   can be read back, so the database is complete.
1. Work out the new epoch.
2. Cross out the **checkpoints** (the "I am done up to block X" notes) that
   reach into the range. First, so that a note can never claim more than
   what is really there.
3. Cross out the transactions, logs, transfers, DEX rows ... of the range.
   Repeated until a count confirms none is left. Then work out the first
   day that is affected.
4. Write one line into the `reorgs` table. From this moment the affected
   days only count the new epoch, so for a moment they show too *little*
   (never too much).
5. Recompute the totals of the affected days from what is left.
6. Cross out the **blocks** of the range. Last, on purpose (next section).
7. Forget cached token / pool discoveries from the range, update metrics.

Then streaming resumes at the fork point.

## What is guaranteed after a crash

The power can go at any step. The guarantee: **after the indexer has
started again and caught up, what readers see is exactly what a clean
index of the real chain would show**: the same blocks, the same rows, the
same totals. Nothing has to be remembered between runs to get there:

* The stored *blocks* of the abandoned fork are the evidence. They are
  crossed out last. If a rollback dies anywhere before that, the next run
  sees the same mismatch, finds the same fork point and does the whole
  rollback again under a newer epoch. Doing it twice is harmless; the
  half-finished epoch is simply never counted.
* If it dies *while* crossing out the blocks, some are gone and some are
  not. The ones that are gone are re-downloaded; the survivors are caught
  by the same parent check when streaming reaches them.
* **Gap healing.** A normal write stores the rows of a block first and the
  block itself last. If the process dies in between, rows exist for a block
  that "is not there". Storing that block again would count those rows
  twice in the totals. So on the first pass after every start, each missing
  range is checked for such leftovers and, if there are any, purged with the
  same routine (reason `gap_heal`) before it is downloaded. Leftovers that
  are already crossed out still count as evidence: they are the only trace
  of a heal that died half way.
* A checkpoint never claims a block that is not stored, at any moment.

This is tested by stopping the rollback at every single step (not done at
all, and done half way), restarting or retrying, and comparing the result
with a clean index: 160 combinations, plus 400 random histories (blocks
arrive, the chain reorgs at random depths, writes crash, purges crash, the
process restarts) and 60 runs of four chains sharing one database, half of
them on a simulated database that sometimes does not show what was written
a moment ago. Each test also has a "negative control": take one safety rule
away and the test fails.

## What the operator sees

Log lines (level WARN for a rollback, INFO for a gap heal):

```
Chain 1: REORG detected. Stored block 19000123 has hash 0xaaaa.. but the chain now has 0xbbbb.. there. Rolling back 2 block(s) from block 19000122 (stored head 19000123).
Chain 1: rolled back blocks [19000122, head]: 2 blocks, 431 rows, 1 checkpoints tombstoned, aggregates rebuilt from unix time 1767225600; epoch is now 4 (212ms).
Chain 1: healed gap [18999000, 18999250) left by an interrupted write: 9120 rows tombstoned, ...
```

The `reorgs` table, one line per rollback / heal, never deleted:

```sql
SELECT detected_at, reason, fork_block, old_head, depth, rows_tombstoned, epoch
FROM reorgs WHERE chain = 1 ORDER BY epoch DESC LIMIT 20;
```

Metrics: reorgs total, depth of the last one, purge duration, purged blocks.

Errors that stop the indexer on purpose (it changes nothing before it
stops, and the message says what to do):

* *reorg deeper than `--max-reorg-depth`*: almost always a wrong endpoint
  or chain id. If the chain really reorganized that deep, restart with a
  larger value; the repair is automatic.
* *stored block 0 is not the source's block 0*: the database holds another
  network under this chain id.
* *tombstones not converging*: something else is writing the same chain.

Everything else (HyperSync unreachable, the source still reorganizing while
we look, a query failing in the middle of a rollback) fails the current pass,
which is retried with the usual back-off.

## Choosing `--confirmations` and `--max-reorg-depth`

`--confirmations` trades freshness for calm. With 0 you see blocks
immediately and the indexer repairs the occasional reorg (a fraction of a
second of work, and for that moment today's totals read low). With N you
are N blocks late and reorgs up to depth N never reach the database at all.
Rules of thumb: chains with fast finality or a single sequencer (most L2s):
0 to 2. Ethereum mainnet: 0 if you want live data (reorgs of 1, rarely 2),
2 to 3 for calm. Chains known for deeper reorgs (Polygon PoS historically):
at least 32 if consumers cannot tolerate data changing under them. If a
consumer must never see a number change, there is no substitute for
confirmations: a rollback is correct, but the old value was visible for a
while.

`--max-reorg-depth` is a safety fuse, not a tuning knob. 512 is far beyond
any normal reorg, so reaching it almost always means misconfiguration.
Raise it only when the error appears and you have checked the endpoint.

## Known limits

* **One indexer process per chain.** Any number of chains can share the
  database and they never affect each other (tested), but two processes
  writing the *same* chain would each keep their own epoch and hide each
  other's totals.
* A reorg that does not make the chain *longer* than what is stored is only
  noticed when the next block arrives (seconds), unless the optional tip
  check is switched on by the pipeline.
* A mismatch found while filling an old gap, below other stored blocks,
  only removes the segment that was proven wrong; the blocks above are
  checked when streaming reaches them.
* Blocks below `--start-block` are never examined or removed.
* After a rollback, totals are recomputed from the first affected *day* to
  now. For a reorg at the tip that is today's numbers. For a heal deep in
  history (a crash during a backfill of old blocks) it re-aggregates
  everything since that day: correct, but it can take a while.
* While a rollback runs (normally well under a second) the affected days
  read too low, and between steps 3 and 6 a rolled-back block is visible
  without its transactions.
* Crossed-out rows stay on disk until ClickHouse merges them away. The
  amount is tiny (only reorged rows). Do not run `OPTIMIZE ... FINAL
  CLEANUP` while an indexer is stopped in the middle of a purge: the
  crossed-out leftovers are what tells the next start to finish the job.
* ClickHouse may not show a row for a few milliseconds after writing it.
  Every decision here that depends on a fresh read is either repeated until
  confirmed, made independent of the fresh data, or read twice. The writer
  must still confirm its last write is readable before a rollback starts;
  that part lives in the pipeline.

## For programmers

`mod.rs` has the traits (`CanonicalChain`, `ReorgStore`, `WriterControl`,
`DiscoveryCache`, `ReorgMetrics`) and the error type; `fork.rs` the
fork-point search; `purge.rs` the purge order; `guard.rs` the state machine
and the calling convention (read its module comment first); `model.rs` +
`tests.rs` the in-memory ClickHouse model, the crash matrix and the random
histories. Run `cargo test --lib reorg -- --nocapture` to see how many
scenarios ran.
