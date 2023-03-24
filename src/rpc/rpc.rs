use crate::{
    chains::chains::Chain,
    configs::config::Config,
    db::models::{
        block::DatabaseBlock,
        contract::DatabaseContract,
        log::DatabaseLog,
        receipt::{DatabaseReceipt, TransactionStatus},
        token_detail::DatabaseTokenDetails,
        transaction::DatabaseTransaction,
    },
    utils::format::format_address,
};
use ethabi::{ethereum_types::H160, Address};
use ethers::{
    prelude::{abigen, Multicall, MulticallVersion},
    providers::{Http, Provider},
    types::{Block, Transaction, TransactionReceipt, U256},
};

use anyhow::Result;
use jsonrpsee::core::{client::ClientT, rpc_params};
use jsonrpsee_http_client::{HttpClient, HttpClientBuilder};
use log::info;
use rand::seq::SliceRandom;
use std::{str::FromStr, sync::Arc, time::Duration};

use serde_json::Error;

abigen!(
    ERC20,
    r#"[
        function name() external view returns (string)
        function symbol() external view returns (string)
        function decimals() external view returns (uint8)
        function token0() external view returns (address)
        function token1() external view returns (address)
        function factory() external view returns (address)
    ]"#,
);
#[derive(Debug, Clone)]
pub struct Rpc {
    pub clients: Vec<HttpClient>,
    pub clients_urls: Vec<String>,
    pub chain: Chain,
}

