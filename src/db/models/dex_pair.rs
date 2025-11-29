use alloy::rpc::types::Log;
use clickhouse::Row;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Row)]
pub struct DatabaseDexPair {
    pub block_number: u32,
    pub chain: u64,
    pub transaction_hash: String,
    pub log_index: u16,
    pub factory: String,
    pub pair: String,
    pub token0: String,
    pub token1: String,
    pub reserve0: String,
    pub reserve1: String,
    pub dex_name: String,
    pub timestamp: u64,
}

impl DatabaseDexPair {
    pub fn from_pair_created(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        // Uniswap V2 PairCreated: event PairCreated(address indexed token0, address indexed token1, address pair, uint);
        // topic0: signature
        // topic1: token0 (indexed)
        // topic2: token1 (indexed)
        // data: pair (address), uint (arg3)

        let token0_topic = log.topics().get(1)?;
        let token1_topic = log.topics().get(2)?;

        // Extract pair address from data (first 32 bytes)
        let data = &log.inner.data.data;
        if data.len() < 32 {
            return None;
        }

        // Address is the last 20 bytes of the first 32-byte word
        let pair_bytes = &data[12..32];
        let pair = format!("0x{}", hex::encode(pair_bytes));

        // Format token addresses from topics (last 20 bytes)
        let token0 =
            format!("0x{}", hex::encode(&token0_topic.as_slice()[12..]));
        let token1 =
            format!("0x{}", hex::encode(&token1_topic.as_slice()[12..]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            factory: format!("{:?}", log.address()),
            pair,
            token0,
            token1,
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            dex_name,
            timestamp,
        })
    }

    pub fn from_pool_created(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        // Uniswap V3 PoolCreated: event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool);
        // topic0: signature
        // topic1: token0 (indexed)
        // topic2: token1 (indexed)
        // topic3: fee (indexed)
        // data: tickSpacing (int24), pool (address)

        let token0_topic = log.topics().get(1)?;
        let token1_topic = log.topics().get(2)?;

        // Extract pool address from data
        // data layout: tickSpacing (32 bytes padded), pool (32 bytes padded)
        let data = &log.inner.data.data;
        if data.len() < 64 {
            return None;
        }

        // Pool address is in the second 32-byte word
        let pool_bytes = &data[44..64];
        let pair = format!("0x{}", hex::encode(pool_bytes));

        let token0 =
            format!("0x{}", hex::encode(&token0_topic.as_slice()[12..]));
        let token1 =
            format!("0x{}", hex::encode(&token1_topic.as_slice()[12..]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            factory: format!("{:?}", log.address()),
            pair,
            token0,
            token1,
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            dex_name,
            timestamp,
        })
    }
}
