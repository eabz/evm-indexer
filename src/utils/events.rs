use alloy::primitives::{b256, B256};

/// `Transfer(address,address,uint256)` - shared by ERC20 (2 indexed
/// arguments) and ERC721 (3 indexed arguments).
pub const TRANSFER_EVENT_SIGNATURE: B256 = b256!(
    "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
);

/// `TransferSingle(address,address,address,uint256,uint256)`
pub const ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE: B256 = b256!(
    "c3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62"
);

/// `TransferBatch(address,address,address,uint256[],uint256[])`
pub const ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE: B256 = b256!(
    "4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb"
);
