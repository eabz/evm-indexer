//! HyperSync response -> core rows. Pure, no I/O.
//!
//! Everything that turns wire data into a row of this dataset lives here:
//! the per-table `from_hypersync` conversions, the ERC-20 / ERC-721 /
//! ERC-1155 transfer decoding, and [`decode`], which joins one response
//! into a [`RowBatch`]. `pipeline::transform` keeps the orchestration
//! (call this, then every other module's `decode`, then assemble the
//! batch); the rows themselves are `core::models`.

use crate::{
    core::RowBatch,
    core::{
        convert::{
            address_to_alloy, data_to_bytes, hash_to_b256, nonce_to_b64,
            quantity_to_u256, quantity_to_u32, quantity_to_u64, sat_u32,
        },
        events::{
            ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
            ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
            TRANSFER_EVENT_SIGNATURE,
        },
        models::{
            block::DatabaseBlock,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer,
            log::DatabaseLog,
            transaction::{
                DatabaseTransaction, STATUS_FAILURE, STATUS_SUCCESS,
            },
            withdrawal::DatabaseWithdrawal,
        },
    },
    db::{format::method_selector, ranges::BlockRange},
    pipeline::transform::{ResponseRows, Transformed},
    tokens::TokenStandard,
};
use alloy::primitives::{Address, B256, U256};
use anyhow::{bail, Context, Result};
use hypersync_client::{
    format::{TransactionStatus, Withdrawal},
    simple_types::{Block, Log, Transaction},
};
use std::collections::HashMap;

/// Per block values joined into the rows of the block's children.
struct BlockContext {
    timestamp: u32,
    base_fee_per_gas: Option<U256>,
}

/// Converts a response covering exactly the blocks of `covered` into the
/// core rows, plus the token contracts its transfers named.
///
/// The response must contain EVERY block of `covered` (the query asks for
/// all blocks). Anything else is an error: storing a partial range would
/// leave silent holes, and rows can not be timestamped without their block.
pub fn decode(
    chain: u64,
    data: &ResponseRows,
    covered: BlockRange,
) -> Result<Transformed> {
    let mut rows = RowBatch::default();

    // Transactions per block: HyperSync blocks carry no transaction list.
    let mut transaction_counts: HashMap<u64, u64> = HashMap::new();
    for transaction in data.transactions.iter().flatten() {
        if let Some(number) = transaction.block_number {
            *transaction_counts.entry(u64::from(number)).or_default() += 1;
        }
    }

    let mut contexts: HashMap<u64, BlockContext> = HashMap::new();

    for block in data.blocks.iter().flatten() {
        let number = block.number.context("block without a number")?;

        if !(covered.from..covered.to).contains(&number) {
            bail!("block {number} is outside the covered range {covered}");
        }

        let row = DatabaseBlock::from_hypersync(
            block,
            chain,
            transaction_counts.get(&number).copied().unwrap_or_default(),
        )?;

        let context = BlockContext {
            timestamp: row.timestamp,
            base_fee_per_gas: row.base_fee_per_gas,
        };

        // A duplicated block inside one response is dropped.
        if contexts.insert(number, context).is_some() {
            continue;
        }

        for withdrawal in block.withdrawals.iter().flatten() {
            rows.withdrawals.push(DatabaseWithdrawal::from_hypersync(
                withdrawal,
                chain,
                row.number,
                row.timestamp,
            ));
        }

        rows.blocks.push(row);
    }

    if contexts.len() as u64 != covered.len() {
        bail!(
            "response for {covered} contains {} of {} blocks",
            contexts.len(),
            covered.len()
        );
    }

    rows.blocks.sort_unstable_by_key(|block| block.number);

    let context_of = |number: u64, what: &str| {
        contexts.get(&number).with_context(|| {
            format!("{what} references block {number} missing in response")
        })
    };

    // Deployed contracts are not rows: `contracts` is a view over these
    // transactions (docs/design.md, section 9).
    for transaction in data.transactions.iter().flatten() {
        let number = transaction
            .block_number
            .map(u64::from)
            .context("transaction without a block number")?;

        let context = context_of(number, "transaction")?;

        rows.transactions.push(DatabaseTransaction::from_hypersync(
            transaction,
            chain,
            context.timestamp,
            context.base_fee_per_gas,
        )?);
    }

    let mut tokens_seen: HashMap<Address, TokenStandard> = HashMap::new();

    for log in data.logs.iter().flatten() {
        let number = log
            .block_number
            .map(u64::from)
            .context("log without a block number")?;

        let context = context_of(number, "log")?;

        let row =
            DatabaseLog::from_hypersync(log, chain, context.timestamp)?;

        decode_transfers(&row, &mut rows, &mut tokens_seen);

        rows.logs.push(row);
    }

    Ok(Transformed { rows, tokens_seen })
}

