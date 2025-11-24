<h1 align="center">
<strong>EVM Blockchain Indexer</strong>
</h1>
<p align="center">
<strong>High-performance SQL indexer for EVM-compatible blockchains</strong>
</p>

[![Docker Image Size](https://badgen.net/docker/size/0xeabz/evm-indexer/main?icon=docker&label=image%20size)](https://hub.docker.com/r/0xeabz/evm-indexer)
![build](https://github.com/eabz/evm-indexer/actions/workflows/build.yml/badge.svg)

A production-ready indexer that fetches and stores blockchain data for later analysis. Optimized for performance with support for parallel block processing, dynamic RPC capability detection, and efficient memory usage.

## Features

- ✅ **Complete blockchain primitives**: blocks, transactions, receipts, logs, traces, withdrawals
- ✅ **Token transfers**: ERC20, ERC721, ERC1155
- ✅ **DEX trades**: Uniswap V2, V3 and multiple high-volume AMM.
- ✅ **Contract tracking**: Automatically indexes deployed contracts
- ✅ **Parallel processing**: Configurable batch size for optimal throughput
- ✅ **Smart RPC usage**: Auto-detects `eth_getBlockReceipts` support

## Requirements

- [Rust](https://www.rust-lang.org/tools/install) 1.70+
- [ClickHouse](https://clickhouse.com/) 23.0+

## Quick Start

### Using Docker Compose (Recommended)

1. Clone the repository:
```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Start the services:
```bash
docker-compose up -d
```

This will start:
- ClickHouse database on ports 8123 (HTTP) and 9000 (native)
- Indexer configured for Ethereum mainnet

3. Monitor logs:
```bash
docker-compose logs -f indexer
```

### Local Development

1. Clone the repository:
```bash
git clone https://github.com/eabz/evm-indexer && cd evm-indexer
```

2. Build the program:
```bash
cargo build --release
```

3. Run the indexer:
```bash
./target/release/indexer \
  --chain 1 \
  --database clickhouse://user:password@localhost:8123/indexer \
  --rpcs https://eth.llamarpc.com \
  --start-block 0 \
  --batch-size 100
```

## Configuration

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--chain` | `1` | Chain ID to index (1=Ethereum, 56=BSC, 137=Polygon, etc.) |
| `--database` | *required* | ClickHouse connection string: `clickhouse://user:pass@host:port/db` |
| `--rpcs` | *required* | Comma-separated list of RPC endpoints |
| `--start-block` | `0` | Block number to start syncing from |
| `--end-block` | `0` | Last block to sync (0 = continuous sync) |
| `--batch-size` | `200` | Number of blocks to fetch in parallel |
| `--ws` | `""` | WebSocket endpoint for real-time block updates |
| `--traces` | `true` | Fetch transaction traces (requires archive node) |
| `--fetch-uncles` | `false` | Fetch uncle blocks (adds 5-10% RPC calls) |
| `--new-blocks-only` | `false` | Only index new blocks (skip historical sync) |
| `--debug` | `false` | Enable debug logging |

### Environment Variables

Create a `.env` file (see `.env.example`):
```bash
# ClickHouse Configuration
CLICKHOUSE_DB=indexer
CLICKHOUSE_USER=indexer
CLICKHOUSE_PASSWORD=indexer

# Indexer Configuration
CHAIN_ID=1
START_BLOCK=0
BATCH_SIZE=10
RPC_URL=https://eth.llamarpc.com
DEBUG=true
FETCH_UNCLES=false
TRACES=true
```

## Database Schema

The indexer creates the following tables in ClickHouse:

- `blocks` - Block headers and metadata
- `transactions` - Transaction data with gas info
- `logs` - Event logs emitted by contracts
- `traces` - Internal transaction traces
- `contracts` - Deployed contract addresses
- `withdrawals` - Validator withdrawals (post-merge)
- `erc20_transfers` - ERC20 token transfers
- `erc721_transfers` - NFT transfers
- `erc1155_transfers` - Multi-token transfers
- `dex_trades` - DEX trades

See `migrations/create_tables.sql` for full schema.

## Performance Tuning

### Batch Size
- **Small batches (10-50)**: Lower memory, slower throughput
- **Medium batches (100-200)**: Balanced (recommended)
- **Large batches (500+)**: Higher memory, faster throughput

### RPC Endpoints
- Use multiple RPCs for better reliability
- Archive nodes required for traces
- `eth_getBlockReceipts` support = 2x faster

### ClickHouse
- Use SSD storage for better performance
- Increase `max_insert_block_size` for large batches
- Enable compression for storage savings

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

MIT License - see LICENSE file for details

## Support

- GitHub Issues: [Report bugs](https://github.com/eabz/evm-indexer/issues)
- Discussions: [Ask questions](https://github.com/eabz/evm-indexer/discussions)
