//! Real-world logs of every family, with their expected decoding.
//!
//! Source of every `RawLog` marked "real": `eth_getLogs` on public RPC
//! endpoints (Ethereum, BNB Chain, Base mainnets), copied verbatim -
//! address, topics, data, block number, transaction hash and log index.
//! The expected numbers were decoded independently (Python big integers),
//! and the Uniswap V4 sign convention was checked against the ERC-20
//! transfers of the same transaction (see `v4_swaps_are_pool_relative`).
//!
//! Logs marked "constructed" are ABI encoded by hand for event variants no
//! public endpoint returned within its range limits.

use alloy::primitives::{Address, Bytes, B256, I256, U256};

use crate::core::models::log::{test_support::log_with, DatabaseLog};

pub struct RawLog {
    pub address: &'static str,
    pub topics: &'static [&'static str],
    pub data: &'static str,
    pub block_number: u32,
    pub transaction_hash: &'static str,
    pub log_index: u16,
}

pub fn address(hex: &str) -> Address {
    hex.parse().unwrap()
}

pub fn hash(hex: &str) -> B256 {
    hex.parse().unwrap()
}

pub fn signed(decimal: &str) -> I256 {
    decimal.parse().unwrap()
}

pub fn unsigned(decimal: &str) -> U256 {
    decimal.parse().unwrap()
}

/// Conversion that compiles (and stays lint free) whatever integer width
/// the log model uses for block numbers / log indices.
fn widen<S, T: From<S>>(value: S) -> T {
    T::from(value)
}

/// A log of `emitter` at the given position.
pub fn build(
    emitter: Address,
    topics: &[B256],
    data: Vec<u8>,
    block_number: u32,
    log_index: u16,
    timestamp: u32,
) -> DatabaseLog {
    let mut log = log_with(topics, data);
    log.address = emitter;
    log.block_number = widen(block_number);
    log.log_index = widen(log_index);
    log.timestamp = timestamp;
    log
}

impl RawLog {
    /// The log as the pipeline would hand it to the decoder.
    pub fn at(&self, timestamp: u32) -> DatabaseLog {
        self.placed(self.block_number, self.log_index, timestamp)
    }

    /// The same event at another position of the chain.
    pub fn placed(
        &self,
        block_number: u32,
        log_index: u16,
        timestamp: u32,
    ) -> DatabaseLog {
        let topics: Vec<B256> =
            self.topics.iter().map(|topic| hash(topic)).collect();
        let data = self.data.parse::<Bytes>().unwrap().to_vec();

        let mut log = build(
            address(self.address),
            &topics,
            data,
            block_number,
            log_index,
            timestamp,
        );
        log.transaction_hash = hash(self.transaction_hash);
        log
    }

    pub fn log(&self) -> DatabaseLog {
        self.at(1_700_000_000)
    }
}

pub const WETH: &str = "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2";
pub const USDC: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
pub const USDT: &str = "0xdac17f958d2ee523a2206206994597c13d831ec7";

/// Uniswap V2 USDC/WETH pair (token0 = USDC, token1 = WETH).
pub const V2_USDC_WETH: &str =
    "0xb4e16d0168e52d35cacd2c6185b44281ec28c9dc";
/// Uniswap V3 USDC/WETH 0.05% pool (token0 = USDC, token1 = WETH).
pub const V3_USDC_WETH: &str =
    "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640";
pub const V4_POOL_MANAGER: &str =
    "0x000000000004444c5dc75cb358380d2e3de08a90";
pub const BALANCER_VAULT: &str =
    "0xba12222222228d8ba445958a75a0704d566bf2c8";

// ------------------------------------------------------------------ real logs

/// Real. Ethereum, Uniswap V2 factory.
pub const V2_PAIR_CREATED: RawLog = RawLog {
    address: "0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f",
    topics: &[
        "0x0d3648bd0f6ba80134a33ba9275ac585d9d315f0ad8355cddefde31afa28d0e9",
        "0x0000000000000000000000002dfde9822a22abb9f92daf9c79fa3b34558748ae",
        "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
    ],
    data: "0x000000000000000000000000961c476fe0d91330b26600d46379f9382fe3b8f0000000000000000000000000000000000000000000000000000000000007f895",
    block_number: 0x18cd705,
    transaction_hash: "0x81f30158fd7540793c31374f57fddaa144d2b65e3d86ce137eafb8305c4a9fdf",
    log_index: 0x3a4,
};

