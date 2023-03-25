use std::{
    cmp::min,
    collections::{HashMap, HashSet},
};

use crate::{
    chains::chains::Chain,
    utils::aggregate::{
        DexPairAggregatedData, ERC1155BalancesChange, ERC20TokenBalanceChange, ERC721OwnerChange,
        NativeTokenBalanceChange,
    },
};
use anyhow::Result;
use field_count::FieldCount;
use futures::future::join_all;
use log::info;
use mongodb::{
    bson::doc,
    options::{ClientOptions, FindOneAndUpdateOptions},
    Client,
};
use redis::Commands;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions, QueryBuilder,
};

use super::{
    keys::{ERC1155_BALANCES_KEY, ERC20_BALANCES_KEY, ERC721_BALANCES_KEY, NATIVE_BALANCES_KEY},
    models::{
        balances::{
            AggDatabaseERC1155Balance, AggDatabaseERC20Balance, AggDatabaseERC721TokenOwner,
            AggDatabaseNativeBalance,
        },
        block::DatabaseBlock,
        chain_state::DatabaseChainIndexedState,
        contract::DatabaseContract,
        dex_trade::DatabaseDexTrade,
        erc1155_transfer::DatabaseERC1155Transfer,
        erc20_transfer::DatabaseERC20Transfer,
        erc721_transfer::DatabaseERC721Transfer,
        log::DatabaseLog,
        receipt::DatabaseReceipt,
        token_detail::DatabaseTokenDetails,
        transaction::DatabaseTransaction,
    },
};

pub const MAX_PARAM_SIZE: u16 = u16::MAX;

#[derive(Debug, Clone)]
pub struct Database {
    pub chain: Chain,
    pub redis: redis::Client,
    pub agg_database: mongodb::Database,
    pub db_conn: sqlx::Pool<sqlx::Postgres>,
}

impl Database {
    pub async fn new(
        db_url: String,
        redis_url: String,
        agg_db_url: String,
        chain: Chain,
    ) -> Result<Self> {
        info!("Starting EVM database service");

        let mut connect_options: PgConnectOptions = db_url.parse().unwrap();

        connect_options.disable_statement_logging();

        let db_conn = PgPoolOptions::new()
            .max_connections(500)
            .connect_with(connect_options)
            .await
            .expect("Unable to connect to the database");

        let redis = redis::Client::open(redis_url).expect("Unable to connect with redis server");

        let client_options = ClientOptions::parse(agg_db_url)
            .await
            .expect("Unable to connect with aggregated database");

        let client = Client::with_options(client_options)?;

        Ok(Self {
            chain,
            redis,
            agg_database: client.database("indexer"),
            db_conn,
        })
    }

    pub fn get_connection(&self) -> &sqlx::Pool<sqlx::Postgres> {
        return &self.db_conn;
    }

