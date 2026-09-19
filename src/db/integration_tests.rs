//! Tests against a real ClickHouse. Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/indexer_test \
//!   cargo test -- --ignored
//! ```
//!
//! The database is created and the embedded migrations are applied by the
//! tests themselves, through the migrator. Every test uses its own chain
//! id and only ever touches rows of that chain. (The migrator refuses a
//! database whose applied migrations were edited since: drop the test
//! database after changing a migration file.)

use super::{
    block_number_column,
    derived::CORE_DERIVED,
    models::{
        block::DatabaseBlock, contract::DatabaseContract,
        erc1155_transfer::DatabaseERC1155Transfer,
        erc20_transfer::DatabaseERC20Transfer,
        erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
        token::DatabaseToken, trace::DatabaseTrace,
        transaction::DatabaseTransaction, withdrawal::DatabaseWithdrawal,
    },
    next_version,
    ranges::BlockRange,
    Database, RowBatch, BLOCK_SCOPED_TABLES,
};
use crate::{
    pipeline::transform::{transform, ResponseRows},
    utils::events::{
        ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE, TRANSFER_EVENT_SIGNATURE,
    },
};
use alloy::primitives::{Address, U256};
use clickhouse::Row;
use hypersync_client::{
    format::{
        AccessList, Address as HsAddress, Data, Hash, LogArgument,
        Quantity, TransactionStatus, TransactionType, UInt, Withdrawal,
    },
    simple_types::{Block, Log, Trace, Transaction},
};
use serde::Deserialize;
use tokio::sync::{Mutex, OnceCell};

/// Migrations are applied once per test process.
static MIGRATED: OnceCell<()> = OnceCell::const_new();

fn test_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for the ignored tests")
}

/// The real startup path: creates the database and applies every embedded
/// migration through the migrator.
async fn migrate() {
    super::migrate::run(&test_url()).await.unwrap();
}

/// Every table a test can leave rows in.
fn all_tables() -> Vec<&'static str> {
    BLOCK_SCOPED_TABLES
        .iter()
        .copied()
        .chain(["tokens"])
        .chain(CORE_DERIVED.iter().map(|table| table.name))
        .collect()
}

/// ClickHouse (seen on 25.12) can silently LOSE a delete when two clients
/// run mutations (`DELETE FROM` / `ALTER ... DELETE`) on the same table at
/// the same time, even synchronous ones with different predicates: both
/// return, every mutation reports `is_done`, one of them was never applied.
/// The tests run in parallel against shared tables, so every mutation they
/// issue goes through this lock. (Production has one writer per chain and
/// purges sequentially; see the report to the pipeline engineer.)
static MUTATIONS: Mutex<()> = Mutex::const_new(());

async fn database(chain: u64) -> Database {
    MIGRATED.get_or_init(migrate).await;

    let database = Database::new(&test_url(), chain).await.unwrap();

    // Leftovers of an earlier run. Every partition key starts with the
    // chain, so this is a metadata operation, not a mutation per table.
    for table in all_tables() {
        if table == "tokens" {
            execute(
                &database,
                &format!(
                    "ALTER TABLE tokens DELETE WHERE chain = {chain} \
                     SETTINGS mutations_sync = 2"
                ),
            )
            .await;
            continue;
        }

        let partitions = strings(
            &database,
            &format!(
                "SELECT DISTINCT partition_id FROM system.parts \
                 WHERE database = currentDatabase() AND table = '{table}' \
                   AND active AND (partition = '{chain}' \
                     OR startsWith(partition, '({chain},'))"
            ),
        )
        .await;

        for partition in partitions {
            execute(
                &database,
                &format!(
                    "ALTER TABLE {table} DROP PARTITION ID '{partition}'"
                ),
            )
            .await;
        }
    }

    database
}

async fn execute(database: &Database, sql: &str) {
    // DROP PARTITION too: it cancels a mutation that is rewriting a part
    // of that partition, and the cancellation fails the (table wide)
    // DELETE of whoever is waiting for it.
    let exclusive =
        sql.starts_with("DELETE FROM") || sql.starts_with("ALTER TABLE");

    let _serialized =
        if exclusive { Some(MUTATIONS.lock().await) } else { None };

    database
        .db
        .query(sql)
        .execute()
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
}

const BASE_TIMESTAMP: u64 = 1_700_000_000; // 2023-11-14T22:13:20Z
const DAY_1: u32 = 1_699_920_000; // 2023-11-14T00:00:00Z
const DAY_2: u32 = DAY_1 + 86_400;

/// Blocks 0-6 fall on `DAY_1`, 7 and later on `DAY_2`.
fn timestamp_of(number: u64) -> u64 {
    BASE_TIMESTAMP + number * 1_000
}

/// More than 128 bits, different per block.
fn big_value(number: u64) -> U256 {
    (U256::from(1u8) << 200) + U256::from(number)
}

fn big_amount(number: u64) -> U256 {
    (U256::from(1u8) << 130) + U256::from(number)
}

fn quantity(value: U256) -> Quantity {
    Quantity::from(value.to_be_bytes_trimmed_vec())
}

const SENDER: [u8; 20] = [0x01; 20];
const RECIPIENT: [u8; 20] = [0x02; 20];
const FACTORY: [u8; 20] = [0xfa; 20];
const ERC20: [u8; 20] = [0x20; 20];
const ERC721: [u8; 20] = [0x21; 20];
const ERC1155: [u8; 20] = [0x22; 20];

fn deployment_hash(number: u64) -> [u8; 32] {
    [number as u8 ^ 0xf0; 32]
}

fn call_hash(number: u64) -> [u8; 32] {
    [number as u8 ^ 0x70; 32]
}

