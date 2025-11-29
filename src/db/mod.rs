pub mod models;

use clickhouse::{Client, Row};
use futures::future::join_all;
use log::{error, info};
use models::{
    block::DatabaseBlock, contract::DatabaseContract,
    dex_trade::DatabaseDexTrade, log::DatabaseLog, token::DatabaseToken,
    trace::DatabaseTrace, transaction::DatabaseTransaction,
    withdrawal::DatabaseWithdrawal,
};
use serde::Serialize;
use std::collections::HashSet;

use self::models::{
    dex_liquidity_update::DatabaseDexLiquidityUpdate,
    dex_pair::DatabaseDexPair, erc1155_transfer::DatabaseERC1155Transfer,
    erc20_transfer::DatabaseERC20Transfer,
    erc721_transfer::DatabaseERC721Transfer,
};

pub struct BlockFetchedData {
    pub blocks: Vec<DatabaseBlock>,
    pub contracts: Vec<DatabaseContract>,
    pub logs: Vec<DatabaseLog>,
    pub traces: Vec<DatabaseTrace>,
    pub transactions: Vec<DatabaseTransaction>,
    pub withdrawals: Vec<DatabaseWithdrawal>,
    pub erc20_transfers: Vec<DatabaseERC20Transfer>,
    pub erc721_transfers: Vec<DatabaseERC721Transfer>,
    pub erc1155_transfers: Vec<DatabaseERC1155Transfer>,
    pub dex_trades: Vec<DatabaseDexTrade>,
    pub dex_pairs: Vec<DatabaseDexPair>,
    pub dex_liquidity_updates: Vec<DatabaseDexLiquidityUpdate>,
    pub tokens: Vec<DatabaseToken>,
}

#[derive(Clone)]
pub struct Database {
    pub chain_id: u64,
    pub db: Client,
}

pub enum DatabaseTables {
    Blocks,
    Contracts,
    Logs,
    Traces,
    Transactions,
    Withdrawals,
    Erc20Transfers,
    Erc721Transfers,
    Erc1155Transfers,
    DexTrades,
    DexPairs,
    DexLiquidityUpdates,
    Tokens,
}

impl DatabaseTables {
    pub fn as_str(&self) -> &'static str {
        match self {
            DatabaseTables::Blocks => "blocks",
            DatabaseTables::Contracts => "contracts",
            DatabaseTables::Logs => "logs",
            DatabaseTables::Traces => "traces",
            DatabaseTables::Transactions => "transactions",
            DatabaseTables::Withdrawals => "withdrawals",
            DatabaseTables::Erc20Transfers => "erc20_transfers",
            DatabaseTables::Erc721Transfers => "erc721_transfers",
            DatabaseTables::Erc1155Transfers => "erc1155_transfers",
            DatabaseTables::DexTrades => "dex_trades",
            DatabaseTables::DexPairs => "dex_pairs",
            DatabaseTables::DexLiquidityUpdates => "dex_liquidity_updates",
            DatabaseTables::Tokens => "tokens",
        }
    }
}

impl Database {
    pub async fn new(database_url: &str, chain_id: u64) -> Self {
        info!("Connecting to database: {}", database_url);

        // Parse the database URL to extract components
        let url = url::Url::parse(database_url)
            .expect("Failed to parse database URL. Expected format: clickhouse://user:password@host:port/database");

        let host = url.host_str().expect("No host in database URL");
        let port = url.port().unwrap_or(9000);
        let username = url.username();
        let password = url.password().unwrap_or("");
        let database = url.path().trim_start_matches('/');

        let db = clickhouse::Client::default()
            .with_url(format!("{}://{}:{}", url.scheme(), host, port))
            .with_user(username)
            .with_password(password)
            .with_database(database);

        // Retry connection test up to 10 times with exponential backoff
        let mut retries = 0;
        let max_retries = 10;

        loop {
            match db.query("SELECT 1").fetch_one::<u8>().await {
                Ok(_) => {
                    info!("Successfully connected to ClickHouse database '{}'", database);
                    break;
                }
                Err(e) => {
                    retries += 1;
                    if retries >= max_retries {
                        error!("Failed to connect to database after {} attempts: {}", max_retries, e);
                        panic!("Could not connect to ClickHouse. Please check your database configuration and ensure ClickHouse is running.");
                    }

                    let wait_time = std::time::Duration::from_secs(
                        2_u64.pow(retries.min(5)),
                    );
                    log::warn!("Database connection attempt {}/{} failed: {}. Retrying in {:?}...", 
                        retries, max_retries, e, wait_time);
                    tokio::time::sleep(wait_time).await;
                }
            }
        }

        Self { chain_id, db }
    }

