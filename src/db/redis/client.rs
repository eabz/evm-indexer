use crate::config::IndexerConfig;
use log::info;

pub struct RedisClient {
    pub client: redis::Client,
}

impl RedisClient {
    pub async fn new(config: &IndexerConfig) -> anyhow::Result<Self> {
        let client = redis::Client::open(config.redis_url.clone())?;

        let mut con = client.get_multiplexed_async_connection().await?;
        let _: () = redis::cmd("PING").query_async(&mut con).await?;

        info!("Successfully connected to Redis");

        Ok(Self {
            client,
        })
    }
}
