use crate::{
    chains::Chain,
    configs::Config,
    db::{
        models::{
            block::DatabaseBlock,
            contract::DatabaseContract,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer,
            log::DatabaseLog,
            trace::{ActionType, DatabaseTrace},
            transaction::DatabaseTransaction,
            withdrawal::DatabaseWithdrawal,
        },
        BlockFetchedData, Database,
    },
    utils::events::{
        ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
        ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
        TRANSFER_EVENTS_SIGNATURE,
    },
};
use alloy::primitives::{Address, B256};
use alloy::providers::{
    Provider, ProviderBuilder, RootProvider, WsConnect,
};
use alloy::rpc::types::{
    BlockNumberOrTag, BlockTransactions, Transaction, TransactionReceipt,
};
use alloy::transports::http::Http;
use alloy_rpc_types_trace::parity::LocalizedTransactionTrace as Trace;
use futures::StreamExt;
use log::{debug, error, info, warn};
use rand::seq::SliceRandom;
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::sleep;
use url::Url;

#[derive(Clone)]
pub struct Rpc {
    pub chain: Chain,
    pub clients: Vec<RootProvider<Http<Client>>>,
    pub clients_urls: Vec<String>,
    pub ws_url: Option<String>,
    pub traces: bool,
}

impl Rpc {
    pub async fn new(config: &Config) -> Self {
        info!("Starting rpc service");

        let mut clients = Vec::new();
        let mut clients_urls = Vec::new();

        for rpc in config.rpcs.iter() {
            let url = Url::parse(rpc).expect("Invalid RPC URL");
            let client = ProviderBuilder::new().on_http(url);

            let chain_id = client.get_chain_id().await;

            match chain_id {
                Ok(id) => {
                    if id != config.chain.id {
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
            traces: config.traces,
        }
    }

    pub async fn get_last_block(&self) -> u32 {
        let client = self.get_client();

        match client.get_block_number().await {
            Ok(number) => number as u32,
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
    )> {
        let block_data = self.get_block(block_number).await;

        let mut traces: Vec<DatabaseTrace> = Vec::new();

        if self.traces {
            let fetched_traces: Vec<DatabaseTrace> =
                self.get_block_traces(block_number).await;

            traces = fetched_traces
        }

        match block_data {
            Some((
                db_block,
                raw_transactions,
                db_withdrawals,
                mut block_uncles,
            )) => {
                let total_block_transactions = raw_transactions.len();

                // Make sure all the transactions are correctly formatted.
                if db_block.transactions != total_block_transactions as u16
                {
                    warn!(
                        "Missing {} transactions for block {}. Actual: {}",
                        db_block.transactions
                            - total_block_transactions as u16,
                        db_block.number,
                        total_block_transactions
                    );
                    return None;
                }

                let mut db_receipts: HashMap<B256, TransactionReceipt> =
                    HashMap::new();

                let mut db_logs: Vec<DatabaseLog> = Vec::new();
                let mut contracts_map: HashMap<Address, DatabaseContract> =
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
                                    receipt.transaction_hash,
                                    receipt,
                                );
                            }
                            db_logs.append(&mut logs);
                            for contract in contracts {
                                contracts_map.insert(
                                    contract.contract_address,
                                    contract.clone(),
                                );
                            }
                        }
                        None => return None,
                    }
                } else {
                    for transaction in raw_transactions.iter() {
                        let receipt_data = self
                            .get_transaction_receipt(
                                transaction.hash,
                                db_block.timestamp,
                                block_number,
                            )
                            .await;

                        match receipt_data {
                            Some((receipt, mut logs, contract)) => {
                                db_receipts.insert(
                                    receipt.transaction_hash,
                                    receipt,
                                );
                                db_logs.append(&mut logs);
                                match contract {
                                    Some(contract) => {
                                        contracts_map.insert(
                                            contract.contract_address,
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

                // Re-create db_transactions with receipt data
                let mut db_transactions = Vec::new();

                for transaction in raw_transactions {
                    let receipt = db_receipts
                        .get(&transaction.hash)
                        .expect("unable to get receipt for transaction");

                    let db_transaction = DatabaseTransaction::from_rpc(
                        &transaction,
                        receipt,
                        self.chain.id,
                        db_block.timestamp,
                        db_block.base_fee_per_gas,
                    );

                    db_transactions.push(db_transaction)
                }

                let mut db_blocks: Vec<DatabaseBlock> = Vec::new();

                for uncle in block_uncles.iter_mut() {
                    db_blocks.push(uncle.to_owned());
                }

                db_blocks.push(db_block);

                // Insert contracts created through the traces
                let create_traces: Vec<&DatabaseTrace> = traces
                    .iter()
                    .filter(|trace| {
                        trace.action_type == ActionType::Create
                    })
                    .collect();

                for trace in create_traces {
                    let contract_address = match trace.address {
                        Some(contract_address) => contract_address,
                        None => continue,
                    };

                    if contracts_map.contains_key(&contract_address) {
                        continue;
                    }

                    let contract = DatabaseContract {
                        block_number: trace.block_number,
                        contract_address,
                        chain: self.chain.id,
                        creator: trace.from.unwrap(),
                        transaction_hash: trace.transaction_hash.unwrap(),
                    };

                    contracts_map.insert(contract_address, contract);
                }

                let mut db_erc20_transfers: Vec<DatabaseERC20Transfer> =
                    Vec::new();

                let mut db_erc721_transfers: Vec<DatabaseERC721Transfer> =
                    Vec::new();

                let mut db_erc1155_transfers: Vec<
                    DatabaseERC1155Transfer,
                > = Vec::new();

                for log in db_logs.iter_mut() {
                    // Check the first topic matches the erc20, erc721, erc1155 or a swap signatures
                    let topic0 = log.topic0;

                    if topic0
                        == Some(TRANSFER_EVENTS_SIGNATURE.parse().unwrap())
                    {
                        // Check if it is a erc20 or a erc721 based on the number of logs

                        // erc721 token transfer events have 3 indexed values.
                        if log.topic3.is_some() {
                            let erc721 =
                                DatabaseERC721Transfer::from_log(log);

                            if let Some(erc721) = erc721 {
                                db_erc721_transfers.push(erc721)
                            }
                        } else if log.topic1.is_some()
                            && log.topic2.is_some()
                        {
                            // erc20 token transfer events have 2 indexed values.
                            let erc20 =
                                DatabaseERC20Transfer::from_log(log);

                            if let Some(erc20) = erc20 {
                                db_erc20_transfers.push(erc20)
                            }
                        }
                    }

                    if topic0
                        == Some(
                            ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                        && log.topic1.is_some()
                        && log.topic2.is_some()
                        && log.topic3.is_some()
                    {
                        let erc1155_transfer =
                            DatabaseERC1155Transfer::from_log(log);

                        if let Some(erc1155_transfer) = erc1155_transfer {
                            db_erc1155_transfers.push(erc1155_transfer);
                        }
                    }

                    if topic0
                        == Some(
                            ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        let erc1155_transfer =
                            DatabaseERC1155Transfer::from_log(log);

                        if let Some(erc1155_transfer) = erc1155_transfer {
                            db_erc1155_transfers.push(erc1155_transfer);
                        }
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
                ))
            }
            None => None,
        }
    }

    pub async fn listen_blocks(&self, db: &Database) {
        info!("Starting new blocks listener.");

        let ws_url = self.ws_url.clone().unwrap();
        let ws_connect = WsConnect::new(ws_url);
        let client = ProviderBuilder::new()
            .on_ws(ws_connect)
            .await
            .expect("unable to connect to websocket");

        let chain_id = client
            .get_chain_id()
            .await
            .expect("unable to get chain id from websocket");

        if chain_id != self.chain.id {
            panic!("websocket chain id doesn't match with configured chain id")
        }

        let subscription = client
            .subscribe_blocks()
            .await
            .expect("unable to start block listener");
        let mut stream = subscription.into_stream();

        while let Some(block) = stream.next().await {
            tokio::spawn({
                let rpc = self.clone();
                let db = db.clone();
                let block = block.clone();
                async move {
                    let block_number = block.header.number.unwrap() as u32;

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
                        };

                        db.store_data(&fetched_data).await;
                    }
                }
            });
        }
    }

    fn get_client(&self) -> &RootProvider<Http<Client>> {
        let client = self.clients.choose(&mut rand::thread_rng()).unwrap();

        client
    }

    pub async fn get_block(
        &self,
        block_number: &u32,
    ) -> Option<(
        DatabaseBlock,
        Vec<Transaction>,
        Vec<DatabaseWithdrawal>,
        Vec<DatabaseBlock>,
    )> {
        let client = self.get_client();
        let block = client
            .get_block_by_number(
                BlockNumberOrTag::Number(*block_number as u64),
                true,
            )
            .await;

        match block {
            Ok(block) => match block {
                Some(block) => {
                    let is_uncle = false;
                    let db_block = DatabaseBlock::from_rpc(
                        &block,
                        self.chain.id,
                        is_uncle,
                    );

                    let mut db_transactions: Vec<Transaction> = Vec::new();

                    if let BlockTransactions::Full(txs) =
                        &block.transactions
                    {
                        for transaction in txs {
                            db_transactions.push(transaction.clone())
                        }
                    }

                    let mut db_withdrawals: Vec<DatabaseWithdrawal> =
                        Vec::new();

                    if let Some(withdrawals) = &block.withdrawals {
                        for withdrawal in withdrawals {
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

                    for (i, _) in block.uncles.iter().enumerate() {
                        let uncle = client
                            .get_uncle(
                                alloy::rpc::types::BlockId::Number(
                                    BlockNumberOrTag::Number(
                                        *block_number as u64,
                                    ),
                                ),
                                i as u64,
                            )
                            .await;

                        if let Ok(Some(block)) = uncle {
                            let db_block = DatabaseBlock::from_rpc(
                                &block,
                                self.chain.id,
                                true,
                            );
                            block_uncles.push(db_block)
                        }
                    }

                    Some((
                        db_block,
                        db_transactions,
                        db_withdrawals,
                        block_uncles,
                    ))
                }
                None => None,
            },
            Err(e) => {
                error!("Error fetching block: {:?}", e);
                None
            }
        }
    }

    async fn get_block_traces(
        &self,
        block_number: &u32,
    ) -> Vec<DatabaseTrace> {
        let client = self.get_client();

        // trace_block is not yet in standard Alloy provider trait in 0.1?
        // We use raw request
        let traces: Result<Vec<Trace>, _> = client
            .raw_request(
                "trace_block".into(),
                vec![format!("0x{:x}", block_number)],
            )
            .await;

        match traces {
            Ok(traces) => {
                let mut db_traces = Vec::new();

                for trace in traces.iter() {
                    let db_trace =
                        DatabaseTrace::from_rpc(trace, self.chain.id);

                    db_traces.push(db_trace)
                }

                db_traces
            }
            Err(_) => Vec::new(),
        }
    }

    async fn get_transaction_receipt(
        &self,
        transaction: B256,
        transaction_timestamp: u32,
        block_number: &u32,
    ) -> Option<(
        TransactionReceipt,
        Vec<DatabaseLog>,
        Option<DatabaseContract>,
    )> {
        let client = self.get_client();

        let receipt = client.get_transaction_receipt(transaction).await;

        match receipt {
            Ok(Some(receipt)) => {
                let mut db_transaction_logs: Vec<DatabaseLog> = Vec::new();

                let status = receipt.status();

                let mut db_contract: Option<DatabaseContract> = None;

                if status {
                    db_contract = receipt.contract_address.map(|_| {
                        DatabaseContract::from_rpc(&receipt, self.chain.id)
                    });
                }

                for log in receipt.inner.logs() {
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
            _ => None,
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

        // eth_getBlockReceipts might not be standard, use raw request
        let receipts: Result<Vec<TransactionReceipt>, _> = client
            .raw_request(
                "eth_getBlockReceipts".into(),
                vec![format!("0x{:x}", block_number)],
            )
            .await;

        match receipts {
            Ok(receipts) => {
                let mut db_logs: Vec<DatabaseLog> = Vec::new();
                let mut db_contracts: Vec<DatabaseContract> = Vec::new();

                for receipt in receipts.iter() {
                    let status = receipt.status();

                    if status && receipt.contract_address.is_some() {
                        db_contracts.push(DatabaseContract::from_rpc(
                            receipt,
                            self.chain.id,
                        ));
                    }

                    for log in receipt.inner.logs() {
                        let db_log = DatabaseLog::from_rpc(
                            log,
                            self.chain.id,
                            block_timestamp,
                            block_number,
                        );

                        db_logs.push(db_log)
                    }
                }

                Some((receipts, db_logs, db_contracts))
            }
            Err(_) => None,
        }
    }
}