/// Real. 0.001 WETH in, 2.624963 USDC out through the V2 router.
pub const V2_SWAP: RawLog = RawLog {
    address: V2_USDC_WETH,
    topics: &[
        "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822",
        "0x0000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d",
        "0x000000000000000000000000a524ecb8ca2592ac3ed9a562fd467c02e09e003e",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000038d7ea4c680000000000000000000000000000000000000000000000000000000000000280dc30000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x18cd71f,
    transaction_hash: "0xda4ec47f6ba4c797c6b6acb416c60fd0a6c8c0c49a76269d23d08fbe1dc333e4",
    log_index: 0xf9,
};

/// Real. The `Sync` emitted right before [`V2_SWAP`].
pub const V2_SYNC: RawLog = RawLog {
    address: V2_USDC_WETH,
    topics: &[
        "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1",
    ],
    data: "0x0000000000000000000000000000000000000000000000000000097381e970be0000000000000000000000000000000000000000000000d5f694c63edc956009",
    block_number: 0x18cd71f,
    transaction_hash: "0xda4ec47f6ba4c797c6b6acb416c60fd0a6c8c0c49a76269d23d08fbe1dc333e4",
    log_index: 0xf8,
};

/// Real.
pub const V2_MINT: RawLog = RawLog {
    address: V2_USDC_WETH,
    topics: &[
        "0x4c209b5fc8ad50758f13e2e1088ba56a560dff690a1c6fef26394f4c03821c4f",
        "0x0000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d",
    ],
    data: "0x000000000000000000000000000000000000000000000000000011883f2a57d00000000000000000000000000000000000000000000001a7f39d1fe205ed40f5",
    block_number: 0x18cb64c,
    transaction_hash: "0x18a9089ba8d0fb023997ef0c9739a81a032867d2eb5fd94b2cf93a0d13390f9b",
    log_index: 0x178,
};

/// Real.
pub const V2_BURN: RawLog = RawLog {
    address: V2_USDC_WETH,
    topics: &[
        "0xdccd412f0b1252819cb1fd330b93224ca42612892bb3f4f789976e6d81936496",
        "0x0000000000000000000000000ea041260095b20feda41ef4fdbef7d801f4c438",
        "0x0000000000000000000000000ea041260095b20feda41ef4fdbef7d801f4c438",
    ],
    data: "0x000000000000000000000000000000000000000000000000000011883f2a57cf0000000000000000000000000000000000000000000001a7f39d1fe201cb279c",
    block_number: 0x18cb64c,
    transaction_hash: "0x18a9089ba8d0fb023997ef0c9739a81a032867d2eb5fd94b2cf93a0d13390f9b",
    log_index: 0x1a0,
};

/// Real. Ethereum, Uniswap V3 factory (fee 3000, tick spacing 60).
pub const V3_POOL_CREATED: RawLog = RawLog {
    address: "0x1f98431c8ad98523631ae4a59f267346ea31f984",
    topics: &[
        "0x783cca1c0412dd0d695e784568c96da2e9c22ff989357a2e8b1d9b2b4e6b7118",
        "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
        "0x000000000000000000000000f21dd66f8f75203a5acff45433d80cde97c69877",
        "0x0000000000000000000000000000000000000000000000000000000000000bb8",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000003c0000000000000000000000007514ea10c75582f76d71226642cf7492d3040606",
    block_number: 0x18cce07,
    transaction_hash: "0x8aca8e7f08c7091fe5470d317b29872bbf1c0be8a561c855ca8fde6df480617e",
    log_index: 0x53,
};

/// Real. 12.57 WETH in, 32,942.90 USDC out.
pub const V3_SWAP: RawLog = RawLog {
    address: V3_USDC_WETH,
    topics: &[
        "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67",
        "0x000000000000000000000000bdb3ba9ffe392549e1f8658dd2630c141fdf47b6",
        "0x000000000000000000000000bdb3ba9ffe392549e1f8658dd2630c141fdf47b6",
    ],
    data: "0xfffffffffffffffffffffffffffffffffffffffffffffffffffffff854732d47000000000000000000000000000000000000000000000000ae7b5bf58b5d28000000000000000000000000000000000000004c4c77365fa082e2130a8a0237a90000000000000000000000000000000000000000000000003d6797b893fba5f700000000000000000000000000000000000000000000000000000000000303e6",
    block_number: 0x18cd7fd,
    transaction_hash: "0x39fd700b210b7a233c3c3e4428f6273d43929c0ed677372b3eb35e990be4428f",
    log_index: 0x2,
};

/// Real. Through the NonfungiblePositionManager.
pub const V3_MINT: RawLog = RawLog {
    address: V3_USDC_WETH,
    topics: &[
        "0x7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde",
        "0x000000000000000000000000c36442b4a4522e871399cd717abdd847ab11fe88",
        "0x000000000000000000000000000000000000000000000000000000000003041c",
        "0x000000000000000000000000000000000000000000000000000000000003073c",
    ],
    data: "0x000000000000000000000000c36442b4a4522e871399cd717abdd847ab11fe880000000000000000000000000000000000000000000000000007f8b0ac256cec000000000000000000000000000000000000000000000000000000008273124b0000000000000000000000000000000000000000000000000c83421412935f76",
    block_number: 0x18ccccf,
    transaction_hash: "0xa007e3f9b81afa6107669e626b024915ca12673dbfe301ea68f073d627b316ad",
    log_index: 0x165,
};

/// Real.
pub const V3_BURN: RawLog = RawLog {
    address: V3_USDC_WETH,
    topics: &[
        "0x0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c",
        "0x000000000000000000000000c36442b4a4522e871399cd717abdd847ab11fe88",
        "0x000000000000000000000000000000000000000000000000000000000003055c",
        "0x00000000000000000000000000000000000000000000000000000000000306c4",
    ],
    data: "0x00000000000000000000000000000000000000000000000000b0c05899f51b1d0000000000000000000000000000000000000000000000000000000a620f3d090000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x18ccd0a,
    transaction_hash: "0x58047d9c3413e5091e05228d33656edecf4f6657f8df4aa090a24268d094ce61",
    log_index: 0x1c4,
};

/// Real. BNB Chain, PancakeSwap V3 (negative tick, protocol fee words).
pub const PANCAKE_V3_SWAP: RawLog = RawLog {
    address: "0x36696169c63e42cd08ce11f5deebbcebae652050",
    topics: &[
        "0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83",
        "0x00000000000000000000000013f4ea83d0bd40e75c8222255bc855a974568dd4",
        "0x00000000000000000000000013f4ea83d0bd40e75c8222255bc855a974568dd4",
    ],
    data: "0x0000000000000000000000000000000000000000000000000e98416b3753afa0fffffffffffffffffffffffffffffffffffffffffffffffffffb1bcb8b8b028100000000000000000000000000000000000000000943daa21d71d568370448820000000000000000000000000000000000000000000209e718490fa7196e0ecafffffffffffffffffffffffffffffffffffffffffffffffffffffffffffefcb20000000000000000000000000000000000000000000000000000a29a1243c7ad0000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x7501f8c,
    transaction_hash: "0xba3be7cdce5cf71fcdd10bb3c2d7cc0282b7ca03133bd09603a913ca8e0f9251",
    log_index: 0x6e,
};

/// Real. Native ETH pool: `currency0` (topic2) is the zero address.
pub const V4_INITIALIZE: RawLog = RawLog {
    address: V4_POOL_MANAGER,
    topics: &[
        "0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438",
        "0xb579bdeb5d5861d821153c57a4772282bd06c1d14d2e6a8fc7cbad1d105bbd65",
        "0x0000000000000000000000000000000000000000000000000000000000000000",
        "0x0000000000000000000000000da899de61658e089444e1071022087966ad9e78",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000271000000000000000000000000000000000000000000000000000000000000000c8000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003e9e6ab219a3c470b5fe63295110000000000000000000000000000000000000000000000000000000000021bd8",
    block_number: 0x18ccc96,
    transaction_hash: "0x3475bb4223c3823ced4bedfc6bbdd03c475e2a947c1650bc7769dd47a03dcb8f",
    log_index: 0x369,
};

/// Real. USDC/WETH pool: the event says amount0 = -1,793,588,760. In the
/// same transaction the caller TRANSFERS 1,793,618,318 USDC TO the
/// PoolManager (log 4; it had taken 29,558 USDC out earlier, log 0), so a
/// negative V4 amount is a payment INTO the pool.
pub const V4_SWAP_USDC_IN: RawLog = RawLog {
    address: V4_POOL_MANAGER,
    topics: &[
        "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f",
        "0xe500210c7ea6bfd9f69dce044b09ef384ec2b34832f132baec3b418208e3a657",
        "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    ],
    data: "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffff951801e8000000000000000000000000000000000000000000000000098454c27bd052fa0000000000000000000000000000000000004c610ecd00b55633f41b6a13939700000000000000000000000000000000000000000000000008dc2bf698cfa45800000000000000000000000000000000000000000000000000000000000303fb0000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x18cd81b,
    transaction_hash: "0x53dd6a130b0f685a75fb412c1fd7cd05e312e69cedb3f8e5638f341bd8bee8af",
    log_index: 0x2,
};

/// Real. Same transaction, ETH/USDT pool: amount1 = -1,428,368,405 and the
/// caller transfers exactly 1,428,368,405 USDT to the PoolManager (log 5).
pub const V4_SWAP_USDT_IN: RawLog = RawLog {
    address: V4_POOL_MANAGER,
    topics: &[
        "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f",
        "0x90078845bceb849b171873cfbc92db8540e9c803ff57d9d21b1215ec158e79b3",
        "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    ],
    data: "0x0000000000000000000000000000000000000000000000000793f0ec05af20a2ffffffffffffffffffffffffffffffffffffffffffffffffffffffffaadcd3eb000000000000000000000000000000000000000000035a19061ebcb640b1488800000000000000000000000000000000000000000000000007b5699f8da66617fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffcfc050000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x18cd81b,
    transaction_hash: "0x53dd6a130b0f685a75fb412c1fd7cd05e312e69cedb3f8e5638f341bd8bee8af",
    log_index: 0x3,
};

/// Real.
pub const V4_MODIFY_LIQUIDITY: RawLog = RawLog {
    address: V4_POOL_MANAGER,
    topics: &[
        "0xf208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec",
        "0xb14d03c5e24e8d59a735b336691c35032eb53739816df40e05890b14a447166e",
        "0x000000000000000000000000bd216513d74c8cf14cf4747e6aaa6420ff64ee9e",
    ],
    data: "0xfffffffffffffffffffffffffffffffffffffffffffffffffffffffffffbc210fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffbf4d80000000000000000000000000000000000000000000000000000be8c10b199c00000000000000000000000000000000000000000000000000000000000063e05",
    block_number: 0x18cd7cd,
    transaction_hash: "0x93c50cb9f5a86f9bf32638c649aba31b6dbc8c4c0702b8db1488e175b941d35a",
    log_index: 0x193,
};

/// Real. Balancer 80 BAL / 20 WETH, block 12,286,257.
pub const BALANCER_POOL_REGISTERED: RawLog = RawLog {
    address: BALANCER_VAULT,
    topics: &[
        "0x3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e",
        "0x647c1fd457b95b75d0972ff08fe01d7d7bda05df000200000000000000000002",
        "0x000000000000000000000000647c1fd457b95b75d0972ff08fe01d7d7bda05df",
    ],
    data: "0x0000000000000000000000000000000000000000000000000000000000000002",
    block_number: 0xbb7931,
    transaction_hash: "0x2c72fcd9cf568064102ef2add2a6dadfe3a566d9d94b2965fd71859d53ad520f",
    log_index: 0x20,
};

/// Real. Same transaction as [`BALANCER_POOL_REGISTERED`].
pub const BALANCER_TOKENS_REGISTERED: RawLog = RawLog {
    address: BALANCER_VAULT,
    topics: &[
        "0xf5847d3f2197b16cdcd2098ec95d0905cd1abdaf415f07bb7cef2bba8ac5dec4",
        "0x647c1fd457b95b75d0972ff08fe01d7d7bda05df000200000000000000000002",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000002000000000000000000000000ba100000625a3754423978a60c9317c58a424e3d000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0xbb7931,
    transaction_hash: "0x2c72fcd9cf568064102ef2add2a6dadfe3a566d9d94b2965fd71859d53ad520f",
    log_index: 0x21,
};

/// Real.
pub const BALANCER_SWAP: RawLog = RawLog {
    address: BALANCER_VAULT,
    topics: &[
        "0x2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b",
        "0xa3c500969accb3d8df08cba313c120818fe0ed9d000200000000000000000471",
        "0x0000000000000000000000000f2d719407fdbeff09d87557abb7232601fd9f29",
        "0x000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
    ],
    data: "0x0000000000000000000000000000000000000000000000012a217700000000000000000000000000000000000000000000000000000000000005e482f3f57278",
    block_number: 0x18cd702,
    transaction_hash: "0x8dd69775f0eda882abdcaa757da1a0234fbe7ffa360f292f52d290a4a3952433",
    log_index: 0x276,
};

/// Real. Curve 3pool (DAI, USDC, USDT): 0.099206 USDT -> 0.099112 USDC.
pub const CURVE_3POOL_EXCHANGE: RawLog = RawLog {
    address: "0xbebc44782c7db0a1a60cb6fe97d0b483032ff1c7",
    topics: &[
        "0x8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140",
        "0x000000000000000000000000ad6cea45f98444a922a2b4fe96b8c90f0862d2f4",
    ],
    data: "0x0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000001838600000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000018328",
    block_number: 0x18cccfb,
    transaction_hash: "0xf5f612d595e61bbccf8dd351e7388a38a66b46bda79bc5d33cfe46b8ca34bb18",
    log_index: 0x1aa,
};

/// Real. FRAX/3CRV metapool: FRAX (underlying 0) -> USDT (underlying 3).
pub const CURVE_UNDERLYING_EXCHANGE: RawLog = RawLog {
    address: "0xd632f22692fac7611d2aa1c0d552930d43caed3b",
    topics: &[
        "0xd013ca23e77a65003c2c659c5442c00c805371b7fc1ebd4c206c41d1536bd90b",
        "0x000000000000000000000000d1c6aca7ea7ed44e1873dafa054c28ceb690c7eb",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000f5e19a25bcefecb5800000000000000000000000000000000000000000000000000000000000000030000000000000000000000000000000000000000000000000000000010c53845",
    block_number: 0x18cb5af,
    transaction_hash: "0x8bb209531c679eb0e95f4ccf025a7cc1d0a63cb0392c3404c882bb03c019b560",
    log_index: 0x103,
};

/// Real. tricrypto2 (USDT, WBTC, WETH), `uint256` indices.
pub const CURVE_CRYPTO_EXCHANGE: RawLog = RawLog {
    address: "0xd51a44d3fae010294c616388b506acda1bfaae46",
    topics: &[
        "0xb2e76ae99761dc136e598d4a629bb347eccb9532a5f8bbd72e18467c3c34cc98",
        "0x0000000000000000000000004d5c2616a1eb1220315cbe7a008f8eca49f0900b",
    ],
    data: "0x00000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000a1959a1ac06209c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006b3935b5",
    block_number: 0x18cb51e,
    transaction_hash: "0x5b52d9609dc55022c13dc532fb0843d22063cb20faed8abbeeb32841ba115554",
    log_index: 0x1c,
};

/// Real. TricryptoUSDC (NG): two extra words (fee, packed price scale).
pub const CURVE_NG_EXCHANGE: RawLog = RawLog {
    address: "0x7f86bf177dd4f3494b841a37e810a34dd56c829b",
    topics: &[
        "0x143f1f8e861fbdeddd5b46e844b7d3ac7b86a122f36e8c463859ee6811b1f29c",
        "0x000000000000000000000000c10ee9031f2a0b84766a86b55a8d90f357910fb4",
    ],
    data: "0x0000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000017a8043453383e30000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000fad44a5000000000000000000000000000000000000000000000000000000000003e3f60000000000000083719c5908bce67b05000000000000101dd10b568690e209d7",
    block_number: 0x18cb528,
    transaction_hash: "0x36408bb950f88475d44abc0bef650eefd20de32a091400e03c3545ef4b0cfd49",
    log_index: 0x246,
};

/// Real. Base, Aerodrome factory (`stable` indexed, false).
pub const AERODROME_POOL_CREATED: RawLog = RawLog {
    address: "0x420dd381b31aef6683db6b902084cb0ffece40da",
    topics: &[
        "0x2128d88d14c80cb081c1252a5acff7a264671bf199ce226b53788fb26065005e",
        "0x0000000000000000000000001adf7cb7033a7f8718e0f4959c089e29b6b0bff0",
        "0x000000000000000000000000833589fcd6edb6e08f4c7c32d4f71b54bda02913",
        "0x0000000000000000000000000000000000000000000000000000000000000000",
    ],
    data: "0x00000000000000000000000064fad8c0320c9d03be2ebeda64aa4bbfa474fafe000000000000000000000000000000000000000000000000000000000000726b",
    block_number: 0x3119fe7,
    transaction_hash: "0x951dd23d3e9a6da708eb068ef2022f048a7162a26b0e37dcc9e438012ffb5d36",
    log_index: 0x1b2,
};

/// Aerodrome vAMM WETH/USDC on Base (token0 = WETH, token1 = USDC).
pub const AERODROME_WETH_USDC: &str =
    "0xcdac0d6c6c59727a65f871236188350531885c43";

/// Real. 2.015 USDC in, 0.000765 WETH out.
pub const AERODROME_SWAP: RawLog = RawLog {
    address: AERODROME_WETH_USDC,
    topics: &[
        "0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b",
        "0x000000000000000000000000cf77a3ba9a5ca399b7c97c74d54e5b1beb874e43",
        "0x00000000000000000000000001784ef301d79e4b2df3a21ad9a536d4cf09a5ce",
    ],
    data: "0x000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001ebf180000000000000000000000000000000000000000000000000002b8762b0cf0600000000000000000000000000000000000000000000000000000000000000000",
    block_number: 0x311b34a,
    transaction_hash: "0x9cff90e38da700eb6cbe5be7c7964d1e8112761db0124a55b2bba6810f4966c5",
    log_index: 0x222,
};

/// Real.
pub const AERODROME_SYNC: RawLog = RawLog {
    address: AERODROME_WETH_USDC,
    topics: &[
        "0xcf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a",
    ],
    data: "0x00000000000000000000000000000000000000000000005b1cbdbce558fbccbb000000000000000000000000000000000000000000000000000004029ef8b75d",
    block_number: 0x311b34a,
    transaction_hash: "0x9cff90e38da700eb6cbe5be7c7964d1e8112761db0124a55b2bba6810f4966c5",
    log_index: 0x221,
};

/// Real.
pub const AERODROME_BURN: RawLog = RawLog {
    address: AERODROME_WETH_USDC,
    topics: &[
        "0x5d624aa9c148153ab3446c1b154f660ee7701e549fe9b62dab7171b1c80e6fa2",
        "0x000000000000000000000000cf77a3ba9a5ca399b7c97c74d54e5b1beb874e43",
        "0x0000000000000000000000000790c7fdc12d046f158e744e9edf0ef60c116be9",
    ],
    data: "0x000000000000000000000000000000000000000000000000000054f0b57df516000000000000000000000000000000000000000000000000000000000003bade",
    block_number: 0x3119642,
    transaction_hash: "0x0241a74701c7a826ca0d41c7eced87853b77837cdad5e641b5f76219ccbec482",
    log_index: 0xfd,
};

// ------------------------------------------- real transfers of those swaps
//
// The ERC-20 `Transfer` logs of the fixture transactions above (same
// receipts, same public endpoints): what corroborates each swap leg.

pub const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

macro_rules! real_transfer {
    ($name:ident, $doc:literal, $token:expr, $from:literal, $to:literal, $amount:literal, $like:expr, $index:literal) => {
        #[doc = $doc]
        pub const $name: RawLog = RawLog {
            address: $token,
            topics: &[TRANSFER_TOPIC, $from, $to],
            data: $amount,
            block_number: $like.block_number,
            transaction_hash: $like.transaction_hash,
            log_index: $index,
        };
    };
}

real_transfer!(
    V2_SWAP_WETH_IN,
    "Real. Router -> pair, 0.001 WETH, log 246 (the swap is log 249).",
    WETH,
    "0x0000000000000000000000007a250d5630b4cf539739df2c5dacb4c659f2488d",
    "0x000000000000000000000000b4e16d0168e52d35cacd2c6185b44281ec28c9dc",
    "0x00000000000000000000000000000000000000000000000000038d7ea4c68000",
    V2_SWAP,
    246
);
real_transfer!(
    V2_SWAP_USDC_OUT,
    "Real. Pair -> recipient, 2.624963 USDC, log 247.",
    USDC,
    "0x000000000000000000000000b4e16d0168e52d35cacd2c6185b44281ec28c9dc",
    "0x000000000000000000000000a524ecb8ca2592ac3ed9a562fd467c02e09e003e",
    "0x0000000000000000000000000000000000000000000000000000000000280dc3",
    V2_SWAP,
    247
);
real_transfer!(
    V3_SWAP_USDC_OUT,
    "Real. Pool -> recipient, 32,942.903993 USDC, log 0 (the swap is log 2).",
    USDC,
    "0x00000000000000000000000088e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
    "0x000000000000000000000000bdb3ba9ffe392549e1f8658dd2630c141fdf47b6",
    "0x00000000000000000000000000000000000000000000000000000007ab8cd2b9",
    V3_SWAP,
    0
);
real_transfer!(
    V3_SWAP_WETH_IN,
    "Real. Callback payment, 12.57 WETH -> pool, log 1.",
    WETH,
    "0x000000000000000000000000bdb3ba9ffe392549e1f8658dd2630c141fdf47b6",
    "0x00000000000000000000000088e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
    "0x000000000000000000000000000000000000000000000000ae7b5bf58b5d2800",
    V3_SWAP,
    1
);
real_transfer!(
    V4_TX_USDC_TAKEN,
    "Real. PoolManager -> caller, 29,558 USDC units, log 0 (before the swaps).",
    USDC,
    "0x000000000000000000000000000000000004444c5dc75cb358380d2e3de08a90",
    "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    "0x0000000000000000000000000000000000000000000000000000000000007376",
    V4_SWAP_USDC_IN,
    0
);
real_transfer!(
    V4_TX_WETH_TAKEN,
    "Real. PoolManager -> caller, 1.2318 WETH = the SUM of both swaps' output, log 1.",
    WETH,
    "0x000000000000000000000000000000000004444c5dc75cb358380d2e3de08a90",
    "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    "0x000000000000000000000000000000000000000000000000111845ae817f739c",
    V4_SWAP_USDC_IN,
    1
);
real_transfer!(
    V4_TX_USDC_SETTLED,
    "Real. Caller -> PoolManager, 1,793,618,318 = swap input + the 29,558 taken, log 4 (AFTER the swaps).",
    USDC,
    "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    "0x000000000000000000000000000000000004444c5dc75cb358380d2e3de08a90",
    "0x000000000000000000000000000000000000000000000000000000006ae8718e",
    V4_SWAP_USDC_IN,
    4
);
real_transfer!(
    V4_TX_USDT_SETTLED,
    "Real. Caller -> PoolManager, exactly the second swap's 1,428,368,405 USDT, log 5.",
    USDT,
    "0x0000000000000000000000000000000aa232009084bd71a5797d089aa4edfad4",
    "0x000000000000000000000000000000000004444c5dc75cb358380d2e3de08a90",
    "0x0000000000000000000000000000000000000000000000000000000055232c15",
    V4_SWAP_USDC_IN,
    5
);
real_transfer!(
    BALANCER_SWAP_TOKEN_IN,
    "Real. Sender -> Vault, the swap's amountIn, log 631 (AFTER the Swap, log 630).",
    "0x0f2d719407fdbeff09d87557abb7232601fd9f29",
    "0x0000000000000000000000008ead31c4801322619584f1dc324cb5925f538049",
    "0x000000000000000000000000ba12222222228d8ba445958a75a0704d566bf2c8",
    "0x0000000000000000000000000000000000000000000000012a21770000000000",
    BALANCER_SWAP,
    631
);
real_transfer!(
    BALANCER_SWAP_WETH_OUT,
    "Real. Vault -> sender, the swap's amountOut, log 633.",
    WETH,
    "0x000000000000000000000000ba12222222228d8ba445958a75a0704d566bf2c8",
    "0x0000000000000000000000008ead31c4801322619584f1dc324cb5925f538049",
    "0x0000000000000000000000000000000000000000000000000005e482f3f57278",
    BALANCER_SWAP,
    633
);
real_transfer!(
    CURVE_3POOL_USDT_IN,
    "Real. Router -> 3pool, 0.099206 USDT, log 424 (the exchange is log 426).",
    USDT,
    "0x000000000000000000000000ad6cea45f98444a922a2b4fe96b8c90f0862d2f4",
    "0x000000000000000000000000bebc44782c7db0a1a60cb6fe97d0b483032ff1c7",
    "0x0000000000000000000000000000000000000000000000000000000000018386",
    CURVE_3POOL_EXCHANGE,
    424
);
real_transfer!(
    CURVE_3POOL_USDC_OUT,
    "Real. 3pool -> router, 0.099112 USDC, log 425.",
    USDC,
    "0x000000000000000000000000bebc44782c7db0a1a60cb6fe97d0b483032ff1c7",
    "0x000000000000000000000000ad6cea45f98444a922a2b4fe96b8c90f0862d2f4",
    "0x0000000000000000000000000000000000000000000000000000000000018328",
    CURVE_3POOL_EXCHANGE,
    425
);

/// An ERC-20 `Transfer` of `token` (constructed).
#[allow(clippy::too_many_arguments)]
pub fn transfer(
    token: Address,
    from: Address,
    to: Address,
    amount: U256,
    block_number: u32,
    log_index: u16,
    timestamp: u32,
) -> DatabaseLog {
    build(
        token,
        &[hash(TRANSFER_TOPIC), from.into_word(), to.into_word()],
        amount.to_be_bytes::<32>().to_vec(),
        block_number,
        log_index,
        timestamp,
    )
}

/// Puts `logs` into one transaction: one hash, and one `transaction_index`
/// (the `tx_index` column of docs/design.md §13).
///
/// The index is the smallest log index of the group. A transaction's logs
/// are contiguous inside a block, so that value is distinct per transaction
/// and orders the transactions exactly as the chain does - which is what
/// `(chain, block_number, tx_index, ordinal)` has to sort by.
pub fn same_transaction(
    mut logs: Vec<DatabaseLog>,
    transaction: u64,
) -> Vec<DatabaseLog> {
    let index =
        logs.iter().map(|log| log.log_index).min().unwrap_or_default();

    for log in &mut logs {
        log.transaction_hash = B256::from(U256::from(transaction));
        log.transaction_index = index;
    }
    logs
}

// ---------------------------------------------------------- constructed logs

fn padded(address: Address) -> B256 {
    address.into_word()
}

fn number(value: u64) -> Vec<u8> {
    U256::from(value).to_be_bytes::<32>().to_vec()
}

/// Constructed. A V2 `PairCreated` announcing `pair`.
pub fn v2_pair_created_for(pair: Address) -> DatabaseLog {
    let mut log = log_with(
        &[
            super::events::V2_PAIR_CREATED.topic0,
            padded(Address::repeat_byte(0x0a)),
            padded(Address::repeat_byte(0x0b)),
        ],
        [padded(pair).to_vec(), number(1)].concat(),
    );
    log.address = Address::repeat_byte(0xfa);
    log
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::{
        decode, events,
        models::{pool_id_of, LiquidityKind, PoolSource, Protocol},
        DexRows,
    };

    fn one(raw: &RawLog) -> DexRows {
        let rows = decode(1, &[raw.log()]);
        assert_eq!(rows.rows(), 1, "{}", raw.transaction_hash);
        rows
    }

    #[test]
    fn uniswap_v2_pair_created() {
        let pool = one(&V2_PAIR_CREATED).pools.remove(0);
        let pair = address("0x961c476fe0d91330b26600d46379f9382fe3b8f0");

        assert_eq!(pool.protocol, Protocol::UniswapV2);
        assert_eq!(pool.pool_id, pool_id_of(pair));
        assert_eq!(pool.emitter, pair);
        assert_eq!(pool.factory, address(V2_PAIR_CREATED.address));
        assert_eq!(
            pool.token0,
            address("0x2dfde9822a22abb9f92daf9c79fa3b34558748ae")
        );
        assert_eq!(pool.token1, address(WETH));
        assert_eq!(pool.tokens, vec![pool.token0, pool.token1]);
        assert_eq!(pool.created_block, 0x18cd705);
        assert_eq!((pool.tx_index, pool.ordinal), (3, 0x3a4));
        assert_eq!(pool.source, PoolSource::Event);
        assert_eq!((pool._version, pool.epoch), (0, 0));
        assert_eq!(pool.timestamp, 1_700_000_000);
    }

    #[test]
    fn uniswap_v2_swap_is_netted_pool_relative() {
        let swap = one(&V2_SWAP).swaps.remove(0);

        assert_eq!(swap.chain, 1);
        assert_eq!(swap.protocol, Protocol::UniswapV2);
        assert_eq!(swap.pool_id, pool_id_of(address(V2_USDC_WETH)));
        assert_eq!(swap.emitter, address(V2_USDC_WETH));
        assert_eq!(swap.block_number, 0x18cd71f);
        assert_eq!(swap.ordinal, 0xf9);
        assert_eq!(
            swap.tx_id,
            crate::db::format::tx_id(hash(V2_SWAP.transaction_hash))
        );
        assert_eq!(
            swap.sender,
            address("0x7a250d5630b4cf539739df2c5dacb4c659f2488d")
        );
        assert_eq!(
            swap.recipient,
            address("0xa524ecb8ca2592ac3ed9a562fd467c02e09e003e")
        );
        assert_eq!(swap.trader, swap.recipient);
        // USDC left the pool, WETH entered it.
        assert_eq!(swap.amount0, signed("-2624963"));
        assert_eq!(swap.amount1, signed("1000000000000000"));
        // Every family: what went in, what came out.
        assert_eq!(swap.amount_in, unsigned("1000000000000000"));
        assert_eq!(swap.amount_out, unsigned("2624963"));
        // Alone, nothing proves the legs.
        assert_eq!(swap.verified_in, Address::ZERO);
        assert_eq!(swap.sqrt_price_x96, U256::ZERO);
    }

    #[test]
    fn uniswap_v2_liquidity() {
        let sync = one(&V2_SYNC).liquidity.remove(0);
        assert_eq!(sync.kind, LiquidityKind::Sync);
        assert_eq!(sync.protocol, Protocol::UniswapV2);
        assert_eq!(sync.reserve0, unsigned("10391705448638"));
        assert_eq!(sync.reserve1, unsigned("3946924532103308992521"));
        assert_eq!(sync.amount0, I256::ZERO);

        let mint = one(&V2_MINT).liquidity.remove(0);
        assert_eq!(mint.kind, LiquidityKind::Mint);
        assert_eq!(mint.amount0, signed("19276872964048"));
        assert_eq!(mint.amount1, signed("7820526965157322899701"));
        assert_eq!(
            mint.sender,
            address("0x7a250d5630b4cf539739df2c5dacb4c659f2488d")
        );

        let burn = one(&V2_BURN).liquidity.remove(0);
        assert_eq!(burn.kind, LiquidityKind::Burn);
        assert_eq!(burn.amount0, signed("-19276872964047"));
        assert_eq!(burn.amount1, signed("-7820526965157253556124"));
        assert_eq!(
            burn.owner,
            address("0x0ea041260095b20feda41ef4fdbef7d801f4c438")
        );
    }

    #[test]
    fn uniswap_v3_pool_created() {
        let pool = one(&V3_POOL_CREATED).pools.remove(0);

        assert_eq!(pool.protocol, Protocol::UniswapV3);
        assert_eq!(
            pool.emitter,
            address("0x7514ea10c75582f76d71226642cf7492d3040606")
        );
        assert_eq!(pool.token0, address(WETH));
        assert_eq!(
            pool.token1,
            address("0xf21dd66f8f75203a5acff45433d80cde97c69877")
        );
        assert_eq!((pool.fee, pool.tick_spacing), (3000, 60));
    }

    #[test]
    fn uniswap_v3_swap_keeps_its_native_signs() {
        let swap = one(&V3_SWAP).swaps.remove(0);

        assert_eq!(swap.protocol, Protocol::UniswapV3);
        assert_eq!(swap.pool_id, pool_id_of(address(V3_USDC_WETH)));
        assert_eq!(swap.amount0, signed("-32942903993"));
        assert_eq!(swap.amount1, signed("12572743894898124800"));
        assert_eq!(
            swap.sqrt_price_x96,
            unsigned("1547521364678359767176169597843369")
        );
        assert_eq!(swap.liquidity, unsigned("4424671977927321079"));
        assert_eq!(swap.tick, 197_606);
        assert_eq!(swap.fee, 0);
    }

    #[test]
    fn uniswap_v3_liquidity() {
        let mint = one(&V3_MINT).liquidity.remove(0);
        let manager =
            address("0xc36442b4a4522e871399cd717abdd847ab11fe88");

        assert_eq!(mint.kind, LiquidityKind::Mint);
        assert_eq!((mint.owner, mint.sender), (manager, manager));
        assert_eq!((mint.tick_lower, mint.tick_upper), (197_660, 198_460));
        assert_eq!(mint.liquidity_delta, signed("2243762523041004"));
        assert_eq!(mint.amount0, signed("2188579403"));
        assert_eq!(mint.amount1, signed("901637004382658422"));

        let burn = one(&V3_BURN).liquidity.remove(0);
        assert_eq!(burn.kind, LiquidityKind::Burn);
        assert_eq!((burn.tick_lower, burn.tick_upper), (197_980, 198_340));
        assert_eq!(burn.liquidity_delta, signed("-49751082673707805"));
        assert_eq!(burn.amount0, signed("-44594838793"));
        assert_eq!(burn.amount1, I256::ZERO);
    }

    #[test]
    fn pancake_v3_swap_variant() {
        let swap = one(&PANCAKE_V3_SWAP).swaps.remove(0);

        assert_eq!(swap.protocol, Protocol::UniswapV3);
        assert_eq!(swap.amount0, signed("1051662441736548256"));
        assert_eq!(swap.amount1, signed("-1376813850099071"));
        assert_eq!(
            swap.sqrt_price_x96,
            unsigned("2867395584693802839876847746")
        );
        assert_eq!(swap.tick, -66_382);
    }

    #[test]
    fn uniswap_v4_initialize_with_the_native_currency() {
        let pool = one(&V4_INITIALIZE).pools.remove(0);

        assert_eq!(pool.protocol, Protocol::UniswapV4);
        assert_eq!(pool.pool_id, hash(V4_INITIALIZE.topics[1]));
        assert_eq!(pool.emitter, address(V4_POOL_MANAGER));
        assert_eq!(pool.token0, Address::ZERO);
        assert_eq!(
            pool.token1,
            address("0x0da899de61658e089444e1071022087966ad9e78")
        );
        assert_eq!((pool.fee, pool.tick_spacing), (10_000, 200));
        assert_eq!(pool.hooks, Address::ZERO);
    }

    #[test]
    fn v4_swaps_are_pool_relative() {
        let rows =
            decode(1, &[V4_SWAP_USDC_IN.log(), V4_SWAP_USDT_IN.log()]);
        assert_eq!(rows.swaps.len(), 2);

        // The caller PAID 1,793.58 USDC (token0): positive = into the pool.
        let usdc = &rows.swaps[0];
        assert_eq!(usdc.protocol, Protocol::UniswapV4);
        assert_eq!(usdc.pool_id, hash(V4_SWAP_USDC_IN.topics[1]));
        assert_eq!(usdc.emitter, address(V4_POOL_MANAGER));
        assert_eq!(usdc.amount0, signed("1793588760"));
        assert_eq!(usdc.amount1, signed("-685766237544796922"));
        assert_eq!(
            usdc.sqrt_price_x96,
            unsigned("1549152842264686185050558906471319")
        );
        assert_eq!(usdc.tick, 197_627);
        assert_eq!(usdc.recipient, Address::ZERO);
        assert_eq!(
            usdc.sender,
            address("0x0000000aa232009084bd71a5797d089aa4edfad4")
        );
        assert_eq!(usdc.trader, usdc.sender);

        // The caller PAID 1,428.37 USDT (token1) for 0.546 ETH (token0).
        let usdt = &rows.swaps[1];
        assert_eq!(usdt.amount0, signed("-546044876340273314"));
        assert_eq!(usdt.amount1, signed("1428368405"));
        assert_eq!(usdt.tick, -197_627);
    }

    #[test]
    fn uniswap_v4_modify_liquidity() {
        let row = one(&V4_MODIFY_LIQUIDITY).liquidity.remove(0);

        assert_eq!(row.kind, LiquidityKind::Modify);
        assert_eq!(row.pool_id, hash(V4_MODIFY_LIQUIDITY.topics[1]));
        assert_eq!((row.tick_lower, row.tick_upper), (-278_000, -265_000));
        assert_eq!(row.liquidity_delta, signed("209508784773568"));
        assert_eq!(row.amount0, I256::ZERO);
    }

    #[test]
    fn balancer_pool_and_its_tokens_become_one_row() {
        let rows = decode(
            1,
            &[
                BALANCER_POOL_REGISTERED.log(),
                BALANCER_TOKENS_REGISTERED.log(),
            ],
        );

        assert_eq!(rows.pools.len(), 1);
        let pool = &rows.pools[0];

        assert_eq!(pool.protocol, Protocol::BalancerV2);
        assert_eq!(pool.pool_id, hash(BALANCER_POOL_REGISTERED.topics[1]));
        assert_eq!(pool.emitter, address(BALANCER_VAULT));
        assert_eq!(
            pool.tokens,
            vec![
                address("0xba100000625a3754423978a60c9317c58a424e3d"),
                address(WETH),
            ]
        );
        // Multi asset family: no token0 / token1.
        assert_eq!(
            (pool.token0, pool.token1),
            (Address::ZERO, Address::ZERO)
        );
        assert_eq!(pool.ordinal, 0x20);

        // Alone, the tokens still produce a row, at a later position:
        // it loses against the `PoolRegistered` one (first event wins).
        let alone = one(&BALANCER_TOKENS_REGISTERED).pools.remove(0);
        assert_eq!(alone.tokens.len(), 2);
        assert!(alone.ordinal > pool.ordinal);
    }

    #[test]
    fn balancer_swap_carries_its_tokens() {
        let swap = one(&BALANCER_SWAP).swaps.remove(0);

        assert_eq!(swap.protocol, Protocol::BalancerV2);
        assert_eq!(swap.pool_id, hash(BALANCER_SWAP.topics[1]));
        assert_eq!(swap.emitter, address(BALANCER_VAULT));
        assert_eq!(
            swap.token_in,
            address("0x0f2d719407fdbeff09d87557abb7232601fd9f29")
        );
        assert_eq!(swap.token_out, address(WETH));
        assert_eq!(swap.amount_in, unsigned("21482582539417681920"));
        assert_eq!(swap.amount_out, unsigned("1658625973383800"));
        assert_eq!((swap.amount0, swap.amount1), (I256::ZERO, I256::ZERO));
    }

    #[test]
    fn curve_exchanges_keep_coin_indices() {
        let plain = one(&CURVE_3POOL_EXCHANGE).swaps.remove(0);
        assert_eq!(plain.protocol, Protocol::Curve);
        assert_eq!(
            plain.pool_id,
            pool_id_of(address(CURVE_3POOL_EXCHANGE.address))
        );
        assert_eq!((plain.coin_in, plain.coin_out), (2, 1));
        assert_eq!(plain.amount_in, unsigned("99206"));
        assert_eq!(plain.amount_out, unsigned("99112"));
        assert!(!plain.underlying);
        assert_eq!(plain.token_in, Address::ZERO);
        assert_eq!(
            plain.sender,
            address("0xad6cea45f98444a922a2b4fe96b8c90f0862d2f4")
        );

        let underlying = one(&CURVE_UNDERLYING_EXCHANGE).swaps.remove(0);
        assert!(underlying.underlying);
        assert_eq!((underlying.coin_in, underlying.coin_out), (0, 3));
        assert_eq!(
            underlying.amount_in,
            unsigned("283481790334824794968")
        );
        assert_eq!(underlying.amount_out, unsigned("281360453"));

        let crypto = one(&CURVE_CRYPTO_EXCHANGE).swaps.remove(0);
        assert_eq!((crypto.coin_in, crypto.coin_out), (2, 0));
        assert_eq!(crypto.amount_in, unsigned("727711365707735196"));
        assert_eq!(crypto.amount_out, unsigned("1798911413"));

        let ng = one(&CURVE_NG_EXCHANGE).swaps.remove(0);
        assert_eq!((ng.coin_in, ng.coin_out), (2, 0));
        assert_eq!(ng.amount_in, unsigned("106538567608796131"));
        assert_eq!(ng.amount_out, unsigned("263013541"));
    }

    #[test]
    fn solidly_aerodrome_events() {
        let pool = one(&AERODROME_POOL_CREATED).pools.remove(0);
        assert_eq!(pool.protocol, Protocol::Solidly);
        assert_eq!(
            pool.emitter,
            address("0x64fad8c0320c9d03be2ebeda64aa4bbfa474fafe")
        );
        assert_eq!(
            pool.token1,
            address("0x833589fcd6edb6e08f4c7c32d4f71b54bda02913")
        );
        assert!(!pool.stable);

        let swap = one(&AERODROME_SWAP).swaps.remove(0);
        assert_eq!(swap.protocol, Protocol::Solidly);
        assert_eq!(swap.amount0, signed("-765767621341280"));
        assert_eq!(swap.amount1, signed("2015000"));
        assert_eq!(
            swap.recipient,
            address("0x01784ef301d79e4b2df3a21ad9a536d4cf09a5ce")
        );

        let sync = one(&AERODROME_SYNC).liquidity.remove(0);
        assert_eq!(sync.protocol, Protocol::Solidly);
        assert_eq!(sync.reserve0, unsigned("1680724729804455922875"));
        assert_eq!(sync.reserve1, unsigned("4409303545693"));

        let burn = one(&AERODROME_BURN).liquidity.remove(0);
        assert_eq!(burn.kind, LiquidityKind::Burn);
        assert_eq!(burn.amount0, signed("-93392813815062"));
        assert_eq!(burn.amount1, signed("-244446"));
        assert_eq!(
            burn.owner,
            address("0x0790c7fdc12d046f158e744e9edf0ef60c116be9")
        );
    }

    /// Constructed: factory variants no public endpoint returned.
    #[test]
    fn constructed_pool_creation_variants() {
        let token0 = Address::repeat_byte(0x0a);
        let token1 = Address::repeat_byte(0x0b);
        let pool = Address::repeat_byte(0x99);
        let mut minus_sixty = [0xffu8; 32];
        minus_sixty[31] = 0xc4;

        let logs = [
            // Solidly V1: stable (true) is NOT indexed.
            log_with(
                &[
                    events::SOLIDLY_PAIR_CREATED.topic0,
                    padded(token0),
                    padded(token1),
                ],
                [number(1), padded(pool).to_vec(), number(7)].concat(),
            ),
            // Slipstream: tick spacing indexed (negative on purpose).
            log_with(
                &[
                    events::SLIPSTREAM_POOL_CREATED.topic0,
                    padded(token0),
                    padded(token1),
                    B256::from(minus_sixty),
                ],
                padded(pool).to_vec(),
            ),
            log_with(
                &[
                    events::ALGEBRA_POOL.topic0,
                    padded(token0),
                    padded(token1),
                ],
                padded(pool).to_vec(),
            ),
            log_with(
                &[
                    events::ALGEBRA_CUSTOM_POOL.topic0,
                    padded(Address::repeat_byte(0xde)),
                    padded(token0),
                    padded(token1),
                ],
                padded(pool).to_vec(),
            ),
        ];

        let pools = decode(1, &logs).pools;
        assert_eq!(pools.len(), 4);

        assert_eq!(pools[0].protocol, Protocol::Solidly);
        assert!(pools[0].stable);
        assert_eq!(pools[1].protocol, Protocol::UniswapV3);
        assert_eq!(pools[1].tick_spacing, -60);

        for created in &pools {
            assert_eq!(created.emitter, pool);
            assert_eq!(created.tokens, vec![token0, token1]);
        }
        for created in &pools[1..] {
            assert_eq!(created.protocol, Protocol::UniswapV3);
        }
    }

    /// Constructed: Algebra Integral swap = V3 swap + two fee words.
    #[test]
    fn constructed_algebra_integral_swap() {
        let mut data = V3_SWAP.data.parse::<Bytes>().unwrap().to_vec();
        data.extend(number(500));
        data.extend(number(0));

        let mut log = log_with(
            &[
                events::ALGEBRA_INTEGRAL_SWAP.topic0,
                hash(V3_SWAP.topics[1]),
                hash(V3_SWAP.topics[2]),
            ],
            data,
        );
        log.address = address(V3_SWAP.address);

        let swap = decode(1, &[log]).swaps.remove(0);
        assert_eq!(swap.protocol, Protocol::UniswapV3);
        assert_eq!(swap.amount0, signed("-32942903993"));
        assert_eq!(swap.tick, 197_606);
    }

    #[test]
    fn a_whole_batch_keeps_input_order() {
        let logs = [
            V2_SYNC.log(),
            V2_SWAP.log(),
            V3_SWAP.log(),
            BALANCER_SWAP.log(),
            CURVE_3POOL_EXCHANGE.log(),
            V4_SWAP_USDC_IN.log(),
            V3_POOL_CREATED.log(),
        ];

        let rows = decode(1, &logs);
        assert_eq!(
            (rows.pools.len(), rows.swaps.len(), rows.liquidity.len()),
            (1, 5, 1)
        );

        let protocols: Vec<Protocol> =
            rows.swaps.iter().map(|swap| swap.protocol).collect();
        assert_eq!(
            protocols,
            vec![
                Protocol::UniswapV2,
                Protocol::UniswapV3,
                Protocol::BalancerV2,
                Protocol::Curve,
                Protocol::UniswapV4,
            ]
        );

        // Contract pools are asked - the one announced in this batch too
        // (its event is only a claim) - singletons never.
        let candidates = rows.pool_candidates();
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[3].protocol, Protocol::UniswapV3);
        assert_eq!(candidates[0].address, address(V2_USDC_WETH));
        assert_eq!(candidates[1].address, address(V3_USDC_WETH));
        assert_eq!(candidates[2].protocol, Protocol::Curve);

        // WETH and the new token of the created pool.
        assert_eq!(rows.token_addresses().len(), 2);
    }
}
