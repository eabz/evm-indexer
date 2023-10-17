use crate::{
    chains::{get_block_reward, Chain},
    configs::Config,
    db::{
        models::{
            block::DatabaseBlock,
            contract::DatabaseContract,
            dex_trade::DatabaseDexTrade,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer,
            log::DatabaseLog,
            trace::{DatabaseTrace, TraceType},
            transaction::DatabaseTransaction,
            withdrawal::DatabaseWithdrawal,
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
        format::{decode_bytes, format_hash},
    },
};
use ethabi::ParamType;
use ethers::{
    abi::ethabi,
    prelude::abigen,
    types::{Block, Trace, Transaction, TransactionReceipt, TxHash},
};
use primitive_types::U256;

use jsonrpsee::{
    core::{
        client::{ClientT, Subscription, SubscriptionClientT},
        rpc_params,
    },
    tracing::debug,
};
use jsonrpsee_http_client::{
    transport::HttpBackend, HttpClient, HttpClientBuilder,
};
use jsonrpsee_ws_client::{WsClient, WsClientBuilder};

use log::{info, warn};
use rand::seq::SliceRandom;
use std::{collections::HashMap, ops::Mul, time::Duration};
use tokio::time::sleep;

use serde_json::Error;

abigen!(
    ERC20,
    r#"[
        function decimals() external view returns (uint8)
        function factory() external view returns (address)
        function name() external view returns (string)
        function symbol() external view returns (string)
        function token0() external view returns (address)
        function token1() external view returns (address)
    ]"#,
);
#[derive(Debug, Clone)]
pub struct Rpc {
    pub chain: Chain,
    pub clients: Vec<HttpClient<HttpBackend>>,
    pub clients_urls: Vec<String>,
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
            chain: config.chain.clone(),
            clients,
            clients_urls,
            ws_url: config.ws_url.clone(),
        }
    }

    pub async fn get_last_block(&self) -> u32 {
        let client = self.get_client();

        let last_block =
            client.request("eth_blockNumber", rpc_params![]).await;

        match last_block {
            Ok(value) => {
                let block_number: U256 = serde_json::from_value(value)
                    .expect(
                        "Unable to deserialize eth_blockNumber response",
                    );

                block_number.as_usize() as u32
            }
            Err(_) => 0,
        }
    }

    pub async fn fetch_block(
        &self,
        block_number: &u32,
        chain: &Chain,
    ) -> Option<(
        Vec<DatabaseBlock>,
        Vec<DatabaseTransaction>,
        Vec<DatabaseLog>,
        Vec<DatabaseContract>,
        Vec<DatabaseTrace>,
        Vec<DatabaseWithdrawal>,
        Vec<DatabaseERC20Transfer>,
        Vec<DatabaseERC721Transfer>,
        Vec<DatabaseERC1155Transfer>,
        Vec<DatabaseDexTrade>,
    )> {
        let block_data = self.get_block(block_number).await;

        let traces: Vec<DatabaseTrace> =
            self.get_block_traces(block_number).await;

        match block_data {
            Some((
                mut db_block,
                mut db_transactions,
                db_withdrawals,
                mut block_uncles,
            )) => {
                let total_block_transactions = db_transactions.len();

                // Make sure all the transactions are correctly formatted.
                if db_block.transactions != total_block_transactions as u16
                {
                    warn!(
                        "Missing {} transactions for block {}.",
                        db_block.transactions
                            - total_block_transactions as u16,
                        db_block.number
                    );
                    return None;
                }

                let mut db_receipts: HashMap<String, TransactionReceipt> =
                    HashMap::new();

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
                        Some((receipts, mut logs, contracts)) => {
                            for receipt in receipts {
                                db_receipts.insert(
                                    format_hash(receipt.transaction_hash),
                                    receipt,
                                );
                            }
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
                                block_number,
                            )
                            .await;

                        match receipt_data {
                            Some((receipt, mut logs, contract)) => {
                                db_receipts.insert(
                                    format_hash(receipt.transaction_hash),
                                    receipt,
                                );
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

                if total_block_transactions != db_receipts.len() {
                    warn!(
                        "Missing receipts for block {}. Transactions {} receipts {}",
                        db_block.number,
                        total_block_transactions,
                        db_receipts.len()
                    );
                    return None;
                }

                // TODO: add receipt data to transactions
                for transaction in db_transactions.iter_mut() {
                    let receipt = db_receipts
                        .get(&transaction.hash.clone())
                        .expect("unable to get receipt for transaction");

                    transaction.add_receipt_data(
                        db_block.base_fee_per_gas,
                        receipt,
                    );
                }

                let (base_block_reward, total_fee_reward, uncle_rewards) =
                    get_block_reward(
                        self.chain.id,
                        &db_block,
                        Some(&db_receipts),
                        &block_uncles,
                        false,
                        None,
                    );

                let burned = match db_block.base_fee_per_gas {
                    Some(base_fee_per_gas) => U256::from(base_fee_per_gas)
                        .mul(U256::from(db_block.gas_used)),
                    None => U256::zero(),
                };

                let mut db_blocks: Vec<DatabaseBlock> = Vec::new();

                db_block.add_rewards(
                    base_block_reward,
                    burned,
                    total_fee_reward,
                    uncle_rewards,
                );

                for uncle in block_uncles.iter_mut() {
                    let (
                        base_block_reward,
                        total_fee_reward,
                        uncle_rewards,
                    ) = get_block_reward(
                        self.chain.id,
                        uncle,
                        None,
                        &[],
                        true,
                        Some(db_block.number),
                    );

                    uncle.add_rewards(
                        base_block_reward,
                        U256::zero(),
                        total_fee_reward,
                        uncle_rewards,
                    );

                    db_blocks.push(uncle.to_owned());
                }

                db_blocks.push(db_block);

                // Insert contracts created through the traces
                let create_traces: Vec<&DatabaseTrace> = traces
                    .iter()
                    .filter(|trace| trace.action_type == TraceType::Create)
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
                        block_number: trace.block_number,
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

                let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> =
                    Vec::new();

                let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> =
                    Vec::new();

                let mut db_erc1155_transfers: Vec<
                    DatabaseERC1155Transfer,
                > = Vec::new();

                let mut db_dex_trades: Vec<DatabaseDexTrade> = Vec::new();

                for log in db_logs.iter_mut() {
                    // Check the first topic matches the erc20, erc721, erc1155 or a swap signatures
                    let topic0 = log.topic0.clone();

                    if topic0 == TRANSFER_EVENTS_SIGNATURE {
                        // Check if it is a erc20 or a erc721 based on the number of logs

                        // erc721 token transfer events have 3 indexed values.
                        if log.topic3.is_some() {
                            let erc721 =
                                DatabaseERC721Transfer::from_rpc(log);

                            db_erc721_transfers.push(erc721)
                        } else if log.topic1.is_some()
                            && log.topic2.is_some()
                        {
                            // erc20 token transfer events have 2 indexed values.
                            let erc20 =
                                DatabaseERC20Transfer::from_rpc(log);

                            db_erc20_transfers.push(erc20)
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

                        let amount = transfer_values[1]
                            .clone()
                            .into_uint()
                            .unwrap();

                        let erc1155_transfer =
                            DatabaseERC1155Transfer::from_single_rpc(
                                log, id, amount,
                            );

                        db_erc1155_transfers.push(erc1155_transfer);
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

                        let ids: Vec<U256> = transfer_values[0]
                            .clone()
                            .into_array()
                            .unwrap()
                            .iter()
                            .map(|token| {
                                token.clone().into_uint().unwrap()
                            })
                            .collect();

                        let amounts: Vec<U256> = transfer_values[1]
                            .clone()
                            .into_array()
                            .unwrap()
                            .iter()
                            .map(|token| {
                                token.clone().into_uint().unwrap()
                            })
                            .collect();

                        let erc1155_transfer =
                            DatabaseERC1155Transfer::from_batch_rpc(
                                log, ids, amounts,
                            );

                        db_erc1155_transfers.push(erc1155_transfer);
                    }

                    if topic0 == SWAP_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                    {
                        let swap = DatabaseDexTrade::from_v2_rpc(log);

                        db_dex_trades.push(swap);
                    }

                    if topic0 == SWAPV3_EVENT_SIGNATURE
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                    {
                        let swap = DatabaseDexTrade::from_v3_rpc(log);

                        db_dex_trades.push(swap);
                    }
                }

                let db_contracts: Vec<DatabaseContract> = contracts_map
                    .values()
                    .map(|value| value.to_owned())
                    .collect();

                debug!(
                    "Found: contracts ({}) logs ({}) traces ({}) transactions ({}) withdrawals ({}) for ({}) block.",
                    db_contracts.len(),
                    db_logs.len(),
                    traces.len(),
                    total_block_transactions,
                    db_withdrawals.len(),
                    block_number,
                );

                Some((
                    db_blocks,
                    db_transactions,
                    db_logs,
                    db_contracts,
                    traces,
                    db_withdrawals,
                    db_erc20_transfers,
                    db_erc721_transfers,
                    db_erc1155_transfers,
                    db_dex_trades,
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
                    let block_number =
                        block.number.unwrap().as_usize() as u32;

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
                        blocks,
                        transactions,
                        logs,
                        contracts,
                        traces,
                        withdrawals,
                        erc20_transfers,
                        erc721_transfers,
                        erc1155_transfers,
                        dex_trades,
                    )) = block_data
                    {
                        let fetched_data = BlockFetchedData {
                            blocks,
                            contracts,
                            logs,
                            traces,
                            transactions,
                            withdrawals,
                            erc20_transfers,
                            erc721_transfers,
                            erc1155_transfers,
                            dex_trades,
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
        block_number: &u32,
    ) -> Option<(
        DatabaseBlock,
        Vec<DatabaseTransaction>,
        Vec<DatabaseWithdrawal>,
        Vec<DatabaseBlock>,
    )> {
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
                        let db_block = DatabaseBlock::from_rpc(
                            &block,
                            self.chain.id,
                            false,
                        );

                        let mut db_transactions = Vec::new();

                        for transaction in block.transactions.iter() {
                            let db_transaction =
                                DatabaseTransaction::from_rpc(
                                    transaction,
                                    self.chain.id,
                                    db_block.timestamp,
                                );

                            db_transactions.push(db_transaction)
                        }

                        let mut db_withdrawals = Vec::new();

                        if block.withdrawals.is_some() {
                            for withdrawal in
                                block.withdrawals.unwrap().iter()
                            {
                                let db_withdrawal =
                                    DatabaseWithdrawal::from_rpc(
                                        withdrawal,
                                        self.chain.id,
                                        db_block.number,
                                        db_block.timestamp,
                                    );

                                db_withdrawals.push(db_withdrawal)
                            }
                        }

                        let mut block_uncles = Vec::new();

                        for (i, _) in db_block.uncles.iter().enumerate() {
                            let raw_uncle = client
                                .request(
                                    "eth_getUncleByBlockNumberAndIndex",
                                    rpc_params![
                                        format!("0x{:x}", db_block.number),
                                        format!("0x{:x}", i)
                                    ],
                                )
                                .await;
                            match raw_uncle {
                                Ok(value) => {
                                    let block: Result<
                                        Block<TxHash>,
                                        Error,
                                    > = serde_json::from_value(value);

                                    match block {
                                        Ok(block) => {
                                            let db_block =
                                                DatabaseBlock::from_rpc(
                                                    &block,
                                                    self.chain.id,
                                                    true,
                                                );

                                            block_uncles.push(db_block)
                                        }
                                        Err(err) => {
                                            println!("{}", err)
                                        }
                                    }
                                }
                                Err(err) => {
                                    println!("{}", err)
                                }
                            }
                        }

                        Some((
                            db_block,
                            db_transactions,
                            db_withdrawals,
                            block_uncles,
                        ))
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    async fn get_block_traces(
        &self,
        block_number: &u32,
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
        transaction_timestamp: u32,
        block_number: &u32,
    ) -> Option<(
        TransactionReceipt,
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
                                block_number,
                            );

                            db_transaction_logs.push(db_log)
                        }

                        Some((receipt, db_transaction_logs, db_contract))
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None,
        }
    }

    async fn get_block_receipts(
        &self,
        block_number: &u32,
        block_timestamp: u32,
    ) -> Option<(
        Vec<TransactionReceipt>,
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
                        let mut db_receipts: Vec<TransactionReceipt> =
                            Vec::new();

                        let mut db_transaction_logs: Vec<DatabaseLog> =
                            Vec::new();

                        let mut db_contracts: Vec<DatabaseContract> =
                            Vec::new();

                        for receipt in receipts {
                            let db_contract =
                                receipt.contract_address.map(|_| {
                                    DatabaseContract::from_rpc(
                                        &receipt,
                                        self.chain.id,
                                    )
                                });

                            match db_contract.is_some() {
                                true => {
                                    db_contracts.push(db_contract.unwrap())
                                }
                                false => (),
                            }

                            for log in receipt.logs.iter() {
                                let db_log = DatabaseLog::from_rpc(
                                    log,
                                    self.chain.id,
                                    block_timestamp,
                                    block_number,
                                );

                                db_transaction_logs.push(db_log)
                            }

                            db_receipts.push(receipt);
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
