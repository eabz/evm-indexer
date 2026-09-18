<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>High-performance SQL indexer for EVM-compatible blockchains</strong>
</p>

[![Docker Image Size](https://badgen.net/docker/size/0xeabz/evm-indexer/main?icon=docker&label=image%20size)](https://hub.docker.com/r/0xeabz/evm-indexer)
![build](https://github.com/eabz/evm-indexer/actions/workflows/build.yml/badge.svg)

An indexer that streams blockchain data from [Envio HyperSync](https://docs.envio.dev/docs/HyperSync/overview) and stores it in [ClickHouse](https://clickhouse.com/) for later analysis. It contains no chain-specific logic: any network served by HyperSync can be indexed by changing the chain ID.

## Features

- ✅ **Complete blockchain primitives**: blocks, transactions, logs, contracts, withdrawals and (opt-in) traces
- ✅ **Token transfers**: ERC20, ERC721, ERC1155
- ✅ **Token metadata**: name, symbol and decimals resolved through Multicall3 and cached in Redis or Dragonfly
- ✅ **HyperSync streams**: historical backfill and chain-head following over a single data source, no per-block JSON-RPC calls
- ✅ **Batched async inserts**: rows are buffered and flushed to ClickHouse by row count or time interval
- ✅ **Self-healing resume**: gaps in the `blocks` table are detected with SQL on every sync pass (not only at startup) and re-indexed
- ✅ **Chain agnostic**: works with every [HyperSync supported network](https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks)

## Architecture

```mermaid
flowchart LR
    HS[("Envio HyperSync")] -->|"blocks, transactions,<br/>logs, traces"| S["Stream"]
    S --> T["Transform"]
    T --> R["Token resolver"]
    R --> W["Batched writer"]
    W -->|"async inserts"| CH[("ClickHouse")]
    R <-.->|"metadata cache"| C[("Redis / Dragonfly")]
    R <-.->|"Multicall3 eth_call"| RPC["RPC (optional)"]
    CH -.->|"gap detection on every pass"| S
```

1. **Stream** - opens a HyperSync stream for every block range that is still missing and, once caught up, keeps following the chain head (when `--end-block` is `0`).
2. **Transform** - converts the HyperSync responses into database rows and decodes ERC20 / ERC721 / ERC1155 transfers from the logs.
3. **Token resolver** - looks up name / symbol / decimals for tokens that have not been seen before. Lookups are batched through Multicall3 over the optional RPC endpoint and cached in Redis or Dragonfly (or in memory when no cache URL is given). Without an RPC endpoint this step is skipped.
4. **Batched writer** - buffers rows per table and flushes them to ClickHouse when `--flush-rows` rows are buffered or `--flush-interval-ms` has elapsed, whichever comes first. Within a flush the `blocks` rows are written last, so a block only exists in the database once all of its data does.

## Requirements

- [ClickHouse](https://clickhouse.com/) 23.0+
- An [Envio HyperSync API token](https://docs.envio.dev/docs/HyperSync/api-tokens)
- *Optional:* a JSON-RPC endpoint for the chain, only used for token metadata `eth_call`s
- *Optional:* [Redis](https://redis.io/) or [Dragonfly](https://www.dragonflydb.io/) to cache token metadata across restarts
- [Rust](https://www.rust-lang.org/tools/install) (stable) when building from source, or [Docker](https://docs.docker.com/get-docker/) with Compose v2.24+

## Quick Start

### Using Docker Compose (Recommended)

1. Clone the repository:
```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Create your configuration and set `ENVIO_API_TOKEN` (required). Optionally set `RPC_URL` to enable token metadata:
```bash
cp .env.example .env
```

3. Start the services:
```bash
docker compose up -d
```

This will start:
- ClickHouse on `localhost:8123` (HTTP) and `localhost:9000` (native), with the tables from `migrations/` created on first start
- Dragonfly as the token metadata cache
- The indexer, configured through the variables in `.env`

4. Monitor logs:
```bash
docker compose logs -f indexer
```

Inside Compose, `DATABASE_URL` and `REDIS_URL` always point to the bundled ClickHouse and Dragonfly services; everything else (`CHAIN_ID`, `START_BLOCK`, `TRACES`, ...) is taken from `.env`.

### Local Development

1. Clone the repository:
```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Create the tables in your ClickHouse instance:
```bash
clickhouse-client --multiquery < migrations/create_tables.sql
clickhouse-client --multiquery < migrations/indexes.sql
```

3. Build the program:
```bash
cargo build --release
```

4. Run the indexer:
```bash
./target/release/indexer \
  --chain 1 \
  --database http://user:password@localhost:8123/indexer \
  --hypersync-token <your-envio-api-token> \
  --rpc https://your-rpc-endpoint.example \
  --redis redis://localhost:6379 \
  --start-block 0
```

`--rpc` and `--redis` are optional, see [Configuration](#configuration).

## Configuration

Every CLI flag can also be set through the environment variable listed next to it. CLI flags take precedence.

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--chain` | `CHAIN_ID` | `1` | Chain ID to index |
| `--database` | `DATABASE_URL` | *required* | ClickHouse HTTP endpoint: `http://user:pass@host:port/db` (use `https://` for TLS). Always include the port (usually `8123`) |
| `--hypersync-url` | `HYPERSYNC_URL` | derived from the chain ID | HyperSync endpoint. Only needed to override the default endpoint for the chain |
| `--hypersync-token` | `ENVIO_API_TOKEN` | *required* | [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens) |
| `--rpc` | `RPC_URL` | *none* | JSON-RPC endpoint, only used for token metadata `eth_call`s. Without it token metadata is skipped |
| `--redis` | `REDIS_URL` | *none* | Redis or Dragonfly URL for the token metadata cache. Without it an in-memory cache is used |
| `--start-block` | `START_BLOCK` | `0` | Block number to start syncing from |
| `--end-block` | `END_BLOCK` | `0` | Block to stop at, **exclusive**: blocks `--start-block` up to `--end-block - 1` are indexed and the process exits. `0` = follow the chain head |
| `--confirmations` | `CONFIRMATIONS` | `0` | Stay this many blocks behind the chain head, so blocks that can still be reorged are not indexed. `0` = index up to the head. See [Reorgs](#reorgs) |
| `--new-blocks-only` | `NEW_BLOCKS_ONLY` | `false` | Only index new blocks (skip historical sync) |
| `--traces` | `TRACES` | `false` | Index transaction traces. Only available on HyperSync networks that serve traces |
| `--flush-rows` | `FLUSH_ROWS` | `100000` | Flush to ClickHouse once this many rows are buffered |
| `--flush-interval-ms` | `FLUSH_INTERVAL_MS` | `2000` | Maximum time in milliseconds between flushes |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

See [`.env.example`](.env.example) for a commented template. For Docker Compose it additionally contains the ClickHouse container credentials (`CLICKHOUSE_DB`, `CLICKHOUSE_USER`, `CLICKHOUSE_PASSWORD`).

## Supported Networks

Any network available on HyperSync: see the [list of supported networks](https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks). The HyperSync endpoint is derived from `--chain`; use `--hypersync-url` to point to a different endpoint.

Trace data is only served for some networks. Leave `--traces` disabled on the others.

## Database Schema

The indexer writes to the following ClickHouse tables:

- `blocks` - Block headers and metadata
- `transactions` - Transaction data with gas and receipt info
- `logs` - Event logs emitted by contracts
- `traces` - Internal transaction traces (only with `--traces`)
- `contracts` - Deployed contract addresses
- `withdrawals` - Validator withdrawals (post-merge)
- `erc20_transfers` - ERC20 token transfers
- `erc721_transfers` - NFT transfers
- `erc1155_transfers` - Multi-token transfers
- `tokens` - Token metadata: name, symbol and decimals (only with `--rpc`)

Every table has a `chain` column, so several chains can share one database by running one indexer per chain.

See `migrations/create_tables.sql` for full schema.

## Resume and Gap Healing

The indexer keeps no separate checkpoint. The `blocks` table is the source of truth:

- On every flush, `blocks` rows are written **last**, after the transactions, logs, transfers and other rows of the same batch. A row in `blocks` is therefore the commit marker for that block: if it is there, the rest of the block's data is too.
- On every sync pass (at startup and again each time the chain head has moved), the indexer runs a gap-detection query over `blocks` for the configured chain and streams only the missing ranges. The first pass covers the whole configured range: from `--start-block` (inclusive) to `--end-block` (**exclusive**), or to the chain head minus `--confirmations` when `--end-block` is `0`. Later passes only look at the blocks above what the previous pass completed, so holes made in older blocks while the indexer is running (for example a manual delete) are picked up on the next start.
- If the process stops in the middle of a flush, the affected blocks have no `blocks` row, show up as a gap on the next start and are indexed again. All tables use `ReplacingMergeTree`, so the rows that were already written are deduplicated by ClickHouse when parts merge (use `FINAL` for queries that need exact results before that happens).
- With `--end-block 0` the indexer keeps following the chain head after the backfill is complete. With `--end-block N` it exits with status `0` once every block below `N` is stored (`N` itself is not indexed). With `--new-blocks-only` the historical backfill is skipped.

Restarting the indexer with the same arguments is always safe, and it is also how holes left by a crash or a manual delete get repaired.

## Reorgs

Reorg handling is **detection only** today:

- The indexer compares the parent hash of every block it streams with the hash of the block it stored right before it (and with HyperSync's rollback guard). On a mismatch it logs a `REORG DETECTED` warning with the block number and both hashes.
- It does **not** repair anything. A block that was indexed and later reorged out stays in the database: its `blocks` row exists, so gap detection never asks for that height again, and the transactions, logs and transfers of the abandoned block stay next to it. Repairing this requires deleting the affected heights by hand and restarting.

The way to avoid storing such blocks in the first place is `--confirmations N`: the indexer then only indexes blocks that are at least `N` blocks behind the chain head. Set it to at least the typical reorg depth of the chain, for example `12` to `64` on Ethereum-like proof-of-stake chains (64 blocks is two epochs, i.e. finality on Ethereum mainnet), and leave it at `0` on chains with instant finality. The price is latency: data shows up `N` blocks later.

The default is `0` (index up to the head), which is the lowest latency and the behaviour of previous versions, but blocks inside the reorg window may then be stored from a fork that is later abandoned.

## Performance Tuning

### Flush size and interval
- `--flush-rows` bounds how many rows are buffered in memory before a flush. Larger values produce fewer, bigger ClickHouse parts at the cost of memory
- `--flush-interval-ms` bounds how long rows wait in the buffer, which is what matters when following the chain head

### Token metadata
- Use Redis or Dragonfly (`--redis`) so token metadata survives restarts and is not requested from the RPC again
- The RPC endpoint is only used for Multicall3 `eth_call`s, an archive node is not required

### ClickHouse
- Use SSD storage for better performance
- Prefer fewer, larger inserts over many small ones (raise `--flush-rows` during a backfill)
- Enable compression for storage savings

## Migrating from 2.x

Version 3 replaces the JSON-RPC / WebSocket fetcher with HyperSync streams ([#15](https://github.com/eabz/evm-indexer/issues/15)).

**New required configuration**

- `--hypersync-token` / `ENVIO_API_TOKEN`: an [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens)

**Removed flags**

| Removed | Replacement |
|---------|-------------|
| `--rpcs` | Data now comes from HyperSync. An optional single `--rpc` is used only for token metadata |
| `--ws` | None. New blocks are followed over HyperSync when `--end-block` is `0` |
| `--batch-size` | `--flush-rows` and `--flush-interval-ms` control write batching |
| `--fetch-uncles` | None |

**Changed defaults**

- `--traces` is now opt-in (`false` by default) and depends on the HyperSync network serving traces
- `--database` should be an `http://` or `https://` URL pointing to the ClickHouse HTTP interface (port `8123`, or `8443` for TLS). 2.x urls of the form `clickhouse://user:pass@host:9000/db` still work: the `clickhouse://` scheme is treated as `http://`, and because the indexer does not speak the native protocol, the native ports `9000` / `9440` are rewritten to `8123` (http) / `8443` (https) with a warning at startup. An explicit `http(s)://` url that points at `9000` / `9440` is used as written and only warned about. Update the url to get rid of the warning
- `--end-block` is exclusive (`--end-block 100` indexes up to block `99`), as it already was in 2.x

**DEX tables are no longer written**

All DEX parsing was removed. The `dex_trades`, `dex_pairs` and `dex_liquidity_updates` tables are no longer created by the migrations and no longer receive data. In an existing deployment they are left untouched: the indexer never drops them, and you can keep them for historical queries or drop them yourself.

**Everything else is unchanged**

The schemas of the remaining tables are unchanged, so version 3 can run against an existing 2.x database. On the first start the gap detection finds the blocks that are already indexed and continues from there.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see LICENSE file for details

## Support

- GitHub Issues: [Report bugs](https://github.com/eabz/evm-indexer/issues)
- Discussions: [Ask questions](https://github.com/eabz/evm-indexer/discussions)
