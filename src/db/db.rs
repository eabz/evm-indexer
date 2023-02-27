use std::collections::HashSet;

use crate::chains::chains::Chain;
use anyhow::Result;
use log::info;
use redis::Commands;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions, QueryBuilder,
};

use super::models::chain_state::DatabaseChainIndexedState;

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

        // TODO: db migrations

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

        let blocks: HashSet<i64> = connection.smembers(self.chain.name).unwrap();

        Ok(blocks)
    }

    pub async fn store_indexed_blocks(&self, blocks: &Vec<i64>) -> Result<()> {
        let mut connection = self.redis.get_connection().unwrap();

        let _: () = connection.sadd(self.chain.name, blocks).unwrap();

        let full_chain_set = self.get_indexed_blocks().await.unwrap();

        self.update_indexed_blocks_number(&DatabaseChainIndexedState {
            chain: self.chain.id,
            indexed_blocks_amount: full_chain_set.len() as i64,
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
            "UPSERT INTO chains_indexed_state (chain, indexed_blocks_amount) VALUES ('{}', {})",
            chain_state.chain.clone(),
            chain_state.indexed_blocks_amount
        );

        QueryBuilder::new(query)
            .build()
            .execute(connection)
            .await
            .expect("Unable to update indexed blocks number");

        Ok(())
    }
}
