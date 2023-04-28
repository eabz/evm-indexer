pub mod models;

use crate::chains::Chain;
use anyhow::Result;
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
    ) -> Result<Self> {
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

        Ok(Self { chain, db })
    }

    pub async fn get_indexed_blocks(&self) -> Result<HashSet<u64>> {
        let query = format!(
            "SELECT number FROM blocks WHERE chain = '{}'",
            self.chain.id
        );

        let tokens = match self.db.query(&query).fetch_all::<u64>().await {
            Ok(tokens) => tokens,
            Err(_) => Vec::new(),
        };

        let blocks: HashSet<u64> = HashSet::from_iter(tokens.into_iter());

        Ok(blocks)
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
        query.push_str(")");

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

        return Vec::new();
    }

    pub async fn store_data(
        &self,
        blocks: &Vec<DatabaseBlock>,
        transactions: &Vec<DatabaseTransaction>,
        receipts: &Vec<DatabaseReceipt>,
        logs: &Vec<DatabaseLog>,
        contracts: &Vec<DatabaseContract>,
        erc20_transfers: &Vec<DatabaseERC20Transfer>,
        erc721_transfers: &Vec<DatabaseERC721Transfer>,
        erc1155_transfers: &Vec<DatabaseERC1155Transfer>,
        dex_trades: &Vec<DatabaseDexTrade>,
    ) {
        let mut stores = vec![];

        if transactions.len() > 0 {
            let work = tokio::spawn({
                let transactions = transactions.clone();
                let db = self.clone();
                async move {
                    db.store_transactions(&transactions)
                        .await
                        .expect("unable to store transactions")
                }
            });
            stores.push(work);
        }

        if receipts.len() > 0 {
            let work = tokio::spawn({
                let receipts = receipts.clone();
                let db = self.clone();
                async move {
                    db.store_receipts(&receipts)
                        .await
                        .expect("unable to store receipts")
                }
            });
            stores.push(work);
        }

        if !logs.is_empty() {
            let work = tokio::spawn({
                let logs = logs.clone();
                let db = self.clone();
                async move {
                    db.store_logs(&logs)
                        .await
                        .expect("unable to store logs")
                }
            });
            stores.push(work);
        }

        if contracts.len() > 0 {
            let work = tokio::spawn({
                let contracts = contracts.clone();
                let db = self.clone();
                async move {
                    db.store_contracts(&contracts)
                        .await
                        .expect("unable to store contracts")
                }
            });
            stores.push(work);
        }

        if erc20_transfers.len() > 0 {
            let work = tokio::spawn({
                let erc20_transfers = erc20_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_erc20_transfers(&erc20_transfers)
                        .await
                        .expect("unable to store erc20 transfers")
                }
            });
            stores.push(work);
        }

        if erc721_transfers.len() > 0 {
            let work = tokio::spawn({
                let erc721_transfers = erc721_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_erc721_transfers(&erc721_transfers)
                        .await
                        .expect("unable to erc721 transfers")
                }
            });
            stores.push(work);
        }

        if erc1155_transfers.len() > 0 {
            let work = tokio::spawn({
                let erc1155_transfers = erc1155_transfers.clone();
                let db = self.clone();
                async move {
                    db.store_erc1155_transfers(&erc1155_transfers)
                        .await
                        .expect("unable to store erc1155 transfers")
                }
            });
            stores.push(work);
        }

        if dex_trades.len() > 0 {
            let work = tokio::spawn({
                let dex_trades = dex_trades.clone();
                let db = self.clone();
                async move {
                    db.store_dex_trades(&dex_trades)
                        .await
                        .expect("unable to store dex trades")
                }
            });
            stores.push(work);
        }

        let res = join_all(stores).await;

        let errored: Vec<_> =
            res.iter().filter(|res| res.is_err()).collect();

        if errored.len() > 0 {
            panic!("failed to store all chain primitive elements")
        }

        if blocks.len() > 0 {
            self.store_blocks(&blocks).await.unwrap();
        }

        info!(
            "Inserted: txs ({}) receipts ({}) logs ({}) contracts ({}) transfers erc20 ({}) erc721 ({}) erc1155 ({}) trades ({}) in ({}) blocks.",
            transactions.len(),
            receipts.len(),
            logs.len(),
            contracts.len(),
            erc20_transfers.len(),
            erc721_transfers.len(),
            erc1155_transfers.len(),
            dex_trades.len(),
            blocks.len(),
        );
    }

    async fn store_transactions(
        &self,
        transactions: &Vec<DatabaseTransaction>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("transactions").unwrap();

        for transaction in transactions {
            inserter.write(transaction).await.unwrap();
        }
        inserter
            .end()
            .await
            .expect("Unable to store transactions into database");

        Ok(())
    }

    async fn store_receipts(
        &self,
        receipts: &Vec<DatabaseReceipt>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("receipts").unwrap();

        for receipt in receipts {
            inserter.write(receipt).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store receipts into database");

        Ok(())
    }

    async fn store_logs(&self, logs: &Vec<DatabaseLog>) -> Result<()> {
        let mut inserter = self.db.inserter("logs").unwrap();

        for log in logs {
            inserter.write(log).await.unwrap();
        }

        inserter.end().await.expect("Unable to store logs into database");

        Ok(())
    }

    async fn store_contracts(
        &self,
        contracts: &Vec<DatabaseContract>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("contracts").unwrap();

        for contract in contracts {
            inserter.write(contract).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store contracts into database");

        Ok(())
    }

    async fn store_erc20_transfers(
        &self,
        transfers: &Vec<DatabaseERC20Transfer>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("erc20_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc20_transfers into database");

        Ok(())
    }

    async fn store_erc721_transfers(
        &self,
        transfers: &Vec<DatabaseERC721Transfer>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("erc721_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc721_transfers into database");

        Ok(())
    }

    async fn store_erc1155_transfers(
        &self,
        transfers: &Vec<DatabaseERC1155Transfer>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("erc1155_transfers").unwrap();

        for transfer in transfers {
            inserter.write(transfer).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store erc1155_transfers into database");

        Ok(())
    }

    async fn store_dex_trades(
        &self,
        trades: &Vec<DatabaseDexTrade>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("dex_trades").unwrap();

        for trade in trades {
            inserter.write(trade).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store dex_trades into database");

        Ok(())
    }

    async fn store_blocks(
        &self,
        blocks: &Vec<DatabaseBlock>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("blocks").unwrap();

        for block in blocks {
            inserter.write(block).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store blocks into database");

        Ok(())
    }

    pub async fn store_token_details(
        &self,
        tokens: &Vec<DatabaseToken>,
    ) -> Result<()> {
        let mut inserter = self.db.inserter("token_details").unwrap();

        for token in tokens {
            inserter.write(token).await.unwrap();
        }

        inserter
            .end()
            .await
            .expect("Unable to store token_details into database");

        info!("Inserted: token details ({})", tokens.len());

        Ok(())
    }

    pub async fn update_indexed_blocks_number(
        &self,
        chain_state: &DatabaseChainIndexedState,
    ) -> Result<()> {
        self.db
            .insert("chains_indexed_state")
            .unwrap()
            .write(chain_state)
            .await
            .expect("Unable to update indexed blocks number");

        Ok(())
    }
}
