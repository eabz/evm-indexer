use crate::{
    chains::Chain,
    configs::Config,
    db::{
        models::{
            block::DatabaseBlock, contract::DatabaseContract,
            dex_trade::DatabaseDexTrade,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
            receipt::DatabaseReceipt, trace::DatabaseTrace,
            transaction::DatabaseTransaction,
        },
        BlockFetchedData, Database,
    },
    utils::{
        events::{
            ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
            ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
            SWAPV3_EVENT_SIGNATURE, SWAP_EVENT_SIGNATURE,
            TRANSFER_EVENTS_SIGNATURE,
        },
        format::decode_bytes,
    },
};
use ethabi::ParamType;
use ethers::{
    prelude::abigen,
    types::{Block, Trace, Transaction, TransactionReceipt, TxHash, U256},
};

use jsonrpsee::core::{
    client::{ClientT, Subscription, SubscriptionClientT},
    rpc_params,
};
use jsonrpsee_http_client::{
    transport::HttpBackend, HttpClient, HttpClientBuilder,
};
use jsonrpsee_ws_client::{WsClient, WsClientBuilder};

use log::{info, warn};
use rand::seq::SliceRandom;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tokio::time::sleep;

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
    pub clients: Vec<HttpClient<HttpBackend>>,
    pub clients_urls: Vec<String>,
    pub chain: Chain,
    pub ws_url: Option<String>,
}

impl Rpc {
    pub async fn new(config: &Config) -> Self {
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

            let client_id =
                client.request("eth_chainId", rpc_params![]).await;

            match client_id {
                Ok(value) => {
                    let chain_id: U256 =
                        match serde_json::from_value(value) {
                            Ok(value) => value,
                            Err(_) => continue,
                        };

                    if chain_id.as_u64() != config.chain.id {
                        continue;
                    }

                    clients.push(client);
                    clients_urls.push(rpc.to_owned());
                }
                Err(_) => continue,
            }
        }

        if clients.is_empty() {
            panic!("No valid rpc client found");
        }

