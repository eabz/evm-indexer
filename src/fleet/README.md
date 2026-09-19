# fleet

> **What this is** — `indexer fleet`: one process indexing many chains, with a supervisor that restarts a failing chain on its own and the control panel on top.
> **What tables** — `fleet_chains` only: which chains to run and their settings. It owns no chain data; every chain writes the same tables `indexer run` writes.
> **Where the queries are** — nowhere here. The read surfaces are the panel, one `/metrics` for the whole process, and the datasets listed in the main [README](../../README.md#what-is-indexed).
> **Read this first** — every chain calls the same `run_with` that `indexer run` calls, so no safety property changes: its own lease, its own epoch, its own tombstones, its own writer.
> **Binding design** — `docs/design.md` section 15.

`indexer fleet`: **one process, many chains** (docs/design.md section 15).
`indexer run` is unchanged and stays the way to index a single chain.

```text
  migrations (ONCE, before any chain starts)
    -> fleet_chains, read ONCE: which chains, which settings
       -> one tokio task per chain, each calling the SAME
          pipeline::run_with / pipeline::solana::run_with that
          `indexer run` calls
            -> per-chain cancellation through Runtime::shutdown
            -> restart with exponential backoff, capped at 5 minutes
    -> one /metrics for every chain, each series labelled `chain`
    -> the control panel (src/admin), off unless ADMIN_PASSWORD is set
```

| File | What lives there |
|---|---|
| `mod.rs` | `run(FleetConfig)`, the production `ChainRunner` (which pipeline to start) and the ClickHouse `DesiredStore` |
| `supervisor.rs` | one task per chain: start, stop, restart, backoff, "running elsewhere", and the panel's whole vocabulary |
| `chains.rs` | the `fleet_chains` table (migration 0007) and the heartbeat read of chains OTHER processes index |
| `status.rs` | the live numbers per chain, read from what `metrics` already records |
| `budgets.rs` | the limits a provider puts on a TOKEN, shared by every chain |
| `metrics.rs` | the merged Prometheus exposition of every chain |
| `fixtures.rs`, `tests.rs` | a fake chain runner and an in-memory store, so all of the above is tested without HyperSync or ClickHouse |
| `integration_tests.rs` | the `fleet_chains` round trip on a real server (ignored) |

## What sharing a process does NOT change

Every chain takes its own lease, keeps its own epoch, writes its own
tombstones and flushes through its own writer, because every chain task
calls the same entry point the single-chain command calls. Nothing in this
module reaches into a running pipeline; it can only start one, stop one, or
change the options the next one starts with.

**Stopping a chain is the graceful path `ctrl-c` already takes.** The
pipeline's `Runtime::shutdown` was already a future: `indexer run` gives it
the process signal, the fleet gives it a per-chain handle. The chain
therefore stops the way it always has - final flush, workers down, lease
released - and the other chains never notice.

**Nothing here removes data.** There is no purge, no drop, no delete and no
schema change in fleet mode, by construction.

## Desired state, and who is the authority

`fleet_chains` (`chain`, `desired`, `settings` JSON, `_version`;
ReplacingMergeTree, insert only, no seed rows) is **desired state**, read
ONCE at start. ClickHouse has no read-your-writes, so a supervisor that
polled the table would keep changing its mind about a row it wrote a second
ago. Every command is therefore applied **in memory first** and written
afterwards; a write that fails is a warning, and the only thing it costs is
the next start's memory of the change.

A chain leaves the fleet by getting `desired = 'stopped'`. There is no
"remove", because insert-only means insert-only.

Adding a chain needs nothing but its id: an absent setting is the `run`
default, so `{}` is the whole configuration of a normal chain.

## Settings: a short allow-list, and one validator

**Which settings** the panel may change is an allow-list:
`configs::fleet::CHAIN_SETTINGS`, and it holds only how a chain behaves
while it runs - how far behind the head to stay, how deep a rollback may
go, how big and how frequent the writes are, which decoders run.

Endpoints and credentials are not on it, and that is structural rather than
careful. The HyperSync token is process-wide and is attached to whatever url
a chain names, so a panel that could set the endpoint could send the token
to any host, reach the indexer host's private network, and feed the indexer
fabricated blocks (security review MAJOR 4). The start block and the start date are not on it
either: they fix the coverage floor, which design section 16 decides once,
on a chain's first start. Each chain's card shows that floor and how far it
is gap-free, read from `coverage_v` in one query for the whole fleet. `configs::fleet::NOT_PANEL_EDITABLE` names every
excluded option with its reason, and a test walks the whole CLI and fails if
a new flag is in neither list.

**How a value is validated** is the second half. A setting typed into a web
page is untrusted input, and the obvious mistake is a small parser next to
the HTTP handler that accepts "about the same" values as the command line.
Instead, `apply_chain_settings` turns the settings map into **the command
line `indexer run` would have been given** and hands it to clap - the same
`IndexerArgs`, the same `value_parser`s. Every number, every boolean
spelling and every unknown option is judged by the code that judges the
command line.

One deliberate difference from `indexer run`: the per-chain parse has clap's
ENVIRONMENT fallbacks removed. A fleet is configured once at start, so a
stray `START_BLOCK=77` in a compose file must not silently apply to every
chain - including chains added later in the panel.

## States

| State | What it means |
|---|---|
| `starting` | connecting, taking the lease, reading the resume point |
| `backfilling` | far behind the chain head and catching up |
| `following` | at the head |
| `stopped` | the owner stopped it, or it reached `--end-block` |
| `failed` | it stopped with an error and is being restarted |
| `running_elsewhere` | another process holds this chain's lease |

`running_elsewhere` is a **state, not a failure**: the supervisor waits a
fixed, unhurried 30 seconds and looks again, instead of backing off towards
five minutes. The moment the other process lets go, this one takes over. A
real failure doubles from 2 seconds up to a 5 minute cap.

Chains that a DIFFERENT process is indexing into the same database show up
in the panel read-only, from the heartbeats of `indexer_instances`
(migration 0005). This process owns no task for them and offers no button.

## Where the numbers come from

Almost nowhere new. `metrics::Metrics` - one handle per chain, the same
handle that feeds `/metrics` - already records the chain head, the stored
head, both timestamps, flush counts and latency, the reorg counters and the
resolver queue depths. `status.rs` reads that snapshot and adds only what no
counter can know: which state the chain is in, the last error and its time,
how often it was restarted, and blocks per second (sampled by ONE task for
the whole fleet, every 5 seconds, from the stored head the pipeline already
publishes). A reorg becomes a panel event because the sampler notices the
counter moved, not because anything new was added to the block path.

The pipeline's only new obligation is `pipeline::status::StatusSink`, two
methods called on a state CHANGE and on a retried failure - a few times an
hour. `indexer run` passes the no-op sink, which costs one branch per call.

## Shared budgets

One HyperSync token serves every chain, and Envio meters the **token**, so a
supervisor that gave each chain its own budget would multiply the allowance
by the number of chains and collect 429s.

- **Solana queries**: one `pipeline::solana::Budget` for the process,
  handed to every Solana chain (`--solana-queries-per-minute`, default 25 of
  the free tier's 30).
- **Memory**: `--fleet-max-inflight-mb` (default 2048) is divided over the
  chains that are meant to be running, so one more chain makes everybody's
  write batches smaller instead of making the process bigger. It only ever
  LOWERS a chain's `--flush-rows`, never raises it.

### What is not shared yet

**The EVM query rate has no shared budget.** There is nothing to share it
through: the EVM path streams through `hypersync_client`'s own
`stream()`, which issues and paces its requests internally, so the process
never sees "a query is about to be sent" and has no seam to hold one back.
Giving the EVM path a budget means replacing the streaming client with
manually paged `get()` calls and re-proving ordering, rollback guards and
throughput against it - a week of work and a risk to the hot path, not a
day. It is deliberately left out; the Solana budget, which the provider
documents and meters visibly, is implemented. In practice EVM HyperSync has
no published per-minute quota, and the fleet's memory cap already bounds
what many chains do to one host.

## Testing

`fixtures.rs` provides a `ChainRunner` that does exactly what a test tells
it (index until cancelled, fail, be held elsewhere, finish) and a
`DesiredStore` in a `Mutex`, in the same spirit as `pipeline::sync_tests`
faking a `BlockSource`. Settings are still judged by the REAL validator, so
nothing about validation is faked.

```sh
cargo test fleet::
# the ClickHouse round trip, against a throwaway server
TEST_DATABASE_URL=http://default@127.0.0.1:8123/scratch \
  cargo test fleet::integration_tests:: -- --ignored --test-threads=1
```
