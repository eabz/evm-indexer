//! Tests against a real ClickHouse. Ignored by default:
//!
//! ```sh
//! TEST_DATABASE_URL=http://default@localhost:8123/indexer_test \
//!   cargo test -- --ignored
//! ```
//!
//! The database named in the url BELONGS to these tests: it is dropped and
//! recreated (through the migrator, the real startup path) once per test
//! process, which is why its name has to end in `_test`. Every test uses
//! its own chain ids, and, like the indexer itself, never deletes anything
//! (docs/design.md, section 2): the tests run in parallel against shared
//! tables with no lock of any kind.

use super::{
    block_number_column,
    derived::repair_start,
    next_version,
    ranges::{BlockRange, DatabaseCheckpoint},
    schema::{live_rows_sql, min_timestamp_sql},
    tombstone_sql, Database, DatabaseParams,
};
use crate::{
    core::events::{
        ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE, TRANSFER_EVENT_SIGNATURE,
    },
    core::models::{
        block::DatabaseBlock, erc1155_transfer::DatabaseERC1155Transfer,
        erc20_transfer::DatabaseERC20Transfer,
        erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
        transaction::DatabaseTransaction, withdrawal::DatabaseWithdrawal,
    },
    core::{self, RowBatch, BASE_TABLES, CORE_DERIVED, SIDE_TABLES},
    pipeline::transform::{transform, ResponseRows},
    tokens::models::DatabaseToken,
};
use alloy::primitives::{Address, U256};
use clickhouse::{Client, Row};
use hypersync_client::{
    format::{
        AccessList, Address as HsAddress, Data, Hash, LogArgument,
        Quantity, TransactionStatus, TransactionType, UInt, Withdrawal,
    },
    simple_types::{Block, Log, Transaction},
};
use serde::Deserialize;
use tokio::sync::OnceCell;

/// The database is recreated once per test process.
static MIGRATED: OnceCell<()> = OnceCell::const_new();

fn test_url() -> String {
    std::env::var("TEST_DATABASE_URL")
        .expect("TEST_DATABASE_URL must be set for the ignored tests")
}

/// Fresh database, then the real startup path: every embedded migration
/// through the migrator.
async fn migrate() {
    let params = DatabaseParams::parse(&test_url()).unwrap();

    assert!(
        params.database.ends_with("_test"),
        "TEST_DATABASE_URL names database '{}': these tests DROP their \
         database, so its name must end in '_test'",
        params.database
    );

    Client::default()
        .with_url(&params.endpoint)
        .with_user(&params.user)
        .with_password(&params.password)
        .query(&format!("DROP DATABASE IF EXISTS `{}`", params.database))
        .execute()
        .await
        .unwrap();

    super::migrate::run(&test_url()).await.unwrap();
}

async fn database(chain: u64) -> Database {
    MIGRATED.get_or_init(migrate).await;
    Database::new(&test_url(), chain).await.unwrap()
}

