use std::{cmp::min, collections::HashSet};

use crate::chains::chains::Chain;
use anyhow::Result;
use field_count::FieldCount;
use log::info;
use redis::Commands;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions, QueryBuilder,
};

use super::models::{chain_state::DatabaseChainIndexedState, token_detail::DatabaseTokenDetails};

pub const MAX_PARAM_SIZE: u16 = u16::MAX;

#[derive(Debug, Clone)]
pub struct Database {
    pub chain: Chain,
    pub redis: redis::Client,
    pub db_conn: sqlx::Pool<sqlx::Postgres>,
}

impl Database {
    pub async fn new(db_url: String, redis_url: String, chain: Chain) -> Result<Self> {
        info!("Starting EVM database service");

        let mut connect_options: PgConnectOptions = db_url.parse().unwrap();

        connect_options.disable_statement_logging();

        let db_conn = PgPoolOptions::new()
            .max_connections(500)
            .connect_with(connect_options)
            .await
            .expect("Unable to connect to the database");

        let redis = redis::Client::open(redis_url).expect("Unable to connect with Redis server");

        Ok(Self {
            chain,
            redis,
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

    pub async fn store_token_details(&self, tokens: &Vec<DatabaseTokenDetails>) -> Result<()> {
        let connection = self.get_connection();

        let chunks = get_chunks(tokens.len(), DatabaseTokenDetails::field_count());

        for (start, end) in chunks {
            let mut query_builder = QueryBuilder::new(
                "UPSERT INTO token_details (chain, token, name, symbol, decimals, token0, token1) ",
            );

            query_builder.push_values(&tokens[start..end], |mut row, token| {
                row.push_bind(token.chain)
                    .push_bind(token.token.clone())
                    .push_bind(token.name.clone())
                    .push_bind(token.symbol.clone())
                    .push_bind(token.decimals)
                    .push_bind(token.token0.clone())
                    .push_bind(token.token1.clone());
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