/// ERC20 / ERC721 / ERC1155 transfers are decoded from the generic logs.
pub fn decode_transfers(
    log: &DatabaseLog,
    rows: &mut RowBatch,
    tokens_seen: &mut HashMap<Address, TokenStandard>,
) {
    let Some(topic0) = log.topic0 else { return };

    if topic0 == TRANSFER_EVENT_SIGNATURE {
        // Same signature: ERC721 indexes the token id (4 topics), ERC20
        // keeps the amount in the data (3 topics).
        if log.topic3.is_some() {
            if let Some(row) = DatabaseERC721Transfer::from_log(log) {
                tokens_seen
                    .entry(row.token_address)
                    .or_insert(TokenStandard::Erc721);
                rows.erc721_transfers.push(row);
            }
        } else if let Some(row) = DatabaseERC20Transfer::from_log(log) {
            tokens_seen
                .entry(row.token_address)
                .or_insert(TokenStandard::Erc20);
            rows.erc20_transfers.push(row);
        }
    } else if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
        || topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
    {
        if let Some(row) = DatabaseERC1155Transfer::from_log(log) {
            tokens_seen
                .entry(row.token_address)
                .or_insert(TokenStandard::Erc1155);
            rows.erc1155_transfers.push(row);
        }
    }
}

// ---------------------------------------------------------------- blocks

impl DatabaseBlock {
    /// `transactions` is the number of transactions of the block (HyperSync
    /// blocks do not carry the transaction list, the caller counts them).
    ///
    /// `number` and `hash` identify the row and are the commit marker, so
    /// they are the only fields that are an error when missing. Everything
    /// else falls back to a zero value / NULL.
    pub fn from_hypersync(
        block: &Block,
        chain: u64,
        transactions: u64,
    ) -> Result<Self> {
        let number =
            block.number.context("block without a number in response")?;

        let hash =
            block.hash.as_ref().map(hash_to_b256).with_context(|| {
                format!("block {number} without a hash")
            })?;

        let opt_hash = |h: &Option<_>| {
            h.as_ref().map(hash_to_b256).unwrap_or_default()
        };

        let opt_u64 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u64).unwrap_or_default()
        };

        let opt_u256 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u256).unwrap_or_default()
        };

        Ok(Self {
            chain,
            number,
            hash,
            parent_hash: opt_hash(&block.parent_hash),
            timestamp: block
                .timestamp
                .as_ref()
                .map(quantity_to_u32)
                .unwrap_or_default(),
            miner: block
                .miner
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            base_fee_per_gas: block
                .base_fee_per_gas
                .as_ref()
                .map(quantity_to_u256),
            difficulty: opt_u256(&block.difficulty),
            total_difficulty: opt_u256(&block.total_difficulty),
            extra_data: block
                .extra_data
                .as_ref()
                .map(data_to_bytes)
                .unwrap_or_default(),
            gas_limit: opt_u64(&block.gas_limit),
            gas_used: opt_u64(&block.gas_used),
            mix_hash: opt_hash(&block.mix_hash),
            nonce: block
                .nonce
                .as_ref()
                .map(nonce_to_b64)
                .unwrap_or_default(),
            receipts_root: opt_hash(&block.receipts_root),
            sha3_uncles: opt_hash(&block.sha3_uncles),
            size: opt_u64(&block.size),
            state_root: opt_hash(&block.state_root),
            transactions: sat_u32(transactions),
            transactions_root: opt_hash(&block.transactions_root),
            uncles: block
                .uncles
                .as_ref()
                .map(|uncles| uncles.iter().map(hash_to_b256).collect())
                .unwrap_or_default(),
            withdrawals_root: opt_hash(&block.withdrawals_root),
            epoch: 0,
            _version: 0,
        })
    }
}

// ----------------------------------------------------------- withdrawals

impl DatabaseWithdrawal {
    pub fn from_hypersync(
        withdrawal: &Withdrawal,
        chain: u64,
        block_number: u64,
        timestamp: u32,
    ) -> Self {
        Self {
            address: withdrawal
                .address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            amount: withdrawal
                .amount
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
            block_number,
            chain,
            timestamp,
            validator_index: withdrawal
                .validator_index
                .as_ref()
                .map(quantity_to_u64)
                .unwrap_or_default(),
            withdrawal_index: withdrawal
                .index
                .as_ref()
                .map(quantity_to_u64)
                .unwrap_or_default(),
            epoch: 0,
            _version: 0,
        }
    }
}

// ---------------------------------------------------------- transactions