async fn execute(database: &Database, sql: &str) {
    // The rule the whole design rests on.
    let upper = sql.to_uppercase();
    assert!(
        !upper.contains("DELETE ")
            && !upper.contains("DROP PARTITION")
            && !upper.contains(" UPDATE "),
        "the indexer never deletes: {sql}"
    );

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
const ERC20: [u8; 20] = [0x20; 20];
const ERC721: [u8; 20] = [0x21; 20];
const ERC1155: [u8; 20] = [0x22; 20];

fn deployment_hash(number: u64) -> [u8; 32] {
    [number as u8 ^ 0xf0; 32]
}

fn call_hash(number: u64) -> [u8; 32] {
    [number as u8 ^ 0x70; 32]
}

/// What a block looks like. `salt` changes the block hash and the gas
/// used (a competing block at the same height). A `slim` block only has the
/// failed call (now at index 0) and the ERC20 transfer: what a canonical
/// replacement with FEWER transactions and logs looks like.
#[derive(Debug, Clone, Copy, Default)]
struct Shape {
    salt: u8,
    slim: bool,
}

const FULL: Shape = Shape { salt: 0, slim: false };
/// The canonical replacement of a reorged `FULL` block.
const CANONICAL: Shape = Shape { salt: 0x55, slim: true };

/// A response with one fully populated block per number (< 128):
/// 2 transactions (a deployment and a failed call), 5 logs (ERC20, ERC721
/// of token id 0, ERC1155 batch, anonymous, ERC721 of a huge id) and a
/// withdrawal.
fn response(numbers: &[u64], shape: Shape) -> ResponseRows {
    let salt = shape.salt;
    let mut data = ResponseRows::default();

    for &number in numbers {
        assert!(number < 128);
        let id = number as u8;
        // Unique per (number, salt): `block_lookup` is keyed by hash.
        let mut block_hash = [id; 32];
        block_hash[31] = id ^ salt;

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
    }

    if shape.slim {
        for transactions in &mut data.transactions {
            transactions.retain(|t| t.to.is_some());
            for transaction in transactions {
                transaction.transaction_index = Some(UInt::from(0u64));
            }
        }
        for (logs, number) in data.logs.iter_mut().zip(numbers) {
            logs.retain(|log| log.log_index == Some(UInt::from(0u64)));
            for log in logs {
                log.transaction_hash =
                    Some(Hash::from(call_hash(*number)));
            }
        }
    }

    data
}

/// Rows of blocks `[from, to)`, stamped like a flush would.
fn rows_at(
    chain: u64,
    from: u64,
    to: u64,
    shape: Shape,
    epoch: u32,
) -> RowBatch {
    let numbers: Vec<u64> = (from..to).collect();
    let mut rows = transform(
        chain,
        &response(&numbers, shape),
        BlockRange::new(from, to),
    )
    .unwrap()
    .rows;
    rows.set_version(next_version());
    rows.set_epoch(epoch);
    rows
}

fn rows(chain: u64, from: u64, to: u64) -> RowBatch {
    rows_at(chain, from, to, FULL, 0)
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

/// Rows per `FULL` / `CANONICAL` block produced by [`response`], for
/// every base and side table.
const ROWS_PER_BLOCK: [(&str, u64, u64); 13] = [
    ("blocks", 1, 1),
    ("transactions", 2, 1),
    ("logs", 5, 1),
    ("withdrawals", 1, 1),
    ("erc20_transfers", 1, 1),
    ("erc721_transfers", 2, 0),
    ("erc1155_transfers", 1, 0),
    ("tx_lookup", 2, 1),
    ("block_lookup", 1, 1),
    // Sender + (created contract | recipient), per transaction.
    ("transactions_by_address", 4, 2),
    ("logs_by_address", 5, 1),
    ("erc20_transfers_by_account", 2, 2),
    // ERC721: 2 x 2. ERC1155 batch of two ids: 4.
    ("nft_transfers_by_account", 8, 0),
];

/// Live rows (`FINAL`) of `full` FULL blocks and `slim` CANONICAL blocks.
async fn assert_blocks_stored(database: &Database, full: u64, slim: u64) {
    for (table, per_full, per_slim) in ROWS_PER_BLOCK {
        let expected = per_full * full + per_slim * slim;
        let started = std::time::Instant::now();
        let mut found = count(database, table).await;
        while found != expected && started.elapsed() < SETTLE {
            settle().await;
            found = count(database, table).await;
        }
        if found != expected {
            let column = block_number_column(table);
            let dump = strings(
                database,
                &format!(
                    "SELECT concat(toString({column}), ' v', \
                       toString(_version), ' deleted=', \
                       toString(is_deleted), ' epoch=', toString(epoch), \
                       ' ', _part) FROM {table} \
                     WHERE chain = {} ORDER BY {column}, _version",
                    database.chain_id
                ),
            )
            .await;
            panic!(
                "{table} of chain {}: {found} live rows, expected \
                 {expected}\n{}",
                database.chain_id,
                dump.join("\n"),
            );
        }
    }
}

#[test]
fn the_fixture_covers_every_block_scoped_table() {
    for table in BASE_TABLES.iter().chain(SIDE_TABLES) {
        assert!(
            ROWS_PER_BLOCK.iter().any(|(name, _, _)| name == table),
            "{table}"
        );
    }
    assert_eq!(
        ROWS_PER_BLOCK.len(),
        BASE_TABLES.len() + SIDE_TABLES.len()
    );
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn stores_and_reads_back_every_table() {
    const CHAIN: u64 = 990_001;
    let database = database(CHAIN).await;

    let batch = rows_at(CHAIN, 10, 13, FULL, 3);

    // Token rows are not part of a flush any more: the token worker
    // inserts them through the same insert path.
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

    core::store(&database, &batch).await.unwrap();

    // wait_for_async_insert=1: rows (and what the materialized views derive
    // from them) are visible as soon as store returns.
    assert_blocks_stored(&database, 3, 0).await;
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

    // `contracts` is a view: the successful deployment of every block,
    // nothing for the failed call.
    assert_eq!(
        strings(
            &database,
            "SELECT concat(toString(block_number), ' ', \
               toString(toUnixTimestamp(timestamp)), ' ', \
               lower(hex(contract_address)), ' ', lower(hex(creator)), ' ', \
               lower(hex(transaction_hash))) \
             FROM contracts WHERE chain = 990001 ORDER BY block_number",
        )
        .await,
        (10..13u64)
            .map(|number| format!(
                "{number} {} {} {} {}",
                timestamp_of(number),
                hex::encode([number as u8 | 0x80; 20]),
                hex::encode(SENDER),
                hex::encode(deployment_hash(number)),
            ))
            .collect::<Vec<_>>()
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

    // Written as data: the epoch of the flush, never a tombstone.
    for (table, _, _) in ROWS_PER_BLOCK {
        assert_eq!(
            strings(
                &database,
                &format!(
                    "SELECT DISTINCT concat(toString(epoch), ' ', \
                       toString(is_deleted)) FROM {table} \
                     WHERE chain = 990001"
                ),
            )
            .await,
            vec!["3 0"],
            "{table}"
        );
    }

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

    core::store(&database, &rows(CHAIN, 20, 22)).await.unwrap();

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
    core::store(&database, &rows(CHAIN, 4, 6)).await.unwrap();
    core::store(&database, &rows(CHAIN, 6, 9)).await.unwrap();

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

    // Real timestamps (2023, not 1970).
    assert_eq!(
        strings(
            &database,
            "SELECT toString(day) FROM daily_block_stats_v \
             WHERE chain = 990004 ORDER BY day",
        )
        .await,
        vec!["2023-11-14 00:00:00", "2023-11-15 00:00:00"]
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
    core::store(&database, &batch).await.unwrap();

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

    // The rebuild path obeys the same rule: hide what the views wrote
    // (epoch 0) behind a repair at epoch 1 and re-aggregate.
    // `to_ts` is the exclusive end of the window the row hides, and the
    // rebuild covers exactly that window: reaching past it would double
    // count, stopping short of it would zero a bucket for ever.
    const REPAIR_END: u32 = DAY_2 + 86_400;
    execute(
        &database,
        &format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
               fork_block, old_head, depth, rows_tombstoned, reason) \
             VALUES ({CHAIN}, 1, {DAY_1}, {REPAIR_END}, 0, 0, 0, 0, \
               'gap_heal')"
        ),
    )
    .await;
    for table in CORE_DERIVED {
        let sql = table.rebuild_slice(
            CHAIN,
            DAY_1,
            REPAIR_END,
            1,
            u64::MAX,
            None,
        );
        execute(&database, &sql).await;
    }
    check("rebuild_sql").await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reinserted_block_replaces_itself_under_final() {
    const CHAIN: u64 = 990_005;
    let database = database(CHAIN).await;

    core::store(&database, &rows(CHAIN, 30, 32)).await.unwrap();
    assert_blocks_stored(&database, 2, 0).await;

    // The same heights again, later (higher _version), different content.
    let again =
        rows_at(CHAIN, 30, 32, Shape { salt: 0x55, slim: false }, 0);
    assert_ne!(again.blocks[0].hash, rows(CHAIN, 30, 31).blocks[0].hash);
    core::store(&database, &again).await.unwrap();

    // Both copies exist until a merge gets to them (which can be any
    // time) ...
    assert!(raw_count(&database, "blocks").await >= 2);

    // ... but FINAL sees ONE row per key, in every base and side table,
    // and it is the later one.
    for (table, per_block, _) in ROWS_PER_BLOCK {
        // block_lookup is keyed by hash, and the hash changed: the stale
        // entry is what a purge tombstones (through the view of blocks).
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
    for (table, _, _) in ROWS_PER_BLOCK {
        execute(&database, &format!("OPTIMIZE TABLE {table} FINAL")).await;
    }
    assert_eq!(raw_count(&database, "blocks").await, 2);
    assert_eq!(raw_count(&database, "logs").await, 10);
    assert_eq!(raw_count(&database, "tx_lookup").await, 4);
}

/// ClickHouse gives no read-your-writes guarantee right after an INSERT
/// returns: seen on 25.12 with concurrent writers, the part can stay
/// invisible to the next query for a few ms (plain MergeTree, same
/// connection, no async insert). So whatever is asserted right after a
/// write is read until it holds, for at most [`SETTLE`].
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

/// Tombstones `[from_block, to_block)` of `table` and verifies it, the way
/// a purge has to: the statement is re-issued until no live row is left (a
/// tombstone INSERT ... SELECT issued right after a flush may not see the
/// flushed rows yet). Idempotent, no lock.
async fn tombstone(
    database: &Database,
    table: &str,
    from_block: u64,
    to_block: Option<u64>,
) {
    let chain = database.chain_id;
    let started = std::time::Instant::now();

    loop {
        let sql = tombstone_sql(
            table,
            chain,
            from_block,
            to_block,
            next_version(),
        )
        .unwrap();
        execute(database, &sql).await;

        let alive: u64 = database
            .db
            .query(&live_rows_sql(table, chain, from_block, to_block))
            .fetch_one()
            .await
            .unwrap();

        if alive == 0 {
            return;
        }
        assert!(
            started.elapsed() < SETTLE,
            "{table}: {alive} rows of chain {chain} survive their tombstones"
        );
        settle().await;
    }
}

/// What `purge_range` does (docs/design.md, section 2), with nothing but
/// INSERTs: tombstone the children, record the purge, repair the
/// aggregates under the new epoch, tombstone `blocks` last.
async fn purge(
    database: &Database,
    from_block: u64,
    to_block: Option<u64>,
    epoch: u32,
    reason: &str,
) {
    let chain = database.chain_id;

    // Dead rows count too: a re-run after a crash finds the same window.
    // Both ends: `from_ts` is the first bucket the repair covers, `to_ts`
    // the first one past it. The validity rule hides exactly that window,
    // so a purge deep in history does NOT touch the buckets after it.
    let mut min_timestamp = u32::MAX;
    let mut max_timestamp = 0u32;
    for table in BASE_TABLES {
        let (found, newest): (u32, u32) = database
            .db
            .query(
                &min_timestamp_sql(table, chain, from_block, to_block)
                    .replace(
                        "SELECT toUInt32(min(timestamp))",
                        "SELECT toUInt32(min(timestamp)), \
                     toUInt32(max(timestamp))",
                    ),
            )
            .fetch_one()
            .await
            .unwrap();
        if found > 0 {
            min_timestamp = min_timestamp.min(found);
            max_timestamp = max_timestamp.max(newest);
        }
    }
    assert_ne!(min_timestamp, u32::MAX, "nothing to purge");
    let from_ts = repair_start(min_timestamp);
    let to_ts = repair_start(max_timestamp) + 86_400;

    let (blocks, children) = BASE_TABLES.split_last().unwrap();
    assert_eq!(*blocks, "blocks");

    for table in children {
        tombstone(database, table, from_block, to_block).await;
    }

    execute(
        database,
        &format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
               fork_block, old_head, depth, rows_tombstoned, reason) \
             VALUES ({chain}, {epoch}, {from_ts}, {to_ts}, {from_block}, \
               0, 0, 0, '{reason}')"
        ),
    )
    .await;

    for table in CORE_DERIVED {
        let sql = table.rebuild_slice(
            chain, from_ts, to_ts, epoch, from_block, to_block,
        );
        execute(database, &sql).await;
    }

    tombstone(database, blocks, from_block, to_block).await;
}

/// `(name, rows, content hash)` of everything a reader can see of a chain:
/// every base and side table under `FINAL`, every `*_v` view. The
/// bookkeeping columns (chain, epoch, _version) are not content.
async fn snapshot(database: &Database) -> Vec<(String, u64, u64)> {
    let chain = database.chain_id;
    let mut snapshot = Vec::new();

    let mut sources: Vec<(String, &str, &str)> = BASE_TABLES
        .iter()
        .chain(SIDE_TABLES)
        .map(|table| {
            (
                table.to_string(),
                " FINAL",
                "tuple(* EXCEPT (chain, epoch, _version, is_deleted))",
            )
        })
        .collect();
    sources.push(("contracts".to_string(), "", "tuple(* EXCEPT (chain))"));
    for table in CORE_DERIVED {
        sources.push((
            format!("{}_v", table.name),
            "",
            "tuple(* EXCEPT (chain))",
        ));
    }

    for (name, modifier, columns) in sources {
        let (rows, hash): (u64, u64) = database
            .db
            .query(&format!(
                "SELECT count(), sum(cityHash64({columns})) \
                 FROM {name}{modifier} WHERE chain = {chain}"
            ))
            .fetch_one()
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        snapshot.push((name, rows, hash));
    }

    snapshot
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn tombstones_reach_every_side_table_with_exactly_the_same_keys() {
    const CHAIN: u64 = 990_006;
    let database = database(CHAIN).await;

    core::store(&database, &rows(CHAIN, 40, 46)).await.unwrap();
    assert_blocks_stored(&database, 6, 0).await;

    // Tombstones go into the BASE tables only, children first.
    for table in BASE_TABLES {
        tombstone(&database, table, 43, None).await;
    }

    // 40, 41, 42 survive everywhere, nothing at or above 43 does - in the
    // side tables too, which nobody wrote to: the views carried the
    // tombstones over, for both rows of a transaction / transfer and for
    // every id of an ERC1155 batch.
    assert_blocks_stored(&database, 3, 0).await;

    for (table, per_block, _) in ROWS_PER_BLOCK {
        let column = block_number_column(table);

        assert_eq!(
            count_where(&database, table, &format!("{column} >= 43"))
                .await,
            0,
            "{table}"
        );

        // Exactly the same keys. A tombstone is the row it kills but for
        // two columns, so by content: every row in range has its
        // tombstone (a missed key would also still be alive above), and
        // there are as many distinct tombstones as there were rows (one
        // with another key would be an extra one). The data rows
        // themselves may already have been merged away.
        let (without_tombstone, tombstones): (u64, u64) = database
            .db
            .query(&format!(
                "SELECT countIf(dead = 0), countIf(dead > 0) FROM ( \
                   SELECT cityHash64(tuple(* EXCEPT (_version, \
                     is_deleted))) AS h, sum(is_deleted) AS dead \
                   FROM {table} WHERE chain = {CHAIN} AND {column} >= 43 \
                   GROUP BY h)"
            ))
            .fetch_one()
            .await
            .unwrap();
        assert_eq!(without_tombstone, 0, "{table}");
        assert_eq!(tombstones, per_block * 3, "{table}");
    }
    assert_eq!(count(&database, "contracts").await, 3);

    // Idempotent: what is dead is not tombstoned again.
    let tombstone_rows = |table: &'static str| {
        let database = &database;
        async move {
            database
                .db
                .query(&format!(
                    "SELECT countIf(is_deleted = 1) FROM {table} \
                     WHERE chain = {CHAIN}"
                ))
                .fetch_one::<u64>()
                .await
                .unwrap()
        }
    };
    let mut before = Vec::new();
    for (table, _, _) in ROWS_PER_BLOCK {
        before.push(tombstone_rows(table).await);
    }
    for table in BASE_TABLES {
        let sql =
            tombstone_sql(table, CHAIN, 43, None, next_version()).unwrap();
        execute(&database, &sql).await;
    }
    for ((table, _, _), before) in ROWS_PER_BLOCK.iter().zip(before) {
        // (A merge may have folded duplicates in the meantime.)
        assert!(tombstone_rows(table).await <= before, "{table}");
    }

    // Bounded variant (gap healing).
    for table in BASE_TABLES {
        tombstone(&database, table, 41, Some(42)).await;
    }
    assert_blocks_stored(&database, 2, 0).await;
    assert_eq!(database.block_hash(41).await.unwrap(), None);
    assert!(database.block_hash(42).await.unwrap().is_some());

    // The purged range can be streamed again: same keys, newer version.
    core::store(&database, &rows(CHAIN, 41, 42)).await.unwrap();
    core::store(&database, &rows(CHAIN, 43, 46)).await.unwrap();
    assert_blocks_stored(&database, 6, 0).await;

    // And it all survives the merges.
    for (table, _, _) in ROWS_PER_BLOCK {
        execute(&database, &format!("OPTIMIZE TABLE {table} FINAL")).await;
    }
    assert_blocks_stored(&database, 6, 0).await;
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn tombstone_columns_are_the_columns_of_the_live_tables() {
    let database = database(990_011).await;

    for table in BASE_TABLES {
        let sql = tombstone_sql(table, 1, 0, None, 1).unwrap();

        let live = strings(
            &database,
            &format!(
                "SELECT concat('`', name, '`') FROM system.columns \
                 WHERE database = currentDatabase() AND table = '{table}' \
                 ORDER BY position"
            ),
        )
        .await;

        assert!(
            sql.starts_with(&format!(
                "INSERT INTO `{table}` ({}) SELECT ",
                live.join(", ")
            )),
            "{table}: {sql}"
        );
    }
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn the_validity_rule_decides_which_epochs_a_view_counts() {
    let database = database(990_012).await;

    const D1: u32 = DAY_1;
    const D2: u32 = DAY_1 + 86_400;
    const D3: u32 = DAY_1 + 2 * 86_400;
    const D4: u32 = DAY_1 + 3 * 86_400;
    const D5: u32 = DAY_1 + 4 * 86_400;

    // (chain, contributions (day, epoch, blocks),
    //  reorgs (epoch, from_ts, to_ts), expected (day, blocks) of the view)
    //
    // A `reorgs` row hides the older epochs of the buckets in
    // [from_ts, to_ts) and NOTHING else: that window is what the purge
    // rebuilt, because its rows only ever contributed to those buckets.
    type Case = (
        &'static str,
        u64,
        &'static [(u32, u32, u64)],
        &'static [(u32, u32, u32)],
        &'static [(u32, u64)],
    );

    let cases: [Case; 15] = [
        (
            "no reorgs: every epoch counts",
            990_101,
            &[(D1, 0, 10), (D1, 5, 3), (D2, 0, 20)],
            &[],
            &[(D1, 13), (D2, 20)],
        ),
        (
            "a bucket before from_ts keeps its old epochs, buckets at and \
             after it ignore them",
            990_102,
            &[(D1, 0, 10), (D2, 0, 20), (D2, 1, 21), (D3, 0, 30), (D3, 1, 31)],
            &[(1, D2, D3 + 86_400)],
            &[(D1, 10), (D2, 21), (D3, 31)],
        ),
        (
            "a repaired bucket nobody rebuilt shows nothing, not stale data",
            990_103,
            &[(D1, 0, 10), (D2, 0, 20)],
            &[(1, D2, D3 + 86_400)],
            &[(D1, 10)],
        ),
        (
            "two successive repairs",
            990_104,
            &[
                (D1, 0, 10),
                (D2, 0, 20),
                (D2, 1, 21),
                (D3, 0, 30),
                (D3, 1, 31),
                (D3, 2, 32),
            ],
            &[(1, D2, D3 + 86_400), (2, D3, D3 + 86_400)],
            &[(D1, 10), (D2, 21), (D3, 32)],
        ),
        (
            "a later repair reaching further back raises the bar for every \
             bucket after it",
            990_105,
            &[
                (D1, 0, 10),
                (D2, 0, 20),
                (D2, 2, 22),
                (D3, 0, 30),
                (D3, 1, 31),
                (D3, 2, 32),
            ],
            &[(1, D3, D3 + 86_400), (2, D2, D3 + 86_400)],
            &[(D1, 10), (D2, 22), (D3, 32)],
        ),
        (
            "an abandoned partial epoch (repair 1 crashed half way, repair \
             2 completed) is invisible",
            990_106,
            &[(D2, 0, 20), (D2, 1, 7), (D2, 2, 21), (D3, 0, 30), (D3, 2, 31)],
            &[(1, D2, D3 + 86_400), (2, D2, D3 + 86_400)],
            &[(D2, 21), (D3, 31)],
        ),
        (
            "a gap heal writing epoch 5 rows into an old bucket ADDS to its \
             epoch 0 rows",
            990_107,
            &[(D1, 0, 10), (D1, 5, 3), (D3, 0, 30), (D3, 5, 35)],
            &[(5, D3, D3 + 86_400)],
            &[(D1, 13), (D3, 35)],
        ),
        (
            "rows streamed after a repair carry its epoch (or a later one) \
             and add to it",
            990_108,
            &[(D2, 0, 20), (D2, 1, 21), (D2, 1, 4), (D2, 3, 5), (D3, 3, 9)],
            &[(1, D2, D3 + 86_400)],
            &[(D2, 30), (D3, 9)],
        ),
        (
            "the reorgs of another chain do not matter",
            990_109,
            &[(D1, 0, 10), (D2, 0, 20)],
            // Recorded for chain 990_102 above, epoch 1 from D2.
            &[],
            &[(D1, 10), (D2, 20)],
        ),
        // --- the bounded window (a `to_ts` that is not the end of time)
        (
            "a tip reorg hides the day it repaired and nothing before it",
            990_110,
            &[(D1, 0, 10), (D2, 0, 20), (D3, 0, 30), (D3, 1, 31)],
            &[(1, D3, D3 + 86_400)],
            &[(D1, 10), (D2, 20), (D3, 31)],
        ),
        (
            "a deep gap heal does NOT hide the later buckets it never \
             rebuilt: they keep their older epochs",
            990_111,
            &[
                (D1, 0, 10),
                (D1, 1, 11),
                (D2, 0, 20),
                (D3, 0, 30),
                (D5, 0, 50),
            ],
            // Repaired the first day only.
            &[(1, D1, D2)],
            &[(D1, 11), (D2, 20), (D3, 30), (D5, 50)],
        ),
        (
            "overlapping repairs: the older, WIDER window keeps its floor \
             where the newer, narrower one did not reach",
            990_112,
            &[
                (D1, 0, 10),
                (D1, 1, 11),
                (D2, 1, 21),
                (D2, 2, 22),
                (D3, 0, 30),
                (D3, 1, 31),
                (D4, 0, 40),
            ],
            // 1 repaired D1..D3, 2 (later, deeper in history) only D2.
            &[(1, D1, D4), (2, D2, D3)],
            &[(D1, 11), (D2, 22), (D3, 31), (D4, 40)],
        ),
        (
            "an abandoned partial epoch inside a bounded window is \
             invisible, and buckets past the window are untouched",
            990_113,
            &[
                (D2, 0, 20),
                // epoch 1 died half way through its rebuild
                (D2, 1, 7),
                (D2, 2, 21),
                (D3, 0, 30),
            ],
            // The re-run computes the SAME window (timestamps survive in
            // the tombstones), so nothing of epoch 1 can leak.
            &[(1, D2, D3), (2, D2, D3)],
            &[(D2, 21), (D3, 30)],
        ),
        (
            "a chain of its own is not affected by any of the windows \
             above",
            990_114,
            &[(D1, 0, 10), (D2, 0, 20), (D3, 0, 30), (D4, 0, 40)],
            &[],
            &[(D1, 10), (D2, 20), (D3, 30), (D4, 40)],
        ),
        (
            "two windows that touch make one run of days, and the floor \
             still drops to 0 after the last of them",
            990_115,
            &[
                (D1, 0, 10),
                (D1, 1, 11),
                (D2, 0, 20),
                (D2, 2, 22),
                (D3, 0, 30),
            ],
            &[(1, D1, D2), (2, D2, D3)],
            &[(D1, 11), (D2, 22), (D3, 30)],
        ),
    ];

    for (_, chain, contributions, reorgs, _) in cases {
        // One INSERT per contribution: separate parts, like the views and
        // the rebuilds produce them.
        for (day, epoch, blocks) in contributions {
            execute(
                &database,
                &format!(
                    "INSERT INTO daily_block_stats (chain, day, epoch, \
                       blocks) VALUES ({chain}, {day}, {epoch}, {blocks})"
                ),
            )
            .await;
        }
        for (epoch, from_ts, to_ts) in reorgs {
            execute(
                &database,
                &format!(
                    "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
                       fork_block, old_head, depth, rows_tombstoned, \
                       reason) \
                     VALUES ({chain}, {epoch}, {from_ts}, {to_ts}, 0, 0, \
                       0, 0, 'reorg')"
                ),
            )
            .await;
        }
    }

    for (name, chain, _, _, expected) in cases {
        let found: Vec<(u32, u64)> = database
            .db
            .query(&format!(
                "SELECT toUnixTimestamp(day), blocks \
                 FROM daily_block_stats_v WHERE chain = {chain} ORDER BY day"
            ))
            .fetch_all()
            .await
            .unwrap();

        assert_eq!(found, expected, "{name}");
    }

    // Filtering the view by day (what a reader does) changes nothing.
    let found: Vec<(u32, u64)> = database
        .db
        .query(&format!(
            "SELECT toUnixTimestamp(day), blocks FROM daily_block_stats_v \
             WHERE chain = 990104 AND day >= {D3}"
        ))
        .fetch_all()
        .await
        .unwrap();
    assert_eq!(found, vec![(D3, 32)]);
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn rebuild_sql_reproduces_what_the_views_wrote() {
    const CHAIN: u64 = 990_007;
    let database = database(CHAIN).await;

    // Two days, several flushes.
    core::store(&database, &rows(CHAIN, 3, 5)).await.unwrap();
    core::store(&database, &rows(CHAIN, 5, 8)).await.unwrap();
    core::store(&database, &rows(CHAIN, 8, 10)).await.unwrap();

    let mut written_by_the_views = Vec::new();
    for table in CORE_DERIVED {
        let rows = view_rows(&database, table.name).await;
        assert!(rows.len() >= 2, "{}", table.name);
        written_by_the_views.push(rows);
    }

    // `to_ts` = the end of the window this repair covers, exclusive.
    let repair = |epoch: u32, from_ts: u32, to_ts: u32| {
        let database = &database;
        async move {
            execute(
                database,
                &format!(
                    "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
                       fork_block, old_head, depth, rows_tombstoned, \
                       reason) \
                     VALUES ({CHAIN}, {epoch}, {}, {to_ts}, 0, 0, 0, 0, \
                       'gap_heal')",
                    repair_start(from_ts)
                ),
            )
            .await;
        }
    };
    const BOTH_DAYS: u32 = DAY_2 + 86_400;

    // Epoch 1 from any timestamp inside the first day: everything the
    // views wrote (epoch 0) is hidden ...
    repair(1, DAY_1 + 80_000, BOTH_DAYS).await;
    for table in CORE_DERIVED {
        assert!(
            view_rows(&database, table.name).await.is_empty(),
            "{}",
            table.name
        );
    }

    // ... and the rebuild brings exactly the same numbers back. (Nothing
    // was purged, so no block range is excluded.)
    for (table, expected) in CORE_DERIVED.iter().zip(&written_by_the_views)
    {
        let sql = table.rebuild_slice(
            CHAIN,
            DAY_1 + 80_000,
            BOTH_DAYS,
            1,
            u64::MAX,
            None,
        );
        execute(&database, &sql).await;
        assert_eq!(
            &view_rows(&database, table.name).await,
            expected,
            "{}",
            table.name
        );
    }

    // Repairing only the second day leaves the first one alone.
    repair(2, DAY_2 + 5, BOTH_DAYS).await;
    for (table, expected) in CORE_DERIVED.iter().zip(&written_by_the_views)
    {
        assert_eq!(
            view_rows(&database, table.name).await.len(),
            expected.len() / 2,
            "{}",
            table.name
        );

        let sql = table.rebuild_slice(
            CHAIN,
            DAY_2 + 5,
            BOTH_DAYS,
            2,
            u64::MAX,
            None,
        );
        execute(&database, &sql).await;
        assert_eq!(
            &view_rows(&database, table.name).await,
            expected,
            "{}",
            table.name
        );
    }
}

/// A reorg, end to end, through the real tables: index `[first, first +
/// 8)`, find out that everything from `first + 5` on was not canonical
/// (the canonical blocks have FEWER transactions and logs), purge, stream
/// the canonical blocks under the new epoch. Afterwards everything a
/// reader can see must equal `clean`, a chain that only ever saw the
/// canonical blocks.
async fn reorg_scenario(
    database: &Database,
    clean: &Database,
    first: u64,
    epoch: u32,
) {
    let chain = database.chain_id;
    let fork = first + 5;
    let end = first + 8;

    core::store(clean, &rows(clean.chain_id, first, fork)).await.unwrap();
    core::store(clean, &rows_at(clean.chain_id, fork, end, CANONICAL, 0))
        .await
        .unwrap();

    // Rows streamed before the purge carry the previous epoch.
    core::store(
        database,
        &rows_at(chain, first, first + 3, FULL, epoch - 1),
    )
    .await
    .unwrap();
    core::store(
        database,
        &rows_at(chain, first + 3, end, FULL, epoch - 1),
    )
    .await
    .unwrap();

    // A purge reads what was flushed: wait until the last flush is
    // visible (see [`SETTLE`]; the pipeline has to do the same).
    let started = std::time::Instant::now();
    for (table, per_block, _) in ROWS_PER_BLOCK {
        if !BASE_TABLES.contains(&table) {
            continue;
        }
        loop {
            let visible: u64 = database
                .db
                .query(&live_rows_sql(table, chain, first, Some(end)))
                .fetch_one()
                .await
                .unwrap();
            if visible == per_block * 8 {
                break;
            }
            assert!(started.elapsed() < SETTLE, "{table} of {chain}");
            settle().await;
        }
    }

    purge(database, fork, None, epoch, "reorg").await;

    // Verified by the purge itself, table by table.
    assert_eq!(database.block_hash(fork).await.unwrap(), None, "{chain}");
    assert_eq!(
        database
            .missing_ranges(BlockRange::new(first, end))
            .await
            .unwrap()
            .ranges,
        vec![BlockRange::new(fork, end)],
        "{chain}"
    );

    core::store(
        database,
        &rows_at(chain, fork, fork + 1, CANONICAL, epoch),
    )
    .await
    .unwrap();
    core::store(
        database,
        &rows_at(chain, fork + 1, end, CANONICAL, epoch),
    )
    .await
    .unwrap();

    let started = std::time::Instant::now();
    loop {
        let (found, expected) =
            (snapshot(database).await, snapshot(clean).await);
        if found == expected {
            break;
        }
        if started.elapsed() > SETTLE {
            assert_eq!(
                found, expected,
                "chain {chain}, blocks {first}..{end}, epoch {epoch}"
            );
        }
        settle().await;
    }
}

#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_reorg_is_repaired_with_inserts_only() {
    let database = database(990_008).await;
    let clean = self::database(990_009).await;

    // Crosses the day boundary: blocks 2-6 are day 1, 7-9 day 2.
    reorg_scenario(&database, &clean, 2, 1).await;

    // What "equal to a clean index" means, spelled out once.
    assert_blocks_stored(&database, 5, 3).await;
    assert_eq!(count(&database, "contracts").await, 5);

    let stats: Vec<(u32, u64, u64)> = database
        .db
        .query(
            "SELECT toUnixTimestamp(day), blocks, transactions \
             FROM daily_block_stats_v WHERE chain = 990008 ORDER BY day",
        )
        .fetch_all()
        .await
        .unwrap();
    // The fork (block 7) is the first block of day 2: day 1 untouched.
    assert_eq!(stats, vec![(DAY_1, 5, 10), (DAY_2, 3, 3)]);

    // The reorged-out deployment is gone from the lookup, the call that
    // made it into the canonical block points at its new position.
    assert_eq!(
        strings(
            &database,
            &format!(
                "SELECT concat(toString(block_number), ':', \
                   toString(transaction_index)) FROM tx_lookup FINAL \
                 WHERE chain = 990008 AND hash IN (unhex('{}'), unhex('{}'))",
                hex::encode(deployment_hash(8)),
                hex::encode(call_hash(8))
            ),
        )
        .await,
        vec!["8:0"]
    );

    // A second, deeper reorg on top: epochs keep working.
    reorg_scenario(&database, &clean, 20, 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "needs TEST_DATABASE_URL"]
async fn eight_chains_reorg_concurrently_without_any_coordination() {
    // The 50 chain scenario, and the reason nothing is ever deleted:
    // ClickHouse loses concurrent DELETEs on one table, concurrent INSERTs
    // it does not. No lock, no retry, no verification loop in here.
    let mut chains = Vec::new();
    for index in 0..8u64 {
        chains.push((
            database(990_200 + index).await,
            database(990_300 + index).await,
        ));
    }

    for round in 0..10u64 {
        let scenarios = chains.iter().map(|(database, clean)| {
            reorg_scenario(database, clean, round * 12, round as u32 + 1)
        });

        futures::future::join_all(scenarios).await;
    }

    for (database, _) in &chains {
        assert_blocks_stored(database, 50, 30).await;
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
    core::store(&database, &rows(990_002, 5, 10)).await.unwrap();
    core::store(&database, &rows(990_002, 20, 30)).await.unwrap();
    core::store(&database, &rows(990_002, 20, 30)).await.unwrap();
    core::store(&database, &rows(990_002, 40, 41)).await.unwrap();

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
        core::store(&database, &rows(990_002, from, to)).await.unwrap();
    }
    let missing = database.missing_ranges(whole).await.unwrap();
    assert!(missing.ranges.is_empty());

    // A tombstoned block is a missing block: it has to be streamed again.
    let sql =
        tombstone_sql("blocks", 990_002, 50, Some(60), next_version())
            .unwrap();
    execute(&database, &sql).await;

    let missing = database.missing_ranges(whole).await.unwrap();
    assert_eq!(missing.ranges, vec![BlockRange::new(50, 60)]);
    assert_eq!(database.block_hash(55).await.unwrap(), None);
    assert!(database.block_hash(49).await.unwrap().is_some());

    // The whole tail gone: the highest LIVE block is what counts.
    let sql = tombstone_sql("blocks", 990_002, 90, None, next_version())
        .unwrap();
    execute(&database, &sql).await;

    let missing = database.missing_ranges(whole).await.unwrap();
    assert_eq!(
        missing.ranges,
        vec![BlockRange::new(50, 60), BlockRange::new(90, 100)]
    );
}

/// A flush that landed while ANOTHER process's purge was rebuilding the
/// same days carries an epoch the validity rule now hides, and nothing
/// else will ever ask for those blocks again: their rows ARE stored, so no
/// gap query reports them. The running indexer queues them in memory; this
/// is the same question asked of the database, so a restart does not lose
/// them (review round 4, MAJOR 3).
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn a_flush_that_raced_another_purge_is_found_again_after_a_restart()
{
    const CHAIN: u64 = 990_013;

    let database = database(CHAIN).await;

    // Nothing has ever been purged: nothing to look for.
    assert!(database.stale_flush_ranges().await.unwrap().is_empty());

    core::store(&database, &rows_at(CHAIN, 0, 8, FULL, 0)).await.unwrap();

    // Another process purges and rebuilds every bucket of both days
    // under epoch 1; `tombstone_version` is what it stamped BEFORE the
    // rebuild read its input.
    let tombstoned = next_version();
    execute(
        &database,
        &format!(
            "INSERT INTO reorgs (chain, epoch, from_ts, to_ts, \
               fork_block, to_block, old_head, depth, rows_tombstoned, \
               reason, tombstone_version, completed) \
             VALUES ({CHAIN}, 1, {DAY_1}, {}, 0, 4, 0, 0, 0, \
               'redecode', {tombstoned}, 1)",
            DAY_2 + 86_400
        ),
    )
    .await;

    // Still nothing: every stored block was written BEFORE the rebuild.
    assert!(database.stale_flush_ranges().await.unwrap().is_empty());

    // Now the flush that raced it: written after the rebuild, still
    // stamped with the old epoch.
    core::store(&database, &rows_at(CHAIN, 8, 12, FULL, 0)).await.unwrap();

    assert_eq!(
        database.stale_flush_ranges().await.unwrap(),
        vec![BlockRange::new(8, 12)],
        "the blocks flushed under the superseded epoch have to be \
         purged and indexed again"
    );

    // Written again under the epoch in force: the question answers
    // itself, so a restart loop is impossible.
    core::store(&database, &rows_at(CHAIN, 8, 12, FULL, 1)).await.unwrap();

    assert!(
        database.stale_flush_ranges().await.unwrap().is_empty(),
        "a range re-indexed under the epoch in force is not stale"
    );
}

/// Checkpoint compaction reads a bounded page, so successive passes have
/// to SWEEP the table. Always reading the lowest rows meant that a chain
/// with more non-contiguous live ranges than one page holds (a partial
/// backfill: holes everywhere, nothing to merge down there) never reached
/// the head's fast growing contiguous run, and the table grew without
/// bound (review round 4, MINOR 15).
#[tokio::test]
#[ignore = "needs TEST_DATABASE_URL"]
async fn checkpoint_compaction_sweeps_past_a_page_of_holes() {
    use super::ranges::MAX_CHECKPOINTS_PER_COMPACTION;

    const CHAIN: u64 = 990_014;
    const HEAD: u64 = 10_000_000;
    const RUN: u64 = 400;

    let database = database(CHAIN).await;

    // A full page of live ranges with a hole between each pair: nothing
    // to merge, and reading them again changes nothing.
    let holes = MAX_CHECKPOINTS_PER_COMPACTION as u64 + 10;
    let mut rows: Vec<DatabaseCheckpoint> = (0..holes)
        .map(|i| DatabaseCheckpoint {
            chain: CHAIN,
            from_block: i * 10,
            to_block: i * 10 + 1,
            epoch: 0,
            _version: next_version(),
        })
        .collect();

    // The head: one contiguous run, one row per flush.
    rows.extend((0..RUN).map(|i| DatabaseCheckpoint {
        chain: CHAIN,
        from_block: HEAD + i,
        to_block: HEAD + i + 1,
        epoch: 0,
        _version: next_version(),
    }));

    for page in rows.chunks(500) {
        database.insert_rows("checkpoints", page).await.unwrap();
    }

    let covered = format!(
        "SELECT toUInt64(count()) FROM checkpoints FINAL WHERE chain = \
         {CHAIN} AND is_deleted = 0 AND from_block = {HEAD} AND \
         to_block = {}",
        HEAD + RUN
    );

    // A handful of passes is enough to sweep past the page of holes and
    // collapse the head's run into one covering row.
    let mut swept = false;
    for _ in 0..6 {
        database.compact_checkpoints().await.unwrap();
        if database.db.query(&covered).fetch_one::<u64>().await.unwrap()
            > 0
        {
            swept = true;
            break;
        }
    }

    assert!(
        swept,
        "the head's contiguous run was never reached: the compaction \
         keeps re-reading the lowest rows"
    );

    // The holes are untouched: the union of the live ranges never changes.
    let live_holes: u64 = database
        .db
        .query(&format!(
            "SELECT toUInt64(count()) FROM checkpoints FINAL WHERE chain \
             = {CHAIN} AND is_deleted = 0 AND from_block < {HEAD}"
        ))
        .fetch_one()
        .await
        .unwrap();
    assert_eq!(live_holes, holes);
}
