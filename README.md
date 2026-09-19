<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>High-performance SQL indexer for EVM-compatible blockchains</strong>
</p>

[![Docker Image Size](https://badgen.net/docker/size/0xeabz/evm-indexer/main?icon=docker&label=image%20size)](https://hub.docker.com/r/0xeabz/evm-indexer)
![build](https://github.com/eabz/evm-indexer/actions/workflows/build.yml/badge.svg)

An indexer that streams blockchain data from [Envio HyperSync](https://docs.envio.dev/docs/HyperSync/overview) and stores it in [ClickHouse](https://clickhouse.com/) for analysis. It contains no chain-specific logic: any network served by HyperSync can be indexed by changing the chain ID, and many chains share one database (one indexer process per chain).

## Contents

- [Features](#features)
- [Architecture](#architecture)
- [Requirements](#requirements)
- [Quick start](#quick-start)
- [Commands and schema migrations](#commands-and-schema-migrations)
- [Configuration](#configuration)
- [Token metadata and RPC endpoints](#token-metadata-and-rpc-endpoints)
- [Database schema](#database-schema)
- [Querying the data](#querying-the-data)
- [Resume and gap healing](#resume-and-gap-healing)
- [Reorgs](#reorgs)
- [DEX analytics](#dex-analytics)
- [Prediction markets and perps](#prediction-markets-and-perps)
- [Solana](#solana)
- [Metrics and health checks](#metrics-and-health-checks)
- [Performance tuning](#performance-tuning)
- [Upgrading from 2.x](#upgrading-from-2x)
- [Development](#development)

## Features

- **Blockchain primitives**: blocks, transactions, logs and withdrawals, plus a `contracts` view of directly deployed contracts
- **Token transfers**: ERC20, ERC721 and ERC1155, with per-account history tables
- **Token metadata**: name, symbol and decimals resolved in the background over JSON-RPC, never on the commit path, with automatic public-endpoint discovery and failover
- **DEX analytics, on by default**: pools, swaps and liquidity decoded by event family (Uniswap V2 / V3 / V4, Solidly, Balancer V2, Curve and their forks), with candles, volume and USD views
- **Automatic reorg repair**: orphaned blocks are rolled back and re-indexed without ever issuing a `DELETE`; every rollback is recorded in an audit table
- **Built for many chains in one database**: 50+ chains can share one ClickHouse database, one indexer process per chain, with no locks or coordination between the processes
- **Compact binary storage**: hashes and addresses as raw bytes, amounts as exact `UInt256`, lookup tables for the common access patterns (by hash, by address, by account)
- **Embedded, versioned schema migrations**: the binary creates the database and its tables itself
- **Self-healing resume**: gaps are detected with SQL and re-indexed; restarting is always safe
- **Prometheus metrics** and `/healthz`, `/readyz` probes
- **Chain agnostic**: works with every [HyperSync supported network](https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks)

## Architecture

```mermaid
flowchart LR
    HS[("Envio HyperSync")] -->|"blocks, transactions, logs"| S["Stream"]
    S --> T["Transform<br/>transfers + DEX decoding"]
    T --> W["Batched writer"]
    W -->|"inserts, blocks last"| CH[("ClickHouse<br/>one database, all chains")]
    CH -.->|"gaps, stored hashes<br/>(fork-point search)"| S
    T -.->|"new tokens / pools<br/>(non-blocking)"| BG["Background resolvers"]
    CH -.->|"tokens / pools still<br/>missing metadata"| BG
    BG -->|"tokens, dex_pools"| CH
    BG <-.->|"eth_call"| RPC["JSON-RPC<br/>(own and/or public)"]
    BG <-.->|"metadata cache"| C[("Redis / Dragonfly<br/>optional")]
```

1. **Stream** - opens a HyperSync stream for every block range that is still missing and, once caught up, keeps following the chain head (when `--end-block` is `0`). It checks parent-hash continuity and rolls back on a reorg (see [Reorgs](#reorgs)).
2. **Transform** - converts the HyperSync responses into database rows and decodes ERC20 / ERC721 / ERC1155 transfers and DEX events from the logs. Pure functions, no I/O.
3. **Batched writer** - buffers rows per table and flushes them to ClickHouse when `--flush-rows` rows are buffered or `--flush-interval-ms` has elapsed, whichever comes first. Within a flush the `blocks` rows are written last, so a block only exists in the database once all of its data does.
4. **Background resolvers** - token metadata (name / symbol / decimals) and the tokens of DEX pools first seen mid-history are resolved over JSON-RPC in the background and written to `tokens` / `dex_pools`. They never block or fail a flush: an RPC outage only delays metadata, and a periodic query over the database finds whatever is still missing.

## Requirements

- [ClickHouse](https://clickhouse.com/) 25.x. The bundled Compose file and CI use the 25.8 LTS line
- An [Envio HyperSync API token](https://docs.envio.dev/docs/HyperSync/api-tokens)
- *Optional:* your own JSON-RPC endpoint for the chain. Without one, public endpoints are discovered automatically (see [Token metadata and RPC endpoints](#token-metadata-and-rpc-endpoints))
- *Optional:* [Redis](https://redis.io/) or [Dragonfly](https://www.dragonflydb.io/) to cache token metadata across restarts
- [Rust](https://www.rust-lang.org/tools/install) (stable) when building from source, or [Docker](https://docs.docker.com/get-docker/) with Compose v2.24+

## Quick start

### Using Docker Compose (recommended)

1. Clone the repository:
```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Create your configuration and set `ENVIO_API_TOKEN` (required):
```bash
cp .env.example .env
```

3. Start the services:
```bash
docker compose up -d
```

This starts:
- ClickHouse on `localhost:8123` (HTTP) and `localhost:9000` (native), bound to `127.0.0.1` only. Nothing is mounted into it: the indexer creates the database and all tables itself on first start
- Dragonfly as the token metadata cache
- The indexer for the chain configured in `.env`, with metrics on `http://127.0.0.1:9101/metrics` (change the host port with `METRICS_PORT`)

4. Monitor logs:
```bash
docker compose logs -f indexer
```

Inside Compose, `DATABASE_URL`, `REDIS_URL` and `METRICS_ADDR` are always set by the Compose file; everything else (`CHAIN_ID`, `START_BLOCK`, `RPC_URL`, ...) is taken from `.env`.

### Several chains, one database

All tables carry a `chain` column and the processes need no coordination, so adding a chain means adding one more indexer process that points at the same `DATABASE_URL`. In [`docker-compose.yml`](docker-compose.yml) the shared settings live in the `x-indexer` block; the commented-out `indexer-base` service shows a second chain:

```yaml
  indexer-base:
    <<: *indexer
    container_name: evm-indexer-base
    environment:
      <<: *indexer-environment
      CHAIN_ID: 8453
      START_BLOCK: 0
    ports:
      - "127.0.0.1:9102:9090" # its own metrics port on the host
```

Values under `environment` win over `.env`, so anything chain specific that your `.env` sets for the first chain (`START_BLOCK`, `END_BLOCK`, `CONFIRMATIONS`, `RPC_URL`, `HYPERSYNC_URL`) has to be set again for every additional service.

### Local development

1. Start ClickHouse, for example the bundled one (published on `127.0.0.1:8123`):
```bash
docker compose up -d clickhouse
```

2. Build the program (`migrations/*.sql` are embedded into the binary at compile time):
```bash
cargo build --release
```

3. Run the indexer. It creates the database named in the URL and applies the schema migrations before it starts indexing:
```bash
./target/release/indexer \
  --chain 1 \
  --database http://indexer:indexer@localhost:8123/indexer \
  --hypersync-token <your-envio-api-token> \
  --rpc https://your-rpc-endpoint.example,auto \
  --redis redis://localhost:6379 \
  --metrics-addr 127.0.0.1:9090 \
  --start-block 0
```

`--rpc`, `--redis` and `--metrics-addr` are optional, see [Configuration](#configuration). The bundled Dragonfly is not published on the host; for `--redis` outside of Compose use any local Redis, or leave it out (in-memory cache).

## Commands and schema migrations

| Command | What it does |
|---------|--------------|
| `indexer run [OPTIONS]` | Index a chain. Applies pending schema migrations first (unless `--no-migrate`). `indexer [OPTIONS]` without a subcommand is the same thing |
| `indexer migrate --database <url> [--dry-run]` | Create the database if it is missing, apply pending migrations and exit. `--dry-run` only lists what is pending and creates nothing |
| `indexer verify --database <url> [--chain N] [--start-block A] [--end-block B]` | Read-only consistency check of what is stored for a chain: missing blocks (gaps), rows without their block (left by an interrupted write; the next `indexer run` purges them), and checkpoints that claim missing blocks. Prints a report; exit status `0` = consistent, `1` = problems found |
| `indexer backfill --module dex\|predictions --database <url> [--chain N] [--from-block A] [--to-block B]` | Decode a module's rows again **from the stored `logs`** (no re-sync, no HyperSync traffic), e.g. after a decoder fix or a new event family. Compares first and writes nothing when the stored rows already match; otherwise the module's rows of the affected block range are replaced and every aggregate of the chain is rebuilt under a new epoch, so nothing is counted twice. Safe to run while `indexer run` is live on the same chain |

The schema lives in `migrations/NNNN_name.sql` and is **compiled into the binary**; the container image needs no SQL files and ClickHouse needs no init scripts.

- Applied migrations are recorded in `schema_migrations (version, name, checksum, applied_at)`. Each start applies whatever is pending, in order.
- The indexer **refuses to start** when the checksum of an already applied migration differs from the one embedded in the binary, or when the database has a newer migration than the binary knows (an older binary against a newer schema). Never edit an applied migration; add a new one.
- **Before the first release: databases created from a pre-release build must be recreated.** The versioned `migrations/` set is new and was still being corrected in place while it was written, so a database created by an earlier build of it carries different checksums and the indexer refuses to start against it, by design and with no `ALTER` path. Drop the database and let `indexer migrate` (or `indexer run`) create it again. Do not force the checksum guard past this: some of the corrections changed what a column MEANS - `sol_token_balances` went from a latest-value projection to an append log with a flush-clock `_version` - and rows written under the old meaning would permanently outrank every new one. From the first released version on, this never happens: an applied migration is never edited again.
- Several indexer processes may start at the same time against an empty database; they converge on one schema.
- The database name comes from the URL, nothing is hard coded.
- To control when the schema changes (for example one deploy step in front of 50 indexer processes), run `indexer migrate` once and start the indexers with `--no-migrate`.

## Configuration

Every CLI flag can also be set through the environment variable listed next to it. CLI flags take precedence. A blank variable (`VAR=`) counts as unset; boolean variables accept `true` / `false`, `1` / `0`, `yes` / `no`, `on` / `off`.

Options of `indexer run`:

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--chain` | `CHAIN_ID` | `1` | Chain ID to index. One process per chain: a second `indexer run` on the same chain and database refuses to start (any number of chains can share a database) |
| `--database` | `DATABASE_URL` | *required* | ClickHouse HTTP endpoint: `http://user:pass@host:port/db` (use `https://` for TLS). Always include the port (usually `8123`). The database is created when missing |
| `--hypersync-url` | `HYPERSYNC_URL` | derived from the chain ID | HyperSync endpoint. Only needed to override the default endpoint for the chain |
| `--hypersync-token` | `ENVIO_API_TOKEN` | *required* | [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens) |
| `--rpc` | `RPC_URL` | `auto` | Comma-separated JSON-RPC endpoints for token / pool metadata `eth_call`s. `auto` = discover public endpoints, `none` = disable. See [Token metadata and RPC endpoints](#token-metadata-and-rpc-endpoints) |
| `--redis` | `REDIS_URL` | *none* | Redis or Dragonfly URL for the token metadata cache. Without it an in-memory cache is used |
| `--start-block` | `START_BLOCK` | `0` | Block number to start syncing from |
| `--end-block` | `END_BLOCK` | `0` | Block to stop at, **exclusive**: blocks `--start-block` up to `--end-block - 1` are indexed and the process exits. `0` = follow the chain head |
| `--new-blocks-only` | `NEW_BLOCKS_ONLY` | `false` | Start from the current chain height instead of `--start-block` (skip the historical sync) |
| `--confirmations` | `CONFIRMATIONS` | `0` | Stay this many blocks behind the chain head. Optional: reorgs are repaired either way, see [Reorgs](#reorgs) |
| `--max-reorg-depth` | `MAX_REORG_DEPTH` | `512` | Deepest rollback the indexer performs on its own. A deeper fork stops the process with an error |
| `--no-dex` | `NO_DEX` | `false` | Turn [DEX analytics](#dex-analytics) off |
| `--no-predictions` | `NO_PREDICTIONS` | `false` | Turn prediction-market analytics off (see [below](#prediction-markets-and-perps)) |
| `--no-launchpads` | `NO_LAUNCHPADS` | `false` | Turn token-launchpad analytics off (see [below](#token-launchpads)) |
| `--no-migrate` | `NO_MIGRATE` | `false` | Do not apply pending schema migrations at startup (run `indexer migrate` yourself) |
| `--metrics-addr` | `METRICS_ADDR` | *off* | `ip:port` to serve `/metrics`, `/healthz` and `/readyz` on. See [Metrics and health checks](#metrics-and-health-checks) |
| `--flush-rows` | `FLUSH_ROWS` | `100000` | Flush to ClickHouse once this many rows are buffered |
| `--flush-interval-ms` | `FLUSH_INTERVAL_MS` | `2000` | Maximum time in milliseconds between flushes during a historical sync. While following the chain head the indexer commits at most once every 2x this value (4 s by default): every commit is one synchronous ClickHouse insert per table, and fewer, larger inserts are what keeps a server shared by many chains healthy |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

`indexer migrate` takes `--database`, `--dry-run` and `--debug`; `indexer verify` and `indexer backfill` take the options shown in the table above (plus `--debug`). See [`.env.example`](.env.example) for a commented template; for Docker Compose it additionally contains the ClickHouse container credentials (`CLICKHOUSE_USER`, `CLICKHOUSE_PASSWORD`, `CLICKHOUSE_DB`) and `METRICS_PORT`.

Any network available on HyperSync can be indexed: see the [list of supported networks](https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks). The HyperSync endpoint is derived from `--chain`; use `--hypersync-url` to point to a different endpoint.

## Token metadata and RPC endpoints

Block data comes from HyperSync only. A JSON-RPC endpoint is needed for the one thing HyperSync cannot serve, `eth_call`: token `name` / `symbol` / `decimals`, and the tokens of DEX pools whose creation event was not indexed (partial syncs, Curve). DEX prices and volumes are meaningless without token decimals, which is why RPC access is on by default.

`--rpc` takes a comma-separated list; entries are tried in order with per-endpoint circuit breakers, and the chain ID of every endpoint is verified.

| Value | Meaning |
|-------|---------|
| *(unset)* or `auto` | **Default.** Public endpoints for the chain ID are discovered at startup. **This is an outbound request to a third party: the indexer downloads `https://chainid.network/chains.json`** and then sends `eth_call`s to the public endpoints listed there (https only). Public endpoints are best effort: rate limited, no guarantees. A failed discovery never stops the indexer |
| `https://my-node.example,auto` | **Recommended for production.** Your endpoint first, public endpoints as fallback |
| `https://a.example,https://b.example` | Only your endpoints, with failover. No request to chainid.network |
| `none` | No RPC at all: `tokens` stays empty, DEX views cannot adjust decimals or compute USD values, pools without a creation event stay unresolved |

How it behaves:

- **Never on the commit path.** Discovered tokens are queued without blocking; if the queue is full they are dropped and found again by a periodic query for token addresses in the transfer tables (and `dex_pools`) that have no `tokens` row. An RPC outage of any length heals by itself.
- Tokens that revert or return garbage still get a row (with empty metadata), so "checked, nothing there" is distinguishable from "not checked yet".
- An archive node is not required.
- `--redis` is optional and only saves repeated RPC calls across restarts. Keys are namespaced by chain, so all chains can share one instance.

## Database schema

All chains live in one database. Base tables:

| Table | Sorting key | Content |
|-------|-------------|---------|
| `blocks` | `(chain, number)` | Block headers. Also the commit marker: written last in every flush |
| `transactions` | `(chain, block_number, transaction_index)` | Transactions with receipt fields (status, gas used, effective gas price, created contract) |
| `logs` | `(chain, block_number, log_index)` | Event logs; four non-null topic columns plus `topic_count` |
| `withdrawals` | `(chain, block_number, withdrawal_index)` | Validator withdrawals |
| `erc20_transfers`, `erc721_transfers`, `erc1155_transfers` | `(chain, block_number, log_index)` | Decoded token transfers |
| `tokens` | `(chain, address)` | Token metadata, written by the background resolver |
| `contracts` | *view* | Contracts deployed **directly** by a successful transaction (`contract_address`, `creator`, `transaction_hash`, `block_number`, `timestamp`). Contracts created by other contracts (factories) are not listed, so do not build statistics on it |

Lookup tables, each fed automatically by a materialized view from its base table. Use them to find rows when you do not know the block number:

| Table | Sorting key | Answers |
|-------|-------------|---------|
| `tx_lookup` | `(chain, hash)` | transaction hash → `block_number`, `transaction_index` |
| `block_lookup` | `(chain, hash)` | block hash → `number` |
| `transactions_by_address` | `(chain, address, block_number, ...)` | transactions sent or received by an address (two rows per transaction) |
| `logs_by_address` | `(chain, address, topic0, block_number, log_index)` | `eth_getLogs` style: contract + event + block range |
| `erc20_transfers_by_account` | `(chain, account, token_address, block_number, ...)` | wallet history and balances (two rows per transfer, `direction` = `-1` out / `1` in) |
| `nft_transfers_by_account` | `(chain, account, token_address, block_number, ...)` | the same for ERC721 and ERC1155 |

Aggregates (read them through the `*_v` views): `daily_block_stats_v`, `daily_transaction_stats_v`, `daily_erc20_transfer_stats_v`, and the [DEX](#dex-analytics) candles and volumes.

Bookkeeping: `schema_migrations`, `checkpoints` (committed block ranges), `reorgs` (audit trail of every rollback and gap repair).

Storage conventions:

- **No hex strings.** Hashes and topics are `FixedString(32)`, addresses `FixedString(20)`, calldata / log data raw bytes in `String`, the 4-byte selector `FixedString(4)`.
- **Exact amounts.** Wei values, gas prices and token amounts / ids are `UInt256` (signed DEX amounts `Int256`). Aggregates sum `Float64` on purpose: `sum()` over 256-bit integers wraps silently and spam tokens emit amounts near `2^256`. The exact values are always in the base tables.
- `Nullable` only where NULL means something different from the default (for example `transactions.to` on contract creations, `status` before Byzantium, EIP-1559 fee fields on legacy transactions).
- Every block-scoped table is `ReplacingMergeTree(_version, is_deleted)` with positional sorting keys, so a re-inserted block replaces itself. Base tables are partitioned by month only (never by chain: `chain` is the first sorting-key column, which is what prunes reads), lookup tables by chain.
- The technical columns `_version`, `is_deleted` and `epoch` belong to the [reorg](#reorgs) machinery. Do not write them.

The full schema is in [`migrations/`](migrations); the rationale is in [`docs/design.md`](docs/design.md).

## Querying the data

Two rules make every query correct, including right after a reorg or a crash:

1. **Query base and lookup tables with `FINAL`.** `FINAL` collapses re-inserted rows and hides rolled-back ones. Without it you can see duplicates and rows of orphaned blocks.
2. **Query aggregates through their `*_v` views**, never the underlying `AggregatingMergeTree` tables. The views finalize the aggregate states and apply the reorg validity rule.

Always filter by `chain` first: it is the first sorting-key column of every table. Format bytes with `concat('0x', lower(hex(x)))` and filter with `unhex('...')` (no `0x` prefix, any letter case).

**Transaction by hash** (through `tx_lookup`):

```sql
SELECT
  block_number,
  transaction_index,
  concat('0x', lower(hex(hash)))   AS hash,
  concat('0x', lower(hex(`from`))) AS `from`,
  if(isNull(`to`), NULL, concat('0x', lower(hex(`to`)))) AS `to`,
  toString(value)                  AS value_wei,
  status
FROM transactions FINAL
WHERE chain = 1
  AND (block_number, transaction_index) IN (
    SELECT block_number, transaction_index
    FROM tx_lookup FINAL
    WHERE chain = 1
      AND hash = unhex('5c504ed432cb51138bcf09aa5e8a410dd4a1e204ef84bfed1be16dfba1b22060')
  );
```

**ERC20 history of a wallet**, decimals adjusted (amounts of tokens without metadata come back as NULL):

```sql
SELECT
  h.timestamp,
  concat('0x', lower(hex(h.token_address)))    AS token,
  t.symbol,
  if(h.direction = 1, 'in', 'out')             AS side,
  concat('0x', lower(hex(h.counterparty)))     AS counterparty,
  toFloat64(h.amount) / pow(10, t.decimals)    AS amount,
  concat('0x', lower(hex(h.transaction_hash))) AS tx
FROM erc20_transfers_by_account AS h FINAL
LEFT JOIN (SELECT address, symbol, decimals FROM tokens FINAL WHERE chain = 1) AS t
  ON t.address = h.token_address
WHERE h.chain = 1
  AND h.account = unhex('d8dA6BF26964aF9D7eEd9e03E53415D37aA96045')
ORDER BY h.block_number DESC, h.log_index DESC
LIMIT 100
SETTINGS join_use_nulls = 1;
```

Exact balances from the same table (only complete when the chain was indexed from the token's first transfer): `SELECT token_address, sum(toInt256(amount) * direction) FROM erc20_transfers_by_account FINAL WHERE chain = 1 AND account = unhex('...') GROUP BY token_address`.

**Logs of a contract**, filtered by event (here ERC20 `Transfer` on USDC) and block range, through `logs_by_address`:

```sql
SELECT
  block_number,
  log_index,
  concat('0x', lower(hex(transaction_hash))) AS tx,
  concat('0x', lower(hex(topic1)))           AS topic1,
  concat('0x', lower(hex(topic2)))           AS topic2,
  concat('0x', lower(hex(data)))             AS data
FROM logs FINAL
WHERE chain = 1
  AND (block_number, log_index) IN (
    SELECT block_number, log_index
    FROM logs_by_address FINAL
    WHERE chain = 1
      AND address = unhex('A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48')
      AND topic0  = unhex('ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef')
      AND block_number >= 20000000
    ORDER BY block_number DESC, log_index DESC
    LIMIT 100
  )
ORDER BY block_number DESC, log_index DESC;
```

**Daily statistics** (an aggregate: use the view, no `FINAL`):

```sql
SELECT day, transactions, successful, failed, unique_senders, fees / 1e18 AS fees_native
FROM daily_transaction_stats_v
WHERE chain = 1 AND day >= today() - 30
ORDER BY day;
```

**DEX candles**: hourly OHLC of a pool, decimals adjusted. `pool_id` is 32 bytes: the pool address left padded with zeros (or the native `bytes32` id for Uniswap V4 and Balancer). The price is token1 per token0; here the Uniswap V3 USDC/WETH 0.05% pool:

```sql
SELECT bucket, symbol0, symbol1, open, high, low, close, volume0_adj, volume1_adj, swaps, traders
FROM dex_pool_prices_1h_v
WHERE chain = 1
  AND pool_id = unhex(concat(repeat('00', 12), '88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640'))
  AND bucket >= now() - INTERVAL 7 DAY
ORDER BY bucket;
```

## Resume and gap healing

- On every flush, `blocks` rows are written **last**, after the transactions, logs, transfers and other rows of the same batch. A row in `blocks` is the commit marker for that block: if it is there, the rest of the block's data is too. After `blocks`, the flush records the committed range in `checkpoints`, which is what a restart resumes from.
- The first sync pass after a start also runs a gap-detection query over `blocks` for the configured range, from `--start-block` (inclusive) to `--end-block` (**exclusive**), or to the chain head minus `--confirmations` when `--end-block` is `0`, and streams only what is missing.
- If the process died in the middle of a flush, the affected heights have transactions or logs but no `blocks` row. Before such a gap is streamed again, its leftovers are removed with the same rollback primitive that repairs reorgs (recorded in `reorgs` with `reason = 'gap_heal'`). Nothing is ever inserted twice, which is what keeps the incremental aggregates exact.
- With `--end-block 0` the indexer keeps following the chain head after the backfill is complete. With `--end-block N` it exits with status `0` once every block below `N` is stored (`N` itself is not indexed). With `--new-blocks-only` the historical backfill is skipped.

Restarting the indexer with the same arguments is always safe, and it is also how holes left by a crash get repaired.

## Reorgs

A reorg is the chain replacing its most recent blocks with different ones. The indexer **detects and repairs reorgs by itself**; no operator action is needed for ordinary ones.

What happens, step by step:

1. **Detection.** Every streamed block must name the previously stored block as its parent (on a restart the comparison starts from the hash stored in the database), and HyperSync's rollback guard is checked as well. A mismatch means the stored chain is no longer the canonical one.
2. **Fork-point search.** The indexer fetches the canonical headers of the last 8, then 16, 32, ... blocks and compares them with the stored hashes until it finds the highest block both agree on. The search is bounded by `--max-reorg-depth` (default `512`).
3. **Rollback.** Everything above the fork point is removed for that chain, children first and `blocks` last, the daily / candle aggregates of the touched days are rebuilt, and one row is written to the `reorgs` table.
4. **Resume.** Streaming continues from the fork point and stores the canonical blocks.

### Insert-only: tombstones and epochs

The indexer never runs `DELETE`, `ALTER ... DELETE / UPDATE` or `DROP PARTITION`. With dozens of indexer processes sharing a database, concurrent deletes are not reliable in ClickHouse, while concurrent inserts need no coordination. So "removing" is done with inserts:

- **Tombstones.** For every row of an orphaned block a copy with a newer `_version` and `is_deleted = 1` is inserted. Under `FINAL`, the tombstone hides the row. When the canonical block is stored, its rows carry a newer version again and win; rows that only existed on the abandoned fork stay hidden. The materialized views pass tombstones on, so the lookup tables follow automatically.
- **Epochs.** Every row carries `epoch`, the chain's rollback generation. A rollback starts a new epoch, records `(chain, epoch, from_ts)` in `reorgs` and re-aggregates the affected buckets under the new epoch. The `*_v` views only count contributions whose epoch is current for their bucket, so stale contributions disappear without being deleted, and older buckets are untouched.

This is why the two reading rules in [Querying the data](#querying-the-data) exist: `FINAL` on tables, `*_v` views on aggregates. A reader that follows them never sees orphaned data. Rolled-back rows stay on disk (a reorg at the tip is a handful of blocks); they are just invisible.

Things worth knowing:

- A rollback is idempotent and crash safe: if the process dies half way, the next start detects the same mismatch and runs it again under a newer epoch.
- For a moment during the repair, the aggregates of the affected day **under-count** (the stale contributions are already invalid, the rebuilt ones not yet written). They never double count.
- Chains do not affect each other, and there is no lock between indexer processes.

### The audit table and alerts

```sql
SELECT detected_at, reason, fork_block, depth, rows_tombstoned,
       concat('0x', lower(hex(old_hash))) AS old_hash,
       concat('0x', lower(hex(new_hash))) AS new_hash
FROM reorgs
WHERE chain = 1
ORDER BY epoch DESC
LIMIT 20;
```

`reason` is `reorg` or `gap_heal`. The metrics `evm_indexer_reorgs_total`, `evm_indexer_reorg_last_depth` and `evm_indexer_purge_duration_seconds` expose the same events; an example alert is in [`src/metrics/README.md`](src/metrics/README.md).

### `--max-reorg-depth`

If no common block is found within `--max-reorg-depth` blocks, the indexer **stops with an error** instead of rewriting that much history on its own: a fork that deep usually means a wrong endpoint, a chain incident or a database shared with a different network. Nothing is modified. After checking, restart with a larger `--max-reorg-depth` to let it proceed.

### `--confirmations`

`--confirmations N` is an optional head lag: only blocks at least `N` behind the head are indexed, so most reorgs are never stored in the first place. It is a trade, not a requirement:

- `0` (default): lowest latency. Reorged blocks are briefly visible and then repaired as described above.
- `N` around the chain's usual reorg depth (for example `12` to `64` on Ethereum-like proof-of-stake chains; 64 blocks is two epochs, i.e. finality on Ethereum mainnet): readers practically never see data that later changes, fewer rollbacks, data shows up `N` blocks later.
- Chains with instant finality: leave it at `0`.

## DEX analytics

On by default; turn it off with `--no-dex`. DEX events are decoded from logs that are fetched anyway, so it costs no extra HyperSync traffic.

**Decoding is by event family, never by a router or factory registry.** A log is a swap when its `topic0` and its shape are the ones the family emits, so a Uniswap V2 fork on a chain nobody has heard of works on day one. `protocol` is therefore the family, not a brand:

| `protocol` | Pools from | Swaps | Liquidity |
|------------|------------|-------|-----------|
| `uniswap_v2` | `PairCreated` | `Swap` (also emitted by Solidly V1 forks) | `Sync`, `Mint`, `Burn` |
| `solidly` | Solidly V1 `PairCreated`, Velodrome V2 / Aerodrome `PoolCreated` | Velodrome V2 / Aerodrome `Swap` | `Sync`, `Burn` |
| `uniswap_v3` | `PoolCreated`, Slipstream, Algebra | `Swap`, PancakeSwap V3 and Algebra variants | `Mint`, `Burn` |
| `uniswap_v4` | `Initialize` (PoolManager) | `Swap` | `ModifyLiquidity` |
| `balancer_v2` | `PoolRegistered` + `TokensRegistered` (Vault) | `Swap` | - |
| `curve` | over RPC (no creation event) | `TokenExchange`, `TokenExchangeUnderlying` | - |

Tables: `dex_pools`, `dex_swaps`, `dex_liquidity` (plus the lookup tables `dex_swaps_by_pool`, `dex_swaps_by_trader`, `dex_pools_by_token`). Aggregates: per-pool candles `dex_candles_1m` / `_1h` / `_1d`, `dex_pool_volume_1d`, `dex_protocol_stats_1d`. They follow the same storage and reorg rules as everything else.

Analyst views join tokens and pools at query time, so results improve as the background resolvers fill in metadata; unknown stays `NULL`, never `0`:

| View | What |
|------|------|
| `dex_pools_v` | pools with token symbols and decimals |
| `dex_swaps_v`, `dex_swaps_usd_v` | every swap as token in / token out with adjusted amounts, whatever the family; plus `amount_usd` |
| `dex_pool_prices_1m_v` / `_1h_v` / `_1d_v` | decimals-adjusted candles ([example](#querying-the-data)) |
| `dex_native_price_1h_v` / `_1d_v` | USD price of the native coin |
| `dex_pool_volume_usd_1d_v`, `dex_protocol_volume_usd_1d_v`, `dex_token_volume_usd_1d_v` | daily USD volume per pool, per family, per token |
| `dex_top_pools_v` | pools by USD volume of the trailing 30 days |

Always filter these views by `chain` (and a time range for the swap-level ones). Signed amounts are pool relative: positive = into the pool.

### USD values: populate `quote_tokens`

**USD columns are `NULL` until you tell the indexer which tokens are dollars.** The indexer ships no chain-specific data and never writes `quote_tokens`; you insert a few rows per chain:

- `kind = 'stable'`: a token worth 1 USD.
- `kind = 'native'`: the wrapped native coin (and the pseudo addresses some protocols use for the native coin). Its USD price is derived per hour / day from the pools that pair a native with a stable.

```sql
INSERT INTO quote_tokens (chain, token, kind) VALUES
  (1, unhex('A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48'), 'stable'),  -- USDC
  (1, unhex('dAC17F958D2ee523a2206206994597C13D831ec7'), 'stable'),  -- USDT
  (1, unhex('C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2'), 'native');  -- WETH
-- pseudo addresses of the native coin have no contract: give the decimals
INSERT INTO quote_tokens (chain, token, kind, decimals, symbol) VALUES
  (1, unhex('0000000000000000000000000000000000000000'), 'native', 18, 'ETH'),  -- Uniswap V4
  (1, unhex('EeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE'), 'native', 18, 'ETH');  -- Curve
```

A swap is valued on its stable side if it has one, else on its native side, else `amount_usd` is `NULL`. Nothing about USD is materialized: the views pick up every change immediately, for all of history. Retire a row by inserting it again with `kind = ''`. Stables are assumed to be worth exactly 1.00.

Conventions, precision, the pool resolver and the known gaps (forged events, aggregator attribution, protocols that are not covered) are documented in [`src/dex/README.md`](src/dex/README.md).

## Prediction markets and perps

- **Prediction markets: on by default** (`--no-predictions` to opt out). Same shape as DEX analytics: decoded by event family, candles of implied probability, trades, positions. Tables, views and the query cookbook are in [`src/predictions/README.md`](src/predictions/README.md).
- **Perpetual futures: deferred.** Only a few percent of perp volume is readable from EVM logs on HyperSync chains; the numbers are in [`docs/perps-research.md`](docs/perps-research.md).

## Token launchpads

**On by default** (`--no-launchpads` to opt out). Decoded by event family from the same logs, so it costs no extra HyperSync traffic: bonding-curve launches, buys and sells, fee sweeps and graduations into a DEX pool, plus launch attribution for venues that launch straight into a Uniswap V3 / V4 pool (their trading is already in `dex_swaps`).

Nothing is trusted by default: a trade leg counts only when the asset contract reported the movement in the same transaction, and the headline views count only emitters an operator listed in `launchpad_trusted_emitters` (the module README ships the verified addresses as ready-to-run `INSERT`s; migrations seed nothing). Tables, views and the query cookbook are in [`src/launchpads/README.md`](src/launchpads/README.md).

## Solana

`indexer run --chain solana` indexes Solana into the same database as every EVM chain, from Envio's Solana HyperSync. It is a **separate sync loop** (`src/pipeline/solana.rs`) because three things genuinely differ; everything else — the ClickHouse insert path, the tombstone/epoch machinery, `purge_range`, the one-process-per-chain lease, the metrics — is shared.

**Analytics only, and program filtered.** Solana produces ~150M non-vote transactions a day and the value sits in a couple of dozen programs, so the indexer asks for those programs and nothing else. `sol_transactions` is *the matched transactions*, not the chain's; there is no wallet history, no chain-wide transfer table and deliberately no daily chain statistics. [`src/svm/README.md`](src/svm/README.md) says what that rules out, and why.

### Running it next to the EVM chains

One more process against the same `DATABASE_URL`, exactly like adding any other chain:

```sh
export ENVIO_API_TOKEN=...            # the same token as the EVM chains; separate rate-limit budget
export DATABASE_URL=http://user:pass@clickhouse:8123/indexer

# follow the head from now on (what you want first)
indexer run --chain solana --new-blocks-only --metrics-addr :9090

# or from a specific slot
indexer run --chain solana --start-block 448000000
```

`--chain` takes the name `solana` or the id `1399811149` (and `CHAIN_ID` takes either too, so a compose file needs no new variable). The indexer writes the `chains` registry row itself at startup, which is what tells a view to print a Solana identity column with `base58Encode` instead of as an EVM address.

In compose, it is one more service in the `x-indexer` block:

```yaml
  indexer-solana:
    <<: *indexer
    container_name: evm-indexer-solana
    environment:
      <<: *indexer-environment
      CHAIN_ID: solana
      NEW_BLOCKS_ONLY: "true"
      RPC_URL: none        # Solana needs no eth_call; decimals come with the data
    ports:
      - "127.0.0.1:9103:9090"
```

**`--start-block` is a SLOT**, and Envio serves Solana only from slot **391,000,000** (2026-01-03). A lower value is refused at startup rather than left to spin, because a query below the served history comes back empty *without advancing the cursor*, which a resume loop cannot tell from "caught up". Anything older exists only in the Old Faithful archive and would need a second ingest path ([`docs/solana-research.md`](docs/solana-research.md) §11.3.1).

### Flags that differ

| Flag | On Solana |
|---|---|
| `--confirmations` | **refused** unless 0. Envio serves Solana at (just behind) `finalized`, so staying further back costs freshness twice and protects against nothing |
| `--no-dex` | **refused**: with the DEX decoder off this pipeline would store empty slot headers and nothing else |
| `--start-block` | a slot, and at least 391,000,000 |
| `--rpc`, `--redis` | ignored, with one log line. Token decimals arrive free on every `account_activity` row, so nothing here makes an RPC call |
| `--max-reorg-depth` | ignored: there is no fork-point search on this chain (see below) |
| `--no-predictions`, `--no-launchpads` | ignored: those decoders have no Solana front end yet |

### Skipped slots, gaps and the tripwire

**A slot with no block is normal**, not a gap: Solana simply produces no block for it. So the Solana pipeline answers "what is missing?" from the **checkpoint tiling** rather than from the rows — a checkpoint's `to_block` is the *server's* `next_slot`, not `max(slot) + 1`, and a hole in that tiling is the only thing that can mean "we never asked for these slots". Continuity between stored slots is `block_height + 1` (Solana's `block_height` counts *produced blocks*, so it is immune to skipped slots) plus the `parent_slot` / `parent_blockhash` pair.

**There is no fork-point search.** On data served at finality there is no fork to find, so a continuity break is treated as what it is — something that is not supposed to happen. The indexer **stops**, loudly, with a message naming the slot, both hashes and what to check; nothing at or above the break is written. It is not repaired silently, because the repair would be indistinguishable from a wrong endpoint. The tombstone/epoch machinery stays fully in place and is used for gap heals: a flush that died between its children and its `sol_slots` insert is purged before the range is streamed again.

`indexer verify --chain solana` runs the Solana checks (cursor tiling, height chain, parent chain, orphan rows, candles against `sol_dex_swaps`) and reports skipped slots as skipped rather than missing.

Give it the slot you actually started from — `indexer verify --chain solana --start-block 448378313`. It defaults to 0, and after a `--new-blocks-only` run that is honest but unhelpful: everything below the start really was never asked for, so the report is one enormous hole.

### Cost and rate limit

The free Envio token is **30 queries per 60 seconds per endpoint** (a flat cost per query, whatever it returns), and the Solana endpoint has its own budget — adding Solana does not eat the EVM chains'. Following the head needs 5 to 15 of those, so the follower caps itself at 25 and additionally honours the `x-ratelimit-*` headers of every response. `GET /height` is free and unmetered, so discovering that nothing happened never costs a query.

Loading the **history** is the expensive part and is not wired up yet: 8.5 months is ~57M slots, which is weeks of the free budget. The options, with numbers, are in [`docs/solana-research.md`](docs/solana-research.md) §11.3.

### Tables

`sol_slots` (the commit marker; `block_number` holds the slot), `sol_transactions`, `sol_tokens`, `sol_dex_swaps`, and the candles `sol_dex_candles_1m` / `_1h` / `_1d` — read those through their `*_v` views, which apply the reorg validity rule. Read every base table with `FINAL`.

```sql
-- hourly candles of a Solana pool, prices scaled by the mints' decimals
SELECT bucket, open, high, low, close, swaps, traders
FROM sol_dex_candles_1h_v
WHERE chain = 1399811149
  AND pool_id = base58Decode('...')
ORDER BY bucket DESC LIMIT 48;
```

## Metrics and health checks

`--metrics-addr <ip:port>` (default: off) serves, on a small built-in HTTP server:

| Endpoint | Answer |
|----------|--------|
| `GET /metrics` | Prometheus text format. Every series is prefixed `evm_indexer_` and labelled `chain="<chain id>"` |
| `GET /healthz` | `200` while the process is alive (liveness) |
| `GET /readyz` | `200` when startup completed, the most recent flush did not fail, no flush has been retrying for more than 2 minutes, and the last successful flush or head poll is recent; otherwise `503` with a one-line reason. **Readiness, not liveness:** use it to take a lagging indexer out of a dashboard or load balancer, never to restart the process (a ClickHouse outage makes it not ready, and a restart loop would not help). Use `/healthz` for liveness |

The most useful series: `evm_indexer_head_block`, `evm_indexer_indexed_block`, `evm_indexer_lag_blocks`, `evm_indexer_lag_seconds`, `evm_indexer_rows_inserted_total{table}`, `evm_indexer_flush_duration_seconds`, `evm_indexer_flushes_total{result}`, `evm_indexer_reorgs_total`, `evm_indexer_reorg_last_depth`, `evm_indexer_resolver_queue_depth{worker}` and `evm_indexer_resolver_endpoints_healthy{worker}`. The full reference and ready-made alert rules are in [`src/metrics/README.md`](src/metrics/README.md).

Run one indexer process per chain, each with its own port. In the bundled Compose file every indexer listens on `9090` inside its container, published on `127.0.0.1:9101`, `9102`, ...:

```yaml
scrape_configs:
  - job_name: evm-indexer
    static_configs:
      - targets: ["127.0.0.1:9101", "127.0.0.1:9102"]
```

The Compose health check of the indexer containers calls `/healthz`. The runtime image ships neither `curl` nor `wget`, so it uses bash's `/dev/tcp`; the same one-liner works for any other orchestrator:

```bash
bash -c "exec 3<>/dev/tcp/127.0.0.1/9090 && printf 'GET /healthz HTTP/1.0\r\n\r\n' >&3 && head -n 1 <&3 | grep -q ' 200 '"
```

The endpoint has no authentication: bind it to localhost or a private network.

## Performance tuning

### Flush size and interval
- `--flush-rows` bounds how many rows are buffered in memory before a flush. Larger values produce fewer, bigger ClickHouse parts at the cost of memory; raise it during a backfill
- `--flush-interval-ms` bounds how long rows wait in the buffer, which is what matters when following the chain head

### Token metadata
- Put your own endpoint in front of `auto` (`--rpc https://mine,auto`); public endpoints are rate limited
- Use Redis or Dragonfly (`--redis`) so token metadata survives restarts and is not requested again

### ClickHouse
- Use SSD storage
- Always filter by `chain`, and by a block or time range where you can; go through the lookup tables instead of scanning a base table by hash or address
- `FINAL` is required for correctness. The tables are laid out for it (`do_not_merge_across_partitions_select_final`, month partitions), and a narrow filter keeps it cheap
- With many chains, size ClickHouse for the number of concurrent inserts: each indexer process flushes every `--flush-interval-ms` while it follows the head

## Upgrading from 2.x

There is **no migration path**. The schema is new (binary storage, different sorting keys, different tables), so a 2.x database cannot be converted or reused:

1. Point `--database` at a **fresh, empty database** (a new name on the same server is fine; the indexer creates it).
2. Get an [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens): data now comes from HyperSync instead of JSON-RPC / WebSocket.
3. Resync from `--start-block`. Drop the old database yourself when you no longer need it; the indexer never touches it.

Flags that no longer exist: `--rpcs`, `--ws`, `--batch-size`, `--fetch-uncles`, `--traces` (traces are not indexed at all). `--rpc` now only serves token / pool metadata. `--database` must be an `http(s)://` URL of the ClickHouse HTTP interface (port `8123`, or `8443` for TLS).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Tests that need a real ClickHouse are `#[ignore]`d. They create and drop their own databases; the database named in the URL **must end in `_test`** because it is dropped and recreated:

```bash
docker compose up -d clickhouse
TEST_DATABASE_URL=http://indexer:indexer@localhost:8123/indexer_test \
  cargo test --locked --lib -- --ignored \
  db::integration_tests:: db::migrate::integration:: dex::integration_tests::
```

`db::migrate::integration` needs a server whose **access storage is writable**: one of its tests runs `CREATE USER ... GRANT SELECT, INSERT` to prove that a least-privilege user can start once the migrations are applied. The `clickhouse/clickhouse-server` image used by `docker-compose.yml` and by CI has one out of the box (a `<user_directories>` with a `<local_directory>` at `/var/lib/clickhouse/access/`), and `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` lets the configured user use it. A ClickHouse started by hand from a config with `users_xml` alone refuses with *"there are no writable access storages"*, so give it:

```xml
<user_directories>
  <users_xml><path>users.xml</path></users_xml>
  <local_directory><path>/var/lib/clickhouse/access/</path></local_directory>
</user_directories>
```

CI runs the same set against a ClickHouse service container, plus the Redis cache round trip (`redis_round_trip_and_restart`, `TOKEN_CACHE_TEST_REDIS_URL`). The remaining ignored tests (`live_*`) need internet access and are meant to be run by hand.

Schema changes are new files in `migrations/` (`NNNN_name.sql`; `0001`-`0009` core, `0010`-`0019` DEX, `0020`-`0029` prediction markets). Never edit a migration that has been released: its checksum is verified at startup. Until the first release the set is still being corrected in place, so a development database from an earlier build of this branch has to be dropped and created again - see the note under [Commands and schema migrations](#commands-and-schema-migrations).

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see LICENSE file for details

## Support

- GitHub Issues: [Report bugs](https://github.com/eabz/evm-indexer/issues)
- Discussions: [Ask questions](https://github.com/eabz/evm-indexer/discussions)