/// EIP-2718 transaction type -> the label stored in `transaction_type`.
/// Unknown types keep their numeric value instead of being mislabeled.
pub fn transaction_type_label(transaction_type: Option<u8>) -> String {
    match transaction_type {
        None | Some(0) => "legacy".to_string(),
        Some(1) => "access_list".to_string(),
        Some(2) => "eip_1559".to_string(),
        Some(3) => "blob".to_string(),
        Some(4) => "set_code".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Receipt status -> the label stored in `status`. Pre-Byzantium receipts
/// have no status, those stay NULL.
pub fn transaction_status_label(
    status: Option<TransactionStatus>,
) -> Option<String> {
    status.map(|status| match status {
        TransactionStatus::Success => STATUS_SUCCESS.to_string(),
        TransactionStatus::Failure => STATUS_FAILURE.to_string(),
    })
}

impl DatabaseTransaction {
    pub fn from_hypersync(
        transaction: &Transaction,
        chain: u64,
        timestamp: u32,
        base_fee_per_gas: Option<U256>,
    ) -> Result<Self> {
        let hash = transaction
            .hash
            .as_ref()
            .map(hash_to_b256)
            .context("transaction without a hash in response")?;

        let block_number =
            transaction.block_number.map(u64::from).with_context(
                || format!("transaction {hash:?} without a block number"),
            )?;

        let access_list: Vec<(Address, Vec<B256>)> = transaction
            .access_list
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item.address
                                .as_ref()
                                .map(address_to_alloy)
                                .unwrap_or_default(),
                            item.storage_keys
                                .as_ref()
                                .map(|keys| {
                                    keys.iter().map(hash_to_b256).collect()
                                })
                                .unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        let gas_price =
            transaction.gas_price.as_ref().map(quantity_to_u256);

        let max_priority_fee_per_gas = transaction
            .max_priority_fee_per_gas
            .as_ref()
            .map(quantity_to_u256);

        // Same fallback chain the RPC based indexer used when the receipt
        // did not report an effective gas price.
        let effective_gas_price = transaction
            .effective_gas_price
            .as_ref()
            .map(quantity_to_u256)
            .filter(|price| !price.is_zero())
            .or(gas_price)
            .or_else(|| {
                base_fee_per_gas.map(|base_fee| {
                    base_fee.saturating_add(
                        max_priority_fee_per_gas.unwrap_or_default(),
                    )
                })
            })
            .unwrap_or_default();

        let input = transaction
            .input
            .as_ref()
            .map(data_to_bytes)
            .unwrap_or_default();

        let opt_u64 = |q: &Option<_>| {
            q.as_ref().map(quantity_to_u64).unwrap_or_default()
        };

        Ok(Self {
            chain,
            block_number,
            transaction_index: sat_u32(
                transaction
                    .transaction_index
                    .map(u64::from)
                    .unwrap_or_default(),
            ),
            hash,
            block_hash: transaction
                .block_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            timestamp,
            from: transaction
                .from
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            // Contract creations have no recipient: NULL, the zero address
            // is a real (burn) recipient.
            to: transaction.to.as_ref().map(address_to_alloy),
            contract_created: transaction
                .contract_address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            value: transaction
                .value
                .as_ref()
                .map(quantity_to_u256)
                .unwrap_or_default(),
            method: method_selector(&input),
            input,
            nonce: opt_u64(&transaction.nonce),
            transaction_type: transaction_type_label(
                transaction.type_.map(u8::from),
            ),
            status: transaction_status_label(transaction.status),
            gas: opt_u64(&transaction.gas),
            gas_used: opt_u64(&transaction.gas_used),
            cumulative_gas_used: opt_u64(&transaction.cumulative_gas_used),
            gas_price,
            effective_gas_price,
            max_fee_per_gas: transaction
                .max_fee_per_gas
                .as_ref()
                .map(quantity_to_u256),
            max_priority_fee_per_gas,
            base_fee_per_gas,
            access_list,
            epoch: 0,
            _version: 0,
        })
    }
}

// ------------------------------------------------------------------ logs

/// Topics are positional: they end at the first missing one.
pub fn topic_count(topics: [&Option<B256>; 4]) -> u8 {
    topics.iter().take_while(|topic| topic.is_some()).count() as u8
}

impl DatabaseLog {
    pub fn from_hypersync(
        log: &Log,
        chain: u64,
        timestamp: u32,
    ) -> Result<Self> {
        let block_number = log
            .block_number
            .map(u64::from)
            .context("log without a block number in response")?;

        let log_index =
            log.log_index.map(u64::from).with_context(|| {
                format!("log without a log index in block {block_number}")
            })?;

        let topic = |index: usize| {
            log.topics
                .get(index)
                .and_then(|topic| topic.as_ref())
                .map(hash_to_b256)
        };

        // A topic after a missing one can not exist on chain; dropping it
        // keeps `topic_count` and the columns consistent.
        let topic0 = topic(0);
        let topic1 = topic0.and(topic(1));
        let topic2 = topic1.and(topic(2));
        let topic3 = topic2.and(topic(3));

        Ok(Self {
            chain,
            block_number,
            log_index: sat_u32(log_index),
            transaction_index: sat_u32(
                log.transaction_index.map(u64::from).unwrap_or_default(),
            ),
            transaction_hash: log
                .transaction_hash
                .as_ref()
                .map(hash_to_b256)
                .unwrap_or_default(),
            timestamp,
            address: log
                .address
                .as_ref()
                .map(address_to_alloy)
                .unwrap_or_default(),
            topic_count: topic_count([&topic0, &topic1, &topic2, &topic3]),
            topic0,
            topic1,
            topic2,
            topic3,
            data: log.data.as_ref().map(data_to_bytes).unwrap_or_default(),
            epoch: 0,
            _version: 0,
        })
    }
}

// -------------------------------------------------------- ERC20 / ERC721

impl DatabaseERC20Transfer {
    /// `Transfer(address indexed from, address indexed to, uint256 value)`:
    /// exactly three topics and the amount as the first data word.
    ///
    /// Log data is attacker controlled. Only the first 32 bytes are read
    /// (extra bytes are ignored) and a log with less than one full word is
    /// not a decodable ERC20 transfer, so it is skipped (`None`).
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        if log.topic0? != TRANSFER_EVENT_SIGNATURE {
            return None;
        }

        let topic1 = log.topic1?;
        let topic2 = log.topic2?;

        if log.topic3.is_some() {
            return None;
        }

        let amount = U256::from_be_slice(log.data.get(..32)?);

        Some(Self {
            chain: log.chain,
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_index: log.transaction_index,
            transaction_hash: log.transaction_hash,
            timestamp: log.timestamp,
            token_address: log.address,
            from: Address::from_word(topic1),
            to: Address::from_word(topic2),
            amount,
            epoch: 0,
            _version: 0,
        })
    }
}

impl DatabaseERC721Transfer {
    /// `Transfer(address indexed from, address indexed to, uint256 indexed
    /// tokenId)`: four topics, no data needed.
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        if log.topic0? != TRANSFER_EVENT_SIGNATURE {
            return None;
        }

        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        Some(Self {
            chain: log.chain,
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_index: log.transaction_index,
            transaction_hash: log.transaction_hash,
            timestamp: log.timestamp,
            token_address: log.address,
            from: Address::from_word(topic1),
            to: Address::from_word(topic2),
            id: U256::from_be_bytes(topic3.0),
            epoch: 0,
            _version: 0,
        })
    }
}

