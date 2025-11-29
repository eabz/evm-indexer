use alloy::primitives::Address;
use std::collections::HashMap;

/// DEX protocol information
#[derive(Debug, Clone)]
pub struct DexInfo {
    pub name: &'static str,
    pub version: &'static str,
}

impl DexInfo {
    pub fn display_name(&self) -> String {
        if self.version.is_empty() {
            self.name.to_string()
        } else {
            format!("{} {}", self.name, self.version)
        }
    }
}

/// DEX router addresses mapped by chain ID for automatic detection
#[derive(Clone)]
pub struct DexRouters {
    // Map: chain_id -> router_address -> DexInfo
    routers: HashMap<u64, HashMap<Address, DexInfo>>,
}

impl DexRouters {
    pub fn new() -> Self {
        let mut routers = HashMap::new();

        // ========== Ethereum (chain_id: 1) ==========
        let mut eth_routers = HashMap::new();

        // Uniswap
        eth_routers.insert(
            "0x7a250d5630B4cF539739dF2C5dAcb4c659F2488D".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V2" },
        );
        eth_routers.insert(
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        eth_routers.insert(
            "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // SushiSwap
        eth_routers.insert(
            "0xd9e1cE17f2641f24aE83637ab66a2cca9C378B9F".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // Balancer
        eth_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Curve
        eth_routers.insert(
            "0x99a58482BD7f6B857d7E1f08Cd40A4c2a0b3053f".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );
        eth_routers.insert(
            "0x4c6e1eF2D04b53d1b16014ceEd20e13f1e00e27F".parse().unwrap(),
            DexInfo { name: "Curve", version: "V2" },
        );

        // Curve 3Crv (deprecated but tracking)
        eth_routers.insert(
            "0x445FE580eF8d70FF569aB36e80c647af338db351".parse().unwrap(),
            DexInfo { name: "Curve", version: "3Crv" },
        );

        routers.insert(1, eth_routers);

        // ========== BSC (chain_id: 56) ==========
        let mut bsc_routers = HashMap::new();

        // PancakeSwap
        bsc_routers.insert(
            "0x10ED43C718714eb63d5aA57B78B54704E256024E".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V2" },
        );
        bsc_routers.insert(
            "0x1b81D678ffb9C0263b24A97847620C99d213eB14".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );
        bsc_routers.insert(
            "0x13f4EA83D0bd40E75C8222255bc855a974568Dd4".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );

        // Uniswap on BSC
        bsc_routers.insert(
            "0xB971eF87ede563556b2ED4b1C0b0019111Dd85d2".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // SushiSwap on BSC
        bsc_routers.insert(
            "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // Balancer on BSC
        bsc_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Thena (Solidly fork on BSC)
        bsc_routers.insert(
            "0xd4ae6eCA985340Dd434D38F470aCCce4DC78D109".parse().unwrap(),
            DexInfo { name: "Thena", version: "V1" },
        );
        bsc_routers.insert(
            "0x327Dd3208f0bCF590A66110aCB6e5e6941A4EfA0".parse().unwrap(),
            DexInfo { name: "Thena", version: "Fusion" },
        );

        // iZiSwap on BSC
        bsc_routers.insert(
            "0xBd3bd95529e0784aD973FD14928eEDF3678cfad8".parse().unwrap(),
            DexInfo { name: "iZiSwap", version: "" },
        );

        routers.insert(56, bsc_routers);

        // ========== Arbitrum (chain_id: 42161) ==========
        let mut arbitrum_routers = HashMap::new();

        // Uniswap on Arbitrum
        arbitrum_routers.insert(
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        arbitrum_routers.insert(
            "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // SushiSwap on Arbitrum
        arbitrum_routers.insert(
            "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // Camelot
        arbitrum_routers.insert(
            "0xc873fEcbd354f5A56E00E710B90EF4201db2448d".parse().unwrap(),
            DexInfo { name: "Camelot", version: "V2" },
        );
        arbitrum_routers.insert(
            "0x1F721E2E82F6676FCE4eA07A5958cF098D339e18".parse().unwrap(),
            DexInfo { name: "Camelot", version: "V3" },
        );

        // Balancer on Arbitrum
        arbitrum_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // TraderJoe on Arbitrum
        arbitrum_routers.insert(
            "0xb4315e873dBcf96Ffd0acd8EA43f689D8c20fB30".parse().unwrap(),
            DexInfo { name: "TraderJoe", version: "V2" },
        );

        // Aave on Arbitrum (lending protocol, not DEX swap)
        arbitrum_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // GMX
        arbitrum_routers.insert(
            "0xaBBc3E6db6a476353B5301aEA43f25AB0cFFb3B1".parse().unwrap(),
            DexInfo { name: "GMX", version: "" },
        );

        // Gains Network
        arbitrum_routers.insert(
            "0x18d96f45F95b73975033547eae59b02dCFF24635".parse().unwrap(),
            DexInfo { name: "Gains Network", version: "" },
        );

        // Ramses (Solidly fork on Arbitrum)
        arbitrum_routers.insert(
            "0xAAA87963EFeB6f7E0a2711F397663105Acb1805e".parse().unwrap(),
            DexInfo { name: "Ramses", version: "V1" },
        );
        arbitrum_routers.insert(
            "0xAA23611badAFB62D37E7295A682D21960ac85A90".parse().unwrap(),
            DexInfo { name: "Ramses", version: "V2" },
        );

        // Zyberswap (Algebra fork)
        arbitrum_routers.insert(
            "0x16e71B13fE6079B4312063F7E81F76d165Ad32Ad".parse().unwrap(),
            DexInfo { name: "Zyberswap", version: "" },
        );

        routers.insert(42161, arbitrum_routers);

        // ========== Optimism (chain_id: 10) ==========
        let mut optimism_routers = HashMap::new();

        // Uniswap on Optimism
        optimism_routers.insert(
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        optimism_routers.insert(
            "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // Velodrome V1
        optimism_routers.insert(
            "0x9c12939334C3742416f400C3793D6d271Fd3666f".parse().unwrap(),
            DexInfo { name: "Velodrome", version: "V1" },
        );
        // Velodrome V2
        optimism_routers.insert(
            "0xa062aE8A9c5e11aaA026fc2670B0D65cCc8B2858".parse().unwrap(),
            DexInfo { name: "Velodrome", version: "V2" },
        );

        // SushiSwap on Optimism
        optimism_routers.insert(
            "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // Balancer on Optimism
        optimism_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Aave on Optimism (lending protocol, not DEX swap)
        optimism_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Synthetix
        optimism_routers.insert(
            "0x2e5dB100552b932b299c3cc3b0253481fFEbA513".parse().unwrap(),
            DexInfo { name: "Synthetix", version: "" },
        );

        // Kwenta (perps)
        optimism_routers.insert(
            "0x8dAEBADE922dF735c38C80C7eBD708Af50815fAa".parse().unwrap(),
            DexInfo { name: "Kwenta", version: "" },
        );

        routers.insert(10, optimism_routers);

        // ========== Base (chain_id: 8453) ==========
        let mut base_routers = HashMap::new();

        // Uniswap on Base
        base_routers.insert(
            "0x2626664c2603336E57B271c5C0b26F421741e481".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        base_routers.insert(
            "0x4752ba5DBc23f44D87826276BF6Fd6b1C372aD24".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // Aerodrome V1
        base_routers.insert(
            "0xcF77a3ba9A5CA399B7c97c74d54e5b1Beb874e43".parse().unwrap(),
            DexInfo { name: "Aerodrome", version: "V1" },
        );
        // Aerodrome V2
        base_routers.insert(
            "0x6Cb442acF35158D5eDa88fe602221b67B400Be3E".parse().unwrap(),
            DexInfo { name: "Aerodrome", version: "V2" },
        );

        // Alien Base
        base_routers.insert(
            "0x8c1A3cF8f83074169FE5D7aD50B978e1cD6b37c7".parse().unwrap(),
            DexInfo { name: "AlienBase", version: "" },
        );

        // SushiSwap on Base
        base_routers.insert(
            "0x6BDED42c6DA8FBf0d2bA55B2fa120C5e0c8D7891".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // BaseSwap
        base_routers.insert(
            "0x327Df1E6de05895d2ab08513aaDD9313Fe505d86".parse().unwrap(),
            DexInfo { name: "BaseSwap", version: "" },
        );

        // Aave on Base (lending protocol, not DEX swap)
        base_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Balancer on Base
        base_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Maverick
        base_routers.insert(
            "0x32aFc0b3f1dFd463B4bFDc4C92f9DF95ed66D08f".parse().unwrap(),
            DexInfo { name: "Maverick", version: "V1" },
        );

        routers.insert(8453, base_routers);

        // ========== Avalanche (chain_id: 43114) ==========
        let mut avalanche_routers = HashMap::new();

        // TraderJoe
        avalanche_routers.insert(
            "0x60aE616a2155Ee3d9A68541Ba4544862310933d4".parse().unwrap(),
            DexInfo { name: "TraderJoe", version: "V1" },
        );
        avalanche_routers.insert(
            "0xb4315e873dBcf96Ffd0acd8EA43f689D8c20fB30".parse().unwrap(),
            DexInfo { name: "TraderJoe", version: "V2" },
        );

        // Pangolin
        avalanche_routers.insert(
            "0xE54Ca86531e17Ef3616d22Ca28b0D458b6C89106".parse().unwrap(),
            DexInfo { name: "Pangolin", version: "" },
        );

        // SushiSwap on Avalanche
        avalanche_routers.insert(
            "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // Aave on Avalanche (lending protocol, not DEX swap)
        avalanche_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Balancer on Avalanche
        avalanche_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Platypus
        avalanche_routers.insert(
            "0x66357dCaCe80353121602d9C76Db1cA6a7b16D8d".parse().unwrap(),
            DexInfo { name: "Platypus", version: "" },
        );

        // Teddy
        avalanche_routers.insert(
            "0x54eAacE40807D8b3927F59985d2Ef8d2bECC5e76".parse().unwrap(),
            DexInfo { name: "Teddy", version: "" },
        );

        // Pharaoh (Solidly fork on Avalanche)
        avalanche_routers.insert(
            "0xAAAE99091Fbb28D400029052821653c1C752483B".parse().unwrap(),
            DexInfo { name: "Pharaoh", version: "" },
        );

        routers.insert(43114, avalanche_routers);

        // ========== Monad (chain_id: 143) ==========
        let mut monad_routers = HashMap::new();

        // Clober - On-chain CLOB DEX
        monad_routers.insert(
            "0x7B58A24C5628881a141D630f101Db433D419B372".parse().unwrap(),
            DexInfo { name: "Clober", version: "" },
        );

        // Kuru - On-chain orderbook exchange and trading hub
        monad_routers.insert(
            "0xd651346d7c789536ebf06dc72aE3C8502cd695CC".parse().unwrap(),
            DexInfo { name: "Kuru", version: "" },
        );

        // Uniswap on Monad - Universal Router
        monad_routers.insert(
            "0x0d97dc33264bfc1c226207428a79b26757fb9dc3".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // PancakeSwap on Monad - Smart Router
        monad_routers.insert(
            "0x21114915Ac6d5A2e156931e20B20b038dEd0Be7C".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );

        // Curve Finance on Monad - Main Router
        monad_routers.insert(
            "0xFF5Cb29241F002fFeD2eAa224e3e996D24A6E8d1".parse().unwrap(),
            DexInfo { name: "Curve", version: "" },
        );

        // LFJ - High-performance DEX with DLMM
        monad_routers.insert(
            "0x18556DA13313f3532c54711497A8FedAC273220E".parse().unwrap(),
            DexInfo { name: "LFJ", version: "" },
        );

        routers.insert(143, monad_routers);

        Self { routers }
    }

    /// Get DEX info from router address
    pub fn get_dex_from_router(
        &self,
        chain_id: u64,
        router: &Address,
    ) -> Option<&DexInfo> {
        self.routers
            .get(&chain_id)
            .and_then(|chain_routers| chain_routers.get(router))
    }
}

impl Default for DexRouters {
    fn default() -> Self {
        Self::new()
    }
}

/// DEX factory addresses mapped by chain ID for automatic detection
#[derive(Clone)]
pub struct DexFactories {
    // Map: chain_id -> factory_address -> DexInfo
    factories: HashMap<u64, HashMap<Address, DexInfo>>,
}

impl DexFactories {
    pub fn new() -> Self {
        let mut factories = HashMap::new();

        // ========== Ethereum (chain_id: 1) ==========
        let mut eth_factories = HashMap::new();

        // Uniswap V2
        eth_factories.insert(
            "0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V2" },
        );
        // Uniswap V3
        eth_factories.insert(
            "0x1F98431c8aD98523631AE4a59f267346ea31F984".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        // SushiSwap V2
        eth_factories.insert(
            "0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // PancakeSwap V3
        eth_factories.insert(
            "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );

        factories.insert(1, eth_factories);

        // ========== BSC (chain_id: 56) ==========
        let mut bsc_factories = HashMap::new();

        // PancakeSwap V2
        bsc_factories.insert(
            "0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V2" },
        );
        // PancakeSwap V3
        bsc_factories.insert(
            "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );
        // Uniswap V3 on BSC
        bsc_factories.insert(
            "0xdB1d10011AD0Ff90774D0C6Bb92e5C5c8b4461F7".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        factories.insert(56, bsc_factories);

        // ========== Arbitrum (chain_id: 42161) ==========
        let mut arbitrum_factories = HashMap::new();

        // Uniswap V3
        arbitrum_factories.insert(
            "0x1F98431c8aD98523631AE4a59f267346ea31F984".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        // SushiSwap V2
        arbitrum_factories.insert(
            "0xc35DADB6501285798e94939d464DAb448135372e".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );
        // Camelot V2
        arbitrum_factories.insert(
            "0x6EcCab422D763aC031210895C81787E87B43A652".parse().unwrap(),
            DexInfo { name: "Camelot", version: "V2" },
        );
        // Camelot V3
        arbitrum_factories.insert(
            "0x1a3c9B1d2F0529D97f2afC5136Cc237bE1C93DCC".parse().unwrap(),
            DexInfo { name: "Camelot", version: "V3" },
        );

        // PancakeSwap V3
        arbitrum_factories.insert(
            "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );

        factories.insert(42161, arbitrum_factories);

        // ========== Optimism (chain_id: 10) ==========
        let mut optimism_factories = HashMap::new();

        // Uniswap V3
        optimism_factories.insert(
            "0x1F98431c8aD98523631AE4a59f267346ea31F984".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        // Velodrome V2
        optimism_factories.insert(
            "0xF1046053aa5682b4F9a81b5481394DA16BE5FF5a".parse().unwrap(),
            DexInfo { name: "Velodrome", version: "V2" },
        );

        factories.insert(10, optimism_factories);

        // ========== Base (chain_id: 8453) ==========
        let mut base_factories = HashMap::new();

        // Uniswap V3
        base_factories.insert(
            "0x33128a8fC17869897dcE68Ed026d694621f6FDfD".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        // Aerodrome V2
        base_factories.insert(
            "0x420DD381b31aEf6683db6B902084cB0FFECe40Da".parse().unwrap(),
            DexInfo { name: "Aerodrome", version: "V2" },
        );
        // BaseSwap
        base_factories.insert(
            "0xFDa619b6d20975be80A10332cD39b9a4b0FAa8BB".parse().unwrap(),
            DexInfo { name: "BaseSwap", version: "" },
        );

        // PancakeSwap V3
        base_factories.insert(
            "0x0BFbCF9fa4f9C56B0F40a671Ad40E0805A091865".parse().unwrap(),
            DexInfo { name: "PancakeSwap", version: "V3" },
        );

        factories.insert(8453, base_factories);

        // ========== Avalanche (chain_id: 43114) ==========
        let mut avalanche_factories = HashMap::new();

        // TraderJoe V1
        avalanche_factories.insert(
            "0x9Ad6C38BE94206CA50bb0d90783181662f0Cfa10".parse().unwrap(),
            DexInfo { name: "TraderJoe", version: "V1" },
        );
        // TraderJoe V2.1
        avalanche_factories.insert(
            "0x8e42f2F4101563bF679975178e880FD87d3eFd4e".parse().unwrap(),
            DexInfo { name: "TraderJoe", version: "V2" },
        );
        // Pangolin
        avalanche_factories.insert(
            "0xefa94DE7a4656D787667C749f7E1223D71E9FD88".parse().unwrap(),
            DexInfo { name: "Pangolin", version: "" },
        );

        factories.insert(43114, avalanche_factories);

        // ========== Monad (chain_id: 143) ==========
        let mut monad_factories = HashMap::new();

        // Uniswap V3 on Monad
        monad_factories.insert(
            "0x204faca1764b154221e35c0d20abb3c525710498".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        factories.insert(143, monad_factories);

        Self { factories }
    }

    /// Get DEX info from factory address
    pub fn get_dex_from_factory(
        &self,
        chain_id: u64,
        factory: &Address,
    ) -> Option<&DexInfo> {
        self.factories
            .get(&chain_id)
            .and_then(|chain_factories| chain_factories.get(factory))
    }
}

impl Default for DexFactories {
    fn default() -> Self {
        Self::new()
    }
}
