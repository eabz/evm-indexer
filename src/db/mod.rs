pub mod models;

use crate::chains::Chain;
use clickhouse::Client;
use futures::future::join_all;
use hyper_tls::HttpsConnector;
use log::info;
use models::{
    block::DatabaseBlock, chain_state::DatabaseChainIndexedState,
    contract::DatabaseContract, dex_trade::DatabaseDexTrade,
    erc1155_transfer::DatabaseERC1155Transfer,
    erc20_transfer::DatabaseERC20Transfer,
    erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
    receipt::DatabaseReceipt, token::DatabaseToken,
    transaction::DatabaseTransaction,
};
use std::{collections::HashSet, time::Duration};

pub struct BlockFetchedData {
    pub blocks: Vec<DatabaseBlock>,
    pub transactions: Vec<DatabaseTransaction>,
    pub receipts: Vec<DatabaseReceipt>,
    pub logs: Vec<DatabaseLog>,
    pub contracts: Vec<DatabaseContract>,
    pub erc20_transfers: Vec<DatabaseERC20Transfer>,
    pub erc721_transfers: Vec<DatabaseERC721Transfer>,
    pub erc1155_transfers: Vec<DatabaseERC1155Transfer>,
    pub dex_trades: Vec<DatabaseDexTrade>,
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
            "SELECT number FROM blocks WHERE chain = '{}'",
            self.chain.id
        );

        let tokens = match self.db.query(&query).fetch_all::<u64>().await {
            Ok(tokens) => tokens,
            Err(_) => Vec::new(),
        };

        let blocks: HashSet<u64> = HashSet::from_iter(tokens.into_iter());

        blocks
    }

    pub async fn get_tokens(
        &self,
        tokens: &HashSet<String>,
    ) -> Vec<DatabaseToken> {
        let mut query = String::from(
            "SELECT * FROM token_details WHERE (token, chain) IN (",
        );

        for token in tokens {
            let condition = format!("('{}',{}),", token, self.chain.id);
            query.push_str(&condition)
        }

        query.pop();
        query.push_str(") FINAL");

        if !tokens.is_empty() {
            let tokens = match self
                .db
                .query(&query)
                .fetch_all::<DatabaseToken>()
                .await
            {
                Ok(tokens) => tokens,
                Err(_) => Vec::new(),
            };

            return tokens;
        }

        Vec::new()
    }

    pub async fn store_data(&self, data: &BlockFetchedData) {
        let mut stores = vec![];

        if !data.transactions.is_empty() {
            let work = tokio::spawn({
                let transactions = data.transactions.clone();
                let db = self.clone();
                async move { db.store_transactions(&transactions).await }
            });
            stores.push(work);
        }

        if !data.receipts.is_empty() {
            let work = tokio::spawn({
                let receipts = data.receipts.clone();
                let db = self.clone();
                async move { db.store_receipts(&receipts).await }
            });
            stores.push(work);
        }

        if !data.logs.is_empty() {
            let work = tokio::spawn({
                let logs = data.logs.clone();
                let db = self.clone();
                async move { db.store_logs(&logs).await }
            });
            stores.push(work);
        }

        if !data.contracts.is_empty() {
            let work = tokio::spawn({
                let contracts = data.contracts.clone();
                let db = self.clone();
                async move { db.store_contracts(&contracts).await }
            });
            stores.push(work);
        }

        if !data.erc20_transfers.is_empty() {
            let work = tokio::spawn({
                let erc20_transfers = data.erc20_transfers.clone();
                let db = self.clone();
                async move { db.store_erc20_transfers(&erc20_transfers).await }
            });
            stores.push(work);
        }

        if !data.erc721_transfers.is_empty() {
            let work = tokio::spawn({
                let erc721_transfers = data.erc721_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_erc721_transfers(&erc721_transfers).await
                }
            });
            stores.push(work);
        }

        if !data.erc1155_transfers.is_empty() {
            let work = tokio::spawn({
                let erc1155_transfers = data.erc1155_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_erc1155_transfers(&erc1155_transfers).await
                }
            });
            stores.push(work);
        }

        if !data.dex_trades.is_empty() {
            let work = tokio::spawn({
                let dex_trades = data.dex_trades.clone();
                let db = self.clone();
                async move { db.store_dex_trades(&dex_trades).await }
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
            self.store_blocks(&data.blocks).await;
        }

        info!(
            "Inserted: txs ({}) receipts ({}) logs ({}) contracts ({}) transfers erc20 ({}) erc721 ({}) erc1155 ({}) trades ({}) in ({}) blocks.",
            data.transactions.len(),
            data.receipts.len(),
            data.logs.len(),
            data.contracts.len(),
            data.erc20_transfers.len(),
            data.erc721_transfers.len(),
            data.erc1155_transfers.len(),
            data.dex_trades.len(),
            data.blocks.len(),
        );
    }

    async fn store_transactions(
        &self,
        transactions: &Vec<DatabaseTransaction>,
    ) {
        let mut inserter = self.db.inserter("transactions").unwrap();

        for transaction in transactions {
            inserter.write(transaction).await.unwrap();
        }
        inserter
            .end()
            .await
            .expect("Unable to store transactions into database");
    }

    async fn store_receipts(&self, receipts: &Vec<DatabaseReceipt>) {
        let mut inserter = self.db.inserter("receipts").unwrap();

        for receipt in receipts {
            inserter.write(receipt).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store receipts into database");
    }

    async fn store_logs(&self, logs: &Vec<DatabaseLog>) {
        let mut inserter = self.db.inserter("logs").unwrap();

        for log in logs {
            inserter.write(log).await.unwrap();
        }

        inserter.end().await.expect("Unable to store logs into database");
    }

    async fn store_contracts(&self, contracts: &Vec<DatabaseContract>) {
        let mut inserter = self.db.inserter("contracts").unwrap();

        for contract in contracts {
            inserter.write(contract).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store contracts into database");
    }

    async fn store_erc20_transfers(
        &self,
        transfers: &Vec<DatabaseERC20Transfer>,
    ) {
        let mut inserter = self.db.inserter("erc20_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc20_transfers into database");
    }

    async fn store_erc721_transfers(
        &self,
        transfers: &Vec<DatabaseERC721Transfer>,
    ) {
        let mut inserter = self.db.inserter("erc721_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc721_transfers into database");
    }

    async fn store_erc1155_transfers(
        &self,
        transfers: &Vec<DatabaseERC1155Transfer>,
    ) {
        let mut inserter = self.db.inserter("erc1155_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc1155_transfers into database");
    }

    async fn store_dex_trades(&self, trades: &Vec<DatabaseDexTrade>) {
        let mut inserter = self.db.inserter("dex_trades").unwrap();

        for trade in trades {
            inserter.write(trade).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store dex_trades into database");
    }

    async fn store_blocks(&self, blocks: &Vec<DatabaseBlock>) {
        let mut inserter = self.db.inserter("blocks").unwrap();

        for block in blocks {
            inserter.write(block).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store blocks into database");
    }

    pub async fn store_token_details(&self, tokens: &Vec<DatabaseToken>) {
        let mut inserter = self.db.inserter("tokens").unwrap();

        for token in tokens {
            inserter.write(token).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store tokens into database");

        info!("Inserted: token details ({})", tokens.len());
    }

    pub async fn update_indexed_blocks_number(
        &self,
        chain_state: &DatabaseChainIndexedState,
    ) {
        self.db
            .insert("chains_indexed_state")
            .unwrap()
            .write(chain_state)
            .await
            .expect("Unable to update indexed blocks number");
    }
}
