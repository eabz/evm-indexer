pub const TRANSFER_EVENTS_SIGNATURE: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

pub const ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE: &str =
    "0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62";

pub const ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE: &str =
    "0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb";

// Uniswap V2 Swap: Swap(address indexed sender, uint amount0In, uint amount1In, uint amount0Out, uint amount1Out, address indexed to)
// Also used by: PancakeSwap V2, SushiSwap V2 (Uniswap V2 forks)
pub const UNISWAP_V2_SWAP_EVENT_SIGNATURE: &str =
    "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";

// Uniswap V3 Swap: Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)
// Also used by: PancakeSwap V3 (Uniswap V3 fork)
pub const UNISWAP_V3_SWAP_EVENT_SIGNATURE: &str =
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

// Curve TokenExchange: TokenExchange(address indexed buyer, uint256 sold_id, uint256 tokens_sold, uint256 bought_id, uint256 tokens_bought)
pub const CURVE_TOKEN_EXCHANGE_EVENT_SIGNATURE: &str =
    "0x8b3e96f2b38596c00065310677151151122ba8153782eeec1e3567cc2a8f3b8b";
