use std::collections::HashMap;

use primitive_types::U256;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BalanceAllocation {
    balance: U256,
}

#[derive(Debug, Clone)]
pub struct Chain {
    pub id: u64,
    pub name: &'static str,
    pub supports_blocks_receipts: bool,
    pub supports_trace_block: bool,
}

pub const ETHEREUM: Chain = Chain {
    id: 1,
    name: "ethereum",
    supports_blocks_receipts: true,
    supports_trace_block: true,
};

pub const POLYGON: Chain = Chain {
    id: 137,
    name: "polygon",
    supports_blocks_receipts: true,
    supports_trace_block: true,
};

pub const BSC: Chain = Chain {
    id: 56,
    name: "bsc",
    supports_blocks_receipts: true,
    supports_trace_block: true,
};

pub static CHAINS: [Chain; 3] = [ETHEREUM, POLYGON, BSC];

pub fn get_chains() -> HashMap<u64, Chain> {
    let mut chains: HashMap<u64, Chain> = HashMap::new();

    for chain in CHAINS.iter() {
        chains.insert(chain.id, chain.to_owned());
    }

    chains
}

pub fn get_chain(chain: u64) -> Chain {
    let chains = get_chains();

    let selected_chain = chains.get(&chain).expect("chain not found.");

    selected_chain.to_owned()
}
