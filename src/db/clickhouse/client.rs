use anyhow::Context;
use clickhouse::Client;
use log::info;

use crate::config::IndexerConfig;

pub struct ClickHouseClient {
    pub client: Client,
}

impl ClickHouseClient {
    pub async fn new(config: &IndexerConfig) -> Result<Self, anyhow::Error> {
        let client = Client::default()
            .with_url(config.database_host.clone())
            .with_user(config.database_user.clone())
            .with_password(config.database_password.clone())
            .with_database(config.database_name.clone())
            .with_validation(false);

        // Test connection with retry logic
        let mut retries = 0;
        let max_retries = 3;

        loop {
            match client.query("SELECT 1").fetch_one::<u8>().await {
                Ok(_) => {
                    info!("Successfully connected to ClickHouse");
                    break;
                },
                Err(e) => {
                    let error_msg = e.to_string();
                    retries += 1;

                    if retries >= max_retries {
                        return Err(anyhow::anyhow!(
                            "Failed to connect to ClickHouse after {} attempts",
                            max_retries,
                        ));
                    }

                    let delay = std::time::Duration::from_millis(100 * 2_u64.pow(retries));

                    log::warn!(
                        "Failed to connect to ClickHouse (attempt {}/{}), retrying in {:?}... Error: {}",
                        retries,
                        max_retries,
                        delay,
                        error_msg
                    );

                    tokio::time::sleep(delay).await;
                },
            }
        }

        Ok(Self {
            client,
        })
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        info!("Running ClickHouse migrations");

        let mut entries = Vec::new();
        let mut read_dir = tokio::fs::read_dir("migrations")
            .await
            .context("Failed to read migrations directory")?;

        while let Some(entry) = read_dir.next_entry().await? {
            entries.push(entry);
        }

        // Sort entries by filename to ensure correct execution order
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();

            if path.is_file() && path.extension().map_or(false, |ext| ext == "sql") {
                info!(
                    "Executing migration: {:?}",
                    path.file_name().unwrap_or_default()
                );

                let schema = tokio::fs::read_to_string(&path)
                    .await
                    .with_context(|| format!("Failed to read migration file: {:?}", path))?;

                for statement in schema.split(';') {
                    let stmt = statement.trim();
                    if stmt.is_empty() {
                        continue;
                    }
                    self.client.query(stmt).execute().await.with_context(|| {
                        format!(
                            "Failed to execute statement in {:?}: {}",
                            path.file_name().unwrap_or_default(),
                            stmt
                        )
                    })?;
                }
            }
        }

        info!("ClickHouse migrations completed successfully");

        Ok(())
    }
}