impl Rpc {
    pub async fn new(config: &Config) -> Result<Self> {
        info!("Starting rpc service");

        let timeout = Duration::from_secs(60);

        let mut clients = Vec::new();
        let mut clients_urls = Vec::new();

        for rpc in config.rpcs.iter() {
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
                    clients_urls.push(rpc.to_owned());
                }
                Err(_) => continue,
            }
        }

        if clients.len() == 0 {
            panic!("No valid rpc client found");
        }

        Ok(Self {
            clients,
            clients_urls,
            chain: config.chain,
        })
    }

    fn get_client(&self) -> &HttpClient {
        let client = self.clients.choose(&mut rand::thread_rng()).unwrap();
        return client;
    }

    fn get_client_url(&self) -> &String {
        let client = self.clients_urls.choose(&mut rand::thread_rng()).unwrap();
        return client;
    }

    pub async fn get_last_block(&self) -> Result<i64> {
        let client = self.get_client();

        let last_block = client.request("eth_blockNumber", rpc_params![]).await;

        match last_block {
            Ok(value) => {
                let block_number: U256 = serde_json::from_value(value)
                    .expect("Unable to deserialize eth_blockNumber response");

                Ok(block_number.as_u64() as i64)
            }
            Err(_) => Ok(0),
        }
    }

    pub async fn get_block(
        &self,
        block_number: &i64,
    ) -> Result<Option<(DatabaseBlock, Vec<DatabaseTransaction>)>> {
        let client = self.get_client();

        let raw_block = client
            .request(
                "eth_getBlockByNumber",
                rpc_params![format!("0x{:x}", block_number), true],
            )
            .await;

        match raw_block {
            Ok(value) => {
                let block: Result<Block<Transaction>, Error> = serde_json::from_value(value);

                match block {
                    Ok(block) => {
                        let db_block = DatabaseBlock::from_rpc(&block, self.chain.id);

                        let mut db_transactions = Vec::new();

                        for transaction in block.transactions {
                            let db_transaction = DatabaseTransaction::from_rpc(
                                transaction,
                                self.chain.id,
                                db_block.timestamp,
                            );

                            db_transactions.push(db_transaction)
                        }

                        Ok(Some((db_block, db_transactions)))
                    }
                    Err(_) => Ok(None),
                }
            }
            Err(_) => Ok(None),
        }
    }

    pub async fn get_transaction_receipt(
        &self,
        transaction: String,
        transaction_timestamp: i64,
    ) -> Result<Option<(DatabaseReceipt, Vec<DatabaseLog>, Option<DatabaseContract>)>> {
        let client = self.get_client();

        let raw_receipt = client
            .request("eth_getTransactionReceipt", rpc_params![transaction])
            .await;

        match raw_receipt {
            Ok(value) => {
                let receipt: Result<TransactionReceipt, Error> = serde_json::from_value(value);

                match receipt {
                    Ok(receipt) => {
                        let db_receipt = DatabaseReceipt::from_rpc(&receipt);

                        let mut db_transaction_logs: Vec<DatabaseLog> = Vec::new();

                        let status: TransactionStatus = match receipt.status {
                            None => TransactionStatus::Succeed,
                            Some(status) => {
                                let status_number = status.as_u64() as i64;

                                if status_number == 0 {
                                    TransactionStatus::Reverted
                                } else {
                                    TransactionStatus::Succeed
                                }
                            }
                        };

                        let mut db_contract: Option<DatabaseContract> = None;

                        if status == TransactionStatus::Succeed {
                            db_contract = match receipt.contract_address {
                                Some(_) => {
                                    Some(DatabaseContract::from_rpc(&receipt, self.chain.id))
                                }
                                None => None,
                            };
                        }

                        for log in receipt.logs.iter() {
                            let db_log =
                                DatabaseLog::from_rpc(log, self.chain.id, transaction_timestamp);

                            db_transaction_logs.push(db_log)
                        }

                        return Ok(Some((db_receipt, db_transaction_logs, db_contract)));
                    }
                    Err(_) => return Ok(None),
                }
            }
            Err(_) => return Ok(None),
        }
    }

    pub async fn get_block_receipts(
        &self,
        block_number: &i64,
        block_timestamp: i64,
    ) -> Result<
        Option<(
            Vec<DatabaseReceipt>,
            Vec<DatabaseLog>,
            Vec<DatabaseContract>,
        )>,
    > {
        let client = self.get_client();

        let raw_receipts = client
            .request(
                "eth_getBlockReceipts",
                rpc_params![format!("0x{:x}", block_number)],
            )
            .await;

        match raw_receipts {
            Ok(value) => {
                let receipts: Result<Vec<TransactionReceipt>, Error> =
                    serde_json::from_value(value);

                match receipts {
                    Ok(receipts) => {
                        let mut db_receipts: Vec<DatabaseReceipt> = Vec::new();

                        let mut db_transaction_logs: Vec<DatabaseLog> = Vec::new();

                        let mut db_contracts: Vec<DatabaseContract> = Vec::new();

                        for receipt in receipts {
                            let db_receipt = DatabaseReceipt::from_rpc(&receipt);

                            db_receipts.push(db_receipt);

                            let db_contract = match receipt.contract_address {
                                Some(_) => {
                                    Some(DatabaseContract::from_rpc(&receipt, self.chain.id))
                                }
                                None => None,
                            };

                            if db_contract.is_some() {
                                db_contracts.push(db_contract.unwrap())
                            }

                            for log in receipt.logs.iter() {
                                let db_log =
                                    DatabaseLog::from_rpc(log, self.chain.id, block_timestamp);

                                db_transaction_logs.push(db_log)
                            }
                        }

                        return Ok(Some((db_receipts, db_transaction_logs, db_contracts)));
                    }
                    Err(_) => return Ok(None),
                }
            }
            Err(_) => return Ok(None),
        }
    }

    pub async fn get_token_metadata(&self, token: String) -> Option<DatabaseTokenDetails> {
        let client = self.get_client_url();

        let provider = match Provider::<Http>::try_from(client) {
            Ok(provider) => provider,
            Err(_) => return None,
        };

        let client = Arc::new(provider);

        let token_contract = ERC20::new(token.parse::<Address>().unwrap(), Arc::clone(&client));

        let mut multicall = Multicall::new(
            client.clone(),
            Some(H160::from_str("0x5e1eE626420A354BbC9a95FeA1BAd4492e3bcB86").unwrap()),
        )
        .await
        .unwrap()
        .version(MulticallVersion::Multicall2);

        multicall
            .add_call(token_contract.name(), true)
            .add_call(token_contract.symbol(), true)
            .add_call(token_contract.token_0(), true)
            .add_call(token_contract.token_1(), true)
            .add_call(token_contract.factory(), true)
            .add_call(token_contract.decimals(), true);

        let data = multicall.call_raw().await.unwrap();

        let name_tuple = data[0].clone().into_tuple().unwrap();
        let symbol_tuple = data[1].clone().into_tuple().unwrap();
        let decimals_tuple = data[5].clone().into_tuple().unwrap();

        let token0_tuple = data[2].clone().into_tuple().unwrap();
        let token1_tuple = data[3].clone().into_tuple().unwrap();
        let factory_tuple = data[4].clone().into_tuple().unwrap();

        let mut name = "".to_string();

        let name_success = name_tuple[0].clone().into_bool().unwrap();
        if name_success {
            name = name_tuple[1].clone().into_string().unwrap();
        }

        let mut symbol = "".to_string();

        let symbol_success = symbol_tuple[0].clone().into_bool().unwrap();
        if symbol_success {
            symbol = symbol_tuple[1].clone().into_string().unwrap();
        }

        let mut decimals: i64 = 0;

        let decimals_success = decimals_tuple[0].clone().into_bool().unwrap();
        if decimals_success {
            decimals = decimals_tuple[1].clone().into_uint().unwrap().as_u64() as i64;
        }

        let mut token0: Option<String> = None;
        let token0_success = token0_tuple[0].clone().into_bool().unwrap();
        if token0_success {
            token0 = Some(format_address(
                token0_tuple[1].clone().into_address().unwrap(),
            ));
        }

        let mut token1: Option<String> = None;
        let token1_success = token1_tuple[0].clone().into_bool().unwrap();
        if token1_success {
            token1 = Some(format_address(
                token1_tuple[1].clone().into_address().unwrap(),
            ));
        }

        let mut factory: Option<String> = None;
        let factory_success = factory_tuple[0].clone().into_bool().unwrap();
        if factory_success {
            factory = Some(format_address(
                factory_tuple[1].clone().into_address().unwrap(),
            ));
        }

        return Some(DatabaseTokenDetails {
            token,
            chain: self.chain.id,
            name,
            decimals,
            symbol,
            token0,
            token1,
            factory,
        });
    }
}
