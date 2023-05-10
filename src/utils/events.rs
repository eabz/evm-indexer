use ethabi::ParamType;
use ethers::abi::ethabi;

pub fn transfer_event() -> ethabi::Event {
    ethabi::Event {
        name: "Transfer".to_owned(),
        inputs: vec![
            ethabi::EventParam {
                name: "from".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "to".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "amount".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
        ],
        anonymous: false,
    }
}

pub static TRANSFER_EVENTS_SIGNATURE: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

pub fn swap_event() -> ethabi::Event {
    ethabi::Event {
        name: "Swap".to_owned(),
        inputs: vec![
            ethabi::EventParam {
                name: "sender".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "amount0In".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "amount1In".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "amount0Out".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "amount1Out".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "to".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
        ],
        anonymous: false,
    }
}

pub static SWAP_EVENT_SIGNATURE: &str =
    "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822";

pub fn swapv_v3_event() -> ethabi::Event {
    ethabi::Event {
        name: "Swap".to_owned(),
        inputs: vec![
            ethabi::EventParam {
                name: "sender".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "recipient".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "amount0".to_owned(),
                kind: ParamType::Int(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "amount1".to_owned(),
                kind: ParamType::Int(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "amount1Out".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "sqrtPriceX96".to_owned(),
                kind: ParamType::Uint(160),
                indexed: false,
            },
            ethabi::EventParam {
                name: "liquidity".to_owned(),
                kind: ParamType::Uint(128),
                indexed: false,
            },
            ethabi::EventParam {
                name: "tick".to_owned(),
                kind: ParamType::Int(24),
                indexed: false,
            },
        ],
        anonymous: false,
    }
}

pub static SWAPV3_EVENT_SIGNATURE: &str =
    "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67";

pub fn erc1155_transfer_single_event() -> ethabi::Event {
    ethabi::Event {
        name: "TransferSingle".to_owned(),
        inputs: vec![
            ethabi::EventParam {
                name: "operator".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "from".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "to".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "id".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
            ethabi::EventParam {
                name: "value".to_owned(),
                kind: ParamType::Uint(256),
                indexed: false,
            },
        ],
        anonymous: false,
    }
}

pub static ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE: &str =
    "0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62";

pub fn erc1155_transfer_batch_event() -> ethabi::Event {
    ethabi::Event {
        name: "TransferBatch".to_owned(),
        inputs: vec![
            ethabi::EventParam {
                name: "operator".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "from".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "to".to_owned(),
                kind: ParamType::Address,
                indexed: true,
            },
            ethabi::EventParam {
                name: "ids".to_owned(),
                kind: ParamType::Array(Box::new(ParamType::Uint(256))),
                indexed: false,
            },
            ethabi::EventParam {
                name: "values".to_owned(),
                kind: ParamType::Array(Box::new(ParamType::Uint(256))),
                indexed: false,
            },
        ],
        anonymous: false,
    }
}

pub static ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE: &str =
    "0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb";
