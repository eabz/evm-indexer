use std::collections::HashMap;

#[derive(Debug, Clone, Copy)]
pub struct Chain {
    pub id: u64,
    pub name: &'static str,
    pub block_explorer: &'static str,
    pub abi_source_api: &'static str,
    pub abi_source_require_auth: bool,
    pub supports_blocks_receipts: bool,
    pub supports_trace_block: bool,
    pub multicall: &'static str,
}

impl Chain {
    pub fn new_from_borrowed(chain: &Chain) -> Self {
        Self {
            id: chain.id,
            name: chain.name,
            block_explorer: chain.block_explorer,
            abi_source_api: chain.abi_source_api,
            abi_source_require_auth: chain.abi_source_require_auth,
            supports_blocks_receipts: chain.supports_blocks_receipts,
            multicall: chain.multicall,
            supports_trace_block: chain.supports_trace_block,
        }
    }
}

pub const ETHEREUM: Chain = Chain {
    id: 1,
    name: "ethereum",
    block_explorer: "https://etherscan.io/",
    abi_source_api: "https://api.etherscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: true,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: true,
};

pub const POLYGON: Chain = Chain {
    id: 137,
    name: "polygon",
    block_explorer: "https://polygonscan.com/",
    abi_source_api: "https://api.polygonscan.com/",
    abi_source_require_auth: true,
    supports_blocks_receipts: true,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: true,
};

pub const FANTOM: Chain = Chain {
    id: 250,
    name: "fantom",
    block_explorer: "https://ftmscan.com/",
    abi_source_api: "https://api.ftmscan.com/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: true,
};

pub const BSC: Chain = Chain {
    id: 56,
    name: "bsc",
    block_explorer: "https://bscscan.com/",
    abi_source_api: "https://api.bscscan.com/",
    abi_source_require_auth: true,
    supports_blocks_receipts: true,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: true,
};

pub const GNOSIS: Chain = Chain {
    id: 100,
    name: "gnosis",
    block_explorer: "https://gnosisscan.io/",
    abi_source_api: "https://api.gnosisscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: true,
};

pub const OPTIMISM: Chain = Chain {
    id: 10,
    name: "optimism",
    block_explorer: "https://optimistic.etherscan.io/",
    abi_source_api: "https://api-optimistic.etherscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: false,
};

pub const ARBITRUM_ONE: Chain = Chain {
    id: 42161,
    name: "arbitrum",
    block_explorer: "https://arbiscan.io/",
    abi_source_api: "https://api.arbiscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: false,
};

pub const ARBITRUM_NOVA: Chain = Chain {
    id: 42170,
    name: "arbitrum-nova",
    block_explorer: "https://nova.arbiscan.io/",
    abi_source_api: "https://api-nova.arbiscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: false,
};

pub const AVALANCHE: Chain = Chain {
    id: 43114,
    name: "avalanche",
    block_explorer: "https://snowtrace.io/",
    abi_source_api: "https://api.snowtrace.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: false,
};

pub const CELO: Chain = Chain {
    id: 42220,
    name: "celo",
    block_explorer: "https://celoscan.io/",
    abi_source_api: "https://api.celoscan.io/",
    abi_source_require_auth: true,
    supports_blocks_receipts: false,
    multicall: "0xcA11bde05977b3631167028862bE2a173976CA11",
    supports_trace_block: false,
};

pub static CHAINS: [Chain; 10] = [
    ETHEREUM,
    POLYGON,
    FANTOM,
    BSC,
    GNOSIS,
    OPTIMISM,
    ARBITRUM_ONE,
    ARBITRUM_NOVA,
    AVALANCHE,
    CELO,
];

pub fn get_chains() -> HashMap<u64, Chain> {
    let mut chains: HashMap<u64, Chain> = HashMap::new();

    for chain in CHAINS.into_iter() {
        chains.insert(chain.id, chain);
    }

    chains
}

pub fn get_chain(chain_id: u64) -> Chain {
    let chains = get_chains();

    let selected_chain = chains.get(&chain_id).expect("chain not found.");

    Chain::new_from_borrowed(selected_chain)
}
