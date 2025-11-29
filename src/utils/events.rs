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

// Maverick SwapFilled: SwapFilled(address indexed recipient, uint256 amountAIn, uint256 amountBIn, uint256 amountAOut, uint256 amountBOut)
pub const MAVERICK_SWAP_FILLED_EVENT_SIGNATURE: &str =
    "0xdb5f3876bb8b6c00fb1a4c32c5947c5ad45a70c07d30a1a3fd90f3ad8dd86c3f";

// TraderJoe V2.1 LB Swap: Swap(address indexed sender, address indexed to, uint24 id, bytes32 amountsIn, bytes32 amountsOut)
pub const TRADERJOE_LB_SWAP_EVENT_SIGNATURE: &str =
    "0xad7d6f97abf51ce18e17a38f4d70e975be9c0708474987bb3e26ad21bd93ca70";

// WooFi WooSwap: WooSwap(address indexed from, address indexed to, address fromToken, address toToken, uint256 fromAmount, uint256 toAmount, address rebateTo)
pub const WOOFI_SWAP_EVENT_SIGNATURE: &str =
    "0x74ef34e2ea7c5d9f7b7ed44e97ad44b4303416c3a660c3fb5b3bdb95a1d6abd3";

// Uniswap V2 PairCreated: PairCreated(address indexed token0, address indexed token1, address pair, uint)
pub const PAIR_CREATED_EVENT_SIGNATURE: &str =
    "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9";

// Uniswap V3 PoolCreated: PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)
pub const POOL_CREATED_EVENT_SIGNATURE: &str =
    "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118";

// Uniswap V2 Sync: Sync(uint112 reserve0, uint112 reserve1)
pub const UNISWAP_V2_SYNC_EVENT_SIGNATURE: &str =
    "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1";

// Uniswap V2 Mint: Mint(address indexed sender, uint amount0, uint amount1)
pub const UNISWAP_V2_MINT_EVENT_SIGNATURE: &str =
    "0x4c209b5fc8ad50758f13e297d88b151d20812dc45bc2cf45941352625615cb68";

// Uniswap V2 Burn: Burn(address indexed sender, uint amount0, uint amount1, address indexed to)
pub const UNISWAP_V2_BURN_EVENT_SIGNATURE: &str =
    "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496";

// Uniswap V3 Mint: Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)
pub const UNISWAP_V3_MINT_EVENT_SIGNATURE: &str =
    "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde";

// Uniswap V3 Burn: Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1)
pub const UNISWAP_V3_BURN_EVENT_SIGNATURE: &str =
    "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67a028c84986e27ed1c";

// Curve Finance AddLiquidity: AddLiquidity(address indexed provider, uint256[N] token_amounts, uint256 fee, uint256 token_supply)
// Note: This signature is for 2-coin pools. 3-coin and 4-coin pools have different signatures due to array size differences.
pub const CURVE_ADD_LIQUIDITY_2_EVENT_SIGNATURE: &str =
    "0x26f55a85081d24974e85c6c00045d0f0453991e95873f52bff0d21af4079a768";

// Curve Finance AddLiquidity for 3-coin pools
pub const CURVE_ADD_LIQUIDITY_3_EVENT_SIGNATURE: &str =
    "0x3f1915775e0c9a38a57a7bb7f1f9005f486fb904e1f84aa215364d567319a58";

// Curve Finance RemoveLiquidity: RemoveLiquidity(address indexed provider, uint256[N] token_amounts, uint256 token_supply)
pub const CURVE_REMOVE_LIQUIDITY_2_EVENT_SIGNATURE: &str =
    "0x7c363854ccf79623411f8995b362bce5eddff18c927edc6f5dbbb5e05819a82c";

// Curve Finance RemoveLiquidity for 3-coin pools
pub const CURVE_REMOVE_LIQUIDITY_3_EVENT_SIGNATURE: &str =
    "0x9878ca375e106f2a43c3b599fc624568131c4c9a4ba66a14563715763be9d59d";

// Curve Finance RemoveLiquidityOne: RemoveLiquidityOne(address indexed provider, uint256 token_amount, uint256 coin_index, uint256 coin_amount)
pub const CURVE_REMOVE_LIQUIDITY_ONE_EVENT_SIGNATURE: &str =
    "0x9e96dd3b997a2a257eec4df9bb6eaf626e206df5f543bd963682d143300be310";

// Curve Finance RemoveLiquidityImbalance: RemoveLiquidityImbalance(address indexed provider, uint256[N] token_amounts, uint256 token_supply)
pub const CURVE_REMOVE_LIQUIDITY_IMBALANCE_2_EVENT_SIGNATURE: &str =
    "0x2b5508378d7e19e0d5fa338419034731416c4f5b219a10379956f764317fd47e";

// Balancer V2 PoolRegistered: PoolRegistered(bytes32 indexed poolId, address indexed poolAddress, uint8 specialization)
pub const BALANCER_POOL_REGISTERED_EVENT_SIGNATURE: &str =
    "0x3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e";

// Balancer V2 PoolBalanceChanged: PoolBalanceChanged(bytes32 indexed poolId, address indexed liquidityProvider, address[] tokens, int256[] deltas, uint256[] protocolFeeAmounts)
pub const BALANCER_POOL_BALANCE_CHANGED_EVENT_SIGNATURE: &str =
    "0xe5ce249087ce04f05a957192435400fd97868dba0e6a4b4c049abf8af80dae78";
