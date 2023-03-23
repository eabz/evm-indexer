<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>A scalable SQL indexer for EVM compatible blockchains</strong>
</p>

<p align="center">
<strong>🚧 🚨 🚧 🚨 This indexer is a work in progress don't use it for production 🚧 🚨 🚧 🚨</strong>
</p>

## Requirements

- [Rust](https://www.rust-lang.org/tools/install)
- [CockroachDB](https://www.cockroachlabs.com/)
- [Redis](https://redis.io/)

## Install

You can try the indexer locally or through Docker.

### Local

1. Clone the repository

```
git clone https://github.com/llamafolio/evm-indexer && cd evm-indexer
```

2. Build the program

```
cargo build --release
```

3. Copy the `.env.example` file to `.env` and add your environment variables.

### Docker

1. Clone the repository

```
git clone https://github.com/llamafolio/evm-indexer && cd evm-indexer
```

2. Build the image and tag it as `indexer`

```
docker build . -t indexer
```

3. Copy the `.env.example` file to `.env` and add your environment variables.
