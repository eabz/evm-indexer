pub mod models;

use crate::chains::Chain;
use clickhouse::{Client, Row};
use futures::future::join_all;
use hyper_tls::HttpsConnector;
use log::info;
use models::{
    block::DatabaseBlock, contract::DatabaseContract,
    dex_trade::DatabaseDexTrade,
    erc1155_transfer::DatabaseERC1155Transfer,
    erc20_transfer::DatabaseERC20Transfer,
    erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
    receipt::DatabaseReceipt, trace::DatabaseTrace,
    transaction::DatabaseTransaction, withdrawal::DatabaseWithdrawal,
};
use serde::Serialize;
use std::{collections::HashSet, time::Duration};

use self::models::block_reward::DatabaseBlockReward;

pub struct BlockFetchedData {
    pub blocks: Vec<DatabaseBlock>,
    pub block_rewards: Vec<DatabaseBlockReward>,
    pub contracts: Vec<DatabaseContract>,
    pub dex_trades: Vec<DatabaseDexTrade>,
    pub erc20_transfers: Vec<DatabaseERC20Transfer>,
    pub erc721_transfers: Vec<DatabaseERC721Transfer>,
    pub erc1155_transfers: Vec<DatabaseERC1155Transfer>,
    pub logs: Vec<DatabaseLog>,
    pub receipts: Vec<DatabaseReceipt>,
    pub traces: Vec<DatabaseTrace>,
    pub transactions: Vec<DatabaseTransaction>,
    pub withdrawals: Vec<DatabaseWithdrawal>,
}

// Ref: https://github.com/loyd/clickhouse.rs/blob/master/src/lib.rs#L51
// ClickHouse uses 3s by default.
// See https://github.com/ClickHouse/ClickHouse/blob/368cb74b4d222dc5472a7f2177f6bb154ebae07a/programs/server/config.xml#L201
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Database {
    pub chain: Chain,
    pub db: Client,
}

pub enum DatabaseTables {
    Blocks,
    BlockRewards,
    Contracts,
    DexTrades,
    Erc1155Transfers,
    Erc20Transfers,
    Erc721Transfers,
    Logs,
    Receipts,
    Traces,
    Transactions,
    Withdrawals,
}

impl DatabaseTables {
    pub fn as_str(&self) -> &'static str {
        match self {
            DatabaseTables::Blocks => "blocks",
            DatabaseTables::BlockRewards => "block_rewards",
            DatabaseTables::Contracts => "contracts",
            DatabaseTables::DexTrades => "dex_trades",
            DatabaseTables::Erc1155Transfers => "erc1155_transfers",
            DatabaseTables::Erc20Transfers => "erc20_transfers",
            DatabaseTables::Erc721Transfers => "erc721_transfers",
            DatabaseTables::Logs => "logs",
            DatabaseTables::Receipts => "receipts",
            DatabaseTables::Traces => "traces",
            DatabaseTables::Transactions => "transactions",
            DatabaseTables::Withdrawals => "withdrawals",
        }
    }
}

impl Database {
    pub async fn new(
        db_host: String,
        db_username: String,
        db_password: String,
        db_name: String,
        chain: Chain,
    ) -> Self {
        info!("Starting EVM database service");

        let https = HttpsConnector::new();

        let client = hyper::Client::builder()
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            .build::<_, hyper::Body>(https);

        let db = Client::with_http_client(client)
            .with_url(db_host)
            .with_user(db_username)
            .with_password(db_password)
            .with_database(db_name);

        Self { chain, db }
    }

    pub async fn get_indexed_blocks(&self) -> HashSet<u64> {
        let query = format!(
            "SELECT number FROM blocks WHERE chain = {}",
            self.chain.id
        );

        let tokens = match self.db.query(&query).fetch_all::<u64>().await {
            Ok(tokens) => tokens,
            Err(_) => Vec::new(),
        };

        let blocks: HashSet<u64> = HashSet::from_iter(tokens.into_iter());

        blocks
    }

    pub async fn store_data(&self, data: &BlockFetchedData) {
        let mut stores = vec![];

        if !data.block_rewards.is_empty() {
            let work = tokio::spawn({
                let block_rewards: Vec<DatabaseBlockReward> =
                    data.block_rewards.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &block_rewards,
                        DatabaseTables::BlockRewards.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.contracts.is_empty() {
            let work = tokio::spawn({
                let contracts = data.contracts.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &contracts,
                        DatabaseTables::Contracts.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.dex_trades.is_empty() {
            let work = tokio::spawn({
                let dex_trades = data.dex_trades.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &dex_trades,
                        DatabaseTables::DexTrades.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.erc1155_transfers.is_empty() {
            let work = tokio::spawn({
                let erc1155_transfers = data.erc1155_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &erc1155_transfers,
                        DatabaseTables::Erc1155Transfers.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.erc20_transfers.is_empty() {
            let work = tokio::spawn({
                let erc20_transfers = data.erc20_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &erc20_transfers,
                        DatabaseTables::Erc20Transfers.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.erc721_transfers.is_empty() {
            let work = tokio::spawn({
                let erc721_transfers = data.erc721_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &erc721_transfers,
                        DatabaseTables::Erc721Transfers.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.logs.is_empty() {
            let work = tokio::spawn({
                let logs = data.logs.clone();
                let db = self.clone();
                async move {
                    db.store_items(&logs, DatabaseTables::Logs.as_str())
                        .await
                }
            });
            stores.push(work);
        }

        if !data.receipts.is_empty() {
            let work = tokio::spawn({
                let receipts = data.receipts.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &receipts,
                        DatabaseTables::Receipts.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.traces.is_empty() {
            let work = tokio::spawn({
                let traces = data.traces.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &traces,
                        DatabaseTables::Traces.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.transactions.is_empty() {
            let work = tokio::spawn({
                let transactions = data.transactions.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &transactions,
                        DatabaseTables::Transactions.as_str(),
                    )
                    .await
                }
            });
            stores.push(work);
        }

        if !data.withdrawals.is_empty() {
            let work = tokio::spawn({
                let withdrawals: Vec<DatabaseWithdrawal> =
                    data.withdrawals.clone();
                let db = self.clone();
                async move {
                    db.store_items(
                        &withdrawals,
                        DatabaseTables::Withdrawals.as_str(),
                    )
                    .await
                }
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
            "Inserted: rewards ({}) contracts ({}) trades ({}) erc1155 ({}) erc20 ({}) erc721 ({}) logs ({}) receipts ({}) traces ({}) transactions ({}) withdrawals ({}) in ({}) blocks.",
            data.block_rewards.len(),
            data.contracts.len(),
            data.dex_trades.len(),
            data.erc1155_transfers.len(),
            data.erc20_transfers.len(),
            data.erc721_transfers.len(),
            data.logs.len(),
            data.receipts.len(),
            data.traces.len(),
            data.transactions.len(),
            data.withdrawals.len(),
            data.blocks.len(),
        );
    }

    pub async fn store_items<T>(&self, items: &Vec<T>, table: &str)
    where
        T: Row + Serialize,
    {
        let mut inserter = self.db.inserter(table).unwrap();

        for item in items {
            inserter.write(item).await.unwrap();
        }

        inserter.end().await.unwrap_or_else(|_| {
            panic!("Unable to store {} into database", table)
        });
    }
}
