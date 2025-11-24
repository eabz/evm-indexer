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
