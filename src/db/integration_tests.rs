//! Tests against a real ClickHouse. Ignored by default:
//!
//! ```sh
//! clickhouse client --multiquery < migrations/create_tables.sql
//! TEST_DATABASE_URL=http://default@localhost:8123/indexer \
//!   cargo test -- --ignored
//! ```
//!
//! They use their own chain ids and never touch other data.

use super::{ranges::BlockRange, Database, RowBatch};
use crate::pipeline::transform::{transform, ResponseRows};
use hypersync_client::{
    format::{
        AccessList, Address as HsAddress, Data, Hash, LogArgument,
        Quantity, TransactionStatus, TransactionType, UInt, Withdrawal,
    },
    simple_types::{Block, Log, Trace, Transaction},
};

async fn database(chain: u64) -> Database {
    let url = std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for the ignored tests");

    let database = Database::new(&url, chain).await.unwrap();

    for table in [
        "blocks",
        "contracts",
        "logs",
        "traces",
        "transactions",
        "withdrawals",
        "erc20_transfers",
        "erc721_transfers",
        "erc1155_transfers",
        "tokens",
    ] {
        database
            .db
            .query(&format!(
                "ALTER TABLE {table} DELETE WHERE chain = {chain} \
                 SETTINGS mutations_sync = 2"
            ))
            .execute()
            .await
            .unwrap();
    }

    database
}

/// A response with one fully populated block per number.
fn response(numbers: &[u64]) -> ResponseRows {
    let mut data = ResponseRows::default();

    for &number in numbers {
        let id = number as u8;

        data.blocks.push(vec![Block {
            number: Some(number),
            hash: Some(Hash::from([id; 32])),
            parent_hash: Some(Hash::from([id.wrapping_sub(1); 32])),
            timestamp: Some(Quantity::from(1_700_000_000 + number)),
            gas_limit: Some(Quantity::from(u64::MAX)),
            gas_used: Some(Quantity::from(21_000u64)),
            size: Some(Quantity::from(1_000u64)),
            base_fee_per_gas: Some(Quantity::from(7u64)),
            difficulty: Some(Quantity::from(0u64)),
            total_difficulty: Some(Quantity::from(vec![1u8; 10])),
            extra_data: Some(Data::from(vec![1u8, 2, 3])),
            logs_bloom: Some(Data::from(vec![0u8; 256])),
            uncles: Some(vec![Hash::from([0xcc; 32])]),
            mix_hash: Some(Hash::from([0xdd; 32])),
            withdrawals_root: Some(Hash::from([0xee; 32])),
            withdrawals: Some(vec![Withdrawal {
                index: Some(Quantity::from(number * 10)),
                validator_index: Some(Quantity::from(number * 100)),
                address: Some(HsAddress::from([id; 20])),
                amount: Some(Quantity::from(5u64)),
            }]),
            ..Default::default()
        }]);

        data.transactions.push(vec![Transaction {
            block_number: Some(UInt::from(number)),
            block_hash: Some(Hash::from([id; 32])),
            transaction_index: Some(UInt::from(0u64)),
            hash: Some(Hash::from([id ^ 0xf0; 32])),
            from: Some(HsAddress::from([1u8; 20])),
            to: None,
            contract_address: Some(HsAddress::from([id; 20])),
            input: Some(Data::from(vec![0xa9, 0x05, 0x9c, 0xbb, 0x00])),
            value: Some(Quantity::from(vec![0xffu8; 32])),
            gas: Some(Quantity::from(100_000u64)),
            gas_price: Some(Quantity::from(9u64)),
            nonce: Some(Quantity::from(1u64)),
            type_: Some(TransactionType::from(2u8)),
            status: Some(TransactionStatus::Success),
            access_list: Some(vec![AccessList {
                address: Some(HsAddress::from([2u8; 20])),
                storage_keys: Some(vec![Hash::from([3u8; 32])]),
            }]),
            ..Default::default()
        }]);

        let mut log = Log {
            block_number: Some(UInt::from(number)),
            log_index: Some(UInt::from(0u64)),
            transaction_index: Some(UInt::from(0u64)),
            transaction_hash: Some(Hash::from([id ^ 0xf0; 32])),
            address: Some(HsAddress::from([0x20; 20])),
            data: Some(Data::from(vec![0u8; 32])),
            ..Default::default()
        };
        log.topics.push(Some(LogArgument::from(
            crate::utils::events::TRANSFER_EVENT_SIGNATURE.0,
        )));
        log.topics.push(Some(LogArgument::from([1u8; 32])));
        log.topics.push(Some(LogArgument::from([2u8; 32])));
        log.topics.push(None);
        data.logs.push(vec![log]);

        data.traces.push(vec![Trace {
            block_number: Some(number),
            block_hash: Some(Hash::from([id; 32])),
            type_: Some("call".to_string()),
            call_type: Some("call".to_string()),
            from: Some(HsAddress::from([1u8; 20])),
            to: Some(HsAddress::from([2u8; 20])),
            gas: Some(Quantity::from(u64::MAX)),
            value: Some(Quantity::from(1u64)),
            input: Some(Data::from(vec![1u8])),
            subtraces: Some(0),
            trace_address: Some(vec![0, 1]),
            transaction_hash: Some(Hash::from([id ^ 0xf0; 32])),
            transaction_position: Some(0),
            ..Default::default()
        }]);
    }

    data
}