/// A response with one fully populated block per number (< 128):
/// 2 transactions (a deployment and a failed call), 5 logs (ERC20, ERC721
/// of token id 0, ERC1155 batch, anonymous, ERC721 of a huge id), 3 traces (call, create,
/// block reward) and a withdrawal. `salt` changes the block hash and the
/// gas used, to tell a re-inserted block from the original.
fn response(numbers: &[u64], salt: u8) -> ResponseRows {
    let mut data = ResponseRows::default();

    for &number in numbers {
        assert!(number < 128);
        let id = number as u8;
        let block_hash = [id ^ salt; 32];

        data.blocks.push(vec![Block {
            number: Some(number),
            hash: Some(Hash::from(block_hash)),
            parent_hash: Some(Hash::from([id.wrapping_sub(1); 32])),
            timestamp: Some(Quantity::from(timestamp_of(number))),
            miner: Some(HsAddress::from([0x99; 20])),
            // Did not fit the old UInt32 column.
            gas_limit: Some(Quantity::from(u64::MAX)),
            gas_used: Some(Quantity::from(42_000 + u64::from(salt))),
            size: Some(Quantity::from(1_000u64)),
            base_fee_per_gas: Some(Quantity::from(7u64)),
            difficulty: Some(Quantity::from(0u64)),
            total_difficulty: Some(quantity(
                big_value(number) + U256::from(1u8),
            )),
            extra_data: Some(Data::from(vec![0x00, 0xff, 0x27])),
            uncles: Some(vec![Hash::from([0xcc; 32])]),
            mix_hash: Some(Hash::from([0xdd; 32])),
            withdrawals_root: Some(Hash::from([0xee; 32])),
            withdrawals: Some(vec![Withdrawal {
                index: Some(Quantity::from(number * 10)),
                validator_index: Some(Quantity::from(number * 100)),
                address: Some(HsAddress::from([id; 20])),
                amount: Some(quantity(big_amount(number))),
            }]),
            ..Default::default()
        }]);

        let transaction = |index: u64, hash: [u8; 32]| Transaction {
            block_number: Some(UInt::from(number)),
            block_hash: Some(Hash::from(block_hash)),
            transaction_index: Some(UInt::from(index)),
            hash: Some(Hash::from(hash)),
            from: Some(HsAddress::from(SENDER)),
            gas: Some(Quantity::from(100_000u64)),
            gas_used: Some(Quantity::from(21_000u64)),
            cumulative_gas_used: Some(Quantity::from(
                21_000 * (index + 1),
            )),
            gas_price: Some(Quantity::from(9u64)),
            effective_gas_price: Some(Quantity::from(9u64)),
            nonce: Some(Quantity::from(number)),
            ..Default::default()
        };

        data.transactions.push(vec![
            Transaction {
                to: None,
                contract_address: Some(HsAddress::from([id | 0x80; 20])),
                input: Some(Data::from(vec![
                    0x60, 0x80, 0x60, 0x40, 0x52,
                ])),
                value: Some(quantity(big_value(number))),
                type_: Some(TransactionType::from(2u8)),
                status: Some(TransactionStatus::Success),
                max_fee_per_gas: Some(Quantity::from(20u64)),
                max_priority_fee_per_gas: Some(Quantity::from(2u64)),
                access_list: Some(vec![AccessList {
                    address: Some(HsAddress::from([2u8; 20])),
                    storage_keys: Some(vec![Hash::from([3u8; 32])]),
                }]),
                ..transaction(0, deployment_hash(number))
            },
            Transaction {
                to: Some(HsAddress::from(RECIPIENT)),
                input: Some(Data::from(vec![
                    0xa9, 0x05, 0x9c, 0xbb, 0x00,
                ])),
                value: Some(Quantity::from(5u64)),
                type_: Some(TransactionType::from(0u8)),
                status: Some(TransactionStatus::Failure),
                ..transaction(1, call_hash(number))
            },
        ]);

        let log = |index: u64,
                   address: [u8; 20],
                   topics: Vec<[u8; 32]>,
                   payload: Vec<u8>| {
            let mut log = Log {
                block_number: Some(UInt::from(number)),
                log_index: Some(UInt::from(index)),
                transaction_index: Some(UInt::from(0u64)),
                transaction_hash: Some(Hash::from(deployment_hash(
                    number,
                ))),
                address: Some(HsAddress::from(address)),
                data: Some(Data::from(payload)),
                ..Default::default()
            };
            for position in 0..4 {
                log.topics.push(
                    topics.get(position).map(|t| LogArgument::from(*t)),
                );
            }
            log
        };

        let sender_topic = Address::from(SENDER).into_word().0;
        let recipient_topic = Address::from(RECIPIENT).into_word().0;

        // (uint256[] ids, uint256[] values) = ([7, big], [1, big]).
        let mut batch_data = Vec::new();
        for word in [
            U256::from(64u8),
            U256::from(160u8),
            U256::from(2u8),
            U256::from(7u8),
            big_value(number),
            U256::from(2u8),
            U256::from(1u8),
            big_amount(number),
        ] {
            batch_data.extend(word.to_be_bytes::<32>());
        }

        data.logs.push(vec![
            log(
                0,
                ERC20,
                vec![
                    TRANSFER_EVENT_SIGNATURE.0,
                    sender_topic,
                    recipient_topic,
                ],
                big_amount(number).to_be_bytes::<32>().to_vec(),
            ),
            // Token id 0: the fourth topic is all zero bytes but present.
            log(
                1,
                ERC721,
                vec![
                    TRANSFER_EVENT_SIGNATURE.0,
                    sender_topic,
                    recipient_topic,
                    [0u8; 32],
                ],
                vec![],
            ),
            log(
                2,
                ERC1155,
                vec![
                    ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE.0,
                    [0u8; 32],
                    sender_topic,
                    recipient_topic,
                ],
                batch_data,
            ),
            log(3, [0x23; 20], vec![], vec![0x00, 0x80, 0xff]),
            // A token id beyond 128 bits.
            log(
                4,
                ERC721,
                vec![
                    TRANSFER_EVENT_SIGNATURE.0,
                    sender_topic,
                    recipient_topic,
                    big_value(number).to_be_bytes::<32>(),
                ],
                vec![],
            ),
        ]);

        let trace = |type_: &str, path: Vec<u64>| Trace {
            block_number: Some(number),
            block_hash: Some(Hash::from(block_hash)),
            type_: Some(type_.to_string()),
            subtraces: Some(0),
            trace_address: Some(path),
            transaction_hash: Some(Hash::from(deployment_hash(number))),
            transaction_position: Some(0),
            ..Default::default()
        };

        data.traces.push(vec![
            Trace {
                call_type: Some("call".to_string()),
                from: Some(HsAddress::from(SENDER)),
                to: Some(HsAddress::from(FACTORY)),
                gas: Some(Quantity::from(u64::MAX)),
                gas_used: Some(Quantity::from(1u64)),
                value: Some(quantity(big_value(number))),
                input: Some(Data::from(vec![0x00, 0x01])),
                output: Some(Data::from(vec![])),
                subtraces: Some(1),
                ..trace("call", vec![])
            },
            Trace {
                from: Some(HsAddress::from(FACTORY)),
                address: Some(HsAddress::from([id | 0x40; 20])),
                init: Some(Data::from(vec![0x60])),
                code: Some(Data::from(vec![0xfe])),
                value: Some(Quantity::from(0u64)),
                gas: Some(Quantity::from(50_000u64)),
                gas_used: Some(Quantity::from(32_000u64)),
                ..trace("create", vec![0])
            },
            Trace {
                author: Some(HsAddress::from([0x99; 20])),
                reward_type: Some("block".to_string()),
                value: Some(Quantity::from(2_000_000_000u64)),
                transaction_hash: None,
                transaction_position: None,
                ..trace("reward", vec![])
            },
        ]);
    }

    data
}

