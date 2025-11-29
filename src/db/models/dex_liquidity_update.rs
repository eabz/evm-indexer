use alloy::primitives::U256;
use alloy::rpc::types::Log;
use clickhouse::Row;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Row)]
pub struct DatabaseDexLiquidityUpdate {
    pub block_number: u32,
    pub chain: u64,
    pub transaction_hash: String,
    pub log_index: u16,
    pub pool_address: String,
    pub r#type: String,
    pub amount0: String,
    pub amount1: String,
    pub reserve0: String,
    pub reserve1: String,
    pub liquidity: String,
    pub timestamp: u64,
}

impl DatabaseDexLiquidityUpdate {
    pub fn from_uniswap_v2_sync(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
    ) -> Option<Self> {
        // Sync(uint112 reserve0, uint112 reserve1)
        // data: reserve0 (32 bytes), reserve1 (32 bytes)

        let data = &log.inner.data.data;
        if data.len() < 64 {
            return None;
        }

        let reserve0 = U256::from_be_slice(&data[0..32]);
        let reserve1 = U256::from_be_slice(&data[32..64]);

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: format!("{:?}", log.address()),
            r#type: "Sync".to_string(),
            amount0: "0".to_string(),
            amount1: "0".to_string(),
            reserve0: reserve0.to_string(),
            reserve1: reserve1.to_string(),
            liquidity: "0".to_string(),
            timestamp,
        })
    }

    pub fn from_uniswap_v2_mint(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
    ) -> Option<Self> {
        // Mint(address indexed sender, uint amount0, uint amount1)
        // data: amount0, amount1

        let data = &log.inner.data.data;
        if data.len() < 64 {
            return None;
        }

        let amount0 = U256::from_be_slice(&data[0..32]);
        let amount1 = U256::from_be_slice(&data[32..64]);

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: format!("{:?}", log.address()),
            r#type: "Mint".to_string(),
            amount0: amount0.to_string(),
            amount1: amount1.to_string(),
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            liquidity: "0".to_string(),
            timestamp,
        })
    }

    pub fn from_uniswap_v2_burn(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
    ) -> Option<Self> {
        // Burn(address indexed sender, uint amount0, uint amount1, address indexed to)
        // data: amount0, amount1

        let data = &log.inner.data.data;
        if data.len() < 64 {
            return None;
        }

        let amount0 = U256::from_be_slice(&data[0..32]);
        let amount1 = U256::from_be_slice(&data[32..64]);

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: format!("{:?}", log.address()),
            r#type: "Burn".to_string(),
            amount0: amount0.to_string(),
            amount1: amount1.to_string(),
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            liquidity: "0".to_string(),
            timestamp,
        })
    }

    pub fn from_uniswap_v3_mint(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
    ) -> Option<Self> {
        // Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)
        // data: sender (32), amount (32), amount0 (32), amount1 (32)
        // Note: sender is not indexed in V3 Mint event signature provided in some docs, but let's check standard.
        // Standard V3 Mint: owner, tickLower, tickUpper are indexed.
        // data: sender, amount, amount0, amount1.

        let data = &log.inner.data.data;
        if data.len() < 128 {
            return None;
        }

        // skip sender (32)
        let amount = U256::from_be_slice(&data[32..64]);
        let amount0 = U256::from_be_slice(&data[64..96]);
        let amount1 = U256::from_be_slice(&data[96..128]);

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: format!("{:?}", log.address()),
            r#type: "Mint".to_string(),
            amount0: amount0.to_string(),
            amount1: amount1.to_string(),
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            liquidity: amount.to_string(),
            timestamp,
        })
    }

    pub fn from_uniswap_v3_burn(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u64,
        transaction_hash: String,
        log_index: u16,
    ) -> Option<Self> {
        // Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)
        // data: amount, amount0, amount1

        let data = &log.inner.data.data;
        if data.len() < 96 {
            return None;
        }

        let amount = U256::from_be_slice(&data[0..32]);
        let amount0 = U256::from_be_slice(&data[32..64]);
        let amount1 = U256::from_be_slice(&data[64..96]);

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: format!("{:?}", log.address()),
            r#type: "Burn".to_string(),
            amount0: amount0.to_string(),
            amount1: amount1.to_string(),
            reserve0: "0".to_string(),
            reserve1: "0".to_string(),
            liquidity: amount.to_string(),
            timestamp,
        })
    }
}
