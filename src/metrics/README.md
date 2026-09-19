# metrics

`--metrics-addr <ip:port>` (default off) serves:

| Endpoint | Answer |
|---|---|
| `GET /metrics` | Prometheus text format 0.0.4 |
| `GET /healthz` | `200 ok` while the process is alive |
| `GET /readyz` | `200 ready` when ALL of: `set_ready(true)` was called; the most recent flush attempt did not fail; no flush has been running (retrying) for longer than the staleness limit; the last successful flush or head poll is younger than the staleness limit. Otherwise `503` and a one-line reason |

Hand-rolled: atomics behind a clonable `Metrics` handle and a ~250 line
HTTP/1.1 responder on a tokio `TcpListener`. No new dependencies, no
metrics-crate types outside this module. One request per connection
(`Connection: close`), request head limited to 8 KiB and 5 s, at most 64
concurrent connections, bodies never read.

**`/readyz` is a readiness probe, not a liveness probe.** It answers "is
this indexer serving fresh data": take a not-ready indexer out of a
dashboard or load balancer, do NOT restart the process on it. A ClickHouse
outage makes every indexer not ready and a restart loop would only add
load; a flush that fails for good already ends the process by itself. Use
`/healthz` for liveness.

## Metric reference

Every series is prefixed `evm_indexer_` and carries the constant label
`chain="<chain id>"`. Series marked *(when known)* are absent until the
first value is recorded.

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `build_info` | gauge | `version`, `commit` | Always 1 |
| `start_time_seconds` | gauge | | Process start, unix time |
| `ready` | gauge | | 1 when `/readyz` answers 200 |
| `head_block` | gauge | | Chain head reported by the source *(when known)* |
| `indexed_block` | gauge | | Highest block durably stored *(when known)* |
| `lag_blocks` | gauge | | `head_block - indexed_block`, never negative |
| `head_timestamp_seconds` | gauge | | Timestamp of the head block *(when known)* |
| `indexed_timestamp_seconds` | gauge | | Timestamp of the indexed block *(when known)* |
| `lag_seconds` | gauge | | Head timestamp (wall clock when unknown) minus indexed timestamp |
| `rows_inserted_total` | counter | `table` | Rows durably inserted; `rate()` gives rows/s |
| `flushes_total` | counter | `result` = `ok` \| `error` | Flushes (one multi-table batch each) |
| `flush_duration_seconds` | histogram | `le` | Flush latency, retries included; 10 ms to 300 s |
| `last_flush_rows` | gauge | | Rows in the most recent flush |
| `last_successful_flush_timestamp_seconds` | gauge | | Unix time of the last successful flush |
| `flush_retries_total` | counter | `table` | Inserts that failed and were retried |
| `channel_len`, `channel_capacity` | gauge | | Fill of the transformer to writer channel |
| `stream_errors_total` | counter | | Sync passes that failed and were retried |
| `reorgs_total` | counter | | Reorganizations detected |
| `reorg_blocks_total` | counter | | Sum of the depths of all reorganizations |
| `reorg_last_depth` | gauge | | Depth of the most recent reorganization |
| `purge_duration_seconds` | histogram | `le` | `purge_range` latency (rollback, gap healing); 50 ms to 900 s |
| `purged_blocks_total` | counter | | Blocks removed by `purge_range` |
| `resolver_queue_depth` | gauge | `worker` = `tokens` \| `pools` \| `venues` | Addresses waiting for the background resolver |
| `resolver_resolved_total` | counter | `worker` | Resolved WITH metadata. Negatives are NOT included (`resolved + negative` = addresses answered) |
| `resolver_negative_total` | counter | `worker` | Resolved to nothing (reverts, garbage) |
| `resolver_codeless_total` | counter | `worker` | No contract code at the address (asked again later, no row) |
| `resolver_dropped_total` | counter | `worker` | Discoveries dropped on a full queue (healed by the backfill) |
| `resolver_inserted_total` | counter | `worker` | Rows durably inserted by the worker |
| `resolver_insert_failures_total` | counter | `worker` | Batches the worker could not store after its retries. **Growing = rows are being lost until the backfill finds them again** |
| `resolver_rpc_failures_total` | counter | `worker` | Addresses the RPC could not be asked about (retried later) |
| `resolver_backfill_found_total` | counter | `worker` | Addresses the database backfill reported missing: what the live path lost and the backfill healed |
| `resolver_backfill_failures_total` | counter | `worker` | Backfill queries that failed. **Growing = nothing heals** |
| `resolver_unconfirmed_total` | counter | `worker` | Answers of a public endpoint no second provider confirmed (nothing stored) |
| `resolver_blank_rechecked_total`, `resolver_blank_healed_total` | counter | `worker` | Blank rows verified again / replaced by real metadata |
| `resolver_cache_hits_total`, `resolver_cache_misses_total` | counter | `worker` | Resolver cache (pools / venues: already known vs queued) |
| `resolver_breaker_open` | gauge | `worker` | 1 when every RPC endpoint's breaker is open |
| `resolver_endpoints_total`, `resolver_endpoints_healthy`, `resolver_endpoints_distrusted` | gauge | `worker` | RPC endpoints configured or discovered / healthy / caught contradicting the others (reported by `tokens`; the workers share one RPC backend) |