fn rows(chain: u64, from: u64, to: u64) -> RowBatch {
    let numbers: Vec<u64> = (from..to).collect();
    transform(chain, &response(&numbers), BlockRange::new(from, to))
        .unwrap()
        .rows
}

async fn count(database: &Database, table: &str) -> u64 {
    database
        .db
        .query(&format!(
            "SELECT count() FROM {table} WHERE chain = {}",
            database.chain_id
        ))
        .fetch_one::<u64>()
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn stores_every_table_with_async_inserts() {
    let database = database(990_001).await;

    let mut batch = rows(990_001, 10, 13);
    batch.tokens.push(crate::db::models::token::DatabaseToken {
        address: Default::default(),
        name: "Token".into(),
        symbol: "TKN".into(),
        decimals: 18,
        r#type: "ERC20".into(),
        chain: 990_001,
    });

    database.store(&batch).await.unwrap();

    // wait_for_async_insert=1: rows are visible as soon as store returns.
    for (table, expected) in [
        ("blocks", 3),
        ("transactions", 3),
        ("logs", 3),
        ("traces", 3),
        ("withdrawals", 3),
        ("erc20_transfers", 3),
        ("contracts", 3),
        ("tokens", 1),
    ] {
        assert_eq!(count(&database, table).await, expected, "{table}");
    }

    // Serialization formats are part of the contract with existing data.
    let (hash, value, tx_type, method, to): (
        String,
        String,
        String,
        String,
        String,
    ) = database
        .db
        .query(
            "SELECT hash, value, transaction_type, method, to \
             FROM transactions WHERE chain = 990001 AND block_number = 10",
        )
        .fetch_one()
        .await
        .unwrap();

    assert_eq!(hash, format!("0x{}", "fa".repeat(32)));
    assert_eq!(value, "f".repeat(64)); // U256: bare hex
    assert_eq!(tx_type, "eip_1559");
    assert_eq!(method, "0xa9059cbb");
    assert_eq!(to, format!("0x{}", "00".repeat(20)));

    let (gas_limit, validator_index, withdrawal_index): (u32, u64, u64) =
        database
            .db
            .query(
                "SELECT b.gas_limit, w.validator_index, w.withdrawal_index \
                 FROM blocks b JOIN withdrawals w \
                 ON w.block_number = b.number AND w.chain = b.chain \
                 WHERE b.chain = 990001 AND b.number = 11",
            )
            .fetch_one()
            .await
            .unwrap();

    assert_eq!(gas_limit, u32::MAX); // saturated
    assert_eq!(validator_index, 1_100);
    assert_eq!(withdrawal_index, 110);

    assert_eq!(
        database.block_hash(12).await.unwrap(),
        Some(format!("0x{}", "0c".repeat(32)))
    );
    assert_eq!(database.block_hash(500).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn missing_ranges_are_computed_in_clickhouse() {
    let database = database(990_002).await;
    let whole = BlockRange::new(0, 100);

    // Nothing indexed.
    let missing = database.missing_ranges(whole).await.unwrap();
    assert_eq!(missing.ranges, vec![whole]);

    // 5..10, 20..30 and 40 indexed; 20..30 twice (duplicates before merge).
    database.store(&rows(990_002, 5, 10)).await.unwrap();
    database.store(&rows(990_002, 20, 30)).await.unwrap();
    database.store(&rows(990_002, 20, 30)).await.unwrap();
    database.store(&rows(990_002, 40, 41)).await.unwrap();

    let missing = database.missing_ranges(whole).await.unwrap();
    assert_eq!(
        missing.ranges,
        vec![
            BlockRange::new(0, 5),
            BlockRange::new(10, 20),
            BlockRange::new(30, 40),
            BlockRange::new(41, 100),
        ]
    );
    assert_eq!(missing.covered_until, 100);

    // A sub range that starts inside indexed data.
    let missing =
        database.missing_ranges(BlockRange::new(7, 25)).await.unwrap();
    assert_eq!(missing.ranges, vec![BlockRange::new(10, 20)]);

    // Dense range: only the tail.
    let missing =
        database.missing_ranges(BlockRange::new(20, 35)).await.unwrap();
    assert_eq!(missing.ranges, vec![BlockRange::new(30, 35)]);

    // Fill everything: nothing left.
    for (from, to) in [(0, 5), (10, 20), (30, 40), (41, 100)] {
        database.store(&rows(990_002, from, to)).await.unwrap();
    }
    let missing = database.missing_ranges(whole).await.unwrap();
    assert!(missing.ranges.is_empty());
}