        Self {
            clients,
            clients_urls,
            chain: config.chain.clone(),
            ws_url: config.ws_url.clone(),
        }
    }

    pub async fn get_last_block(&self) -> u64 {
        let client = self.get_client();

        let last_block =
            client.request("eth_blockNumber", rpc_params![]).await;

        match last_block {
            Ok(value) => {
                let block_number: U256 = serde_json::from_value(value)
                    .expect(
                        "Unable to deserialize eth_blockNumber response",
                    );

                block_number.as_u64()
            }
            Err(_) => 0,
        }
    }

    pub async fn fetch_block(
        &self,
        block_number: &u64,
        chain: &Chain,
    ) -> Option<(
        DatabaseBlock,
        Vec<DatabaseTransaction>,
        Vec<DatabaseReceipt>,
        Vec<DatabaseLog>,
        Vec<DatabaseContract>,
        Vec<DatabaseERC20Transfer>,
        Vec<DatabaseERC721Transfer>,
        Vec<DatabaseERC1155Transfer>,
        Vec<DatabaseDexTrade>,
        Vec<DatabaseTrace>,
    )> {
        let block_data: Option<(DatabaseBlock, Vec<DatabaseTransaction>)> =
            self.get_block(block_number).await;

        let traces: Vec<DatabaseTrace> =
            self.get_block_traces(block_number).await;

        match block_data {
            Some((db_block, db_transactions)) => {
                let total_block_transactions = db_transactions.len();

                // Make sure all the transactions are correctly formatted.
                if db_block.transactions != total_block_transactions as u64
                {
                    warn!(
                        "Missing {} transactions for block {}.",
                        db_block.transactions
                            - total_block_transactions as u64,
                        db_block.number
                    );
                    return None;
                }

                let mut db_receipts: Vec<DatabaseReceipt> = Vec::new();
                let mut db_logs: Vec<DatabaseLog> = Vec::new();
                let mut contracts_map: HashMap<String, DatabaseContract> =
                    HashMap::new();

                if chain.supports_blocks_receipts {
                    let receipts_data = self
                        .get_block_receipts(
                            block_number,
                            db_block.timestamp,
                        )
                        .await;

                    match receipts_data {
                        Some((mut receipts, mut logs, contracts)) => {
                            db_receipts.append(&mut receipts);
                            db_logs.append(&mut logs);
                            for contract in contracts {
                                contracts_map.insert(
                                    contract.contract_address.clone(),
                                    contract.clone(),
                                );
                            }
                        }
                        None => return None,
                    }
                } else {
                    for transaction in db_transactions.iter() {
                        let receipt_data = self
                            .get_transaction_receipt(
                                transaction.hash.clone(),
                                transaction.timestamp,
                            )
                            .await;

                        match receipt_data {
                            Some((receipt, mut logs, contract)) => {
                                db_receipts.push(receipt);
                                db_logs.append(&mut logs);
                                match contract {
                                    Some(contract) => {
                                        contracts_map.insert(
                                            contract
                                                .contract_address
                                                .clone(),
                                            contract.clone(),
                                        );
                                    }
                                    None => continue,
                                }
                            }
                            None => continue,
                        }
                    }
                }

                // Insert contracts created through the traces
                let create_traces: Vec<&DatabaseTrace> = traces
                    .iter()
                    .filter(|trace| trace.action_type == "create")
                    .collect();

                for trace in create_traces {
                    let contract_address = match &trace.address {
                        Some(contract_address) => contract_address,
                        None => continue,
                    };

                    if contracts_map.contains_key(contract_address) {
                        continue;
                    }

                    let contract = DatabaseContract {
                        block: trace.block_number,
                        contract_address: contract_address.to_string(),
                        chain: self.chain.id,
                        creator: trace.from.clone().unwrap(),
                        transaction_hash: trace
                            .transaction_hash
                            .clone()
                            .unwrap(),
                    };

                    contracts_map
                        .insert(contract_address.to_string(), contract);
                }

                if total_block_transactions != db_receipts.len() {
                    warn!(
                        "Missing receipts for block {}. Transactions {} receipts {}",
                        db_block.number,
                        total_block_transactions,
                        db_receipts.len()
                    );
                    return None;
                }

                let mut tokens_metadata_required: HashSet<String> =
                    HashSet::new();

                // filter only logs with topic
                let logs_scan: Vec<&DatabaseLog> = db_logs
                    .iter()
                    .filter(|log| log.topic0.is_some())
                    .collect();

                // insert all the tokens from the logs to metadata check
                for log in logs_scan.iter() {
                    let topic_0 = log.topic0.clone().unwrap();

                    if topic_0 == TRANSFER_EVENTS_SIGNATURE
                        || topic_0
                            == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
                        || topic_0
                            == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
                        || topic_0 == SWAPV3_EVENT_SIGNATURE
                        || topic_0 == SWAP_EVENT_SIGNATURE
                    {
                        tokens_metadata_required
                            .insert(log.address.clone());
                    }
                }

                let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> =
                    Vec::new();
                let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> =
                    Vec::new();
                let mut db_erc1155_transfers: Vec<
                    DatabaseERC1155Transfer,
                > = Vec::new();
                let mut db_dex_trades: Vec<DatabaseDexTrade> = Vec::new();

                for log in logs_scan.iter() {
                    // Check the first topic matches the erc20, erc721, erc1155 or a swap signatures
                    let topic0 = log.topic0.clone().unwrap();

                    if topic0 == TRANSFER_EVENTS_SIGNATURE {
                        // Check if it is a erc20 or a erc721 based on the number of logs

                        // erc721 token transfer events have 3 indexed values.
                        if log.topic3.is_some() {
                            let db_erc721_transfer =
                                DatabaseERC721Transfer::from_log(
                                    log, chain.id,
                                );

                            db_erc721_transfers.push(db_erc721_transfer);
                        } else if log.topic1.is_some()
                            && log.topic2.is_some()
                        {
                            // erc20 token transfer events have 2 indexed values.

                            let db_erc20_transfer =
                                DatabaseERC20Transfer::from_log(
                                    log, chain.id,
                                );

                            db_erc20_transfers.push(db_erc20_transfer);
                        }
                    }

                    if topic0 == ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                        && log.topic3.is_some()
                    {
                        let log_data = decode_bytes(log.data.clone());

                        let transfer_values = ethabi::decode(
                            &[ParamType::Uint(256), ParamType::Uint(256)],
                            &log_data[..],
                        )
                        .unwrap();

                        let id = transfer_values[0]
                            .clone()
                            .into_uint()
                            .unwrap();
                        let value = transfer_values[1]
                            .clone()
                            .into_uint()
                            .unwrap();

                        let db_erc1155_transfer =
                            DatabaseERC1155Transfer::from_log(
                                log, chain.id, id, value,
                            );

                        db_erc1155_transfers.push(db_erc1155_transfer)
                    }

                    if topic0 == ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                        && log.topic3.is_some()
                    {
                        let log_data = decode_bytes(log.data.clone());

                        let transfer_values = ethabi::decode(
                            &[
                                ParamType::Array(Box::new(
                                    ParamType::Uint(256),
                                )),
                                ParamType::Array(Box::new(
                                    ParamType::Uint(256),
                                )),
                            ],
                            &log_data[..],
                        )
                        .unwrap();

                        let transfer_ids: Vec<U256> = transfer_values[0]
                            .clone()
                            .into_array()
                            .unwrap()
                            .iter()
                            .map(|token| {
                                token.clone().into_uint().unwrap()
                            })
                            .collect();

                        let transfer_values: Vec<U256> = transfer_values
                            [1]
                        .clone()
                        .into_array()
                        .unwrap()
                        .iter()
                        .map(|token| token.clone().into_uint().unwrap())
                        .collect();

                        for (i, id) in transfer_ids.into_iter().enumerate()
                        {
                            let db_erc1155_transfer =
                                DatabaseERC1155Transfer::from_log(
                                    log,
                                    chain.id,
                                    id,
                                    transfer_values[i],
                                );

                            db_erc1155_transfers.push(db_erc1155_transfer)
                        }
                    }

                    if topic0 == SWAP_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                    {
                        let db_dex_trade =
                            DatabaseDexTrade::from_v2_log(log, chain.id);

                        db_dex_trades.push(db_dex_trade);
                    }

                    if topic0 == SWAPV3_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                    {
                        let db_dex_trade =
                            DatabaseDexTrade::from_v3_log(log, chain.id);

                        db_dex_trades.push(db_dex_trade);
                    }
                }

                let db_contracts: Vec<DatabaseContract> = contracts_map
                    .values()
                    .map(|value| value.to_owned())
                    .collect();

                info!(
                    "Found: txs ({}) receipts ({}) logs ({}) contracts ({}) transfers erc20 ({}) erc721 ({}) erc1155 ({}) trades ({}) traces ({}) for block {}.",
                    total_block_transactions,
                    db_receipts.len(),
                    db_logs.len(),
                    db_contracts.len(),
                    db_erc20_transfers.len(),
                    db_erc721_transfers.len(),
                    db_erc1155_transfers.len(),
                    db_dex_trades.len(),
                    traces.len(),
                    block_number,
                );

                Some((
                    db_block,
                    db_transactions,
                    db_receipts,
                    db_logs,
                    db_contracts,
                    db_erc20_transfers,
                    db_erc721_transfers,
                    db_erc1155_transfers,
                    db_dex_trades,
                    traces,
                ))
            }
            None => None,
        }
    }

    pub async fn listen_blocks(&self, db: &Database) {
        info!("Starting new blocks listener.");

        let client = self.get_ws_client().await;

        let client_id = client.request("eth_chainId", rpc_params![]).await;

        match client_id {
            Ok(value) => {
                let chain_id: U256 = match serde_json::from_value(value) {
                    Ok(value) => value,
                    Err(_) => {
                        panic!("unable to get chain id from websocket")
                    }
                };

                if chain_id.as_u64() != self.chain.id {
                    panic!("websocket chain id doesn't match with configured chain id")
                }
            }
            Err(_) => panic!("unable to access websocket"),
        }

        let mut subscription: Subscription<Block<TxHash>> = client
            .subscribe(
                "eth_subscribe",
                rpc_params!["newHeads"],
                "eth_unsubscribe",
            )
            .await
            .expect("unable to start block listener");

        while let Some(block) = subscription.next().await {
            if block.is_err() {
                continue;
            }
            tokio::spawn({
                let rpc = self.clone();
                let db = db.clone();
                let block = block.unwrap().clone();
                async move {
                    let block_number = block.number.unwrap().as_u64();

                    info!("New head found {}.", block_number.clone());

                    // Some chains require a small delay between receiving the head and fetching the block
                    // to allow the chain and nodes propagate and execute the block data.

                    // The list of chains to add delay should be added manually and tested
                    // Right now this is tested for ETH (1) and BSC (56)
                    // These values can change depending on network load

                    // ETH requires 300ms
                    if rpc.chain.id == 1 {
                        sleep(Duration::from_millis(300)).await;
                    }

                    // BSC requires 4s
                    if rpc.chain.id == 56 {
                        sleep(Duration::from_secs(4)).await;
                    }

                    let block_data =
                        rpc.fetch_block(&block_number, &rpc.chain).await;

                    if let Some((
                        block,
                        transactions,
                        receipts,
                        logs,
                        contracts,
                        erc20_transfers,
                        erc721_transfers,
                        erc1155_transfers,
                        dex_trades,
                        traces,
                    )) = block_data
                    {
                        let fetched_data = BlockFetchedData {
                            blocks: vec![block],
                            transactions,
                            receipts,
                            logs,
                            contracts,
                            erc20_transfers,
                            erc721_transfers,
                            erc1155_transfers,
                            dex_trades,
                            traces,
                        };

                        db.store_data(&fetched_data).await;
                    }
                }
            });
        }
    }

    fn get_client(&self) -> &HttpClient<HttpBackend> {
        let client = self.clients.choose(&mut rand::thread_rng()).unwrap();

        client
    }

    async fn get_ws_client(&self) -> WsClient {
        let url = self.ws_url.clone().unwrap();

        let client_wss: WsClient =
            WsClientBuilder::default().build(url).await.unwrap();

        client_wss
    }

    async fn get_block(
        &self,
        block_number: &u64,
    ) -> Option<(DatabaseBlock, Vec<DatabaseTransaction>)> {
        let client = self.get_client();

        let raw_block = client
            .request(
                "eth_getBlockByNumber",
                rpc_params![format!("0x{:x}", block_number), true],
            )
            .await;

        match raw_block {
            Ok(value) => {
                let block: Result<Block<Transaction>, Error> =
                    serde_json::from_value(value);

                match block {
                    Ok(block) => {
                        let db_block =
                            DatabaseBlock::from_rpc(&block, self.chain.id);

                        let mut db_transactions = Vec::new();

                        for transaction in block.transactions {
                            let db_transaction =
                                DatabaseTransaction::from_rpc(
                                    transaction,
                                    self.chain.id,
                                    db_block.timestamp,
                                );

                            db_transactions.push(db_transaction)
                        }

                        Some((db_block, db_transactions))
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    async fn get_block_traces(
        &self,
        block_number: &u64,
    ) -> Vec<DatabaseTrace> {
        let client = self.get_client();

        let raw_block = client
            .request(
                "trace_block",
                rpc_params![format!("0x{:x}", block_number)],
            )
            .await;

        match raw_block {
            Ok(value) => {
                let traces: Result<Vec<Trace>, Error> =
                    serde_json::from_value(value);

                match traces {
                    Ok(traces) => {
                        let mut db_traces = Vec::new();

                        for trace in traces.iter() {
                            let db_trace = DatabaseTrace::from_rpc(
                                trace,
                                self.chain.id,
                            );

                            db_traces.push(db_trace)
                        }

                        db_traces
                    }
                    Err(_) => Vec::new(),
                }
            }
            Err(_) => Vec::new(),
        }
    }

    async fn get_transaction_receipt(
        &self,
        transaction: String,
        transaction_timestamp: u64,
    ) -> Option<(
        DatabaseReceipt,
        Vec<DatabaseLog>,
        Option<DatabaseContract>,
    )> {
        let client = self.get_client();

        let raw_receipt = client
            .request("eth_getTransactionReceipt", rpc_params![transaction])
            .await;

        match raw_receipt {
            Ok(value) => {
                let receipt: Result<TransactionReceipt, Error> =
                    serde_json::from_value(value);

                match receipt {
                    Ok(receipt) => {
                        let db_receipt = DatabaseReceipt::from_rpc(
                            &receipt,
                            self.chain.id,
                        );

                        let mut db_transaction_logs: Vec<DatabaseLog> =
                            Vec::new();

                        let status: bool = match receipt.status {
                            None => true,
                            Some(status) => {
                                let status_number = status.as_u64() as i64;

                                status_number != 0
                            }
                        };

                        let mut db_contract: Option<DatabaseContract> =
                            None;

                        if status {
                            db_contract =
                                receipt.contract_address.map(|_| {
                                    DatabaseContract::from_rpc(
                                        &receipt,
                                        self.chain.id,
                                    )
                                });
                        }

                        for log in receipt.logs.iter() {
                            let db_log = DatabaseLog::from_rpc(
                                log,
                                self.chain.id,
                                transaction_timestamp,
                            );

                            db_transaction_logs.push(db_log)
                        }

                        Some((
                            db_receipt,
                            db_transaction_logs,
                            db_contract,
                        ))
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    async fn get_block_receipts(
        &self,
        block_number: &u64,
        block_timestamp: u64,
    ) -> Option<(
        Vec<DatabaseReceipt>,
        Vec<DatabaseLog>,
        Vec<DatabaseContract>,
    )> {
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
                        let mut db_receipts: Vec<DatabaseReceipt> =
                            Vec::new();

                        let mut db_transaction_logs: Vec<DatabaseLog> =
                            Vec::new();

                        let mut db_contracts: Vec<DatabaseContract> =
                            Vec::new();

                        for receipt in receipts {
                            let db_receipt = DatabaseReceipt::from_rpc(
                                &receipt,
                                self.chain.id,
                            );

                            db_receipts.push(db_receipt);

                            let db_contract =
                                receipt.contract_address.map(|_| {
                                    DatabaseContract::from_rpc(
                                        &receipt,
                                        self.chain.id,
                                    )
                                });

                            if db_contract.is_some() {
                                db_contracts.push(db_contract.unwrap())
                            }

                            for log in receipt.logs.iter() {
                                let db_log = DatabaseLog::from_rpc(
                                    log,
                                    self.chain.id,
                                    block_timestamp,
                                );

                                db_transaction_logs.push(db_log)
                            }
                        }

                        Some((
                            db_receipts,
                            db_transaction_logs,
                            db_contracts,
                        ))
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }
}
