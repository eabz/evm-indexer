use log::error;

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub chain_id: u64,
    pub hypersync_url: String,
    pub database_host: String,
    pub database_user: String,
    pub database_password: String,
    pub database_name: String,
    pub redis_url: String,
}

impl IndexerConfig {
    pub fn new() -> Self {
        let mut missing_vars = Vec::new();

        let chain_id_str = std::env::var("CHAIN_ID").unwrap_or_else(|_| "1".to_string());

        let chain_id: u64 = chain_id_str.parse().expect("Failed to parse CHAIN_ID");

        let hypersync_url = std::env::var("HYPERSYNC_URL")
            .ok()
            .or_else(|| get_default_hypersync_url(chain_id))
            .unwrap_or_else(|| {
                missing_vars.push("HYPERSYNC_URL");
                String::new()
            });

        let database_host = std::env::var("DATABASE_HOST").unwrap_or_else(|_| {
            missing_vars.push("DATABASE_HOST");
            String::new()
        });

        let database_user = std::env::var("DATABASE_USERNAME").unwrap_or_else(|_| {
            missing_vars.push("DATABASE_USERNAME");
            String::new()
        });

        let database_password = std::env::var("DATABASE_PASSWORD").unwrap_or_else(|_| {
            missing_vars.push("DATABASE_PASSWORD");
            String::new()
        });

        let database_name = std::env::var("DATABASE_NAME").unwrap_or_else(|_| {
            missing_vars.push("DATABASE_NAME");
            String::new()
        });

        let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| {
            missing_vars.push("REDIS_URL");
            String::new()
        });

        let mut invalid_vars = Vec::new();

        if !hypersync_url.is_empty() {
            if let Err(_) = url::Url::parse(&hypersync_url) {
                invalid_vars.push(format!(
                    "HYPERSYNC_URL: Invalid URL format '{}'",
                    hypersync_url
                ));
            }
        }

        if !redis_url.is_empty() {
            match url::Url::parse(&redis_url) {
                Ok(url) => {
                    if url.scheme() != "redis" && url.scheme() != "rediss" {
                        invalid_vars.push(format!(
                            "REDIS_URL: Invalid scheme '{}'. Must be 'redis://' or 'rediss://'",
                            url.scheme()
                        ));
                    }
                },
                Err(_) => {
                    invalid_vars.push(format!("REDIS_URL: Invalid URL format '{}'", redis_url))
                },
            }
        }

        if !missing_vars.is_empty() {
            error!("Missing environment variables: {:?}", missing_vars);
            std::process::exit(1);
        }

        if !invalid_vars.is_empty() {
            for error in invalid_vars {
                error!("Invalid configuration: {}", error);
            }
            std::process::exit(1);
        }

        Self {
            chain_id,
            hypersync_url,
            database_host,
            database_user,
            database_password,
            database_name,
            redis_url,
        }
    }
}

fn get_default_hypersync_url(chain_id: u64) -> Option<String> {
    match chain_id {
        1 => Some("https://eth.hypersync.xyz".to_string()),
        10 => Some("https://optimism.hypersync.xyz".to_string()),
        56 => Some("https://bsc.hypersync.xyz".to_string()),
        _ => None,
    }
}
