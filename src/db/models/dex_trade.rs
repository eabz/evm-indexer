use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

/// Negate a 256-bit two's complement number to get absolute value
/// Used for converting negative int256 values from V3 swap events
fn negate_i256(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0u8; 32];
    let mut carry = true;

    // Two's complement negation: invert all bits and add 1
    for i in (0..32).rev() {
        let inverted = !bytes[i];
        if carry {
            let (sum, overflow) = inverted.overflowing_add(1);
            result[i] = sum;
            carry = overflow;
        } else {
            result[i] = inverted;
        }
    }

    result
}

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

        // V3 amounts are signed integers (int256)
        // Positive = tokens going INTO the pool (user sends)
        // Negative = tokens going OUT of the pool (user receives)
        let is_amount0_negative = data[0] & 0x80 != 0;
        let is_amount1_negative = data[32] & 0x80 != 0;

        // For negative amounts, compute absolute value (negate two's complement)
        let (amount0_in, amount0_out) = if !is_amount0_negative {
            // Positive: user sends this amount (amount_in)
            (format!("0x{}", hex::encode(&data[0..32])), "0x0".to_string())
        } else {
            // Negative: user receives this amount (amount_out) - compute absolute value
            let abs_amount = negate_i256(&data[0..32]);
            ("0x0".to_string(), format!("0x{}", hex::encode(abs_amount)))
        };

        let (amount1_in, amount1_out) = if !is_amount1_negative {
            (
                format!("0x{}", hex::encode(&data[32..64])),
                "0x0".to_string(),
            )
        } else {
            let abs_amount = negate_i256(&data[32..64]);
            ("0x0".to_string(), format!("0x{}", hex::encode(abs_amount)))
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

    /// Create DatabaseDexTrade from Balancer Swap event
    /// Event: Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut)
    pub fn from_balancer_swap(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        if log.topics().len() < 4 {
            return None;
        }

        let token_in =
            Address::from_slice(&log.topics()[2].as_slice()[12..]);
        let token_out =
            Address::from_slice(&log.topics()[3].as_slice()[12..]);

        let data = &log.data().data;
        if data.len() < 64 {
            return None;
        }

        let amount_in = format!("0x{}", hex::encode(&data[0..32]));
        let amount_out = format!("0x{}", hex::encode(&data[32..64]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender: token_in,
            recipient: token_out,
            amount0_in: amount_in,
            amount1_in: "0x0".to_string(),
            amount0_out: amount_out,
            amount1_out: "0x0".to_string(),
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from DODO Swap event
    /// Event: Swap(address indexed sender, address indexed receiver, address tokenB, address tokenQuote, uint256 payQuote, uint256 receiveBase)
    pub fn from_dodo_swap(
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
        let receiver =
            Address::from_slice(&log.topics()[2].as_slice()[12..]);

        let data = &log.data().data;
        if data.len() < 160 {
            return None;
        }

        // Skip first 2 addresses (tokenB, tokenQuote) as they're 32-byte aligned
        // payQuote is at offset 64, receiveBase at offset 96
        let pay_quote = format!("0x{}", hex::encode(&data[64..96]));
        let receive_base = format!("0x{}", hex::encode(&data[96..128]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender,
            recipient: receiver,
            amount0_in: pay_quote,
            amount1_in: "0x0".to_string(),
            amount0_out: receive_base,
            amount1_out: "0x0".to_string(),
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from Kyber Swapped event
    /// Event: Swapped(address indexed sender, IERC20 indexed srcToken, IERC20 indexed dstToken, address dstReceiver, uint256 spentAmount, uint256 returnedAmount)
    /// Note: EVM logs support max 4 topics (topic0 = signature + 3 indexed params)
    pub fn from_kyber_swapped(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        if log.topics().len() < 4 {
            return None;
        }

        let sender =
            Address::from_slice(&log.topics()[1].as_slice()[12..]);

        let data = &log.data().data;
        // Data: dstReceiver (32) + spentAmount (32) + returnedAmount (32) = 96 bytes
        if data.len() < 96 {
            return None;
        }

        // Skip dstReceiver at offset 0-32
        let spent_amount = format!("0x{}", hex::encode(&data[32..64]));
        let returned_amount = format!("0x{}", hex::encode(&data[64..96]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender,
            recipient: Address::from_slice(&data[12..32]), // dstReceiver
            amount0_in: spent_amount,
            amount1_in: "0x0".to_string(),
            amount0_out: returned_amount,
            amount1_out: "0x0".to_string(),
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from Maverick SwapFilled event
    /// Event: SwapFilled(address indexed recipient, uint256 amountAIn, uint256 amountBIn, uint256 amountAOut, uint256 amountBOut)
    pub fn from_maverick_swap_filled(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
        dex_name: String,
    ) -> Option<Self> {
        if log.topics().len() < 2 {
            return None;
        }

        let recipient =
            Address::from_slice(&log.topics()[1].as_slice()[12..]);

        let data = &log.data().data;
        // 4 x uint256 = 128 bytes
        if data.len() < 128 {
            return None;
        }

        let amount_a_in = format!("0x{}", hex::encode(&data[0..32]));
        let amount_b_in = format!("0x{}", hex::encode(&data[32..64]));
        let amount_a_out = format!("0x{}", hex::encode(&data[64..96]));
        let amount_b_out = format!("0x{}", hex::encode(&data[96..128]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender: recipient,
            recipient,
            amount0_in: amount_a_in,
            amount1_in: amount_b_in,
            amount0_out: amount_a_out,
            amount1_out: amount_b_out,
            dex_name,
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from Curve TokenExchangeUnderlying event
    /// Event: TokenExchangeUnderlying(address indexed buyer, int128 sold_id, uint256 tokens_sold, int128 bought_id, uint256 tokens_bought)
    /// Used for meta pools where users swap underlying tokens directly
    pub fn from_curve_token_exchange_underlying(
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
        // int128 sold_id (32) + uint256 tokens_sold (32) + int128 bought_id (32) + uint256 tokens_bought (32) = 128 bytes
        if data.len() < 128 {
            return None;
        }

        // Skip sold_id at offset 0-32, extract tokens_sold at offset 32-64
        let tokens_sold = format!("0x{}", hex::encode(&data[32..64]));
        // Skip bought_id at offset 64-96, extract tokens_bought at offset 96-128
        let tokens_bought = format!("0x{}", hex::encode(&data[96..128]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender: buyer,
            recipient: buyer, // Curve doesn't have separate recipient
            amount0_in: tokens_sold,
            amount1_in: "0x0".to_string(),
            amount0_out: tokens_bought,
            amount1_out: "0x0".to_string(),
            dex_name: "Curve".to_string(),
            timestamp,
        })
    }

    /// Create DatabaseDexTrade from TraderJoe V2.1 LB Swap event
    /// Event: Swap(address indexed sender, address indexed to, uint24 id, bytes32 amountsIn, bytes32 amountsOut)
    /// The amountsIn and amountsOut are packed as (amountX, amountY) where X is token0 and Y is token1
    pub fn from_traderjoe_lb_swap(
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
        // uint24 id (32 padded) + bytes32 amountsIn (32) + bytes32 amountsOut (32) = 96 bytes
        if data.len() < 96 {
            return None;
        }

        // amountsIn is packed as (amountXIn, amountYIn) in bytes32
        // amountsOut is packed as (amountXOut, amountYOut) in bytes32
        // The packing is: upper 128 bits = amountX, lower 128 bits = amountY
        let amounts_in = &data[32..64];
        let amounts_out = &data[64..96];

        // Extract amounts (upper 16 bytes = tokenX, lower 16 bytes = tokenY)
        let amount0_in = format!("0x{}", hex::encode(&amounts_in[0..16]));
        let amount1_in = format!("0x{}", hex::encode(&amounts_in[16..32]));
        let amount0_out =
            format!("0x{}", hex::encode(&amounts_out[0..16]));
        let amount1_out =
            format!("0x{}", hex::encode(&amounts_out[16..32]));

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

    /// Create DatabaseDexTrade from WooFi WooSwap event
    /// Event: WooSwap(address indexed from, address indexed to, address fromToken, address toToken, uint256 fromAmount, uint256 toAmount, address rebateTo)
    pub fn from_woofi_swap(
        log: &Log,
        chain: u64,
        block_number: u32,
        timestamp: u32,
        transaction_hash: B256,
        log_index: u16,
    ) -> Option<Self> {
        if log.topics().len() < 3 {
            return None;
        }

        let sender =
            Address::from_slice(&log.topics()[1].as_slice()[12..]);
        let recipient =
            Address::from_slice(&log.topics()[2].as_slice()[12..]);

        let data = &log.data().data;
        // fromToken (32) + toToken (32) + fromAmount (32) + toAmount (32) + rebateTo (32) = 160 bytes
        if data.len() < 160 {
            return None;
        }

        let from_amount = format!("0x{}", hex::encode(&data[64..96]));
        let to_amount = format!("0x{}", hex::encode(&data[96..128]));

        Some(Self {
            block_number,
            chain,
            transaction_hash,
            log_index,
            pool_address: log.address(),
            sender,
            recipient,
            amount0_in: from_amount,
            amount1_in: "0x0".to_string(),
            amount0_out: to_amount,
            amount1_out: "0x0".to_string(),
            dex_name: "WooFi".to_string(),
            timestamp,
        })
    }
}