/// Rows of blocks `[from, to)`, stamped like a flush would.
fn rows_with(chain: u64, from: u64, to: u64, salt: u8) -> RowBatch {
    let numbers: Vec<u64> = (from..to).collect();
    let mut rows = transform(
        chain,
        &response(&numbers, salt),
        BlockRange::new(from, to),
    )
    .unwrap()
    .rows;
    rows.set_version(next_version());
    rows
}

fn rows(chain: u64, from: u64, to: u64) -> RowBatch {
    rows_with(chain, from, to, 0)
}

async fn count_where(
    database: &Database,
    table: &str,
    filter: &str,
) -> u64 {
    database
        .db
        .query(&format!(
            "SELECT count() FROM {table} FINAL WHERE chain = {} AND {filter}",
            database.chain_id
        ))
        .fetch_one::<u64>()
        .await
        .unwrap_or_else(|e| panic!("{table}: {e}"))
}

async fn count(database: &Database, table: &str) -> u64 {
    count_where(database, table, "1").await
}

/// Rows before dedup (no FINAL).
async fn raw_count(database: &Database, table: &str) -> u64 {
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

async fn strings(database: &Database, sql: &str) -> Vec<String> {
    database
        .db
        .query(sql)
        .fetch_all::<String>()
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

/// Reads rows back through the models. Validation is off for the same
/// reason as for inserts: the crate can not validate (U)Int256.
async fn read_back<T>(
    database: &Database,
    table: &str,
    order: &str,
) -> Vec<T>
where
    T: Row + for<'b> Deserialize<'b> + 'static,
    for<'a> T: Row<Value<'a> = T>,
{
    database
        .db
        .clone()
        .with_validation(false)
        .query(&format!(
            "SELECT ?fields FROM {table} FINAL WHERE chain = {} \
             ORDER BY {order}",
            database.chain_id
        ))
        .fetch_all::<T>()
        .await
        .unwrap_or_else(|e| panic!("{table}: {e}"))
}

/// Rows per block produced by [`response`], base and side tables.
const ROWS_PER_BLOCK: [(&str, u64); 16] = [
    ("blocks", 1),
    ("transactions", 2),
    ("logs", 5),
    ("traces", 3),
    ("withdrawals", 1),
    ("erc20_transfers", 1),
    ("erc721_transfers", 2),
    ("erc1155_transfers", 1),
    // Deployment transaction + create trace.
    ("contracts", 2),
    ("tx_lookup", 2),
    ("block_lookup", 1),
    // Sender + (created contract | recipient), per transaction.
    ("transactions_by_address", 4),
    ("logs_by_address", 5),
    ("erc20_transfers_by_account", 2),
    // ERC721: 2 x 2. ERC1155 batch of two ids: 4.
    ("nft_transfers_by_account", 8),
    // The reward trace has no transaction.
    ("traces_by_tx", 2),
];

async fn assert_blocks_stored(database: &Database, blocks: u64) {
    for (table, per_block) in ROWS_PER_BLOCK {
        let found = count(database, table).await;
        if found != per_block * blocks {
            let column = block_number_column(table);
            let dump = strings(
                database,
                &format!(
                    "SELECT concat(toString({column}), ' v', \
                       toString(_version), ' ', _part, ' exists=', \
                       toString(_row_exists)) FROM {table} \
                     WHERE chain = {} ORDER BY {column}, _version \
                     SETTINGS apply_deleted_mask = 0",
                    database.chain_id
                ),
            )
            .await;
            let mutations = strings(
                database,
                &format!(
                    "SELECT concat(mutation_id, ' ', command, ' done=', \
                       toString(is_done), ' ', toString(create_time)) \
                     FROM system.mutations WHERE table = '{table}' \
                       AND database = currentDatabase() \
                     ORDER BY mutation_id DESC LIMIT 8"
                ),
            )
            .await;
            panic!(
                "{table}: {found} rows, expected {}\n{}\n--\n{}",
                per_block * blocks,
                dump.join("\n"),
                mutations.join("\n")
            );
        }
    }
}

#[test]
fn the_fixture_covers_every_block_scoped_table() {
    for table in BLOCK_SCOPED_TABLES {
        assert!(
            ROWS_PER_BLOCK.iter().any(|(name, _)| name == table),
            "{table}"
        );
    }
    assert_eq!(ROWS_PER_BLOCK.len(), BLOCK_SCOPED_TABLES.len());
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn stores_and_reads_back_every_table() {
    const CHAIN: u64 = 990_001;
    let database = database(CHAIN).await;

    let mut batch = rows(CHAIN, 10, 13);
    batch.tokens.push(DatabaseToken {
        address: Address::from(ERC20),
        name: "Token".into(),
        symbol: "TKN".into(),
        decimals: 18,
        r#type: "ERC20".into(),
        chain: CHAIN,
    });

    database.store(&batch).await.unwrap();

    // wait_for_async_insert=1: rows (and what the materialized views derive
    // from them) are visible as soon as store returns.
    assert_blocks_stored(&database, 3).await;
    assert_eq!(count(&database, "tokens").await, 1);

    // Every table round trips through the models, bit for bit.
    assert_eq!(
        read_back::<DatabaseBlock>(&database, "blocks", "number").await,
        batch.blocks
    );
    assert_eq!(
        read_back::<DatabaseTransaction>(
            &database,
            "transactions",
            "block_number, transaction_index"
        )
        .await,
        batch.transactions
    );
    assert_eq!(
        read_back::<DatabaseTrace>(
            &database,
            "traces",
            "block_number, transaction_position, trace_address"
        )
        .await,
        batch.traces
    );
    assert_eq!(
        read_back::<DatabaseWithdrawal>(
            &database,
            "withdrawals",
            "block_number"
        )
        .await,
        batch.withdrawals
    );
    assert_eq!(
        read_back::<DatabaseERC20Transfer>(
            &database,
            "erc20_transfers",
            "block_number, log_index"
        )
        .await,
        batch.erc20_transfers
    );
    assert_eq!(
        read_back::<DatabaseERC721Transfer>(
            &database,
            "erc721_transfers",
            "block_number, log_index"
        )
        .await,
        batch.erc721_transfers
    );
    assert_eq!(
        read_back::<DatabaseERC1155Transfer>(
            &database,
            "erc1155_transfers",
            "block_number, log_index"
        )
        .await,
        batch.erc1155_transfers
    );

    let mut contracts = batch.contracts.clone();
    contracts.sort_by_key(|c| (c.block_number, c.contract_address));
    assert_eq!(
        read_back::<DatabaseContract>(
            &database,
            "contracts",
            "block_number, contract_address"
        )
        .await,
        contracts
    );

    // Logs: topics are not nullable in SQL, `topic_count` is what tells an
    // all zero topic (ERC721 id 0, ERC1155 operator 0x0) from a missing
    // one, in SQL and when reading back through the model.
    let logs = read_back::<DatabaseLog>(
        &database,
        "logs",
        "block_number, log_index",
    )
    .await;
    assert_eq!(logs, batch.logs);
    assert_eq!(logs[1].topic_count, 4);
    assert_eq!(logs[1].topic3, Some(alloy::primitives::B256::ZERO));
    assert_eq!(logs[3].topic_count, 0);
    assert_eq!(logs[3].topic0, None);
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(topic_count), ' ', hex(topic3)) \
             FROM logs FINAL WHERE chain = 990001 AND block_number = 10 \
             ORDER BY log_index",
        )
        .await,
        vec![
            format!("3 {}", "00".repeat(32)),
            format!("4 {}", "00".repeat(32)),
            format!(
                "4 {}",
                format!("{}{}", "00".repeat(12), "02".repeat(20))
            ),
            format!("0 {}", "00".repeat(32)),
            format!("4 {:064X}", big_value(10)),
        ]
    );

    let tokens = database
        .db
        .clone()
        .with_validation(false)
        .query("SELECT ?fields FROM tokens FINAL WHERE chain = 990001")
        .fetch_all::<DatabaseToken>()
        .await
        .unwrap();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].address, Address::from(ERC20));
    assert_eq!(tokens[0].symbol, "TKN");
    assert_eq!(tokens[0].decimals, 18);
    // Assigned by the server.
    let version = database
        .db
        .query("SELECT _version FROM tokens WHERE chain = 990001")
        .fetch_one::<u64>()
        .await
        .unwrap();
    assert!(version > 1_577_836_800_000);

    // ... and the values mean the same thing to SQL as they do to Rust.
    let row = strings(
        &database,
        "SELECT ifNull(concat( \
           lower(hex(hash)), ' ', toString(value), ' ', \
           lower(hex(method)), ' ', transaction_type, ' ', \
           toString(`to` IS NULL), ' ', lower(hex(contract_created)), ' ', \
           lower(hex(input)), ' ', toString(status), ' ', \
           toString(base_fee_per_gas), ' ', \
           lower(hex(access_list[1].1)), ' ', \
           lower(hex(access_list[1].2[1]))), 'NULL') \
         FROM transactions FINAL \
         WHERE chain = 990001 AND block_number = 10 \
           AND transaction_index = 0",
    )
    .await;
    assert_eq!(
        row,
        vec![format!(
            "{} {} 60806040 eip_1559 1 {} 6080604052 success 7 {} {}",
            "fa".repeat(32),
            // 2^200 + 10, far beyond 128 bits.
            "1606938044258990275541962092341162602522202993782792835301386",
            "8a".repeat(20),
            "02".repeat(20),
            "03".repeat(32),
        )]
    );
    assert_eq!(
        big_value(10).to_string(),
        row[0].split(' ').nth(1).unwrap()
    );

    // UInt256 is a number in SQL: it compares and orders.
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(count()), ' ', toString(max(amount))) \
             FROM erc20_transfers FINAL \
             WHERE chain = 990001 AND amount > pow(2, 129) \
               AND amount < toUInt256(pow(2, 131))",
        )
        .await,
        vec![format!("3 {}", big_amount(12))]
    );

    // Every table with a 256 bit column: the value SQL sees (toString) is
    // the value Rust sent, beyond 128 bits. A symmetric mistake in the
    // adapters (say, big endian both ways) would pass the model round trip
    // above, it can not pass this.
    let value = big_value(10).to_string();
    let amount = big_amount(10).to_string();
    for (sql, expected) in [
        (
            "SELECT toString(total_difficulty - 1) FROM blocks FINAL \
             WHERE chain = 990001 AND number = 10",
            &value,
        ),
        (
            "SELECT toString(assumeNotNull(value)) FROM traces FINAL \
             WHERE chain = 990001 AND block_number = 10 \
               AND action_type = 'call'",
            &value,
        ),
        (
            "SELECT toString(amount) FROM withdrawals FINAL \
             WHERE chain = 990001 AND block_number = 10",
            &amount,
        ),
        (
            "SELECT toString(amount) FROM erc20_transfers FINAL \
             WHERE chain = 990001 AND block_number = 10",
            &amount,
        ),
        (
            "SELECT toString(id) FROM erc721_transfers FINAL \
             WHERE chain = 990001 AND block_number = 10 AND log_index = 4",
            &value,
        ),
        (
            "SELECT toString(ids[2]) FROM erc1155_transfers FINAL \
             WHERE chain = 990001 AND block_number = 10",
            &value,
        ),
        (
            "SELECT toString(amounts[2]) FROM erc1155_transfers FINAL \
             WHERE chain = 990001 AND block_number = 10",
            &amount,
        ),
        (
            "SELECT toString(value) FROM transactions_by_address FINAL \
             WHERE chain = 990001 AND block_number = 10 \
               AND transaction_index = 0 AND direction = 1",
            &value,
        ),
        (
            "SELECT toString(amount) FROM erc20_transfers_by_account FINAL \
             WHERE chain = 990001 AND block_number = 10 AND direction = 1",
            &amount,
        ),
        (
            "SELECT toString(token_id) FROM nft_transfers_by_account FINAL \
             WHERE chain = 990001 AND block_number = 10 AND log_index = 4 \
               AND direction = 1",
            &value,
        ),
    ] {
        assert_eq!(strings(&database, sql).await, vec![expected.clone()], "{sql}");
    }

    // Raw bytes, not hex text and not utf-8.
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(length(data)), ' ', hex(data)) \
             FROM logs FINAL \
             WHERE chain = 990001 AND block_number = 10 AND log_index = 3",
        )
        .await,
        vec!["3 0080FF".to_string()]
    );

    let (gas_limit, validator_index, withdrawal_index): (u64, u64, u64) =
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

    assert_eq!(gas_limit, u64::MAX); // no saturation
    assert_eq!(validator_index, 1_100);
    assert_eq!(withdrawal_index, 110);

    // Reward traces: sentinel position, NULL where nothing applies.
    assert_eq!(
        strings(
            &database,
            "SELECT ifNull(concat(toString(transaction_position), ' ', \
               toString(gas IS NULL), ' ', toString(`from` IS NULL), ' ', \
               toString(reward_type), ' ', lower(hex(transaction_hash))), \
               'NULL') \
             FROM traces FINAL \
             WHERE chain = 990001 AND block_number = 10 \
               AND action_type = 'reward'",
        )
        .await,
        vec![format!("4294967295 1 1 block {}", "00".repeat(32))]
    );

    assert_eq!(
        database.block_hash(12).await.unwrap(),
        Some(alloy::primitives::B256::repeat_byte(0x0c))
    );
    assert_eq!(database.block_hash(500).await.unwrap(), None);
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn materialized_views_feed_the_read_path_tables() {
    const CHAIN: u64 = 990_003;
    let database = database(CHAIN).await;

    database.store(&rows(CHAIN, 20, 22)).await.unwrap();

    let query = |sql: &str| sql.replace("{chain}", &CHAIN.to_string());

    // Transaction / block by hash.
    assert_eq!(
        strings(
            &database,
            &query(&format!(
                "SELECT concat(toString(block_number), ':', \
                   toString(transaction_index)) \
                 FROM tx_lookup FINAL \
                 WHERE chain = {{chain}} AND hash = unhex('{}')",
                hex::encode(call_hash(21))
            )),
        )
        .await,
        vec!["21:1"]
    );
    assert_eq!(
        strings(
            &database,
            &query(&format!(
                "SELECT toString(block_number) FROM block_lookup FINAL \
                 WHERE chain = {{chain}} AND hash = unhex('{}')",
                "14".repeat(32)
            )),
        )
        .await,
        vec!["20"]
    );

    // Transactions of an address: both sides, deployments list the
    // created contract as the receiving side.
    assert_eq!(
        strings(
            &database,
            &query(
                "SELECT concat(lower(hex(address)), ' ', \
                   toString(direction), ' ', lower(hex(counterparty)), ' ', \
                   toString(transaction_index)) \
                 FROM transactions_by_address FINAL \
                 WHERE chain = {chain} AND block_number = 20 \
                 ORDER BY transaction_index, direction"
            ),
        )
        .await,
        vec![
            format!("{} -1 {} 0", "01".repeat(20), "94".repeat(20)),
            format!("{} 1 {} 0", "94".repeat(20), "01".repeat(20)),
            format!("{} -1 {} 1", "01".repeat(20), "02".repeat(20)),
            format!("{} 1 {} 1", "02".repeat(20), "01".repeat(20)),
        ]
    );

    // eth_getLogs style lookup.
    assert_eq!(
        strings(
            &database,
            &query(&format!(
                "SELECT concat(toString(block_number), ':', \
                   toString(log_index)) \
                 FROM logs_by_address FINAL \
                 WHERE chain = {{chain}} AND address = unhex('{}') \
                   AND topic0 = unhex('{}') \
                 ORDER BY block_number",
                hex::encode(ERC20),
                hex::encode(TRANSFER_EVENT_SIGNATURE)
            )),
        )
        .await,
        vec!["20:0", "21:0"]
    );

    // Balances from the signed two-rows-per-transfer table.
    assert_eq!(
        strings(
            &database,
            &query(
                "SELECT concat(lower(hex(account)), ' ', \
                   toString(sum(toInt256(amount) * direction))) \
                 FROM erc20_transfers_by_account FINAL \
                 WHERE chain = {chain} GROUP BY account ORDER BY account"
            ),
        )
        .await,
        vec![
            format!(
                "{} -{}",
                "01".repeat(20),
                big_amount(20) + big_amount(21)
            ),
            format!(
                "{} {}",
                "02".repeat(20),
                big_amount(20) + big_amount(21)
            ),
        ]
    );

    // NFTs: ERC721 id 0 and both ids of the ERC1155 batch, per side.
    assert_eq!(
        strings(
            &database,
            &query(
                "SELECT concat(standard, ' ', toString(batch_index), ' ', \
                   toString(token_id), ' ', toString(amount), ' ', \
                   toString(direction)) \
                 FROM nft_transfers_by_account FINAL \
                 WHERE chain = {chain} AND block_number = 20 \
                   AND account = unhex('0202020202020202020202020202020202020202') \
                 ORDER BY standard, batch_index, token_id"
            ),
        )
        .await,
        vec![
            "ERC1155 0 7 1 1".to_string(),
            format!("ERC1155 1 {} {} 1", big_value(20), big_amount(20)),
            "ERC721 0 0 1 1".to_string(),
            format!("ERC721 0 {} 1 1", big_value(20)),
        ]
    );

    // Traces of a transaction (the reward trace is not listed).
    assert_eq!(
        strings(
            &database,
            &query(&format!(
                "SELECT concat(toString(block_number), ' ', \
                   toString(transaction_position), ' ', \
                   toString(trace_address)) \
                 FROM traces_by_tx FINAL \
                 WHERE chain = {{chain}} AND transaction_hash = unhex('{}') \
                 ORDER BY trace_address",
                hex::encode(deployment_hash(20))
            )),
        )
        .await,
        vec!["20 0 []", "20 0 [0]"]
    );
    assert_eq!(count(&database, "traces_by_tx").await, 4);
}