    pub async fn get_indexed_blocks(&self) -> HashSet<u32> {
        let query = format!(
            "SELECT number FROM blocks WHERE chain = {} AND is_uncle = false",
            self.chain_id
        );

        let tokens = (self.db.query(&query).fetch_all::<u32>().await)
            .unwrap_or_default();

        let blocks: HashSet<u32> = HashSet::from_iter(tokens.into_iter());

        blocks
    }

    pub async fn store_data(&self, data: &BlockFetchedData) {
        use std::sync::Arc;

        let mut stores = vec![];

        if !data.contracts.is_empty() {
            let contracts = Arc::new(data.contracts.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &contracts,
                    DatabaseTables::Contracts.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.logs.is_empty() {
            let logs = Arc::new(data.logs.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(&logs, DatabaseTables::Logs.as_str()).await
            });
            stores.push(work);
        }

        if !data.traces.is_empty() {
            let traces = Arc::new(data.traces.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(&traces, DatabaseTables::Traces.as_str())
                    .await
            });
            stores.push(work);
        }

        if !data.transactions.is_empty() {
            let transactions = Arc::new(data.transactions.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &transactions,
                    DatabaseTables::Transactions.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.withdrawals.is_empty() {
            let withdrawals = Arc::new(data.withdrawals.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &withdrawals,
                    DatabaseTables::Withdrawals.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.erc20_transfers.is_empty() {
            let transfers = Arc::new(data.erc20_transfers.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &transfers,
                    DatabaseTables::Erc20Transfers.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.erc721_transfers.is_empty() {
            let transfers = Arc::new(data.erc721_transfers.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &transfers,
                    DatabaseTables::Erc721Transfers.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.erc1155_transfers.is_empty() {
            let transfers = Arc::new(data.erc1155_transfers.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &transfers,
                    DatabaseTables::Erc1155Transfers.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.dex_trades.is_empty() {
            let dex_trades = Arc::new(data.dex_trades.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &dex_trades,
                    DatabaseTables::DexTrades.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.dex_pairs.is_empty() {
            let dex_pairs = Arc::new(data.dex_pairs.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &dex_pairs,
                    DatabaseTables::DexPairs.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.dex_liquidity_updates.is_empty() {
            let dex_liquidity_updates =
                Arc::new(data.dex_liquidity_updates.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(
                    &dex_liquidity_updates,
                    DatabaseTables::DexLiquidityUpdates.as_str(),
                )
                .await
            });
            stores.push(work);
        }

        if !data.tokens.is_empty() {
            let tokens = Arc::new(data.tokens.clone());
            let db = self.clone();
            let work = tokio::spawn(async move {
                db.store_items(&tokens, DatabaseTables::Tokens.as_str())
                    .await
            });
            stores.push(work);
        }

        let res = join_all(stores).await;

        let errored: Vec<_> =
            res.iter().filter(|res| res.is_err()).collect();

        if !errored.is_empty() {
            panic!("failed to store all chain primitive elements")
        }

        if !data.blocks.is_empty() {
            self.store_items(
                &data.blocks,
                DatabaseTables::Blocks.as_str(),
            )
            .await;
        }

        info!(
            "Inserted: contracts ({}) logs ({}) traces ({}) transactions ({}) withdrawals ({}) erc20 ({}) erc721 ({}) erc1155 ({}) dex_trades ({}) dex_pairs ({}) dex_liquidity_updates ({}) tokens ({}) in ({}) blocks.",
            data.contracts.len(),
            data.logs.len(),
            data.traces.len(),
            data.transactions.len(),
            data.withdrawals.len(),
            data.erc20_transfers.len(),
            data.erc721_transfers.len(),
            data.erc1155_transfers.len(),
            data.dex_trades.len(),
            data.dex_pairs.len(),
            data.dex_liquidity_updates.len(),
            data.tokens.len(),
            data.blocks.len()
        );
    }

    pub async fn store_items<T>(&self, items: &Vec<T>, table: &str)
    where
        T: Serialize,
        for<'a> T: Row<Value<'a> = T>,
    {
        if items.is_empty() {
            return;
        }

        let mut inserter = self.db.insert::<T>(table).await.unwrap();

        // Write all items - ClickHouse client handles batching internally
        for item in items {
            inserter.write(item).await.unwrap();
        }

        match inserter.end().await {
            Ok(_) => (),
            Err(err) => {
                error!("{}", err);
                panic!("Unable to store {} into database", table)
            }
        }
    }
}
