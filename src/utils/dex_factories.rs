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

        // 1inch
        eth_routers.insert(
            "0x1111111254EEB25477B68fb85Ed929f73A960582".parse().unwrap(),
            DexInfo { name: "1inch", version: "V5" },
        );
        eth_routers.insert(
            "0x1111111254fb6c44bAC0beD2854e76F90643097d".parse().unwrap(),
            DexInfo { name: "1inch", version: "V4" },
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

        // DODO
        eth_routers.insert(
            "0x6B0431840294e53f1991bF8051413d90b8692CCb".parse().unwrap(),
            DexInfo { name: "DODO", version: "V1" },
        );
        eth_routers.insert(
            "0x3E712033605604bC3da248719D46B6b61a341142".parse().unwrap(),
            DexInfo { name: "DODO", version: "V2" },
        );

        // Curve 3Crv (deprecated but tracking)
        eth_routers.insert(
            "0x445FE580eF8d70FF569aB36e80c647af338db351".parse().unwrap(),
            DexInfo { name: "Curve", version: "3Crv" },
        );

        // Aave Flashloan Router
        eth_routers.insert(
            "0x7d2768dE32b0b80b7a3454c06BdAc94A69DDc7A9".parse().unwrap(),
            DexInfo { name: "Aave", version: "V2" },
        );

        // Compound
        eth_routers.insert(
            "0x3d9819210A31b4961b30EF54bE2aeB56B84ee3a0".parse().unwrap(),
            DexInfo { name: "Compound", version: "V2" },
        );

        // Yearn
        eth_routers.insert(
            "0x19D3364A9d1d463b7d7C6f95bcF2F7F482E2eBB1".parse().unwrap(),
            DexInfo { name: "Yearn", version: "V2" },
        );

        // Lido
        eth_routers.insert(
            "0xae7ab96520DE3a18E5e111B5EaAc1417064f0C31".parse().unwrap(),
            DexInfo { name: "Lido", version: "stETH" },
        );

        // Paraswap
        eth_routers.insert(
            "0x216B4B4ba9F3E719726886d346f1D6C3644QA8".parse().unwrap(),
            DexInfo { name: "ParaSwap", version: "V5" },
        );

        // OpenOcean
        eth_routers.insert(
            "0x6352a56caadc4f1e25cd6c75970fa768a3aaf514".parse().unwrap(),
            DexInfo { name: "OpenOcean", version: "" },
        );

        // Kyber
        eth_routers.insert(
            "0x1c87257F5e8609940Bc751a07BB085Bb7Fed0c64".parse().unwrap(),
            DexInfo { name: "Kyber", version: "V3" },
        );

        // CoW Protocol
        eth_routers.insert(
            "0x9008D19f58AAbD9eD0D60971565AA15BAa120Ff2".parse().unwrap(),
            DexInfo { name: "CoW Protocol", version: "V2" },
        );

        // Matcha / 0x
        eth_routers.insert(
            "0x6000da47483062A0D734Ba3dc87f17aA63c1e16F".parse().unwrap(),
            DexInfo { name: "Matcha", version: "V1" },
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

        // Biswap
        bsc_routers.insert(
            "0x3a6d8cA21D1CF76F653A67577FA0D27453350dD8".parse().unwrap(),
            DexInfo { name: "Biswap", version: "V2" },
        );

        // ApeSwap
        bsc_routers.insert(
            "0xcF0feBd3f17CEf5b47b0cD257aCf6025c5BFf3b7".parse().unwrap(),
            DexInfo { name: "ApeSwap", version: "" },
        );

        // BabySwap
        bsc_routers.insert(
            "0x325E343f1dE602396E256B67eFd1F61C3A6B38Bd".parse().unwrap(),
            DexInfo { name: "BabySwap", version: "" },
        );

        // Balancer on BSC
        bsc_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // DODO on BSC
        bsc_routers.insert(
            "0x8F8Bb984e652Cb8D0aa7E399189dD6C7e2F90f5E".parse().unwrap(),
            DexInfo { name: "DODO", version: "V2" },
        );

        // Wault Finance
        bsc_routers.insert(
            "0xD48745E1F8dB63Ba37e7300F2C4Ee3629D2a58a6".parse().unwrap(),
            DexInfo { name: "Wault", version: "" },
        );

        // Bakery Swap
        bsc_routers.insert(
            "0xCDe540411ECFb16eC3DC027ed4Cc912FFbE31405".parse().unwrap(),
            DexInfo { name: "BakerySwap", version: "" },
        );

        // SafeMoon
        bsc_routers.insert(
            "0x05fF2B0DB69458A0750Bada338Cb0455B5148e8E".parse().unwrap(),
            DexInfo { name: "SafeMoon", version: "" },
        );

        // Ellipsis Finance (curve fork)
        bsc_routers.insert(
            "0x7552c756E293f6b5c754eF32881Ec9b43215de26".parse().unwrap(),
            DexInfo { name: "Ellipsis", version: "" },
        );

        routers.insert(56, bsc_routers);

        // ========== Polygon (chain_id: 137) ==========
        let mut polygon_routers = HashMap::new();

        // Uniswap on Polygon
        polygon_routers.insert(
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );
        polygon_routers.insert(
            "0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45".parse().unwrap(),
            DexInfo { name: "Uniswap", version: "V3" },
        );

        // SushiSwap on Polygon
        polygon_routers.insert(
            "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506".parse().unwrap(),
            DexInfo { name: "SushiSwap", version: "V2" },
        );

        // QuickSwap
        polygon_routers.insert(
            "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff".parse().unwrap(),
            DexInfo { name: "QuickSwap", version: "V2" },
        );
        polygon_routers.insert(
            "0xf5b509bB0909a69B1c207E495f687a596C168E12".parse().unwrap(),
            DexInfo { name: "QuickSwap", version: "V3" },
        );

        // Balancer on Polygon
        polygon_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // Curve on Polygon
        polygon_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );

        // Aave on Polygon
        polygon_routers.insert(
            "0x7d2768dE32b0b80b7a3454c06BdAc94A69DDc7A9".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Wmatic Staking
        polygon_routers.insert(
            "0x5f3Dc4Cc9f4FfB386d925B51b10b33B64000FC91".parse().unwrap(),
            DexInfo { name: "PoS Portal", version: "Wmatic" },
        );

        // Polycat Finance
        polygon_routers.insert(
            "0x1E4F97b9f9F913EA8ee06f7D93a2D3A0Cc8EB2FC".parse().unwrap(),
            DexInfo { name: "Polycat", version: "" },
        );

        // Dfyn
        polygon_routers.insert(
            "0xA102072A7FD54eD864e64e75ec46F7C62c03a72b".parse().unwrap(),
            DexInfo { name: "Dfyn", version: "" },
        );

        // Matic Network
        polygon_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Matic", version: "Network" },
        );

        routers.insert(137, polygon_routers);

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

        // Curve on Arbitrum
        arbitrum_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );

        // DODO V2 on Arbitrum
        arbitrum_routers.insert(
            "0x88d7D573Ae20f34384D50fa5f1d2ff1E441667e2".parse().unwrap(),
            DexInfo { name: "DODO", version: "V2" },
        );

        // Aave on Arbitrum
        arbitrum_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Compound on Arbitrum
        arbitrum_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Compound", version: "V3" },
        );

        // Yearn on Arbitrum
        arbitrum_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Yearn", version: "V2" },
        );

        // GMX
        arbitrum_routers.insert(
            "0xaBBc3E6db6a476353B5301aEA43f25AB0cFFb3B1".parse().unwrap(),
            DexInfo { name: "GMX", version: "" },
        );

        // 1inch on Arbitrum
        arbitrum_routers.insert(
            "0x1111111254fb6c44bAC0beD2854e76F90643097d".parse().unwrap(),
            DexInfo { name: "1inch", version: "V5" },
        );

        // Gains Network
        arbitrum_routers.insert(
            "0x18d96f45F95b73975033547eae59b02dCFF24635".parse().unwrap(),
            DexInfo { name: "Gains Network", version: "" },
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

        // Velodrome
        optimism_routers.insert(
            "0x9c12939334C3742416f400C3793D6d271Fd3666f".parse().unwrap(),
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

        // Curve on Optimism
        optimism_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );

        // Aave on Optimism
        optimism_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Compound on Optimism
        optimism_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Compound", version: "V3" },
        );

        // 1inch on Optimism
        optimism_routers.insert(
            "0x1111111254fb6c44bAC0beD2854e76F90643097d".parse().unwrap(),
            DexInfo { name: "1inch", version: "V5" },
        );

        // Synthetix
        optimism_routers.insert(
            "0x2e5dB100552b932b299c3zc3b0253481fFEbA513".parse().unwrap(),
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

        // Aerodrome
        base_routers.insert(
            "0xcF77a3ba9A5CA399B7c97c74d54e5b1Beb874e43".parse().unwrap(),
            DexInfo { name: "Aerodrome", version: "V2" },
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

        // Curve on Base
        base_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );

        // DODO V2 on Base
        base_routers.insert(
            "0x6B0431840294e53f1991bF8051413d90b8692CCb".parse().unwrap(),
            DexInfo { name: "DODO", version: "V2" },
        );

        // Aave on Base
        base_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Compound on Base
        base_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Compound", version: "V3" },
        );

        // Balancer on Base
        base_routers.insert(
            "0xBA12222222228d8Ba445958a75a0704d566BF2C8".parse().unwrap(),
            DexInfo { name: "Balancer", version: "V2" },
        );

        // 1inch on Base
        base_routers.insert(
            "0x1111111254fb6c44bAC0beD2854e76F90643097d".parse().unwrap(),
            DexInfo { name: "1inch", version: "V5" },
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

        // Curve on Avalanche
        avalanche_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Curve", version: "V1" },
        );

        // DODO V2 on Avalanche
        avalanche_routers.insert(
            "0xb27682b145913e06bdb3d379762cf87e80e3c6e3".parse().unwrap(),
            DexInfo { name: "DODO", version: "V2" },
        );

        // Aave on Avalanche
        avalanche_routers.insert(
            "0x794a61358D6845594F94dc1DB02A252b5b4814aD".parse().unwrap(),
            DexInfo { name: "Aave", version: "V3" },
        );

        // Compound on Avalanche
        avalanche_routers.insert(
            "0x0000000000000000000000000000000000000000".parse().unwrap(),
            DexInfo { name: "Compound", version: "V3" },
        );

        // 1inch on Avalanche
        avalanche_routers.insert(
            "0x1111111254fb6c44bAC0beD2854e76F90643097d".parse().unwrap(),
            DexInfo { name: "1inch", version: "V5" },
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

        routers.insert(43114, avalanche_routers);

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
