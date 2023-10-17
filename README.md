<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>Scalable SQL indexer for EVM compatible blockchains</strong>
</p>

![build](https://github.com/eabz/evm-indexer/actions/workflows/build.yml/badge.svg)

An indexer is a program that fetches and stores blockchain data for later analysis.

This indexer is specifically created to parse known data for EVM compatible chains.

It stores all the blockchain primitives (blocks, transactions, receipts, logs, traces, withdrawals) and some other useful information (contracts created, dex trades, erc20 transfers, erc721 transfers, erc1155 transfers)

## Requirements

- [Rust](https://www.rust-lang.org/tools/install)
- [ClickHouse](https://clickhouse.com/)

## Install

You can install the indexer from the crate public repositor

```
cargo install evm-indexer
```

And run it

```
evm-indexer --rpcs "" --database ""
```

## Build

You can try the indexer locally or through Docker.

### Local

1. Clone the repository

```
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Build the program

```
cargo build --release
```

3. Copy the `.env.example` file to `.env` and add your environment variables.

### Docker

1. Clone the repository

```
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Build the image and tag it as `indexer`

```
docker build . -t indexer
```

3. Copy the `.env.example` file to `.env` and add your environment variables.

## Program flags

| Flag            | Default | Purpose                                                |
| --------------- | :-----: | ------------------------------------------------------ |
| `--debug`       |  false  | Start log with debug.                                  |
| `--chain`       |    1    | Number identifying the chain id to sync.               |
| `--start-block` |    0    | Block to start syncing.                                |
| `--end-block`   |    0    | Last block to sync (0 to sync all the blocks).         |
| `--batch-size`  |   200   | Amount of blocks to fetch in parallel.                 |
| `--rpcs`        | `empty` | Comma separated list of rpcs to use to fetch blocks.   |
| `--database`    | `empty` | Clickhouse database string with username and password. |
| `--ws`          | `empty` | Url of the websocket endpoint to fetch new blocks.     |
