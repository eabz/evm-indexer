use std::time::Duration;

use crate::{chains::chains::Chain, config::config::Config};
use anyhow::Result;
use ethers::types::U256;
use jsonrpsee::core::{client::ClientT, rpc_params};
use jsonrpsee_http_client::{HttpClient, HttpClientBuilder};
use log::info;

#[derive(Debug, Clone)]
pub struct Rpc {
    pub clients: Vec<HttpClient>,
    pub chain: Chain,
}

impl Rpc {
    pub async fn new(config: &Config) -> Result<Self> {
        info!("Starting EVM rpc service");

        let timeout = Duration::from_secs(60);

        let mut clients = Vec::new();

        for rpc in config.rpcs.clone() {
            let client = HttpClientBuilder::default()
                .max_concurrent_requests(100000)
                .request_timeout(timeout)
                .build(rpc)
                .unwrap();

            let client_id = client.request("eth_chainId", rpc_params![]).await;

            match client_id {
                Ok(value) => {
                    let chain_id: U256 = match serde_json::from_value(value) {
                        Ok(value) => value,
                        Err(_) => continue,
                    };

                    if chain_id.as_u64() as i64 != config.chain.id {
                        continue;
                    }

                    clients.push(client);
                }
                Err(_) => continue,
            }
        }

        if clients.len() == 0 {
            panic!("No valid RPC client found");
        }

        Ok(Self {
            clients,
            chain: config.chain,
        })
    }
}
