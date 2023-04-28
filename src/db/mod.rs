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

    pub fn get_connection(&self) -> &Client {
        return &self.db;
    }

    pub async fn get_indexed_blocks(&self) -> Result<HashSet<i64>> {
        let connection = self.get_connection();

        let query = format!(
            "SELECT number FROM blocks WHERE chain = '{}'",
            self.chain.id
        );

        let tokens =
            match connection.query(&query).fetch_all::<i64>().await {
                Ok(tokens) => tokens,
                Err(_) => Vec::new(),
            };

        let blocks: HashSet<i64> =
            HashSet::from_iter(tokens.into_iter().clone());

        Ok(blocks)
    }

    pub async fn get_tokens(
        &self,
        tokens: &HashSet<String>,
    ) -> Vec<DatabaseToken> {
        let connection = self.get_connection();

        let mut query = String::from(
            "SELECT * FROM token_details WHERE (token, chain) IN (",
        );

        for token in tokens {
            let condition = format!("('{}',{}),", token, self.chain.id);
            query.push_str(&condition)
        }

        query.pop();
        query.push_str(")");

        if tokens.len() > 0 {
            let tokens = match connection
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

        if logs.len() > 0 {
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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("transactions").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("receipts").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("logs").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("contracts").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("erc20_transfers").unwrap();

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
        let connection = self.get_connection();

        let mut inserter =
            connection.inserter("erc721_transfers").unwrap();

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
        let connection = self.get_connection();

        let mut inserter =
            connection.inserter("erc1155_transfers").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("dex_trades").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("blocks").unwrap();

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
        let connection = self.get_connection();

        let mut inserter = connection.inserter("token_details").unwrap();

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
        let connection = self.get_connection();

        connection
            .insert("chains_indexed_state")
            .unwrap()
            .write(chain_state)
            .await
            .expect("Unable to update indexed blocks number");

        Ok(())
    }

    /*
    TODO: recover aggregations

    pub async fn update_balances(
        &self,
        native: &HashMap<String, NativeTokenBalanceChange>,
        erc20: &HashMap<(String, String), ERC20TokenBalanceChange>,
        erc721: &HashMap<(String, String), ERC721OwnerChange>,
        erc1155: &HashMap<(String, String, String), ERC1155BalancesChange>,
    ) -> Result<()> {
        let mut stores = vec![];

        if native.len() > 0 {
            let work = tokio::spawn({
                let native = native.clone();
                let db = self.clone();
                async move {
                    db.update_native_balances(&native)
                        .await
                        .expect("unable to update native balances")
                }
            });
            stores.push(work);
        }

        if erc20.len() > 0 {
            let work = tokio::spawn({
                let erc20 = erc20.clone();
                let db = self.clone();
                async move {
                    db.update_erc20_balances(&erc20)
                        .await
                        .expect("unable to update erc20 balances")
                }
            });
            stores.push(work);
        }

        if erc721.len() > 0 {
            let work = tokio::spawn({
                let erc721 = erc721.clone();
                let db = self.clone();
                async move {
                    db.update_erc721_balances(&erc721)
                        .await
                        .expect("unable to update erc721 balances")
                }
            });
            stores.push(work);
        }

        if erc1155.len() > 0 {
            let work = tokio::spawn({
                let erc1155 = erc1155.clone();
                let db = self.clone();
                async move {
                    db.update_erc1155_balances(&erc1155)
                        .await
                        .expect("unable to update erc1155 balances")
                }
            });
            stores.push(work);
        }

        let res = join_all(stores).await;

        let errored: Vec<_> = res.iter().filter(|res| res.is_err()).collect();

        if errored.len() > 0 {
            panic!("failed to store all balances")
        }

        info!(
            "Updated balances: native ({}) erc20 ({}) erc721 ({}) erc1155 ({}).",
            native.len(),
            erc20.len(),
            erc721.len(),
            erc1155.len(),
        );

        Ok(())
    }

    pub async fn update_native_balances(
        &self,
        balances: &HashMap<String, NativeTokenBalanceChange>,
    ) -> Result<()> {
        let collection = self
            .agg_database
            .collection::<AggDatabaseNativeBalance>(NATIVE_BALANCES_KEY);

        let options = UpdateOptions::builder().upsert(Some(true)).build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "owner": changes.address.clone(), "chain": self.chain.id },
                "$inc": { "balance": changes.balance_change, "received": received, "sent": sent },
            };

            stores.push(collection.update_one(
                doc! { "chain": self.chain.id,  "owner": changes.address.clone() },
                update,
                options.clone(),
            ));
        }

        join_all(stores).await;

        Ok(())
    }

    pub async fn update_erc20_balances(
        &self,
        balances: &HashMap<(String, String), ERC20TokenBalanceChange>,
    ) -> Result<()> {
        let collection = self
            .agg_database
            .collection::<AggDatabaseERC20Balance>(ERC20_BALANCES_KEY);

        let options = UpdateOptions::builder().upsert(Some(true)).build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "owner": changes.address.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "balance": changes.balance_change, "received": received, "sent": sent },
            };

            stores.push(collection
                .update_one(
                    doc! { "owner": changes.address.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                    update,
                    options.clone(),
                )
            )
        }

        join_all(stores).await;

        Ok(())
    }

    pub async fn update_erc721_balances(
        &self,
        balances: &HashMap<(String, String), ERC721OwnerChange>,
    ) -> Result<()> {
        let collection = self
            .agg_database
            .collection::<AggDatabaseERC721TokenOwner>(ERC721_BALANCES_KEY);

        let options = UpdateOptions::builder().upsert(Some(true)).build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let update = doc! {
                "$set": { "id": changes.id.clone(), "owner": changes.to_owner.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "transactions": 1 },
            };

            stores.push(collection.update_one(
                doc! { "id": changes.id.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                update,
                options.clone(),
            ))
        }

        join_all(stores).await;

        Ok(())
    }

    pub async fn update_erc1155_balances(
        &self,
        balances: &HashMap<(String, String, String), ERC1155BalancesChange>,
    ) -> Result<()> {
        let collection = self
            .agg_database
            .collection::<AggDatabaseERC1155Balance>(ERC1155_BALANCES_KEY);

        let options = UpdateOptions::builder().upsert(Some(true)).build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "id": changes.id.clone(), "owner": changes.address.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "transactions": 1, "sent": sent, "received": received, "balance": changes.balance_change },
            };

            stores.push(collection.update_one(
            doc! { "id": changes.id.clone(), "chain": self.chain.id, "token": changes.token.clone(), "owner": changes.address.clone() },
            update,
            options.clone(),
        ))
        }

        join_all(stores).await;

        Ok(())
    }

    pub async fn update_dex_aggregates(
        &self,
        minutes: &HashMap<(String, String), DexPairAggregatedData>,
        hours: &HashMap<(String, String), DexPairAggregatedData>,
        days: &HashMap<(String, String), DexPairAggregatedData>,
    ) -> Result<()> {
        let mut stores = vec![];

        if minutes.len() > 0 {
            stores.push(self.update_dex_aggregated_data(minutes))
        }

        if hours.len() > 0 {
            stores.push(self.update_dex_aggregated_data(hours))
        }

        if days.len() > 0 {
            stores.push(self.update_dex_aggregated_data(days))
        }

        let res = join_all(stores).await;

        let errored: Vec<_> = res.iter().filter(|res| res.is_err()).collect();

        if errored.len() > 0 {
            panic!("failed to store all dex aggregates")
        }

        info!(
            "Inserted dex aggregates: minutes ({}) hourly ({}) daily ({}).",
            minutes.len(),
            hours.len(),
            days.len(),
        );

        Ok(())
    }

    pub async fn update_dex_aggregated_data(
        &self,
        data: &HashMap<(String, String), DexPairAggregatedData>,
    ) -> Result<()> {
        Ok(())
    }
    */
}