A worker that is off (`--no-dex`, `--no-predictions`, `--rpc none`) has no
series at all.

### Solana only

`indexer run --chain solana` publishes five more series. They are absent
on every other chain, rather than zero there.

| Metric | Type | Meaning |
|---|---|---|
| `hypersync_queries_last_minute` | gauge | Metered HyperSync queries sent in the last 60 s. The free Solana budget is **30 per 60 s per endpoint** and the follower caps itself at 25, so this is the number that says how much room is left |
| `hypersync_queries_total` | counter | Metered queries since the process started. `GET /height` is free and unmetered and is NOT counted |
| `hypersync_ratelimit_requests_left` | gauge | What the server's own `x-ratelimit-*` headers last said: `remaining / cost`, because `remaining` counts budget units and not requests *(when known)* |
| `solana_swaps_total` | counter | Swaps decoded and handed to the writer. `rate()` gives swaps/s |
| `solana_skipped_slots_total` | counter | Slots inside served windows that produced no block. **Normal on Solana**; worth watching because every rows-per-day estimate assumes it stays near zero |

**Read `lag_seconds`, not `lag_blocks`, when Solana shares a dashboard with
EVM chains.** A Solana slot is 0.27 s and an Ethereum block is 12 s, so one
`lag_blocks` panel across both families compares numbers that do not mean
the same thing, and it would be read wrong on the first bad day. On Solana
the loop does not publish a head timestamp, so `lag_seconds` is
`now() - block_time` of the last committed slot: the honest end-to-end
figure, Envio's own 10-13 s ingest lag included.

There is no `solana_parent_mismatch_total`: a continuity break is not a
counter to watch but a **fatal stop** (see the Solana section of the main
README), so the signal is the process exiting and `ready` going to 0.

Cache hit rate:

```promql
rate(evm_indexer_resolver_cache_hits_total[5m])
  / (rate(evm_indexer_resolver_cache_hits_total[5m])
     + rate(evm_indexer_resolver_cache_misses_total[5m]))
```

## Alerts

```yaml
groups:
  - name: evm-indexer
    rules:
      # Falling behind the chain, or stalled: the lag grows while nothing
      # is being committed. (During a historical sync the lag is large but
      # shrinking, which does not fire.)
      - alert: IndexerLagging
        expr: |
          evm_indexer_lag_seconds > 300
            and delta(evm_indexer_indexed_block[10m]) <= 0
        for: 5m
        labels: { severity: page }
        annotations:
          summary: "chain {{ $labels.chain }}: {{ $value }}s behind and not advancing"

      # A failed flush is fatal for the process (it exits and resumes), so
      # a single one deserves a look; retries are the early warning.
      - alert: IndexerFlushFailing
        expr: |
          increase(evm_indexer_flushes_total{result="error"}[15m]) > 0
            or sum by (chain) (increase(evm_indexer_flush_retries_total[15m])) > 5
            or changes(evm_indexer_start_time_seconds[30m]) > 2
        labels: { severity: page }
        annotations:
          summary: "chain {{ $labels.chain }}: ClickHouse inserts are failing"

      # Deep reorg: beyond what the chain's finality should allow. Tune
      # the depth per chain; `--max-reorg-depth` (512) is fatal.
      - alert: IndexerDeepReorg
        expr: |
          evm_indexer_reorg_last_depth > 32
            and increase(evm_indexer_reorgs_total[10m]) > 0
        labels: { severity: warn }
        annotations:
          summary: "chain {{ $labels.chain }}: reorg of {{ $value }} blocks"
```

## Wiring (for the pipeline)

```rust
let metrics = match args.metrics_addr {
    Some(addr) => {
        let metrics = Metrics::new(chain_id, Duration::from_secs(120));
        let server = metrics::bind(addr, metrics.clone()).await?; // fails fast
        tokio::spawn(server.run(shutdown_signal()));
        metrics
    }
    None => Metrics::disabled(),
};
```

- `set_head` on **every** successful head poll (it is the idle-time sign
  of life for `/readyz`), `set_indexed_height` / `set_indexed_timestamp`
  after each successful flush, `flush_observed` around `Sink::store`
  (failures too), `rows_inserted` per table inside the sink,
  `flush_retry(table)` in the insert retry loop.
- `table` labels are `&'static str`: pass table-name literals.
- `flush_started` right before a flush, `flush_observed` right after:
  the pair is what lets `/readyz` see a flush that is stuck retrying.
- `set_token_stats` / `set_pool_stats` / `set_venue_stats` take
  `WorkerStatsSnapshot`; `pipeline::workers` maps the workers' own stats
  into it every 5 s. `resolved` there EXCLUDES negatives:
  `tokens::TokenWorkerStats::resolved` includes them, so the mapping
  subtracts (`token_stats_snapshot`), otherwise they are counted twice.