// --------------------------------------------------------------- ERC1155

const WORD: usize = 32;

/// Reads the ABI word starting at `offset`, `None` when out of bounds.
fn read_word(data: &[u8], offset: usize) -> Option<U256> {
    let end = offset.checked_add(WORD)?;
    data.get(offset..end).map(U256::from_be_slice)
}

/// Reads an ABI word that must be usable as an offset / length.
fn read_usize(data: &[u8], offset: usize) -> Option<usize> {
    usize::try_from(read_word(data, offset)?).ok()
}

/// Decodes a dynamic `uint256[]` whose head lives at `head_offset`.
fn read_u256_array(data: &[u8], head_offset: usize) -> Option<Vec<U256>> {
    let array_offset = read_usize(data, head_offset)?;
    let length = read_usize(data, array_offset)?;

    let first = array_offset.checked_add(WORD)?;
    let byte_length = length.checked_mul(WORD)?;
    let end = first.checked_add(byte_length)?;

    // Bounds check BEFORE allocating: `length` is attacker controlled.
    let items = data.get(first..end)?;

    let (words, _) = items.as_chunks::<WORD>();

    Some(words.iter().map(|word| U256::from_be_bytes(*word)).collect())
}

/// `TransferSingle` data: `(uint256 id, uint256 value)`.
fn decode_single(data: &[u8]) -> Option<(Vec<U256>, Vec<U256>)> {
    Some((vec![read_word(data, 0)?], vec![read_word(data, WORD)?]))
}

/// `TransferBatch` data: `(uint256[] ids, uint256[] values)`.
fn decode_batch(data: &[u8]) -> Option<(Vec<U256>, Vec<U256>)> {
    let ids = read_u256_array(data, 0)?;
    let amounts = read_u256_array(data, WORD)?;

    // EIP-1155 requires both arrays to have the same length.
    (ids.len() == amounts.len()).then_some((ids, amounts))
}

impl DatabaseERC1155Transfer {
    /// Decodes `TransferSingle` and `TransferBatch`. Malformed data returns
    /// `None`; nothing in here can panic on hostile input.
    pub fn from_log(log: &DatabaseLog) -> Option<Self> {
        let topic0 = log.topic0?;
        let topic1 = log.topic1?;
        let topic2 = log.topic2?;
        let topic3 = log.topic3?;

        let (ids, amounts) =
            if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE {
                decode_single(&log.data)?
            } else if topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE {
                decode_batch(&log.data)?
            } else {
                return None;
            };

        Some(Self {
            chain: log.chain,
            block_number: log.block_number,
            log_index: log.log_index,
            transaction_index: log.transaction_index,
            transaction_hash: log.transaction_hash,
            timestamp: log.timestamp,
            token_address: log.address,
            operator: Address::from_word(topic1),
            from: Address::from_word(topic2),
            to: Address::from_word(topic3),
            ids,
            amounts,
            epoch: 0,
            _version: 0,
        })
    }
}

