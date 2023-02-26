use crate::chains::chains::Chain;
use anyhow::Result;
use log::info;
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    ConnectOptions,
};

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
}
