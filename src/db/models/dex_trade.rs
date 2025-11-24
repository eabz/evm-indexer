use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Row, Serialize, Deserialize)]
pub struct DatabaseDexTrade {
    pub block_number: u32,
    pub chain: u64,
    pub transaction_hash: B256,
    pub log_index: u16,
    pub pool_address: Address,
    pub sender: Address,
    pub recipient: Address,
    pub amount0_in: String,
    pub amount1_in: String,
    pub amount0_out: String,
    pub amount1_out: String,
    pub dex_name: String,
    pub timestamp: u32,
}

impl DatabaseDexTrade {
    /// Create DatabaseDexTrade from Uniswap V2-style Swap event
    /// Event: Swap(address indexed sender, uint amount0In, uint amount1In, uint amount0Out, uint amount1Out, address indexed to)
    /// Used by: Uniswap V2, PancakeSwap V2, SushiSwap V2, QuickSwap V2, etc.
    pub fn from_uniswap_v2_swap(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        if log.topics().len() < 3 {
            return None;
        }

        let sender =
            Address::from_slice(&log.topics()[1].as_slice()[12..]);
        let recipient =
            Address::from_slice(&log.topics()[2].as_slice()[12..]);

        let data = &log.data().data;
        if data.len() < 128 {
            return None;
        }

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender,
            recipient,
            amount0_in: format!("0x{}", hex::encode(&data[0..32])),
            amount1_in: format!("0x{}", hex::encode(&data[32..64])),
            amount0_out: format!("0x{}", hex::encode(&data[64..96])),
            amount1_out: format!("0x{}", hex::encode(&data[96..128])),
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from Uniswap V3-style Swap event
    /// Event: Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)
    /// Used by: Uniswap V3, PancakeSwap V3, etc.
    pub fn from_uniswap_v3_swap(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        if log.topics().len() < 3 {
            return None;
        }

        let sender =
            Address::from_slice(&log.topics()[1].as_slice()[12..]);
        let recipient =
            Address::from_slice(&log.topics()[2].as_slice()[12..]);

        let data = &log.data().data;
        if data.len() < 160 {
            return None;
        }

        // V3 amounts are signed integers
        let amount0 = format!("0x{}", hex::encode(&data[0..32]));
        let amount1 = format!("0x{}", hex::encode(&data[32..64]));

        // Determine in/out based on sign (positive = in, negative = out)
        let (amount0_in, amount0_out) = if data[0] & 0x80 == 0 {
            (amount0.clone(), "0x0".to_string())
        } else {
            ("0x0".to_string(), amount0.clone())
        };

        let (amount1_in, amount1_out) = if data[32] & 0x80 == 0 {
            (amount1.clone(), "0x0".to_string())
        } else {
            ("0x0".to_string(), amount1.clone())
        };

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender,
            recipient,
            amount0_in,
            amount1_in,
            amount0_out,
            amount1_out,
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from Curve TokenExchange event
    /// Event: TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought)
    pub fn from_curve_token_exchange(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
    ) -> Option<Self> {
        if log.topics().len() < 2 {
            return None;
        }

        let buyer = Address::from_slice(&log.topics()[1].as_slice()[12..]);

        let data = &log.data().data;
        if data.len() < 128 {
            return None;
        }

        // Curve: sold_id, tokens_sold, bought_id, tokens_bought
        let tokens_sold = format!("0x{}", hex::encode(&data[32..64]));
        let tokens_bought = format!("0x{}", hex::encode(&data[96..128]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender: buyer,
            recipient: buyer, // Curve doesn't have separate recipient
            amount0_in: tokens_bought,
            amount1_in: "0x0".to_string(),
            amount0_out: tokens_sold,
            amount1_out: "0x0".to_string(),
            dex_name: "Curve".to_string(),
            timestamp,
        })
    }
}