#[cfg(test)]
mod block_tests {
    use super::*;
    use alloy::primitives::{B64, U256};
    use hypersync_client::format::{Hash, Quantity};

    fn minimal() -> Block {
        Block {
            number: Some(19_000_000),
            hash: Some(Hash::from([1u8; 32])),
            ..Default::default()
        }
    }

    #[test]
    fn sparse_block_uses_defaults() {
        let row = DatabaseBlock::from_hypersync(&minimal(), 1, 3).unwrap();

        assert_eq!(row.number, 19_000_000);
        assert_eq!(row.transactions, 3);
        assert_eq!(row.base_fee_per_gas, None);
        assert_eq!(row.total_difficulty, U256::ZERO);
        assert_eq!(row.difficulty, U256::ZERO);
        assert_eq!(row.nonce, B64::ZERO);
        assert_eq!(row.mix_hash, B256::ZERO);
        assert_eq!(row.withdrawals_root, B256::ZERO);
        assert!(row.uncles.is_empty());
        assert_eq!(row._version, 0);
    }

    #[test]
    fn number_and_hash_are_required() {
        assert!(DatabaseBlock::from_hypersync(&Block::default(), 1, 0)
            .is_err());

        let mut block = minimal();
        block.hash = None;
        assert!(DatabaseBlock::from_hypersync(&block, 1, 0).is_err());
    }

    #[test]
    fn wide_values_are_kept() {
        let mut block = minimal();
        block.number = Some(u64::MAX);
        block.gas_limit = Some(Quantity::from(u64::MAX));
        block.gas_used = Some(Quantity::from(u32::MAX as u64 + 1));
        block.size = Some(Quantity::from(u32::MAX as u64 + 2));
        // 9 bytes: more than the UInt64 the old schema saturated at.
        block.base_fee_per_gas = Some(Quantity::from(vec![1u8; 9]));

        let row =
            DatabaseBlock::from_hypersync(&block, 1, 100_000).unwrap();

        assert_eq!(row.number, u64::MAX);
        assert_eq!(row.gas_limit, u64::MAX);
        assert_eq!(row.gas_used, u32::MAX as u64 + 1);
        assert_eq!(row.size, u32::MAX as u64 + 2);
        assert_eq!(
            row.base_fee_per_gas,
            Some(U256::from_be_slice(&[1u8; 9]))
        );
        assert_eq!(row.transactions, 100_000);
    }

    #[test]
    fn values_wider_than_the_column_saturate_instead_of_panicking() {
        let mut block = minimal();
        block.size = Some(Quantity::from(vec![1u8; 20]));
        block.timestamp = Some(Quantity::from(u64::MAX));

        let row = DatabaseBlock::from_hypersync(&block, 1, 0).unwrap();

        assert_eq!(row.size, u64::MAX);
        assert_eq!(row.timestamp, u32::MAX);
    }
}

#[cfg(test)]
mod withdrawal_tests {
    use super::*;
    use hypersync_client::format::{Address as HsAddress, Quantity};

    #[test]
    fn validator_and_withdrawal_index_are_distinct_fields() {
        let withdrawal = Withdrawal {
            index: Some(Quantity::from(1_000u64)),
            validator_index: Some(Quantity::from(42u64)),
            address: Some(HsAddress::from([7u8; 20])),
            amount: Some(Quantity::from(32_000_000_000u64)),
        };

        let row =
            DatabaseWithdrawal::from_hypersync(&withdrawal, 1, 17, 99);

        assert_eq!(row.withdrawal_index, 1_000);
        assert_eq!(row.validator_index, 42);
        assert_eq!(row.amount, U256::from(32_000_000_000u64));
        assert_eq!(row.address, Address::repeat_byte(7));
        assert_eq!(row.block_number, 17);
        assert_eq!(row.timestamp, 99);
    }

    #[test]
    fn empty_withdrawal_defaults() {
        let row = DatabaseWithdrawal::from_hypersync(
            &Withdrawal::default(),
            1,
            1,
            1,
        );
        assert_eq!(row.validator_index, 0);
        assert_eq!(row.amount, U256::ZERO);
    }
}

#[cfg(test)]
mod transaction_tests {
    use super::*;
    use alloy::primitives::Selector;
    use hypersync_client::format::{
        Address as HsAddress, Data, Hash, Quantity, TransactionType, UInt,
    };

    #[test]
    fn transaction_type_labels() {
        assert_eq!(transaction_type_label(None), "legacy");
        assert_eq!(transaction_type_label(Some(0)), "legacy");
        assert_eq!(transaction_type_label(Some(1)), "access_list");
        assert_eq!(transaction_type_label(Some(2)), "eip_1559");
        assert_eq!(transaction_type_label(Some(3)), "blob");
        assert_eq!(transaction_type_label(Some(4)), "set_code");
        // Unknown (e.g. OP deposit 0x7e) keeps the numeric value.
        assert_eq!(transaction_type_label(Some(126)), "126");
    }

