<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>A scalable SQL indexer for EVM compatible blockchains</strong>
</p>

The indexer is ready and used in production. If you want to use it or contribute and need help you can [send me a DM on my personal twitter.](https://twitter.com/eaberrueta)

You can track the indexer in action from https://dashboard.kindynos.mx or use it through the API at https://indexer.kindynos.mx

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
