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

// Curve TokenExchangeUnderlying: TokenExchangeUnderlying(address indexed buyer, int128 sold_id, uint256 tokens_sold, int128 bought_id, uint256 tokens_bought)
// Used for meta pools to swap underlying tokens directly
pub const CURVE_TOKEN_EXCHANGE_UNDERLYING_EVENT_SIGNATURE: &str =
    "0xd013ca23e77a65003c2c659c5442c00c805371b7fc1ebd4c206c41d1536bd90b";

// Balancer V2 Swap: Swap(bytes32 indexed poolId, address indexed tokenIn, address indexed tokenOut, uint256 amountIn, uint256 amountOut)
pub const BALANCER_SWAP_EVENT_SIGNATURE: &str =
    "0x908fb5ee0f4747fbf483d1ff1405d916f86720130038586180403d7e46b6b5f8";

// DODO Swap: Swap(address indexed sender, address indexed receiver, address tokenB, address tokenQuote, uint256 payQuote, uint256 receiveBase)
pub const DODO_SWAP_EVENT_SIGNATURE: &str =
    "0xc2c0b5e1ab1ec6b34b26ff79c0e1be2fdc89c4ed0cf2cc9e906f986de7ded41f";

// Kyber Swapped: Swapped(bytes32 indexed pool, address indexed router, address indexed token0, address indexed token1, int256 delta0, int256 delta1)
pub const KYBER_SWAPPED_EVENT_SIGNATURE: &str =
    "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496";

// Maverick SwapFilled: SwapFilled(address indexed recipient, uint256 amountAIn, uint256 amountBIn, uint256 amountAOut, uint256 amountBOut)
pub const MAVERICK_SWAP_FILLED_EVENT_SIGNATURE: &str =
    "0xdb5f3876bb8b6c00fb1a4c32c5947c5ad45a70c07d30a1a3fd90f3ad8dd86c3f";

// TraderJoe V2.1 LB Swap: Swap(address indexed sender, address indexed to, uint24 id, bytes32 amountsIn, bytes32 amountsOut)
pub const TRADERJOE_LB_SWAP_EVENT_SIGNATURE: &str =
    "0xad7d6f97abf51ce18e17a38f4d70e975be9c0708474987bb3e26ad21bd93ca70";

// WooFi WooSwap: WooSwap(address indexed from, address indexed to, address fromToken, address toToken, uint256 fromAmount, uint256 toAmount, address rebateTo)
pub const WOOFI_SWAP_EVENT_SIGNATURE: &str =
    "0x74ef34e2ea7c5d9f7b7ed44e97ad44b4303416c3a660c3fb5b3bdb95a1d6abd3";