    #[test]
    fn transaction_status_labels() {
        assert_eq!(
            transaction_status_label(Some(TransactionStatus::Success)),
            Some("success".to_string())
        );
        assert_eq!(
            transaction_status_label(Some(TransactionStatus::Failure)),
            Some("failure".to_string())
        );
        assert_eq!(transaction_status_label(None), None);
    }

    fn minimal_transaction() -> Transaction {
        Transaction {
            hash: Some(Hash::from([1u8; 32])),
            block_number: Some(UInt::from(10u64)),
            ..Default::default()
        }
    }

    #[test]
    fn sparse_transaction_uses_defaults_and_never_panics() {
        let row = DatabaseTransaction::from_hypersync(
            &minimal_transaction(),
            1,
            1_700_000_000,
            None,
        )
        .unwrap();

        assert_eq!(row.block_number, 10);
        assert_eq!(row.to, None);
        assert_eq!(row.from, Address::ZERO);
        assert_eq!(row.contract_created, Address::ZERO);
        assert_eq!(row.created_contract(), None);
        assert_eq!(row.status, None);
        assert_eq!(row.transaction_type, "legacy");
        assert_eq!(row.method, Selector::ZERO);
        assert_eq!(row.effective_gas_price, U256::ZERO);
        assert_eq!(row.gas_price, None);
        assert_eq!(row.gas_used, 0);
        assert!(row.access_list.is_empty());
        assert_eq!(row._version, 0);
    }

