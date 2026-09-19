<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>High-performance SQL indexer for EVM-compatible blockchains and Solana</strong>
</p>

[![Docker Image Size](https://badgen.net/docker/size/0xeabz/evm-indexer/main?icon=docker&label=image%20size)](https://hub.docker.com/r/0xeabz/evm-indexer)
![build](https://github.com/eabz/evm-indexer/actions/workflows/build.yml/badge.svg)

An indexer that streams blockchain data from [Envio HyperSync](https://docs.envio.dev/docs/HyperSync/overview) and stores it in [ClickHouse](https://clickhouse.com/) for analysis. It contains no chain-specific logic: any network served by HyperSync can be indexed by naming its chain id, and every chain shares one database. One command, `indexer fleet`, runs all of them in one process behind a small password-protected web page.

## Contents

- [Quick start](#quick-start)
- [What it promises: the coverage floor](#what-it-promises-the-coverage-floor)
- [What is indexed](#what-is-indexed)
- [Requirements and hardware](#requirements-and-hardware)
- [Where the data comes from: HyperSync and RPC](#where-the-data-comes-from-hypersync-and-rpc)
- [Commands and schema migrations](#commands-and-schema-migrations)
- [Configuration](#configuration)
- [Reorg safety, in five sentences](#reorg-safety-in-five-sentences)
- [Database schema](#database-schema)
- [Querying the data](#querying-the-data)
- [Resume and gap healing](#resume-and-gap-healing)
- [Reorgs in detail](#reorgs-in-detail)
- [DEX analytics](#dex-analytics)
- [Prediction markets](#prediction-markets)
- [Token launchpads](#token-launchpads)
- [Solana](#solana)
- [Fleet mode and the control panel](#fleet-mode-and-the-control-panel)
- [Single-chain `indexer run`](#single-chain-indexer-run)
- [Metrics and health checks](#metrics-and-health-checks)
- [Performance tuning](#performance-tuning)
- [Upgrading](#upgrading)
- [Development](#development)

## Quick start

Three blocks. Copy them in order. You need [Docker](https://docs.docker.com/get-docker/) with Compose v2.24+ and an [Envio HyperSync API token](https://docs.envio.dev/docs/HyperSync/api-tokens) (the free tier is enough to start).

### 1. Write your `.env`

Configuration comes first: the Compose file refuses to load without these two values.

```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
cp .env.example .env
openssl rand -base64 18          # copy this, it is your control-panel password
$EDITOR .env                     # set ENVIO_API_TOKEN=... and ADMIN_PASSWORD=...
```

Everything else in `.env` already has a working default. The password must be at least **12 characters**: anything shorter, one character repeated, or a word on the built-in denylist is refused and the panel simply does not start — it can start and stop the indexing of every chain, so it will not run behind a password that can be guessed. `.env` is git-ignored and no secret is written into `docker-compose.yml`.

### 2. Start the database

ClickHouse holds every chain in one database. Dragonfly (a Redis-compatible cache) is optional; it keeps token metadata across restarts.

```bash
docker compose up -d clickhouse dragonfly
```

Both are published on `127.0.0.1` only. Nothing is mounted into ClickHouse: the indexer creates the database and every table itself on first start.

### 3. Start indexing

```bash
docker compose up -d --build fleet
docker compose logs -f fleet
```

That is the `fleet` service of [`docker-compose.yml`](docker-compose.yml): **one process, every chain**, with the control panel on **<http://127.0.0.1:8090/>** and one Prometheus endpoint on <http://127.0.0.1:9101/metrics>. Sign in with the password from step 1.

Which chains it starts with is the `--chain` list in that service's `command:`, which ships as Ethereum, Base and Solana — edit it before the first start if you want different ones. After that first start the chains are remembered in the database and you add more from the panel's **Add a chain** button; `--chain` is only needed while the `fleet_chains` table is still empty.

To check what you have at any time:

```bash
docker compose exec fleet indexer verify --chain 1
# Coverage: gap-free from 2024-09-19 (block 20779400) to 2025-09-19 (block 23400512).
```

Outside Docker the same thing is one command:

```bash
export ENVIO_API_TOKEN=...
export ADMIN_PASSWORD="$(openssl rand -base64 18)"   # keep it, you need it to sign in
indexer fleet \
  --database http://indexer:indexer@localhost:8123/indexer \
  --chain 1 --chain 8453 --chain solana \
  --redis redis://localhost:6379 \
  --metrics-addr 127.0.0.1:9090
```

### Reaching the panel from another machine

The panel speaks plain HTTP and binds `127.0.0.1`. **Do not put it on the open internet as it is.** Two safe ways:

**An SSH tunnel** — nothing to configure on the server, and the right answer for one person:

```bash
ssh -N -L 8090:127.0.0.1:8090 you@your-indexer-host
# then open http://127.0.0.1:8090/ in your browser
```

**A TLS reverse proxy** — for a panel several people use. The proxy does TLS; the panel needs to be told the name it is served under and where the proxy is:

```bash
indexer fleet ... \
  --admin-allow-remote \
  --admin-addr 127.0.0.1:8090 \
  --admin-host indexer.example.com \
  --admin-secure-cookie \
  --admin-trusted-proxy 203.0.113.10
```

- `--admin-host <name>` is **required behind a proxy**: a request that asks for any other host name is refused with `421` before it is routed, which is what stops a web page you merely visit from reaching the panel through your own browser (DNS rebinding). Repeat the flag for more names.
- `--admin-secure-cookie` marks the session cookie `Secure`, so it is only ever sent over HTTPS.
- `--admin-trusted-proxy <ip>` is the proxy's address. Only a connection from exactly that address has its `X-Forwarded-For` believed, and then the sign-in throttle counts per client. Without it the header is ignored entirely and everyone behind the proxy shares one throttle — one attacker's lock-out would fall on you too.
- `--admin-allow-remote` is only needed when the panel binds something other than loopback (which it must inside a container, because Compose publishes the port from outside).

A minimal nginx server block:

```nginx
server {
    listen 443 ssl;
    server_name indexer.example.com;
    ssl_certificate     /etc/letsencrypt/live/indexer.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/indexer.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8090;
        proxy_set_header Host              $host;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

## What it promises: the coverage floor

The promise is **gap-free and consistent from a known date to now, everything kept** — not "all of history". That known date is the chain's **coverage floor**, and it is the single most important thing to understand about this indexer.

### Where the floor comes from

It is chosen once, the first time a chain is indexed, and only then:

| What you gave it | The floor |
|---|---|
| nothing | **one year back from the moment you first start it**, resolved to a block |
| nothing, but the database already holds older blocks | the oldest block it already holds |
| `--start-date 2024-03-01` | the first block at or after midnight UTC of that day |
| `--start-block N` (N greater than 0) | block `N` |
| `--new-blocks-only` | the head |
| nothing, on Solana | the head |

A year is the default because it is what makes "all-time", "last 12 months" and every year-on-year comparison a real answer rather than an accident of the day you happened to start the indexer. It is measured from the newest block the source *has*, or from now, whichever is earlier: an archive that is behind would otherwise quietly give you less than a year.

Solana starts at the head instead, because Envio serves Solana only from 2026-01-03 and the free tier is slow — going back there is a choice, never a default.

### The floor never moves by itself

From then on it is a fact about your data, not a setting:

- a restart keeps it;
- a different `--start-block` or `--start-date` keeps it, and the log says so and tells you what to run instead;
- the control panel cannot touch it — it shows the line, read-only;
- moving it **later** is refused everywhere, by the database engine as well as by the code: data is never dropped, and a higher floor would be a claim your stored rows contradict;
- moving it **earlier** is one command, `indexer backfill --start-block N` (or `--start-date D`), which lowers the floor **after checking** that every block between `N` and the old floor really is stored and gap-free. If it is not, the floor stays where it was and the command says how many blocks are missing.

That last point has a catch worth knowing: `indexer backfill` re-decodes logs this database already has, it does not fetch new ones. So it moves the floor over blocks you already indexed — with an older version, or with an explicit `--start-block` — not over blocks nobody has ever asked for. Streaming a range below the floor that was never fetched is not implemented by any command.

### "All-time" means "since the floor"

Every total, every "all-time volume", every leaderboard covers the window that chain actually has. `coverage_v` is where a dashboard should read the window from. Two consequences are worth telling whoever reads your data:

- **A launchpad token launched before the floor** keeps its DEX data but has no launch attribution: the launch event is below the window. Documented, not fixed.
- **A prediction market created before the floor** would have no question and no outcomes, and its open interest could go negative. That one *is* fixed: `indexer run` reads the market metadata and the split / merge / redeem events from below the floor once, in the background, from the addresses in your own `prediction_trusted` — and stores no trade from down there, so volume means the same thing on both sides of the floor. See [`src/predictions/README.md`](src/predictions/README.md).

### Seeing the floor

```sh
indexer verify --chain 8453 --database http://default@localhost:8123/indexer
# Coverage: gap-free from 2024-09-19 (block 20779400) to 2025-09-19 (block 23400512).
```

`indexer verify` prints that line first and the detail after it: missing blocks, rows without their block, checkpoints that claim missing blocks, aggregates that disagree with the base tables. Exit status `0` means consistent, `1` means problems were found. `SELECT * FROM coverage_v` gives the same window for every chain, and the control panel shows it per chain.

## What is indexed

Everything below is decoded from logs that are fetched anyway, so the analytics modules cost no extra HyperSync traffic.

| Group | Tables | On by default | Query cookbook |
|---|---|---|---|
| **Core EVM** | `blocks`, `transactions`, `logs`, `withdrawals`, `erc20_transfers`, `erc721_transfers`, `erc1155_transfers`, `tokens`, the `contracts` view, and the lookup tables | always | [`src/core/README.md`](src/core/README.md) |
| **DEX** | `dex_pools`, `dex_swaps`, `dex_liquidity`, candles `1m`/`1h`/`1d`, volume and USD views | yes (`--no-dex` to opt out) | [`src/dex/README.md`](src/dex/README.md) |
| **Prediction markets** | `prediction_markets`, `prediction_trades`, positions, probability candles | yes (`--no-predictions`) | [`src/predictions/README.md`](src/predictions/README.md) |
| **Token launchpads** | `launchpad_tokens`, `launchpad_trades`, `launchpad_graduations`, `launchpad_creator_fees` | yes (`--no-launchpads`) | [`src/launchpads/README.md`](src/launchpads/README.md) |
| **Solana** | `sol_slots`, `sol_transactions`, `sol_tokens`, `sol_dex_swaps` and its candles, plus Solana launchpads in the same `launchpad_*` tables | always, on `--chain solana` | [`src/svm/README.md`](src/svm/README.md) |
| **Coverage, reorgs, fleet** | `chain_coverage` / `coverage_v`, `reorgs`, `checkpoints`, `fleet_chains` | always | [`src/coverage/README.md`](src/coverage/README.md), [`src/reorg/README.md`](src/reorg/README.md), [`src/fleet/README.md`](src/fleet/README.md) |

Three things the analytics modules have in common, and they are the reason they work on chains nobody has heard of:

- **Decoding is by event family, never by a router or factory registry.** A log is a swap when its `topic0` and its shape are the ones the family emits, so a Uniswap V2 fork works on day one. `protocol` is the family, not a brand.
- **Nothing is trusted by default.** A trade leg counts only when the asset contract reported the movement in the same transaction; the headline launchpad and prediction views count only addresses an operator listed (`launchpad_trusted_emitters`, `prediction_trusted`). The migrations seed nothing; the module READMEs ship the verified addresses as ready-to-run `INSERT`s.
- **Everything is re-decodable.** `indexer backfill --module dex|predictions|launchpads` decodes a module's rows again from the stored `logs`, with no re-sync and no HyperSync traffic, and is safe to run while `indexer run` is live.

## Requirements and hardware

- [ClickHouse](https://clickhouse.com/) 25.x. The bundled Compose file and CI use the 25.8 LTS line
- An [Envio HyperSync API token](https://docs.envio.dev/docs/HyperSync/api-tokens)
- *Optional:* your own JSON-RPC endpoint per chain, for token and pool metadata
- *Optional:* [Redis](https://redis.io/) or [Dragonfly](https://www.dragonflydb.io/) to cache that metadata across restarts
- [Rust](https://www.rust-lang.org/tools/install) (stable) when building from source, or [Docker](https://docs.docker.com/get-docker/) with Compose v2.24+

### Sizing

**Start small and grow.** Nothing about the indexer assumes a big machine, and the honest way to size one is to run a week of the chains you actually want and measure.

Rough guidance for planning, not a guarantee:

- **A handful of EVM chains, one year of history each**: a few hundred gigabytes of SSD. Ethereum is by far the heaviest; most chains are a fraction of it.
- **Solana is the expensive one: on the order of 12 GB a day, compressed**, even though this pipeline is program-filtered and analytics-only. Add it last, watch the disk for a week, and decide.
- **Everything together, year one**: plan for something like 8 TB of NVMe and 64–128 GB of RAM if you intend to keep Solana and a large fleet of EVM chains. You do not need that on day one.
- **Use SSD/NVMe.** `FINAL` is required for correctness on the base tables and the layout is built for it, but it is a read amplifier on spinning disks.
- Size ClickHouse for the number of **concurrent inserts**, not just for bytes: each chain commits every `--flush-interval-ms` while it follows the head, and `indexer fleet` keeps the sum of the write buffers under `--fleet-max-inflight-mb` (default 2048) precisely so that adding a chain makes everybody's batches smaller instead of making the process bigger.

## Where the data comes from: HyperSync and RPC

**Block data comes from HyperSync only**, and one Envio token serves every chain in the fleet. The HyperSync endpoint is derived from the chain id; `--hypersync-url` overrides it. A custom endpoint must be able to say which chain it serves: the indexer asks it at startup and refuses to start if it answers with a different chain or cannot answer at all. Your API token is sent to whatever endpoint you name, and an endpoint that cannot be checked could feed the indexer blocks from anywhere.

A JSON-RPC endpoint is needed for the one thing HyperSync cannot serve, `eth_call`: token `name` / `symbol` / `decimals`, and the tokens of DEX pools whose creation event was not indexed (partial syncs, Curve). DEX prices and volumes are meaningless without token decimals, which is why RPC access is on by default.

`--rpc` takes a comma-separated list; entries are tried in order with per-endpoint circuit breakers, and the chain id of every endpoint is verified.

| Value | Meaning |
|-------|---------|
| *(unset)* or `auto` | **Default.** Public endpoints for the chain id are discovered at startup. **This is an outbound request to a third party: the indexer downloads `https://chainid.network/chains.json`** and then sends `eth_call`s to the https endpoints listed there. Public endpoints are best effort: rate limited, no guarantees. A failed discovery never stops the indexer |
| `https://my-node.example,auto` | **Recommended for production.** Your endpoint first, public endpoints as fallback |
| `https://a.example,https://b.example` | Only your endpoints, with failover. No request to chainid.network |
| `none` | No RPC at all: `tokens` stays empty, DEX views cannot adjust decimals or compute USD values, pools without a creation event stay unresolved |

**Two providers have to agree.** A value is only written once two independent endpoints return the same answer for it, which is what keeps one rate-limited or hostile public endpoint from deciding a token's decimals — and therefore every price derived from them. The consequence is worth knowing before you pick a chain: **a chain with only one usable public endpoint will resolve no metadata on `auto` alone.** There is nothing to disagree with, so nothing is written, and DEX amounts on that chain stay unadjusted. Configure a real endpoint for it — `--rpc https://mine,auto` is enough, since your endpoint and one public one are two providers.

How the RPC path behaves otherwise:

- **Never on the commit path.** Discovered tokens are queued without blocking; if the queue is full they are dropped and found again by a periodic query for token addresses in the transfer tables (and `dex_pools`) that have no `tokens` row. An RPC outage of any length heals by itself.
- Tokens that revert or return garbage still get a row with empty metadata, so "checked, nothing there" is distinguishable from "not checked yet".
- An archive node is not required.
- `--redis` is optional and only saves repeated RPC calls across restarts. Keys are namespaced by chain, so every chain can share one instance.
- On Solana `--rpc` and `--redis` are ignored, with one log line: token decimals arrive free on every `account_activity` row.

## Commands and schema migrations

| Command | What it does |
|---------|--------------|
| `indexer fleet --database <url> --hypersync-token <token> [--chain N]...` | Index MANY chains in one process, with the web control panel. Applies pending migrations once, at start. See [Fleet mode](#fleet-mode-and-the-control-panel) |
| `indexer run [OPTIONS]` | Index ONE chain in this process. Applies pending migrations first (unless `--no-migrate`). `indexer [OPTIONS]` without a subcommand is the same thing |
| `indexer migrate --database <url> [--dry-run]` | Create the database if it is missing, apply pending migrations and exit. `--dry-run` only lists what is pending and creates nothing |
| `indexer verify --database <url> [--chain N] [--start-block A] [--end-block B]` | Read-only consistency check of what is stored for a chain. Its first line is the promise — `Coverage: gap-free from 2024-09-19 (block 20779400) to 2025-09-19 (block 23400512)` — and the rest is the detail. Exit status `0` = consistent, `1` = problems found |
| `indexer backfill --module dex\|predictions\|launchpads --database <url> [--chain N] [--from-block A] [--to-block B]` | Decode a module's rows again **from the stored `logs`** (no re-sync, no HyperSync traffic), e.g. after a decoder fix. Compares first and writes nothing when the stored rows already match; otherwise the module's rows of the affected range are replaced and every aggregate of the chain is rebuilt under a new epoch, so nothing is counted twice. Safe while `indexer run` is live on the same chain |
| `indexer backfill --module predictions --registry-only --database <url> [--chain N]` | Fetch what a prediction market created BELOW the coverage floor needs to be describable: its metadata, its question, its outcomes, and the split / merge / redeem events its open interest is made of. **No trade below the floor is stored.** `indexer run` does this by itself, once, in the background; this is the on-demand version |

The schema lives in `migrations/NNNN_name.sql` and is **compiled into the binary**; the container image needs no SQL files and ClickHouse needs no init scripts.

- Applied migrations are recorded in `schema_migrations (version, name, checksum, applied_at)`. Each start applies whatever is pending, in order.
- The indexer **refuses to start** when the checksum of an already applied migration differs from the one embedded in the binary, or when the database has a newer migration than the binary knows (an older binary against a newer schema). Never edit an applied migration; add a new one.
- Several indexer processes may start at the same time against an empty database; they converge on one schema.
- The database name comes from the URL, nothing is hard coded.
- To control when the schema changes (for example one deploy step in front of many indexer processes), run `indexer migrate` once and start the indexers with `--no-migrate`.

### Upgrade note: recreate databases made by earlier builds of this branch

**A database created by a pre-release build of this branch has to be dropped and created again.** The versioned `migrations/` set is new and was corrected in place while it was being written, so an older database carries different checksums and the indexer refuses to start against it, by design and with no `ALTER` path. Drop the database and let `indexer migrate` (or `indexer run`) create it again.

Do not force the checksum guard past this. Some of the corrections changed what a column *means* — `sol_token_balances` went from a latest-value projection to an append log with a flush-clock `_version` — and rows written under the old meaning would permanently outrank every new one. From the first released version on this never happens again: an applied migration is never edited.

## Configuration

Every CLI flag can also be set through the environment variable listed next to it. CLI flags take precedence. A blank variable (`VAR=`) counts as unset; boolean variables accept `true` / `false`, `1` / `0`, `yes` / `no`, `on` / `off`.

### `indexer fleet`

Everything here is the same for every chain in the process. The per-chain options come from the `fleet_chains` table and the control panel, **not** from the environment.

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--database` | `DATABASE_URL` | *required* | ClickHouse HTTP endpoint: `http://user:pass@host:port/db` (`https://` for TLS). Always include the port (`8123`, or `8443` for ClickHouse Cloud). The database is created when missing |
| `--hypersync-token` | `ENVIO_API_TOKEN` | *required* | [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens). One token serves every chain |
| `--chain <CHAIN>` | | *none* | Index this chain even when `fleet_chains` does not list it yet. **Repeatable**; a chain id or the name `solana`. A fresh database needs this once, after that the panel adds chains |
| `--rpc` | `RPC_URL` | `auto` | Default JSON-RPC endpoints, for chains whose own setting is empty. Same syntax as `indexer run --rpc` |
| `--redis` | `REDIS_URL` | *none* | Redis or Dragonfly URL for the token metadata cache, shared by every chain |
| `--metrics-addr` | `METRICS_ADDR` | *off* | `ip:port` for ONE `/metrics`, `/healthz` and `/readyz` for the whole fleet; every series carries its `chain` label |
| `--admin-addr` | `ADMIN_ADDR` | `127.0.0.1:8090` | Where the control panel listens. Refused unless it is a loopback address or `--admin-allow-remote` is given |
| | `ADMIN_PASSWORD` | *unset* | **Environment only, never a flag** (a flag is visible in `ps`). The panel is off and its port unbound while this is unset, and also while it is shorter than **12 characters**, one character repeated, or on the built-in denylist. Generate one with `openssl rand -base64 18` |
| `--admin-allow-remote` | | `false` | Allow the panel to bind something other than loopback. Only behind a TLS reverse proxy — or inside a container, where Compose publishes the port from outside |
| `--admin-secure-cookie` | | `false` | Mark the session cookie `Secure` (the panel is behind TLS) |
| `--admin-trust-forwarded-proto` | | `false` | Believe `X-Forwarded-Proto: https` from the proxy when deciding whether the cookie is `Secure`. Off by default: any client can set that header |
| `--admin-host <NAME>` | | *none* | A host name the panel answers to besides its own address and the loopback names. **Required behind a reverse proxy**; a request for any other name is refused with `421` before it is routed, which is what stops DNS rebinding. Repeatable |
| `--admin-trusted-proxy <IP>` | | *none* | The reverse proxy's address. Only a connection from exactly this address has its `X-Forwarded-For` believed, and then the sign-in throttle counts per client instead of counting everyone behind the proxy as one. Without it the header is ignored entirely |
| `--fleet-max-inflight-mb` | `FLEET_MAX_INFLIGHT_MB` | `2048` | Rough cap, in megabytes, on the rows the WHOLE fleet buffers before writing, split over the running chains |
| `--solana-queries-per-minute` | `SOLANA_QUERIES_PER_MINUTE` | `25` | Metered Solana HyperSync queries a minute, shared by every Solana chain in the process. The free tier allows 30 |
| `--no-migrate` | `NO_MIGRATE` | `false` | Do not apply pending migrations at startup. The fleet otherwise migrates once, before any chain starts |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

Note that `--chain`, and every `--admin-*` flag, have **no environment variable**: they are command-line only.

### `indexer run`

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--chain` | `CHAIN_ID` | `1` | Chain to index: a chain id, or the name `solana` (= `1399811149`). One process per chain: a second `indexer run` on the same chain and database refuses to start |
| `--database` | `DATABASE_URL` | *required* | As above |
| `--hypersync-url` | `HYPERSYNC_URL` | derived from the chain id | HyperSync endpoint. Only needed to override the default for the chain, or for a private endpoint. The indexer asks it which chain it serves and refuses to start on a wrong or missing answer |
| `--hypersync-token` | `ENVIO_API_TOKEN` | *required* | As above |
| `--rpc` | `RPC_URL` | `auto` | Comma-separated JSON-RPC endpoints for token / pool metadata `eth_call`s. `auto` = discover public endpoints, `none` = disable. See [above](#where-the-data-comes-from-hypersync-and-rpc) |
| `--redis` | `REDIS_URL` | *none* | As above. Without it an in-memory cache is used |
| `--start-block` | `START_BLOCK` | `0` | First block to index. **Read only on a chain's FIRST start**: it sets the [coverage floor](#what-it-promises-the-coverage-floor), which is fixed from then on. `0` means "not given", so the default applies: one year back on EVM, the head on Solana |
| `--start-date` | `START_DATE` | *unset* | The same decision as a date, `YYYY-MM-DD` in UTC, resolved to a block by a binary search over block timestamps (about 25 requests, once). Mutually exclusive with `--start-block`. Also read only on the first start |
| `--end-block` | `END_BLOCK` | `0` | Block to stop at, **exclusive**: blocks up to `--end-block - 1` are indexed and the process exits. `0` = follow the chain head |
| `--confirmations` | `CONFIRMATIONS` | `0` | Stay this many blocks behind the chain head. Optional: reorgs are repaired either way, see [Reorgs](#reorgs-in-detail) |
| `--max-reorg-depth` | `MAX_REORG_DEPTH` | `512` | Deepest rollback the indexer performs on its own. A deeper fork stops the process with an error |
| `--new-blocks-only` | `NEW_BLOCKS_ONLY` | `false` | Start from the current chain height instead of `--start-block` (skip the historical sync). The coverage floor is then the head |
| `--flush-rows` | `FLUSH_ROWS` | `100000` | Flush to ClickHouse once this many rows are buffered |
| `--flush-interval-ms` | `FLUSH_INTERVAL_MS` | `2000` | Maximum time in milliseconds between flushes during a historical sync. While following the chain head the indexer commits at most once every 2x this value (4 s by default): every commit is one synchronous ClickHouse insert per table, and fewer, larger inserts are what keeps a server shared by many chains healthy |
| `--no-dex` | `NO_DEX` | `false` | Turn [DEX analytics](#dex-analytics) off |
| `--no-predictions` | `NO_PREDICTIONS` | `false` | Turn [prediction-market analytics](#prediction-markets) off |
| `--no-launchpads` | `NO_LAUNCHPADS` | `false` | Turn [launchpad analytics](#token-launchpads) off |
| `--metrics-addr` | `METRICS_ADDR` | *off* | `ip:port` to serve `/metrics`, `/healthz` and `/readyz` on. See [Metrics](#metrics-and-health-checks) |
| `--no-migrate` | `NO_MIGRATE` | `false` | Do not apply pending migrations at startup |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

### `indexer migrate`

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--database` | `DATABASE_URL` | *required* | The database is created when missing |
| `--dry-run` | | `false` | List the pending migrations without applying (or creating) anything |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

### `indexer verify`

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--chain` | `CHAIN_ID` | `1` | Chain to verify: a chain id, or `solana` |
| `--database` | `DATABASE_URL` | *required* | |
| `--start-block` | `START_BLOCK` | `0` | First block to verify. On Solana, a slot |
| `--end-block` | `END_BLOCK` | `0` | Block to stop at, exclusive. `0` verifies up to the highest indexed block |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

### `indexer backfill`

| Flag | Environment variable | Default | Description |
|------|----------------------|---------|-------------|
| `--module` | | *required* | `dex`, `predictions` or `launchpads` |
| `--chain` | `CHAIN_ID` | `1` | Chain to backfill: a chain id, or `solana` |
| `--database` | `DATABASE_URL` | *required* | |
| `--from-block` (alias `--start-block`) | | `0` | First block to re-decode. Below the chain's coverage floor this lowers the floor too, but only after a check that every block in between really is stored and gap-free |
| `--from-date` (alias `--start-date`) | | *unset* | The same as a `YYYY-MM-DD` UTC day. Mutually exclusive with `--from-block` |
| `--registry-only` | | `false` | Prediction markets only: fetch the metadata and the settlement events of markets created below the coverage floor, from the module's own trusted addresses. No trades are stored outside the covered window |
| `--to-block` | | `0` | Block to stop at, exclusive. `0` = up to the highest indexed block |
| `--chunk-blocks` | | `2000` | Blocks re-decoded per chunk |
| `--debug` | `DEBUG` | `false` | Enable debug logging |

`--from-block` / `--from-date` / `--to-block` deliberately have **no environment fallback**: the `START_BLOCK` and `END_BLOCK` of a compose file describe the sync, not a one-off backfill.

See [`.env.example`](.env.example) for a commented template. For Docker Compose it additionally carries the ClickHouse container credentials (`CLICKHOUSE_USER`, `CLICKHOUSE_PASSWORD`, `CLICKHOUSE_DB`) and the host ports `METRICS_PORT` and `ADMIN_PORT`, which are read by `docker-compose.yml` and not by the indexer.

Any network available on HyperSync can be indexed: see the [list of supported networks](https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks).

## Reorg safety, in five sentences

1. **The indexer never deletes anything**: no `DELETE`, no `ALTER ... DELETE / UPDATE`, no `DROP PARTITION`, because with many processes sharing one ClickHouse those are not reliable, while concurrent inserts need no coordination at all.
2. **Removing a row is inserting a tombstone** — a copy of it with a newer `_version` and `is_deleted = 1` — which `FINAL` hides, and which the materialized views carry into every lookup table for free.
3. **Aggregates are fixed with epochs, not deletes**: every row carries the chain's rollback generation, a rollback starts a new one and re-aggregates the affected buckets under it, and the `*_v` views count only contributions whose epoch is current for their bucket.
4. **So readers have exactly two rules**: query base and lookup tables with `FINAL`, and query aggregates through their `*_v` views — follow those and you never see orphaned data, even mid-repair.
5. **Every rollback is recorded** in the `reorgs` table and in the metrics, and a fork deeper than `--max-reorg-depth` stops the process with an error instead of rewriting that much history on its own.

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

Bookkeeping: `schema_migrations`, `checkpoints` (committed block ranges), `reorgs` (audit trail of every rollback and gap repair), `chain_coverage` / `coverage_v` (the floor), `chains` (the naming registry), `fleet_chains` (which chains the fleet runs).

Storage conventions:

- **No hex strings.** Hashes and topics are `FixedString(32)`, addresses `FixedString(20)`, calldata / log data raw bytes in `String`, the 4-byte selector `FixedString(4)`. In the chain-neutral analytics tables (`dex_*`, `prediction_*`, `launchpad_*`) every identity column is `FixedString(32)`: an EVM address is 12 zero bytes plus the 20 address bytes, a Solana pubkey is 32 raw bytes.
- **Exact amounts.** Wei values, gas prices and token amounts / ids are `UInt256` (signed DEX amounts `Int256`). Aggregates sum `Float64` on purpose: `sum()` over 256-bit integers wraps silently and spam tokens emit amounts near `2^256`. The exact values are always in the base tables.
- `Nullable` only where NULL means something different from the default (for example `transactions.to` on contract creations, `status` before Byzantium, EIP-1559 fee fields on legacy transactions).
- Every block-scoped table is `ReplacingMergeTree(_version, is_deleted)` with positional sorting keys, so a re-inserted block replaces itself. Base tables are partitioned by month only (never by chain: `chain` is the first sorting-key column, which is what prunes reads), lookup tables by chain.
- The technical columns `_version`, `is_deleted` and `epoch` belong to the [reorg](#reorgs-in-detail) machinery. Do not write them.

The full schema is in [`migrations/`](migrations); the rationale is in [`docs/design.md`](docs/design.md).

## Querying the data

Two rules make every query correct, including right after a reorg or a crash:

1. **Query base and lookup tables with `FINAL`.** `FINAL` collapses re-inserted rows and hides rolled-back ones. Without it you can see duplicates and rows of orphaned blocks.
2. **Query aggregates through their `*_v` views**, never the underlying `AggregatingMergeTree` tables. The views finalize the aggregate states and apply the reorg validity rule.

Always filter by `chain` first: it is the first sorting-key column of every table. Format bytes with `concat('0x', lower(hex(x)))` and filter with `unhex('...')` (no `0x` prefix, any letter case).

Per-module cookbooks live in the module READMEs listed under [What is indexed](#what-is-indexed). A few core examples:

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

- On every flush, `blocks` rows are written **last**, after the transactions, logs, transfers and other rows of the same batch. A row in `blocks` is the commit marker for that block: if it is there, the rest of the block's data is too. After `blocks`, the flush records the committed range in `checkpoints`.
- The first sync pass after a start also runs a gap-detection query over `blocks` for the configured range, from the [coverage floor](#what-it-promises-the-coverage-floor) (inclusive) to `--end-block` (**exclusive**), or to the chain head minus `--confirmations` when `--end-block` is `0`, and streams only what is missing.
- If the process died in the middle of a flush, the affected heights have transactions or logs but no `blocks` row. Before such a gap is streamed again, its leftovers are removed with the same rollback primitive that repairs reorgs (recorded in `reorgs` with `reason = 'gap_heal'`). Nothing is ever inserted twice, which is what keeps the incremental aggregates exact.
- With `--end-block 0` the indexer keeps following the chain head after the backfill is complete. With `--end-block N` it exits with status `0` once every block below `N` is stored. With `--new-blocks-only` the historical backfill is skipped.

Restarting the indexer with the same arguments is always safe, and it is also how holes left by a crash get repaired.

## Reorgs in detail

A reorg is the chain replacing its most recent blocks with different ones. The indexer **detects and repairs reorgs by itself**; no operator action is needed for ordinary ones. The short version is [above](#reorg-safety-in-five-sentences); this is what happens step by step:

1. **Detection.** Every streamed block must name the previously stored block as its parent (on a restart the comparison starts from the hash stored in the database), and HyperSync's rollback guard is checked as well. A mismatch means the stored chain is no longer the canonical one.
2. **Fork-point search.** The indexer fetches the canonical headers of the last 8, then 16, 32, ... blocks and compares them with the stored hashes until it finds the highest block both agree on. The search is bounded by `--max-reorg-depth` (default `512`).
3. **Rollback.** Everything above the fork point is removed for that chain, children first and `blocks` last, the daily and candle aggregates of the touched days are rebuilt under a new epoch, and the rollback is recorded in `reorgs`.
4. **Resume.** Streaming continues from the fork point and stores the canonical blocks.

Things worth knowing:

- A rollback is idempotent and crash safe: if the process dies half way, the next start detects the same mismatch and runs it again under a newer epoch.
- For a moment during the repair, the aggregates of the affected day **under-count** (the stale contributions are already invalid, the rebuilt ones not yet written). They never double count.
- Chains do not affect each other, and there is no lock between indexer processes.
- Rolled-back rows stay on disk until ClickHouse merges the tombstones away; the volume is negligible and a cleanup is never required for correctness.

### The audit table and alerts

```sql
SELECT detected_at, reason, fork_block, depth, rows_tombstoned,
       concat('0x', lower(hex(old_hash))) AS old_hash,
       concat('0x', lower(hex(new_hash))) AS new_hash
FROM reorgs
WHERE chain = 1 AND completed = 1
ORDER BY epoch DESC
LIMIT 20;
```

`reason` is `reorg`, `gap_heal` or `redecode`. The metrics `evm_indexer_reorgs_total`, `evm_indexer_reorg_last_depth` and `evm_indexer_purge_duration_seconds` expose the same events; an example alert is in [`src/metrics/README.md`](src/metrics/README.md).

### `--max-reorg-depth`

If no common block is found within `--max-reorg-depth` blocks, the indexer **stops with an error** instead of rewriting that much history on its own: a fork that deep usually means a wrong endpoint, a chain incident or a database shared with a different network. Nothing is modified. After checking, restart with a larger `--max-reorg-depth` to let it proceed.

### `--confirmations`

`--confirmations N` is an optional head lag: only blocks at least `N` behind the head are indexed, so most reorgs are never stored in the first place. It is a trade, not a requirement:

- `0` (default): lowest latency. Reorged blocks are briefly visible and then repaired as described above.
- `N` around the chain's usual reorg depth (for example `12` to `64` on Ethereum-like proof-of-stake chains; 64 blocks is two epochs, i.e. finality on Ethereum mainnet): readers practically never see data that later changes, fewer rollbacks, data shows up `N` blocks later.
- Chains with instant finality: leave it at `0`.

## DEX analytics

On by default; turn it off with `--no-dex`.

**Decoding is by event family, never by a router or factory registry.** A log is a swap when its `topic0` and its shape are the ones the family emits, so a Uniswap V2 fork on a chain nobody has heard of works on day one. `protocol` is therefore the family, not a brand:

| `protocol` | Pools from | Swaps | Liquidity |
|------------|------------|-------|-----------|
| `uniswap_v2` | `PairCreated` | `Swap` (also emitted by Solidly V1 forks) | `Sync`, `Mint`, `Burn` |
| `solidly` | Solidly V1 `PairCreated`, Velodrome V2 / Aerodrome `PoolCreated` | Velodrome V2 / Aerodrome `Swap` | `Sync`, `Burn` |
| `uniswap_v3` | `PoolCreated`, Slipstream, Algebra | `Swap`, PancakeSwap V3 and Algebra variants | `Mint`, `Burn` |
| `uniswap_v4` | `Initialize` (PoolManager) | `Swap` | `ModifyLiquidity` |
| `balancer_v2` | `PoolRegistered` + `TokensRegistered` (Vault) | `Swap` | - |
| `curve` | over RPC (no creation event) | `TokenExchange`, `TokenExchangeUnderlying` | - |

Tables: `dex_pools`, `dex_swaps`, `dex_liquidity`, plus the lookup tables `dex_swaps_by_pool`, `dex_swaps_by_trader`, `dex_pools_by_token` and the operator-populated `dex_trusted_emitters` and `quote_tokens`. Aggregates: per-pool candles `dex_candles_1m` / `_1h` / `_1d` and `dex_pool_volume_1h`. They follow the same storage and reorg rules as everything else.

Analyst views join tokens and pools at query time, so results improve as the background resolvers fill in metadata; unknown stays `NULL`, never `0`:

| View | What |
|------|------|
| `dex_pools_v` | pools with token symbols and decimals |
| `dex_swaps_v`, `dex_swaps_usd_v` | every swap as token in / token out with adjusted amounts, whatever the family; plus `amount_usd` |
| `dex_pool_prices_1m_v` / `_1h_v` / `_1d_v` | decimals-adjusted candles ([example](#querying-the-data)) |
| `dex_native_price_1h_v` | USD price of the native coin, per hour |
| `dex_pool_volume_1h_v`, `dex_pool_volume_usd_1h_v` / `_1d_v` | volume per pool, raw and in USD |
| `dex_protocol_volume_usd_1d_v`, `dex_token_volume_1d_v`, `dex_pool_stats_1d_v`, `dex_protocol_stats_1d_v` | daily rollups per family, per token, per pool |
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

## Prediction markets

**On by default** (`--no-predictions` to opt out). Same shape as DEX analytics: decoded by event family, candles of implied probability, trades, positions, portfolios and leaderboards. Nothing about any one venue is configured anywhere — a fork on a chain nobody has heard of is decoded on day one.

The headline views count only the registry and exchange addresses an operator has listed in `prediction_trusted`; the module README ships them as ready-to-run `INSERT`s. Markets created below the coverage floor are handled by the registry-only history pass described [above](#all-time-means-since-the-floor).

Tables, views and the query cookbook: [`src/predictions/README.md`](src/predictions/README.md).

**Perpetual futures are deferred**, deliberately: only about 4% of perp volume is readable from EVM event logs on chains HyperSync serves, and there is almost no "one ABI, many forks" effect to exploit. The reasoning and the candidate venue list are in [`docs/design.md`](docs/design.md), section 17.2.

## Token launchpads

**On by default** (`--no-launchpads` to opt out). Decoded by event family from the same logs: bonding-curve launches, buys and sells, fee sweeps and graduations into a DEX pool, plus launch attribution for venues that launch straight into a Uniswap V3 / V4 pool (their trading is already in `dex_swaps`).

Nothing is trusted by default: a trade leg counts only when the asset contract reported the movement in the same transaction, and the headline views count only emitters an operator listed in `launchpad_trusted_emitters` (the module README ships the verified addresses as ready-to-run `INSERT`s; the migrations seed nothing). Front ends such as GMGN or Axiom are **not venues** — they have no contracts of their own, and their volume is never added to a venue's.

Tables, views and the query cookbook: [`src/launchpads/README.md`](src/launchpads/README.md).

## Solana

`--chain solana` indexes Solana into the same database and the same analytics tables as every EVM chain, from Envio's Solana HyperSync. It is a **separate sync loop** (`src/pipeline/solana.rs`) because three things genuinely differ; everything else — the ClickHouse insert path, the tombstone / epoch machinery, the one-process-per-chain lease, the metrics — is shared.

**Analytics only, and program filtered.** Solana produces ~150M non-vote transactions a day and the value sits in a couple of dozen programs, so the indexer asks for those programs and nothing else. `sol_transactions` is *the matched transactions*, not the chain's; there is no wallet history, no chain-wide transfer table and deliberately no daily chain statistics. [`src/svm/README.md`](src/svm/README.md) says what that rules out, and why.

In `indexer fleet` it is one more `--chain solana` (or `--chain 1399811149`) and nothing else. On its own:

```sh
# follow the head from now on (what you want first)
indexer run --chain solana --new-blocks-only --metrics-addr :9090

# or from a specific slot
indexer run --chain solana --start-block 448000000
```

`CHAIN_ID` accepts the name too, so a compose file needs no new variable. The indexer writes the `chains` registry row itself at startup, which is what tells a view to print a Solana identity with `base58Encode` instead of as an EVM address.

**`--start-block` is a SLOT**, and Envio serves Solana only from slot **391,000,000** (2026-01-03). A lower value is refused at startup rather than left to spin, because a query below the served history comes back empty *without advancing the cursor*, which a resume loop cannot tell from "caught up". Anything older exists only in the Old Faithful archive and would need a second ingest path (`docs/design.md`, section 14.3).

### Flags that differ on Solana

| Flag | On Solana |
|---|---|
| `--confirmations` | **refused** unless 0. Envio serves Solana at (just behind) `finalized`, so staying further back costs freshness twice and protects against nothing |
| `--no-dex` | **refused**: with the DEX decoder off this pipeline would store empty slot headers and nothing else |
| `--start-block` | a slot, and at least 391,000,000 |
| `--start-date` | **refused**. A slot carries no timestamp, so there is nothing to resolve a date against, and this indexer will not guess one into a coverage floor it can never move later. Use `--start-block <slot>` |
| the default start | the **head**, not a year of history |
| `--rpc`, `--redis` | ignored, with one log line. Token decimals arrive free on every `account_activity` row |
| `--max-reorg-depth` | ignored: there is no fork-point search on this chain (see below) |
| `--no-predictions`, `--no-launchpads` | `--no-launchpads` is honoured (Solana launchpads write the same `launchpad_*` tables); `--no-predictions` is ignored, as no prediction decoder has a Solana front end |

### Skipped slots, gaps and the tripwire

**A slot with no block is normal**, not a gap: Solana simply produces no block for it. So the Solana pipeline answers "what is missing?" from the **checkpoint tiling** rather than from the rows — a checkpoint's `to_block` is the *server's* `next_slot`, not `max(slot) + 1`, and a hole in that tiling is the only thing that can mean "we never asked for these slots". Continuity between stored slots is `block_height + 1` (Solana's `block_height` counts *produced blocks*, so it is immune to skipped slots) plus the `parent_slot` / `parent_blockhash` pair.

**There is no fork-point search.** On data served at finality there is no fork to find, so a continuity break is treated as what it is — something that is not supposed to happen. The indexer **stops**, loudly, with a message naming the slot, both hashes and what to check; nothing at or above the break is written. It is not repaired silently, because the repair would be indistinguishable from a wrong endpoint. The tombstone / epoch machinery stays fully in place and is used for gap heals: a flush that died between its children and its `sol_slots` insert is purged before the range is streamed again.

`indexer verify --chain solana` runs the Solana checks (cursor tiling, height chain, parent chain, orphan rows, candles against `sol_dex_swaps`) and reports skipped slots as skipped rather than missing. Give it the slot you actually started from — `indexer verify --chain solana --start-block 448378313`. It defaults to 0, and after a `--new-blocks-only` run that is honest but unhelpful: everything below the start really was never asked for, so the report is one enormous hole.

### Cost and rate limit

The free Envio token is **30 queries per 60 seconds per endpoint** (a flat cost per query, whatever it returns), and the Solana endpoint has its own budget — adding Solana does not eat the EVM chains'. Following the head needs 5 to 15 of those, so the follower caps itself at 25 (`--solana-queries-per-minute`) and additionally honours the `x-ratelimit-*` headers of every response. `GET /height` is free and unmetered, so discovering that nothing happened never costs a query.

Loading the **history** is the expensive part and is not wired up: 8.5 months is ~57M slots, which is weeks of the free budget. The options and their costs are recorded in [`docs/design.md`](docs/design.md), section 14.3.

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

## Fleet mode and the control panel

`indexer fleet` is **one process, many chains**, with a small web page to start and stop them. It is the way to run this, and the [quick start](#quick-start) uses it.

```bash
indexer fleet \
  --database http://indexer:indexer@localhost:8123/indexer \
  --hypersync-token <your-envio-api-token> \
  --chain 1 --chain 8453 --chain solana \
  --metrics-addr 127.0.0.1:9090
```

Each chain runs in its own task and calls exactly the same code `indexer run` calls, so every safety property is unchanged: its own lease, its own epoch, its own tombstones, its own writer. What the fleet adds is a supervisor.

- **One chain failing never touches the others.** It is restarted on its own, waiting 2 s, 4 s, 8 s ... up to 5 minutes, and the last error is kept.
- **A chain another process already indexes is a state, not an error.** It shows as "running elsewhere" and is looked at again every 30 seconds, so it takes over the moment the other process stops.
- **Migrations run once**, before any chain starts.
- **One `/metrics` for the whole fleet.** Every series carries its `chain` label, exactly as it does for a single chain, so one scrape target replaces one per chain.
- **Shared budgets.** One HyperSync token serves every chain and the provider meters the token, so the Solana query allowance (`--solana-queries-per-minute`, default 25) is one budget for the process. `--fleet-max-inflight-mb` (default 2048) is likewise split over the running chains.

Which chains the process indexes is remembered in the `fleet_chains` table (read once, at start), so a restart comes up the way you left it. `--chain` adds a chain the table does not know yet, which a fresh database needs once; after that the panel is the place to add them. Adding a chain needs nothing but its id — every `indexer run` default applies.

### The control panel

Set `ADMIN_PASSWORD` and the fleet serves a single page on `--admin-addr` (default `127.0.0.1:8090`). **Without `ADMIN_PASSWORD` the panel is off** and the port is not bound. The password is read from the environment only and never from a flag: a flag is visible to every user on the host through `ps`.

The page shows, per chain: a status badge, how far behind it is in blocks and in time, its speed, when it last wrote and how long that took, the reorganizations it has seen, its coverage line, and the last problem in plain words. The buttons are Start, Stop, Restart, Settings, and Add a chain — and that is the whole list. **The panel cannot delete, purge, re-index or change the schema; there is no endpoint for any of it.** Stop is the same graceful stop `ctrl-c` does: the chain writes what it has buffered, lets go of the chain and stops, while the others keep indexing.

Settings changed in the panel apply the next time that chain starts; press Restart to apply them now. They are validated by the same parser the command line uses, so the page cannot accept a configuration `indexer run` would refuse.

**What the panel can change is a short list on purpose**: how far behind the head to stay, how deep a rollback may go, how big and how frequent the writes are, and which decoders run. It cannot change where a chain reads from — the HyperSync endpoint and token, the RPC endpoints, the database and the cache come from how you started the process, and the panel shows them only as "set" or "not set". A web page that could redirect an endpoint could send your Envio API token to someone else's server and feed the indexer made-up blocks, so that door is closed rather than guarded. It also cannot change a chain's start block: that fixes the coverage floor, which is decided once, on a chain's first start.

Chains that a DIFFERENT indexer process is writing into the same database appear in the list read-only, so you can see the whole database from one page without being able to interfere with a process this one does not own.

Reaching the panel from another machine is covered in the [quick start](#reaching-the-panel-from-another-machine).

## Single-chain `indexer run`

`indexer run` indexes one chain per process and stays exactly as it is. Use it when a chain needs something the fleet cannot give it per chain:

- **A private or non-default HyperSync endpoint.** `--hypersync-url` is process-wide in the fleet and is deliberately **not** editable from the control panel, because the HyperSync token is attached to whatever URL a chain is configured with — a web page that could redirect it could send your token to someone else's server. So a chain that must talk to its own endpoint gets its own process:

  ```bash
  indexer run --chain 4242 \
    --database http://indexer:indexer@localhost:8123/indexer \
    --hypersync-url https://my-private-endpoint.example \
    --hypersync-token <the token that endpoint accepts> \
    --metrics-addr 127.0.0.1:9091
  ```

  It writes into the same database as the fleet, needs no coordination with it, and shows up in the panel read-only as "running elsewhere". Do not list that chain in the fleet's `--chain` set as well: the lease would refuse the second process anyway.

- **A bounded run.** `--end-block N` indexes up to `N - 1` and exits with status `0`.
- **A different coverage floor for one chain**, set on its first start with `--start-block` or `--start-date`.

Everything else — the safety properties, the tables, the metric names — is identical to a chain inside the fleet.

## Metrics and health checks

`--metrics-addr <ip:port>` (default: off) serves, on a small built-in HTTP server:

| Endpoint | Answer |
|----------|--------|
| `GET /metrics` | Prometheus text format. Every series is prefixed `evm_indexer_` and labelled `chain="<chain id>"` |
| `GET /healthz` | `200` while the process is alive (liveness) |
| `GET /readyz` | `200` when startup completed, the most recent flush did not fail, no flush has been retrying for more than 2 minutes, and the last successful flush or head poll is recent; otherwise `503` with a one-line reason. **Readiness, not liveness:** use it to take a lagging indexer out of a dashboard or load balancer, never to restart the process (a ClickHouse outage makes it not ready, and a restart loop would not help). Use `/healthz` for liveness |

The most useful series: `evm_indexer_head_block`, `evm_indexer_indexed_block`, `evm_indexer_lag_blocks`, `evm_indexer_lag_seconds`, `evm_indexer_rows_inserted_total{table}`, `evm_indexer_flush_duration_seconds`, `evm_indexer_flushes_total{result}`, `evm_indexer_reorgs_total`, `evm_indexer_reorg_last_depth`, `evm_indexer_resolver_queue_depth{worker}` and `evm_indexer_resolver_endpoints_healthy{worker}`. The full reference and ready-made alert rules are in [`src/metrics/README.md`](src/metrics/README.md).

With `indexer fleet` there is ONE endpoint for every chain in the process; with `indexer run` there is one per process. The series are identical and each carries its own `chain` label, so a dashboard built for one shape works for the other:

```yaml
scrape_configs:
  - job_name: evm-indexer
    static_configs:
      - targets: ["127.0.0.1:9101"]
```

The Compose health check calls `/healthz`. The runtime image ships neither `curl` nor `wget`, so it uses bash's `/dev/tcp`; the same one-liner works for any other orchestrator:

```bash
bash -c "exec 3<>/dev/tcp/127.0.0.1/9090 && printf 'GET /healthz HTTP/1.0\r\n\r\n' >&3 && head -n 1 <&3 | grep -q ' 200 '"
```

**The metrics endpoint has no authentication and no loopback guard.** Unlike `--admin-addr`, `--metrics-addr` accepts any address, and a bare `:9090` even expands to `0.0.0.0:9090`. It exposes no secret, but in fleet mode that one endpoint publishes every chain's position, lag and error counts. Bind it to localhost or a private network, or put it behind the same reverse proxy as the panel.

## Performance tuning

### Flush size and interval
- `--flush-rows` bounds how many rows are buffered in memory before a flush. Larger values produce fewer, bigger ClickHouse parts at the cost of memory; raise it during a backfill
- `--flush-interval-ms` bounds how long rows wait in the buffer, which is what matters when following the chain head
- In the fleet, `--fleet-max-inflight-mb` caps the sum across chains, so these are an upper bound rather than a per-chain guarantee

### Token metadata
- Put your own endpoint in front of `auto` (`--rpc https://mine,auto`); public endpoints are rate limited, and a chain with only one usable public endpoint resolves nothing at all (see [above](#where-the-data-comes-from-hypersync-and-rpc))
- Use Redis or Dragonfly (`--redis`) so token metadata survives restarts and is not requested again

### ClickHouse
- Use SSD or NVMe storage
- Always filter by `chain`, and by a block or time range where you can; go through the lookup tables instead of scanning a base table by hash or address
- `FINAL` is required for correctness. The tables are laid out for it (`do_not_merge_across_partitions_select_final`, month partitions), and a narrow filter keeps it cheap
- With many chains, size ClickHouse for the number of concurrent inserts: each chain flushes every `--flush-interval-ms` while it follows the head

## Upgrading

**From an earlier build of this branch:** see [the upgrade note above](#upgrade-note-recreate-databases-made-by-earlier-builds-of-this-branch) — the database has to be recreated.

**From 2.x:** there is **no migration path**. The schema is new (binary storage, different sorting keys, different tables), so a 2.x database cannot be converted or reused:

1. Point `--database` at a **fresh, empty database** (a new name on the same server is fine; the indexer creates it).
2. Get an [Envio API token](https://docs.envio.dev/docs/HyperSync/api-tokens): data now comes from HyperSync instead of JSON-RPC / WebSocket.
3. Resync. Drop the old database yourself when you no longer need it; the indexer never touches it.

Flags that no longer exist: `--rpcs`, `--ws`, `--batch-size`, `--fetch-uncles`, `--traces` (traces are not indexed at all), and `--dex` (DEX analytics are on by default; the opt-out is `--no-dex`). `--rpc` now only serves token / pool metadata. `--database` must be an `http(s)://` URL of the ClickHouse HTTP interface (port `8123`, or `8443` for TLS).

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
  cargo test --locked --lib -- --ignored --test-threads=1 \
  db::integration_tests:: db::migrate::integration:: dex::integration_tests::
```

The full set of database suites, which is what CI runs, is:

```text
db::integration_tests::        db::migrate::integration::
pipeline::acceptance::         pipeline::solana_acceptance::
dex::integration_tests::       predictions::integration_tests::
launchpads::integration_tests::  svm::integration_tests::
fleet::integration_tests::
```

Two things to get right when running them by hand:

- **Never use the bare filter `svm::`.** It also selects `svm::live_tests`, which need an Envio token and the network.
- **Use `--test-threads=1`**, and give each suite its own `_test` database. A single ClickHouse server accumulating dozens of test databases can hit its memory limit; CI runs the suites as a matrix, one database per suite, for exactly that reason.

`db::migrate::integration` needs a server whose **access storage is writable**: one of its tests runs `CREATE USER ... GRANT SELECT, INSERT` to prove that a least-privilege user can start once the migrations are applied. The `clickhouse/clickhouse-server` image used by `docker-compose.yml` and by CI has one out of the box (a `<user_directories>` with a `<local_directory>` at `/var/lib/clickhouse/access/`), and `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` lets the configured user use it. A ClickHouse started by hand from a config with `users_xml` alone refuses with *"there are no writable access storages"*, so give it:

```xml
<user_directories>
  <users_xml><path>users.xml</path></users_xml>
  <local_directory><path>/var/lib/clickhouse/access/</path></local_directory>
</user_directories>
```

CI also runs the Redis cache round trip (`redis_round_trip_and_restart`, `TOKEN_CACHE_TEST_REDIS_URL`). The remaining ignored tests (`live_*`) need internet access and a token, and are meant to be run by hand.

Schema changes are new files in `migrations/` (`NNNN_name.sql`; `0001`-`0009` core, `0010`-`0019` DEX, `0020`-`0029` prediction markets, `0030`-`0039` launchpads, `0040`-`0049` Solana, `0090`+ cross-module). Never edit a migration that has been released: its checksum is verified at startup.

`tests/layout.rs` keeps the module layout of [`docs/design.md`](docs/design.md) section 12 from rotting; `docs/design.md` is the only design document and is binding.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see LICENSE file for details

## Support

- GitHub Issues: [Report bugs](https://github.com/eabz/evm-indexer/issues)
- Discussions: [Ask questions](https://github.com/eabz/evm-indexer/discussions)
