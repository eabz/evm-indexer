use self::{clickhouse::ClickHouseClient, redis::RedisClient};

pub mod clickhouse;
pub mod redis;

pub struct Database {
    pub clickhouse: ClickHouseClient,
    pub redis: RedisClient,
}

impl Database {
    pub async fn new(config: &crate::config::IndexerConfig) -> Result<Self, anyhow::Error> {
        let clickhouse = ClickHouseClient::new(config).await?;
        let redis = RedisClient::new(config).await?;

        clickhouse.migrate().await?;

        Ok(Self {
            clickhouse,
            redis,
        })
    }
}