    #[test]
    fn missing_identity_fields_are_errors_not_panics() {
        let transaction = Transaction::default();
        assert!(DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            None
        )
        .is_err());
    }

    #[test]
    fn wide_values_are_kept() {
        let mut transaction = minimal_transaction();
        transaction.gas = Some(Quantity::from(u64::MAX));
        transaction.nonce = Some(Quantity::from(u32::MAX as u64 + 5));
        transaction.transaction_index = Some(UInt::from(70_000u64));
        transaction.type_ = Some(TransactionType::from(4u8));
        transaction.status = Some(TransactionStatus::Success);
        transaction.input =
            Some(Data::from(vec![0xa9, 0x05, 0x9c, 0xbb, 0x01]));
        transaction.to = Some(HsAddress::from([0u8; 20]));
        transaction.contract_address = Some(HsAddress::from([7u8; 20]));

        let row =
            DatabaseTransaction::from_hypersync(&transaction, 1, 0, None)
                .unwrap();

        assert_eq!(row.gas, u64::MAX);
        assert_eq!(row.nonce, u32::MAX as u64 + 5);
        assert_eq!(row.transaction_index, 70_000);
        assert_eq!(row.transaction_type, "set_code");
        assert_eq!(row.status.as_deref(), Some(STATUS_SUCCESS));
        assert_eq!(row.method, Selector::from([0xa9, 0x05, 0x9c, 0xbb]));
        // The zero address is a recipient, not "no recipient".
        assert_eq!(row.to, Some(Address::ZERO));
        assert_eq!(row.created_contract(), Some(Address::repeat_byte(7)));
    }

    #[test]
    fn effective_gas_price_fallbacks() {
        // 1. reported by the receipt
        let mut transaction = minimal_transaction();
        transaction.effective_gas_price = Some(Quantity::from(7u64));
        transaction.gas_price = Some(Quantity::from(9u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(7u64));

        // 2. zero / missing -> gas price
        transaction.effective_gas_price = Some(Quantity::from(0u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(9u64));

        // 3. no gas price -> base fee + priority fee
        transaction.gas_price = None;
        transaction.max_priority_fee_per_gas = Some(Quantity::from(2u64));
        let row = DatabaseTransaction::from_hypersync(
            &transaction,
            1,
            0,
            Some(U256::from(100u64)),
        )
        .unwrap();
        assert_eq!(row.effective_gas_price, U256::from(102u64));
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;
    use alloy::primitives::Bytes;
    use hypersync_client::format::{
        Address as HsAddress, Data, Hash, LogArgument, UInt,
    };

    fn hypersync_log() -> Log {
        let mut log = Log {
            block_number: Some(UInt::from(55u64)),
            log_index: Some(UInt::from(70_000u64)),
            transaction_index: Some(UInt::from(4u64)),
            transaction_hash: Some(Hash::from([3u8; 32])),
            address: Some(HsAddress::from([5u8; 20])),
            data: Some(Data::from(vec![1u8, 2, 3])),
            ..Default::default()
        };
        log.topics.push(Some(LogArgument::from([9u8; 32])));
        log.topics.push(None);
        log.topics.push(None);
        log.topics.push(None);
        log
    }

    #[test]
    fn converts_without_narrowing() {
        let row =
            DatabaseLog::from_hypersync(&hypersync_log(), 1, 42).unwrap();

        assert_eq!(row.block_number, 55);
        // Did not fit the old UInt16 column.
        assert_eq!(row.log_index, 70_000);
        assert_eq!(row.timestamp, 42);
        assert_eq!(row.topic_count, 1);
        assert_eq!(row.topic0, Some(B256::repeat_byte(9)));
        assert_eq!(row.topic1, None);
        assert_eq!(row.transaction_index, 4);
        assert_eq!(row.data, Bytes::from(vec![1u8, 2, 3]));
        assert_eq!(row._version, 0);
    }

    #[test]
    fn a_zero_topic_is_still_a_topic() {
        let mut log = hypersync_log();
        log.topics[1] = Some(LogArgument::from([1u8; 32]));
        log.topics[2] = Some(LogArgument::from([2u8; 32]));
        log.topics[3] = Some(LogArgument::from([0u8; 32]));

        let row = DatabaseLog::from_hypersync(&log, 1, 0).unwrap();

        assert_eq!(row.topic_count, 4);
        assert_eq!(row.topic3, Some(B256::ZERO));
    }

    #[test]
    fn topics_end_at_the_first_missing_one() {
        let mut log = hypersync_log();
        log.topics[2] = Some(LogArgument::from([2u8; 32]));

        let row = DatabaseLog::from_hypersync(&log, 1, 0).unwrap();

        assert_eq!(row.topic_count, 1);
        assert_eq!(row.topic2, None);
    }

    #[test]
    fn log_without_topics_or_data_is_fine() {
        let log = Log {
            block_number: Some(UInt::from(1u64)),
            log_index: Some(UInt::from(0u64)),
            ..Default::default()
        };

        let row = DatabaseLog::from_hypersync(&log, 1, 0).unwrap();

        assert_eq!(row.topic_count, 0);
        assert_eq!(row.topic0, None);
        assert!(row.data.is_empty());
        assert_eq!(row.address, Address::ZERO);
    }

    #[test]
    fn missing_identity_is_an_error() {
        assert!(
            DatabaseLog::from_hypersync(&Log::default(), 1, 0).is_err()
        );
    }
}

#[cfg(test)]
mod erc20_tests {
    use super::*;
    use crate::core::models::log::test_support::{
        address_topic, log_with, word,
    };

    fn topics() -> Vec<B256> {
        vec![TRANSFER_EVENT_SIGNATURE, address_topic(1), address_topic(2)]
    }

    #[test]
    fn decodes_a_standard_transfer() {
        let log = log_with(&topics(), word(1_000));
        let transfer = DatabaseERC20Transfer::from_log(&log).unwrap();

        assert_eq!(transfer.from, Address::repeat_byte(1));
        assert_eq!(transfer.to, Address::repeat_byte(2));
        assert_eq!(transfer.amount, U256::from(1_000u64));
        assert_eq!(transfer.token_address, log.address);
        assert_eq!(transfer.log_index, 7);
        assert_eq!(transfer.transaction_index, 3);
    }

    #[test]
    fn data_longer_than_one_word_does_not_panic() {
        // A hostile token can emit Transfer with arbitrary extra data.
        let mut data = word(5);
        data.extend_from_slice(&[0xff; 100]);

        let transfer =
            DatabaseERC20Transfer::from_log(&log_with(&topics(), data))
                .unwrap();

        // Only the first word is the amount.
        assert_eq!(transfer.amount, U256::from(5u64));
    }

    #[test]
    fn max_amount_is_kept() {
        let transfer = DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![0xff; 32],
        ))
        .unwrap();
        assert_eq!(transfer.amount, U256::MAX);
    }

    #[test]
    fn short_or_empty_data_is_skipped() {
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![]
        ))
        .is_none());
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics(),
            vec![1; 31]
        ))
        .is_none());
    }

    #[test]
    fn other_shapes_are_not_erc20() {
        // ERC721: four topics.
        let mut four = topics();
        four.push(B256::repeat_byte(9));
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &four,
            word(1)
        ))
        .is_none());

        // Too few topics.
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &topics()[..2],
            word(1)
        ))
        .is_none());

        // Another event.
        let mut other = topics();
        other[0] = B256::repeat_byte(1);
        assert!(DatabaseERC20Transfer::from_log(&log_with(
            &other,
            word(1)
        ))
        .is_none());
    }
}

#[cfg(test)]
mod erc721_tests {
    use super::*;
    use crate::core::models::log::test_support::{
        address_topic, log_with,
    };

    #[test]
    fn decodes_token_id_from_the_fourth_topic() {
        let topics = [
            TRANSFER_EVENT_SIGNATURE,
            address_topic(1),
            address_topic(2),
            B256::from(U256::from(4242u64)),
        ];

        let transfer =
            DatabaseERC721Transfer::from_log(&log_with(&topics, vec![]))
                .unwrap();

        assert_eq!(transfer.from, Address::repeat_byte(1));
        assert_eq!(transfer.to, Address::repeat_byte(2));
        assert_eq!(transfer.id, U256::from(4242u64));
    }