    pub async fn get_indexed_blocks(&self) -> Result<HashSet<i64>> {
        let mut connection = self.redis.get_connection().unwrap();

        let keys: Vec<String> = connection
            .keys(format!("{}*", self.chain.name.to_string()))
            .unwrap();

        let mut blocks: HashSet<i64> = HashSet::new();

        for key in keys {
            let chunk_blocks: HashSet<i64> = match connection.get::<String, String>(key) {
                Ok(blocks) => match serde_json::from_str(&blocks) {
                    Ok(deserialized) => deserialized,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

            blocks.extend(&chunk_blocks);
        }

        Ok(blocks)
    }

    pub async fn get_tokens(&self, tokens: &HashSet<String>) -> Vec<DatabaseTokenDetails> {
        let connection = self.get_connection();

        let mut query =
            String::from("SELECT * FROM token_details WHERE (token, chain) IN ( VALUES ");

        for token in tokens {
            let condition = format!("(('{}',{})),", token, self.chain.id);
            query.push_str(&condition)
        }

        query.pop();
        query.push_str(")");

        if tokens.len() > 0 {
            let rows = sqlx::query_as::<_, DatabaseTokenDetails>(&query)
                .fetch_all(connection)
                .await;

            match rows {
                Ok(transfers) => return transfers,
                Err(_) => return Vec::new(),
            };
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
                async move { db.store_logs(&logs).await.expect("unable to store logs") }
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

        let errored: Vec<_> = res.iter().filter(|res| res.is_err()).collect();

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

    async fn store_transactions(&self, transactions: &Vec<DatabaseTransaction>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(transactions.len(), DatabaseTransaction::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new("UPSERT INTO transactions (block_hash, block_number, chain, from_address, gas, gas_price, hash, input, max_priority_fee_per_gas, max_fee_per_gas, method, nonce, timestamp, to_address, transaction_index, transaction_type, value) ");

            query_builder.push_values(&transactions[start..end], |mut row, transaction| {
                row.push_bind(transaction.block_hash.clone())
                    .push_bind(transaction.block_number)
                    .push_bind(transaction.chain)
                    .push_bind(transaction.from_address.clone())
                    .push_bind(transaction.gas)
                    .push_bind(transaction.gas_price)
                    .push_bind(transaction.hash.clone())
                    .push_bind(transaction.input.clone())
                    .push_bind(transaction.max_priority_fee_per_gas)
                    .push_bind(transaction.max_fee_per_gas)
                    .push_bind(transaction.method.clone())
                    .push_bind(transaction.nonce)
                    .push_bind(transaction.timestamp)
                    .push_bind(transaction.to_address.clone())
                    .push_bind(transaction.transaction_index)
                    .push_bind(transaction.transaction_type)
                    .push_bind(transaction.value);
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store transactions into database");
        }

        Ok(())
    }

    async fn store_receipts(&self, receipts: &Vec<DatabaseReceipt>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(receipts.len(), DatabaseReceipt::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new("UPSERT INTO receipts (contract_address, cumulative_gas_used, effective_gas_price, gas_used, hash, status) ");

            query_builder.push_values(&receipts[start..end], |mut row, receipt| {
                row.push_bind(receipt.contract_address.clone())
                    .push_bind(receipt.cumulative_gas_used)
                    .push_bind(receipt.effective_gas_price)
                    .push_bind(receipt.gas_used)
                    .push_bind(receipt.hash.clone())
                    .push_bind(receipt.status.as_str());
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store receipts into database");
        }

        Ok(())
    }

    async fn store_logs(&self, logs: &Vec<DatabaseLog>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(logs.len(), DatabaseLog::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO logs (address, chain, data, hash, log_index, removed, topics, transaction_log_index, timestamp) ",
            );

            query_builder.push_values(&logs[start..end], |mut row, log| {
                row.push_bind(log.address.clone())
                    .push_bind(log.chain)
                    .push_bind(log.data.clone())
                    .push_bind(log.hash.clone())
                    .push_bind(log.log_index)
                    .push_bind(log.removed)
                    .push_bind(log.topics.clone())
                    .push_bind(log.transaction_log_index)
                    .push_bind(log.timestamp);
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store logs into database");
        }

        Ok(())
    }

    async fn store_contracts(&self, contracts: &Vec<DatabaseContract>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(contracts.len(), DatabaseContract::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO contracts (block, contract_address, chain, creator, hash) ",
            );

            query_builder.push_values(&contracts[start..end], |mut row, contract| {
                row.push_bind(contract.block)
                    .push_bind(contract.contract_address.clone())
                    .push_bind(contract.chain)
                    .push_bind(contract.creator.clone())
                    .push_bind(contract.hash.clone());
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store contracts into database");
        }

        Ok(())
    }

    async fn store_erc20_transfers(&self, transfers: &Vec<DatabaseERC20Transfer>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(transfers.len(), DatabaseERC20Transfer::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO erc20_transfers (chain, from_address, hash, log_index, to_address, token, transaction_log_index, amount, timestamp) ",
            );

            query_builder.push_values(&transfers[start..end], |mut row, transfers| {
                row.push_bind(transfers.chain)
                    .push_bind(transfers.from_address.clone())
                    .push_bind(transfers.hash.clone())
                    .push_bind(transfers.log_index)
                    .push_bind(transfers.to_address.clone())
                    .push_bind(transfers.token.clone())
                    .push_bind(transfers.transaction_log_index)
                    .push_bind(transfers.amount)
                    .push_bind(transfers.timestamp);
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store erc20 transfers into database");
        }

        Ok(())
    }

    async fn store_erc721_transfers(&self, transfers: &Vec<DatabaseERC721Transfer>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(transfers.len(), DatabaseERC721Transfer::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO erc721_transfers (chain, from_address, hash, log_index, to_address, token, transaction_log_index, id, timestamp) ",
            );

            query_builder.push_values(&transfers[start..end], |mut row, transfers| {
                row.push_bind(transfers.chain)
                    .push_bind(transfers.from_address.clone())
                    .push_bind(transfers.hash.clone())
                    .push_bind(transfers.log_index)
                    .push_bind(transfers.to_address.clone())
                    .push_bind(transfers.token.clone())
                    .push_bind(transfers.transaction_log_index)
                    .push_bind(transfers.id.clone())
                    .push_bind(transfers.timestamp);
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store erc721 transfers into database");
        }

        Ok(())
    }

    async fn store_erc1155_transfers(
        &self,
        transfers: &Vec<DatabaseERC1155Transfer>,
    ) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(transfers.len(), DatabaseERC1155Transfer::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO erc1155_transfers (chain, operator, from_address, hash, log_index, to_address, token, transaction_log_index, ids, values, timestamp) ",
            );

            query_builder.push_values(&transfers[start..end], |mut row, transfers| {
                row.push_bind(transfers.chain)
                    .push_bind(transfers.operator.clone())
                    .push_bind(transfers.from_address.clone())
                    .push_bind(transfers.hash.clone())
                    .push_bind(transfers.log_index)
                    .push_bind(transfers.to_address.clone())
                    .push_bind(transfers.token.clone())
                    .push_bind(transfers.transaction_log_index)
                    .push_bind(transfers.ids.clone())
                    .push_bind(transfers.values.clone())
                    .push_bind(transfers.timestamp);
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store erc1155 transfers into database");
        }

        Ok(())
    }

    async fn store_dex_trades(&self, trades: &Vec<DatabaseDexTrade>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(trades.len(), DatabaseDexTrade::field_count());

        for (start, end) in chunks {
            let mut query_builder =
                QueryBuilder::new("UPSERT INTO dex_trades (chain, maker, hash, log_index, receiver, token0, token1, pair_address, token0_amount, token1_amount, swap_rate, transaction_log_index, timestamp, trade_type) ");

            query_builder.push_values(&trades[start..end], |mut row, transfers| {
                row.push_bind(transfers.chain)
                    .push_bind(transfers.maker.clone())
                    .push_bind(transfers.hash.clone())
                    .push_bind(transfers.log_index)
                    .push_bind(transfers.receiver.clone())
                    .push_bind(transfers.token0.clone())
                    .push_bind(transfers.token1.clone())
                    .push_bind(transfers.pair_address.clone())
                    .push_bind(transfers.token0_amount)
                    .push_bind(transfers.token1_amount)
                    .push_bind(transfers.swap_rate)
                    .push_bind(transfers.transaction_log_index)
                    .push_bind(transfers.timestamp)
                    .push_bind(transfers.trade_type.as_str());
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store dex trades into database");
        }

        Ok(())
    }

    async fn store_blocks(&self, blocks: &Vec<DatabaseBlock>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(blocks.len(), DatabaseBlock::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new("UPSERT INTO blocks (base_fee_per_gas, chain, difficulty, extra_data, gas_limit, gas_used, hash, logs_bloom, miner, mix_hash, nonce, number, parent_hash, receipts_root, sha3_uncles, size, state_root, status, timestamp, total_difficulty, transactions, transactions_root, uncles) ");

            query_builder.push_values(&blocks[start..end], |mut row, block| {
                row.push_bind(block.base_fee_per_gas)
                    .push_bind(block.chain)
                    .push_bind(block.difficulty.clone())
                    .push_bind(block.extra_data.clone())
                    .push_bind(block.gas_limit)
                    .push_bind(block.gas_used)
                    .push_bind(block.hash.clone())
                    .push_bind(block.logs_bloom.clone())
                    .push_bind(block.miner.clone())
                    .push_bind(block.mix_hash.clone())
                    .push_bind(block.nonce.clone())
                    .push_bind(block.number)
                    .push_bind(block.parent_hash.clone())
                    .push_bind(block.receipts_root.clone())
                    .push_bind(block.sha3_uncles.clone())
                    .push_bind(block.size)
                    .push_bind(block.state_root.clone())
                    .push_bind(block.status.as_str())
                    .push_bind(block.timestamp)
                    .push_bind(block.total_difficulty.clone())
                    .push_bind(block.transactions)
                    .push_bind(block.transactions_root.clone())
                    .push_bind(block.uncles.clone());
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store blocks into database");
        }

        Ok(())
    }

    pub async fn store_token_details(&self, tokens: &Vec<DatabaseTokenDetails>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(tokens.len(), DatabaseTokenDetails::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO token_details (chain, token, name, symbol, decimals, token0, token1, factory) ",
            );

            query_builder.push_values(&tokens[start..end], |mut row, token| {
                row.push_bind(token.chain)
                    .push_bind(token.token.clone())
                    .push_bind(token.name.clone())
                    .push_bind(token.symbol.clone())
                    .push_bind(token.decimals)
                    .push_bind(token.token0.clone())
                    .push_bind(token.token1.clone())
                    .push_bind(token.factory.clone());
            });

            let query = query_builder.build();

            query
                .execute(connection)
                .await
                .expect("Unable to store token details into database");
        }

        info!("Inserted: token details ({})", tokens.len());

        Ok(())
    }

    pub async fn store_indexed_blocks(&self, blocks: &Vec<i64>) -> Result<()> {
        let mut connection = self.redis.get_connection().unwrap();

        let chunks = blocks.chunks(30_000_000);

        for (i, chunk) in chunks.enumerate() {
            let chunk_vec = chunk.to_vec();

            let serialized = serde_json::to_string(&chunk_vec).unwrap();

            let _: () = connection
                .set(format!("{}-{}", self.chain.name.to_owned(), i), serialized)
                .unwrap();
        }

        self.update_indexed_blocks_number(&DatabaseChainIndexedState {
            chain: self.chain.id,
            indexed_blocks_amount: blocks.len() as i64,
        })
        .await
        .unwrap();

        Ok(())
    }

    pub async fn update_indexed_blocks_number(
        &self,
        chain_state: &DatabaseChainIndexedState,
    ) -> Result<()> {
        let connection = self.get_connection();

        let query = format!(
            "UPSERT INTO chains_indexed_state (chain, indexed_blocks_amount) VALUES ({}, {})",
            chain_state.chain, chain_state.indexed_blocks_amount,
        );

        QueryBuilder::new(query)
            .build()
            .execute(connection)
            .await
            .expect("Unable to update indexed blocks number");

        Ok(())
    }

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

        let options = FindOneAndUpdateOptions::builder()
            .upsert(Some(true))
            .build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "owner": changes.address.clone(), "chain": self.chain.id },
                "$inc": { "balance": changes.balance_change, "received": received, "sent": sent },
            };

            stores.push(collection.find_one_and_update(
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

        let options = FindOneAndUpdateOptions::builder()
            .upsert(Some(true))
            .build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "owner": changes.address.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "balance": changes.balance_change, "received": received, "sent": sent },
            };

            stores.push(collection
                .find_one_and_update(
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

        let options = FindOneAndUpdateOptions::builder()
            .upsert(Some(true))
            .build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let update = doc! {
                "$set": { "id": changes.id.clone(), "owner": changes.to_owner.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "transactions": 1 },
            };

            stores.push(collection.find_one_and_update(
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

        let options = FindOneAndUpdateOptions::builder()
            .upsert(Some(true))
            .build();

        let mut stores = vec![];

        for (_, changes) in balances {
            let received = if changes.balance_change > 0.0 { 1 } else { 0 };
            let sent = if changes.balance_change > 0.0 { 0 } else { 1 };

            let update = doc! {
                "$set": { "id": changes.id.clone(), "owner": changes.address.clone(), "chain": self.chain.id, "token": changes.token.clone() },
                "$inc": { "transactions": 1, "sent": sent, "received": received, "balance": changes.balance_change },
            };

            stores.push(collection.find_one_and_update(
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
}

/// Ref: https://github.com/aptos-labs/aptos-core/blob/main/crates/indexer/src/database.rs#L32
/// Given diesel has a limit of how many parameters can be inserted in a single operation (u16::MAX)
/// we may need to chunk an array of items based on how many columns are in the table.
/// This function returns boundaries of chunks in the form of (start_index, end_index)
pub fn get_chunks(num_items_to_insert: usize, column_count: usize) -> Vec<(usize, usize)> {
    let max_item_size = MAX_PARAM_SIZE as usize / column_count;
    let mut chunk: (usize, usize) = (0, min(num_items_to_insert, max_item_size));
    let mut chunks = vec![chunk];
    while chunk.1 != num_items_to_insert {
        chunk = (
            chunk.0 + max_item_size,
            min(num_items_to_insert, chunk.1 + max_item_size),
        );
        chunks.push(chunk);
    }
    chunks
}