/// Equal up to Float64 rounding.
fn close(left: f64, right: f64) -> bool {
    (left - right).abs() <= right.abs() * 1e-12
}

/// `*_v` rows of a chain as comparable strings.
async fn view_rows(database: &Database, table: &str) -> Vec<String> {
    strings(
        database,
        &format!(
            "SELECT toString(tuple(*)) FROM {table}_v WHERE chain = {} \
             ORDER BY ALL",
            database.chain_id
        ),
    )
    .await
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn aggregate_views_return_correct_numbers() {
    const CHAIN: u64 = 990_004;
    let database = database(CHAIN).await;

    // Blocks 4-6 on the first day, 7-8 on the second, in two flushes that
    // split the first day: states must merge, not add up distinct counts.
    database.store(&rows(CHAIN, 4, 6)).await.unwrap();
    database.store(&rows(CHAIN, 6, 9)).await.unwrap();

    #[derive(Debug, Row, Deserialize, PartialEq)]
    struct BlockStats {
        day: u32,
        blocks: u64,
        transactions: u64,
        gas_used: u64,
        first_block: u64,
        last_block: u64,
        unique_miners: u64,
        avg_base_fee_per_gas: Option<f64>,
    }

    let stats = database
        .db
        .query(
            "SELECT toUnixTimestamp(day) AS day, blocks, transactions, \
               gas_used, first_block, last_block, unique_miners, \
               avg_base_fee_per_gas \
             FROM daily_block_stats_v WHERE chain = 990004 ORDER BY day",
        )
        .fetch_all::<BlockStats>()
        .await
        .unwrap();

    assert_eq!(
        stats,
        vec![
            BlockStats {
                day: DAY_1,
                blocks: 3,
                transactions: 6,
                gas_used: 126_000,
                first_block: 4,
                last_block: 6,
                unique_miners: 1,
                avg_base_fee_per_gas: Some(7.0),
            },
            BlockStats {
                day: DAY_2,
                blocks: 2,
                transactions: 4,
                gas_used: 84_000,
                first_block: 7,
                last_block: 8,
                unique_miners: 1,
                avg_base_fee_per_gas: Some(7.0),
            },
        ]
    );

    // Status values are the stored ones: both counters are non zero.
    // Amounts are Float64 (256-bit arithmetic rule).
    #[derive(Debug, Row, Deserialize)]
    struct TransactionStats {
        day: u32,
        transactions: u64,
        successful: u64,
        failed: u64,
        contract_creations: u64,
        gas_used: u64,
        value: f64,
        fees: f64,
        unique_senders: u64,
        unique_recipients: u64,
        avg_effective_gas_price: f64,
    }

    let stats = database
        .db
        .query(
            "SELECT toUnixTimestamp(day) AS day, transactions, successful, \
               failed, contract_creations, gas_used, value, fees, \
               unique_senders, unique_recipients, avg_effective_gas_price \
             FROM daily_transaction_stats_v WHERE chain = 990004 \
             ORDER BY day",
        )
        .fetch_all::<TransactionStats>()
        .await
        .unwrap();

    assert_eq!(stats.len(), 2);
    for (day, start, blocks, numbers) in
        [(&stats[0], DAY_1, 3u64, 4..7u64), (&stats[1], DAY_2, 2, 7..9)]
    {
        assert_eq!(day.day, start);
        assert_eq!(day.transactions, blocks * 2);
        assert_eq!(day.successful, blocks);
        assert_eq!(day.failed, blocks);
        assert_eq!(day.contract_creations, blocks);
        assert_eq!(day.gas_used, blocks * 42_000);
        assert_eq!(day.unique_senders, 1);
        assert_eq!(day.unique_recipients, 1);
        assert_eq!(day.avg_effective_gas_price, 9.0);
        assert_eq!(day.fees, (blocks * 2 * 21_000 * 9) as f64);

        let value: U256 = numbers
            .map(|number| big_value(number) + U256::from(5u8))
            .fold(U256::ZERO, |sum, value| sum + value);
        assert!(close(day.value, f64::from(value)), "{day:?}");
    }

    // ERC20: raw units as Float64, scaled once the token is known.
    #[derive(Debug, Row, Deserialize)]
    struct Erc20Stats {
        day: u32,
        transfers: u64,
        volume_raw: f64,
        volume: Option<f64>,
        unique_senders: u64,
        unique_recipients: u64,
    }

    let erc20_stats = || async {
        database
            .db
            .query(&format!(
                "SELECT toUnixTimestamp(day) AS day, transfers, volume_raw, \
                   volume, unique_senders, unique_recipients \
                 FROM daily_erc20_transfer_stats_v \
                 WHERE chain = 990004 AND token_address = unhex('{}') \
                 ORDER BY day",
                hex::encode(ERC20)
            ))
            .fetch_all::<Erc20Stats>()
            .await
            .unwrap()
    };

    let stats = erc20_stats().await;
    assert_eq!(stats.len(), 2);
    assert_eq!((stats[0].day, stats[0].transfers), (DAY_1, 3));
    assert_eq!((stats[1].day, stats[1].transfers), (DAY_2, 2));
    assert_eq!(stats[0].unique_senders, 1);
    assert_eq!(stats[0].unique_recipients, 1);
    assert!(close(
        stats[0].volume_raw,
        f64::from(big_amount(4) + big_amount(5) + big_amount(6))
    ));
    // No metadata yet: unknown, not zero.
    assert_eq!(stats[0].volume, None);

    database
        .insert_rows(
            "tokens",
            &[DatabaseToken {
                address: Address::from(ERC20),
                name: "Token".into(),
                symbol: "TKN".into(),
                decimals: 18,
                r#type: "ERC20".into(),
                chain: CHAIN,
            }],
        )
        .await
        .unwrap();

    let stats = erc20_stats().await;
    assert!(close(stats[1].volume.unwrap(), stats[1].volume_raw / 1e18));

    // Real timestamps (2023, not 1970), two deployers: sender and factory.
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(day), ' ', toString(contracts), ' ', \
               toString(unique_deployers)) \
             FROM daily_contract_deployments_v WHERE chain = 990004 \
             ORDER BY day",
        )
        .await,
        vec!["2023-11-14 00:00:00 6 2", "2023-11-15 00:00:00 4 2"]
    );
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn aggregates_do_not_wrap_on_hostile_amounts() {
    const CHAIN: u64 = 990_010;
    let database = database(CHAIN).await;

    // A spam token (and, why not, a transaction) moving 2^256-1, 3 times.
    let mut batch = rows(CHAIN, 1, 4);
    for transfer in &mut batch.erc20_transfers {
        transfer.amount = U256::MAX;
    }
    for transaction in &mut batch.transactions {
        transaction.value = U256::MAX;
        transaction.effective_gas_price = U256::MAX;
    }
    database.store(&batch).await.unwrap();

    // The premise: the exact values are stored, and summing them as
    // integers silently wraps (3 * (2^256-1) mod 2^256 = 2^256-3).
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(max(amount)), ' ', \
               toString(sum(amount))) \
             FROM erc20_transfers FINAL WHERE chain = 990010",
        )
        .await,
        vec![format!("{} {}", U256::MAX, U256::MAX - U256::from(2u8))]
    );

    let max = f64::from(U256::MAX);
    let check = |label: &'static str| {
        let database = &database;
        async move {
            let (volume_raw, value, fees, price): (f64, f64, f64, f64) =
                database
                    .db
                    .query(
                        "SELECT e.volume_raw, t.value, t.fees, \
                           t.avg_effective_gas_price \
                         FROM daily_erc20_transfer_stats_v AS e, \
                           daily_transaction_stats_v AS t \
                         WHERE e.chain = 990010 AND t.chain = 990010",
                    )
                    .fetch_one()
                    .await
                    .unwrap();

            assert!(close(volume_raw, 3.0 * max), "{label}: {volume_raw}");
            assert!(close(value, 6.0 * max), "{label}: {value}");
            assert!(close(fees, 6.0 * 21_000.0 * max), "{label}: {fees}");
            assert!(close(price, max), "{label}: {price}");
        }
    };

    check("materialized view").await;

    // The rebuild path obeys the same rule.
    for table in CORE_DERIVED {
        execute(&database, &table.delete_sql(CHAIN, DAY_1)).await;
        execute(&database, &table.rebuild_sql(CHAIN, DAY_1)).await;
    }
    check("rebuild_sql").await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reinserted_block_replaces_itself_under_final() {
    const CHAIN: u64 = 990_005;
    let database = database(CHAIN).await;

    database.store(&rows(CHAIN, 30, 32)).await.unwrap();
    assert_blocks_stored(&database, 2).await;

    // The same heights again, later (higher _version), different content.
    let again = rows_with(CHAIN, 30, 32, 0x55);
    assert_ne!(again.blocks[0].hash, rows(CHAIN, 30, 31).blocks[0].hash);
    database.store(&again).await.unwrap();

    // Both copies exist until a merge gets to them (which can be any
    // time) ...
    assert!(raw_count(&database, "blocks").await >= 2);

    // ... but FINAL sees ONE row per key, in every base and side table,
    // and it is the later one.
    for (table, per_block) in ROWS_PER_BLOCK {
        // block_lookup is keyed by hash, and the hash changed: the stale
        // entry is what purge_range removes (by block_number).
        let expected =
            if table == "block_lookup" { 4 } else { per_block * 2 };
        assert_eq!(count(&database, table).await, expected, "{table}");
    }

    assert_eq!(
        database.block_hash(30).await.unwrap(),
        Some(again.blocks[0].hash)
    );
    assert_eq!(
        read_back::<DatabaseBlock>(&database, "blocks", "number").await,
        again.blocks
    );
    assert_eq!(
        strings(
            &database,
            "SELECT toString(gas_used) FROM blocks FINAL \
             WHERE chain = 990005 ORDER BY number",
        )
        .await,
        vec!["42085", "42085"]
    );

    // Same after the merge really happened.
    for (table, _) in ROWS_PER_BLOCK {
        execute(&database, &format!("OPTIMIZE TABLE {table} FINAL")).await;
    }
    assert_eq!(raw_count(&database, "blocks").await, 2);
    assert_eq!(raw_count(&database, "logs").await, 10);
    assert_eq!(raw_count(&database, "traces").await, 6);
    assert_eq!(raw_count(&database, "tx_lookup").await, 4);
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn lightweight_deletes_work_on_every_block_scoped_table() {
    const CHAIN: u64 = 990_006;
    let database = database(CHAIN).await;

    database.store(&rows(CHAIN, 40, 46)).await.unwrap();
    assert_blocks_stored(&database, 6).await;

    // The rollback statement of purge_range, children first, blocks last.
    for table in BLOCK_SCOPED_TABLES {
        execute(
            &database,
            &format!(
                "DELETE FROM {table} WHERE chain = {CHAIN} AND {} >= 43 \
                 SETTINGS lightweight_deletes_sync = 2",
                block_number_column(table)
            ),
        )
        .await;
    }

    // 40, 41, 42 survive everywhere, nothing at or above 43 does.
    assert_blocks_stored(&database, 3).await;
    for table in BLOCK_SCOPED_TABLES {
        let column = block_number_column(table);
        assert_eq!(
            count_where(&database, table, &format!("{column} >= 43"))
                .await,
            0,
            "{table}"
        );
        // Also without FINAL: deleted rows are gone, not just shadowed.
        assert_eq!(
            raw_count(&database, table).await,
            count(&database, table).await,
            "{table}"
        );
    }

    // Bounded variant (gap healing) and idempotency.
    for _ in 0..2 {
        for table in BLOCK_SCOPED_TABLES {
            let column = block_number_column(table);
            execute(
                &database,
                &format!(
                    "DELETE FROM {table} WHERE chain = {CHAIN} \
                     AND {column} >= 41 AND {column} < 42"
                ),
            )
            .await;
        }
    }
    assert_blocks_stored(&database, 2).await;
    assert_eq!(database.block_hash(41).await.unwrap(), None);
    assert!(database.block_hash(42).await.unwrap().is_some());

    // The purged range can be streamed again.
    database.store(&rows(CHAIN, 41, 42)).await.unwrap();
    database.store(&rows(CHAIN, 43, 46)).await.unwrap();
    assert_blocks_stored(&database, 6).await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn rebuild_sql_reproduces_what_the_views_wrote() {
    const CHAIN: u64 = 990_007;
    let database = database(CHAIN).await;

    // Two days, several flushes.
    database.store(&rows(CHAIN, 3, 5)).await.unwrap();
    database.store(&rows(CHAIN, 5, 8)).await.unwrap();
    database.store(&rows(CHAIN, 8, 10)).await.unwrap();

    for table in CORE_DERIVED {
        let written_by_the_view = view_rows(&database, table.name).await;
        assert!(written_by_the_view.len() >= 2, "{}", table.name);

        // Any timestamp inside the first day repairs from its start.
        let from_ts = DAY_1 + 80_000;

        execute(&database, &table.delete_sql(CHAIN, from_ts)).await;
        assert!(
            view_rows(&database, table.name).await.is_empty(),
            "{}",
            table.name
        );

        execute(&database, &table.rebuild_sql(CHAIN, from_ts)).await;
        assert_eq!(
            view_rows(&database, table.name).await,
            written_by_the_view,
            "{}",
            table.name
        );

        // Repairing only the second day leaves the first one alone.
        execute(&database, &table.delete_sql(CHAIN, DAY_2)).await;
        assert_eq!(
            view_rows(&database, table.name).await.len(),
            written_by_the_view.len() / 2,
            "{}",
            table.name
        );
        execute(&database, &table.rebuild_sql(CHAIN, DAY_2 + 5)).await;
        assert_eq!(
            view_rows(&database, table.name).await,
            written_by_the_view,
            "{}",
            table.name
        );
    }
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn bucket_repair_after_a_rollback_matches_a_clean_index() {
    const CHAIN: u64 = 990_008;
    const CLEAN: u64 = 990_009;
    let database = database(CHAIN).await;
    let clean = self::database(CLEAN).await;

    // What the aggregates of blocks 3..8 look like without any rollback.
    clean.store(&rows(CLEAN, 3, 8)).await.unwrap();

    // Index 3..10, then roll back to 8 (second day) the way purge_range
    // does: base rows first, then the affected buckets.
    database.store(&rows(CHAIN, 3, 10)).await.unwrap();

    let min_ts: u32 = database
        .db
        .query(
            "SELECT toUnixTimestamp(min(timestamp)) FROM blocks FINAL \
             WHERE chain = 990008 AND number >= 8",
        )
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(u64::from(min_ts), timestamp_of(8));

    for table in BLOCK_SCOPED_TABLES {
        execute(
            &database,
            &format!(
                "DELETE FROM {table} WHERE chain = {CHAIN} AND {} >= 8",
                block_number_column(table)
            ),
        )
        .await;
    }

    for table in CORE_DERIVED {
        // Deleting base rows does not touch the aggregate: still 3..10.
        assert_ne!(
            view_rows(&database, table.name).await,
            view_rows(&clean, table.name)
                .await
                .iter()
                .map(|row| row.replace("990009", "990008"))
                .collect::<Vec<_>>(),
            "{}",
            table.name
        );

        execute(&database, &table.delete_sql(CHAIN, min_ts)).await;
        execute(&database, &table.rebuild_sql(CHAIN, min_ts)).await;

        assert_eq!(
            view_rows(&database, table.name).await,
            view_rows(&clean, table.name)
                .await
                .iter()
                .map(|row| row.replace("990009", "990008"))
                .collect::<Vec<_>>(),
            "{}",
            table.name
        );
    }

    // Streaming the range again flows through the views incrementally and
    // ends up where a clean index of 3..10 would.
    database.store(&rows(CHAIN, 8, 10)).await.unwrap();
    // (Wipes the reference chain first.)
    let reference = self::database(CLEAN).await;
    reference.store(&rows(CLEAN, 3, 10)).await.unwrap();

    for table in CORE_DERIVED {
        assert_eq!(
            view_rows(&database, table.name).await,
            view_rows(&reference, table.name)
                .await
                .iter()
                .map(|row| row.replace("990009", "990008"))
                .collect::<Vec<_>>(),
            "{}",
            table.name
        );
    }
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