    #[test]
    fn token_id_zero_is_a_transfer() {
        // The fourth topic is all zero bytes but PRESENT.
        let topics = [
            TRANSFER_EVENT_SIGNATURE,
            address_topic(1),
            address_topic(2),
            B256::ZERO,
        ];

        let transfer =
            DatabaseERC721Transfer::from_log(&log_with(&topics, vec![]))
                .unwrap();

        assert_eq!(transfer.id, U256::ZERO);
    }

    #[test]
    fn three_topics_is_not_erc721() {
        let topics =
            [TRANSFER_EVENT_SIGNATURE, address_topic(1), address_topic(2)];

        assert!(DatabaseERC721Transfer::from_log(&log_with(
            &topics,
            vec![]
        ))
        .is_none());
    }
}

#[cfg(test)]
mod erc1155_tests {
    use super::*;
    use crate::core::models::log::test_support::{
        address_topic, log_with, word,
    };

    fn topics(signature: B256) -> Vec<B256> {
        vec![
            signature,
            address_topic(1),
            address_topic(2),
            address_topic(3),
        ]
    }

    /// ABI encoding of `(uint256[] ids, uint256[] values)`.
    fn encode_batch(ids: &[u64], values: &[u64]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend(word(64));
        data.extend(word(64 + 32 + 32 * ids.len() as u64));
        data.extend(word(ids.len() as u64));
        ids.iter().for_each(|id| data.extend(word(*id)));
        data.extend(word(values.len() as u64));
        values.iter().for_each(|value| data.extend(word(*value)));
        data
    }

    fn u256s(values: &[u64]) -> Vec<U256> {
        values.iter().map(|v| U256::from(*v)).collect()
    }

    #[test]
    fn decodes_transfer_single() {
        let mut data = word(11);
        data.extend(word(500));

        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE),
            data,
        ))
        .unwrap();

        assert_eq!(transfer.operator, Address::repeat_byte(1));
        assert_eq!(transfer.from, Address::repeat_byte(2));
        assert_eq!(transfer.to, Address::repeat_byte(3));
        assert_eq!(transfer.ids, u256s(&[11]));
        assert_eq!(transfer.amounts, u256s(&[500]));
    }

    #[test]
    fn short_transfer_single_is_skipped() {
        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE),
            word(11),
        ))
        .is_none());
    }

    #[test]
    fn decodes_transfer_batch() {
        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE),
            encode_batch(&[1, 2, 3], &[10, 20, 30]),
        ))
        .unwrap();

        assert_eq!(transfer.ids, u256s(&[1, 2, 3]));
        assert_eq!(transfer.amounts, u256s(&[10, 20, 30]));
        assert_eq!(transfer.from, Address::repeat_byte(2));
    }

    #[test]
    fn decodes_an_empty_batch() {
        let transfer = DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE),
            encode_batch(&[], &[]),
        ))
        .unwrap();

        assert!(transfer.ids.is_empty());
        assert!(transfer.amounts.is_empty());
    }

    #[test]
    fn malformed_batches_return_none_and_never_panic() {
        let signature = topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE);
        let decode = |data: Vec<u8>| {
            DatabaseERC1155Transfer::from_log(&log_with(&signature, data))
        };

        // Empty / too short for the two offsets.
        assert!(decode(vec![]).is_none());
        assert!(decode(word(64)).is_none());

        // Offset pointing outside the data.
        let mut data = encode_batch(&[1], &[1]);
        data[..32].copy_from_slice(&word(10_000));
        assert!(decode(data).is_none());

        // Offset that does not even fit usize (2^256 - 1).
        let mut data = encode_batch(&[1], &[1]);
        data[..32].copy_from_slice(&[0xff; 32]);
        assert!(decode(data).is_none());

        // Length claiming far more items than the data holds: must not
        // allocate or overflow.
        let mut data = encode_batch(&[1], &[1]);
        data[64..96].copy_from_slice(&word(u64::MAX));
        assert!(decode(data).is_none());

        let mut data = encode_batch(&[1], &[1]);
        data[64..96].copy_from_slice(&[0xff; 32]);
        assert!(decode(data).is_none());

        // Truncated in the middle of the values.
        let mut data = encode_batch(&[1, 2], &[1, 2]);
        data.truncate(data.len() - 16);
        assert!(decode(data).is_none());

        // ids / values of different length.
        assert!(decode(encode_batch(&[1, 2], &[1])).is_none());
    }

    #[test]
    fn other_events_and_missing_topics_are_ignored() {
        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(B256::repeat_byte(1)),
            encode_batch(&[1], &[1]),
        ))
        .is_none());

        assert!(DatabaseERC1155Transfer::from_log(&log_with(
            &topics(ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE)[..3],
            encode_batch(&[1], &[1]),
        ))
        .is_none());
    }
}
