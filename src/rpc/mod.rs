use crate::{
    configs::Config,
    db::{
        models::{
            block::DatabaseBlock, contract::DatabaseContract,
            dex_trade::DatabaseDexTrade,
            erc1155_transfer::DatabaseERC1155Transfer,
            erc20_transfer::DatabaseERC20Transfer,
            erc721_transfer::DatabaseERC721Transfer, log::DatabaseLog,
            token::DatabaseToken, trace::DatabaseTrace,
            transaction::DatabaseTransaction,
            withdrawal::DatabaseWithdrawal,
        },
        BlockFetchedData, Database,
    },
    utils::{
        dex_factories::DexRouters,
        events::{
            BALANCER_SWAP_EVENT_SIGNATURE,
            CURVE_TOKEN_EXCHANGE_EVENT_SIGNATURE,
            CURVE_TOKEN_EXCHANGE_UNDERLYING_EVENT_SIGNATURE,
            DODO_SWAP_EVENT_SIGNATURE,
            ERC1155_TRANSFER_BATCH_EVENT_SIGNATURE,
            ERC1155_TRANSFER_SINGLE_EVENT_SIGNATURE,
            KYBER_SWAPPED_EVENT_SIGNATURE,
            MAVERICK_SWAP_FILLED_EVENT_SIGNATURE,
            TRADERJOE_LB_SWAP_EVENT_SIGNATURE, TRANSFER_EVENTS_SIGNATURE,
            UNISWAP_V2_SWAP_EVENT_SIGNATURE,
            UNISWAP_V3_SWAP_EVENT_SIGNATURE, WOOFI_SWAP_EVENT_SIGNATURE,
        },
    },
};
use alloy::primitives::{Address, B256};
use alloy::providers::{
    Provider, ProviderBuilder, RootProvider, WsConnect,
};
use alloy::rpc::types::{
    BlockNumberOrTag, BlockTransactions, Transaction, TransactionReceipt,
};
use alloy::sol_types::SolCall;
use alloy::transports::http::Http;
use alloy_rpc_types_trace::parity::LocalizedTransactionTrace as Trace;
use futures::StreamExt;
use log::{debug, error, info, warn};
use rand::seq::SliceRandom;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use url::Url;

alloy::sol! {
    #[sol(rpc)]
    contract IERC20 {
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    contract IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        struct Result {
            bool success;
            bytes returnData;
        }

        function aggregate3(Call3[] calldata calls) public payable returns (Result[] memory returnData);
    }
}

#[derive(Clone)]
pub struct Rpc {
    pub chain_id: u64,
    pub clients: Vec<RootProvider<Http<Client>>>,
    pub clients_urls: Vec<String>,
    pub ws_url: Option<String>,
    pub traces: bool,
    pub supports_blocks_receipts: bool,
    pub fetch_uncles: bool,
    pub dex_routers: DexRouters,
    pub known_tokens: Arc<RwLock<HashSet<Address>>>,
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
                    if id != config.chain_id {
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

        let mut rpc = Self {
            chain_id: config.chain_id,
            clients,
            clients_urls,
            ws_url: config.ws_url.clone(),
            traces: config.traces,
            supports_blocks_receipts: false,
            fetch_uncles: config.fetch_uncles,
            dex_routers: DexRouters::new(),
            known_tokens: Arc::new(RwLock::new(HashSet::new())),
        };

        rpc.detect_capabilities().await;

        rpc
    }

    async fn detect_capabilities(&mut self) {
        info!("Detecting RPC capabilities for chain {}", self.chain_id);
        let start = std::time::Instant::now();

        let client = self.get_client();
        let latest_block = client.get_block_number().await;

        if let Ok(block_number) = latest_block {
            let receipts = client
                .raw_request::<_, Vec<serde_json::Value>>(
                    "eth_getBlockReceipts".into(),
                    [format!("0x{:x}", block_number)],
                )
                .await;

            self.supports_blocks_receipts = receipts.is_ok();

            let elapsed = start.elapsed();
            info!(
                "RPC capability detection completed in {:.2}s: eth_getBlockReceipts={}",
                elapsed.as_secs_f64(),
                self.supports_blocks_receipts
            );
        } else {
            warn!("Failed to detect RPC capabilities: unable to get latest block");
        }
    }

    pub async fn get_last_block(&self) -> u32 {
        debug!("Fetching latest block number for chain {}", self.chain_id);
        let client = self.get_client();

        let block = client.get_block_number().await.unwrap();
        let block_number = block.try_into().unwrap();
        debug!("Latest block: {}", block_number);
        block_number
    }

    pub async fn fetch_tokens_metadata(
        &self,
        tokens: &HashSet<Address>,
    ) -> Vec<DatabaseToken> {
        let mut new_tokens = Vec::new();

        // Filter out known tokens
        {
            let cache = self.known_tokens.read().unwrap();
            for token in tokens {
                if !cache.contains(token) {
                    new_tokens.push(*token);
                }
            }
        }

        if new_tokens.is_empty() {
            return Vec::new();
        }

        let mut db_tokens = Vec::new();
        let client = self.get_client();
        let multicall_address = Address::parse_checksummed(
            "0xcA11bde05977b3631167028862bE2a173976CA11",
            None,
        )
        .unwrap();
        let multicall =
            IMulticall3::new(multicall_address, client.clone());

        // Process in chunks to avoid gas limits
        for chunk in new_tokens.chunks(50) {
            let mut calls = Vec::new();
            for &token in chunk {
                // name()
                calls.push(IMulticall3::Call3 {
                    target: token,
                    allowFailure: true,
                    callData: IERC20::nameCall {}.abi_encode().into(),
                });
                // symbol()
                calls.push(IMulticall3::Call3 {
                    target: token,
                    allowFailure: true,
                    callData: IERC20::symbolCall {}.abi_encode().into(),
                });
                // decimals()
                calls.push(IMulticall3::Call3 {
                    target: token,
                    allowFailure: true,
                    callData: IERC20::decimalsCall {}.abi_encode().into(),
                });
            }

            let result = multicall.aggregate3(calls).call().await;

            match result {
                Ok(res) => {
                    let return_data = res.returnData;
                    for (i, token) in chunk.iter().enumerate() {
                        let name_res = &return_data[i * 3];
                        let symbol_res = &return_data[i * 3 + 1];
                        let decimals_res = &return_data[i * 3 + 2];

                        let name = if name_res.success {
                            IERC20::nameCall::abi_decode_returns(
                                &name_res.returnData,
                                true,
                            )
                            .map(|r| r._0)
                            .unwrap_or_default()
                        } else {
                            "".to_string()
                        };

                        let symbol = if symbol_res.success {
                            IERC20::symbolCall::abi_decode_returns(
                                &symbol_res.returnData,
                                true,
                            )
                            .map(|r| r._0)
                            .unwrap_or_default()
                        } else {
                            "".to_string()
                        };

                        let decimals = if decimals_res.success {
                            IERC20::decimalsCall::abi_decode_returns(
                                &decimals_res.returnData,
                                true,
                            )
                            .map(|r| r._0)
                            .unwrap_or(0)
                        } else {
                            0
                        };

                        let token_type =
                            if !name.is_empty() || !symbol.is_empty() {
                                "ERC20".to_string()
                            } else {
                                "Unknown".to_string()
                            };

                        db_tokens.push(DatabaseToken {
                            address: *token,
                            name,
                            symbol,
                            decimals,
                            r#type: token_type,
                            chain: self.chain_id,
                        });
                    }
                }
                Err(e) => {
                    error!("Multicall failed for chunk: {}", e);
                    // Fallback to individual fetching or just skip?
                    // For now, we skip but we don't add to cache so it might be retried.
                }
            }
        }

        // Update cache
        {
            let mut cache = self.known_tokens.write().unwrap();
            for token in &db_tokens {
                cache.insert(token.address);
            }
        }

        db_tokens
    }

    pub async fn fetch_block(
        &self,
        block_number: &u32,
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
        Vec<DatabaseToken>,
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
                    HashMap::with_capacity(total_block_transactions);

                let mut db_logs: Vec<DatabaseLog> = Vec::new();
                let mut contracts_map: HashMap<Address, DatabaseContract> =
                    HashMap::new();

                if self.supports_blocks_receipts {
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
                                    contract,
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
                                if let Some(contract) = contract {
                                    contracts_map.insert(
                                        contract.contract_address,
                                        contract,
                                    );
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
                        self.chain_id,
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
                    .filter(|trace| trace.action_type == "create")
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
                        chain: self.chain_id,
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

                // Decode DEX trades with automatic DEX detection
                let mut db_dex_trades: Vec<DatabaseDexTrade> = Vec::new();

                // Create mapping of transaction_hash -> to_address (router) for DEX detection
                let mut tx_routers: HashMap<B256, Address> =
                    HashMap::new();
                for tx in db_transactions.iter() {
                    tx_routers.insert(tx.hash, tx.to);
                }

                // Convert DatabaseLog to alloy Log for processing
                for log in db_logs.iter() {
                    let topic0 = log.topic0;

                    // Get router address for this transaction to detect DEX
                    let router = tx_routers.get(&log.transaction_hash);

                    // Detect DEX name from router address
                    let router_dex_name = if let Some(router_addr) = router
                    {
                        self.dex_routers
                            .get_dex_from_router(
                                self.chain_id,
                                router_addr,
                            )
                            .map(|info| info.display_name())
                    } else {
                        None
                    };

                    // Reconstruct alloy Log from DatabaseLog
                    let alloy_log = alloy::rpc::types::Log {
                        inner: alloy::primitives::Log {
                            address: log.address,
                            data: alloy::primitives::LogData::new(
                                vec![
                                    log.topic0.unwrap_or_default(),
                                    log.topic1.unwrap_or_default(),
                                    log.topic2.unwrap_or_default(),
                                    log.topic3.unwrap_or_default(),
                                ],
                                log.data.clone(),
                            )
                            .unwrap(),
                        },
                        block_hash: None,
                        block_number: Some(log.block_number as u64),
                        block_timestamp: None,
                        transaction_hash: Some(log.transaction_hash),
                        transaction_index: None,
                        log_index: Some(log.log_index as u64),
                        removed: false,
                    };

                    // Uniswap V2-style Swap (PancakeSwap, SushiSwap, QuickSwap, Solidly forks, etc.)
                    if topic0
                        == Some(
                            UNISWAP_V2_SWAP_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        // For V2 swaps, use chain-specific default DEX when router not detected
                        // This properly labels Solidly forks (Velodrome, Aerodrome, Thena, etc.)
                        let v2_dex_name =
                            router_dex_name.clone().unwrap_or_else(|| {
                                self.dex_routers
                                    .get_default_v2_dex(self.chain_id)
                                    .to_string()
                            });
                        if let Some(trade) =
                            DatabaseDexTrade::from_uniswap_v2_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                v2_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Uniswap V3-style Swap (PancakeSwap V3, Algebra forks, etc.)
                    if topic0
                        == Some(
                            UNISWAP_V3_SWAP_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        // For V3 swaps, use chain-specific default DEX when router not detected
                        let v3_dex_name =
                            router_dex_name.clone().unwrap_or_else(|| {
                                self.dex_routers
                                    .get_default_v3_dex(self.chain_id)
                                    .to_string()
                            });
                        if let Some(trade) =
                            DatabaseDexTrade::from_uniswap_v3_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                v3_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Curve TokenExchange
                    if topic0
                        == Some(
                            CURVE_TOKEN_EXCHANGE_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        if let Some(trade) =
                            DatabaseDexTrade::from_curve_token_exchange(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Balancer Swap
                    if topic0
                        == Some(
                            BALANCER_SWAP_EVENT_SIGNATURE.parse().unwrap(),
                        )
                    {
                        // Balancer has its own unique event, so fallback is always Balancer
                        let balancer_dex_name = router_dex_name
                            .clone()
                            .unwrap_or_else(|| "Balancer V2".to_string());
                        if let Some(trade) =
                            DatabaseDexTrade::from_balancer_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                balancer_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // DODO Swap
                    if topic0
                        == Some(DODO_SWAP_EVENT_SIGNATURE.parse().unwrap())
                    {
                        let dodo_dex_name = router_dex_name
                            .clone()
                            .unwrap_or_else(|| "DODO".to_string());
                        if let Some(trade) =
                            DatabaseDexTrade::from_dodo_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                dodo_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Kyber Swapped
                    if topic0
                        == Some(
                            KYBER_SWAPPED_EVENT_SIGNATURE.parse().unwrap(),
                        )
                    {
                        let kyber_dex_name = router_dex_name
                            .clone()
                            .unwrap_or_else(|| "Kyber".to_string());
                        if let Some(trade) =
                            DatabaseDexTrade::from_kyber_swapped(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                kyber_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Maverick SwapFilled
                    if topic0
                        == Some(
                            MAVERICK_SWAP_FILLED_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        let maverick_dex_name = router_dex_name
                            .clone()
                            .unwrap_or_else(|| "Maverick".to_string());
                        if let Some(trade) =
                            DatabaseDexTrade::from_maverick_swap_filled(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                maverick_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // Curve TokenExchangeUnderlying (meta pools)
                    if topic0
                        == Some(
                            CURVE_TOKEN_EXCHANGE_UNDERLYING_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        if let Some(trade) =
                            DatabaseDexTrade::from_curve_token_exchange_underlying(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // TraderJoe V2.1 LB Swap
                    if topic0
                        == Some(
                            TRADERJOE_LB_SWAP_EVENT_SIGNATURE
                                .parse()
                                .unwrap(),
                        )
                    {
                        let traderjoe_dex_name = router_dex_name
                            .clone()
                            .unwrap_or_else(|| "TraderJoe".to_string());
                        if let Some(trade) =
                            DatabaseDexTrade::from_traderjoe_lb_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                                traderjoe_dex_name,
                            )
                        {
                            db_dex_trades.push(trade);
                        }
                    }

                    // WooFi WooSwap
                    if topic0
                        == Some(
                            WOOFI_SWAP_EVENT_SIGNATURE.parse().unwrap(),
                        )
                    {
                        if let Some(trade) =
                            DatabaseDexTrade::from_woofi_swap(
                                &alloy_log,
                                self.chain_id,
                                log.block_number,
                                log.timestamp,
                                log.transaction_hash,
                                log.log_index,
                            )
                        {
                            db_dex_trades.push(trade);
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

                // Collect unique token addresses for metadata fetching
                let mut token_addresses = HashSet::new();
                for transfer in &db_erc20_transfers {
                    token_addresses.insert(transfer.token_address);
                }
                for transfer in &db_erc721_transfers {
                    token_addresses.insert(transfer.token_address);
                }
                for transfer in &db_erc1155_transfers {
                    token_addresses.insert(transfer.token_address);
                }

                let db_tokens =
                    self.fetch_tokens_metadata(&token_addresses).await;

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
                    db_tokens,
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

        if chain_id != self.chain_id {
            panic!("websocket chain id doesn't match with configured chain id")
        }

        // Detect capabilities on websocket connection
        let latest_block = client.get_block_number().await;
        let mut ws_supports_block_receipts = false;

        if let Ok(block_number) = latest_block {
            let receipts = client
                .raw_request::<_, Vec<serde_json::Value>>(
                    "eth_getBlockReceipts".into(),
                    [format!("0x{:x}", block_number)],
                )
                .await;

            ws_supports_block_receipts = receipts.is_ok();
            info!(
                "Websocket capability detection: eth_getBlockReceipts={}",
                ws_supports_block_receipts
            );
        } else {
            warn!("Failed to detect websocket capabilities: unable to get latest block");
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
                let supports_receipts = ws_supports_block_receipts;
                async move {
                    let block_number = block.header.number.unwrap() as u32;

                    info!("New head found {}.", block_number);

                    // Use websocket capability detection to optimize trace fetching
                    let mut rpc_with_ws_capability = rpc.clone();
                    rpc_with_ws_capability.supports_blocks_receipts =
                        supports_receipts;

                    // Retry logic for new heads - RPC may not have synced yet
                    let max_retries = 10;
                    let mut retry_delay =
                        std::time::Duration::from_millis(100);

                    for attempt in 1..=max_retries {
                        // Small delay before first attempt to let RPC sync
                        if attempt == 1 {
                            tokio::time::sleep(
                                std::time::Duration::from_millis(50),
                            )
                            .await;
                        }

                        let block_data = rpc_with_ws_capability
                            .fetch_block(&block_number)
                            .await;

                        match block_data {
                            Some((
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
                                tokens,
                            )) => {
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
                                    tokens,
                                };

                                db.store_data(&fetched_data).await;
                                break;
                            }
                            None => {
                                if attempt < max_retries {
                                    debug!(
                                        "Failed to fetch new head {} (attempt {}/{}), retrying in {:?}",
                                        block_number, attempt, max_retries, retry_delay
                                    );
                                    tokio::time::sleep(retry_delay).await;
                                    retry_delay *= 2; // Exponential backoff
                                } else {
                                    error!(
                                        "Failed to fetch new head {} after {} attempts, block will be picked up by indexer later",
                                        block_number, max_retries
                                    );
                                }
                            }
                        }
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
                        self.chain_id,
                        is_uncle,
                    );

                    let mut db_transactions: Vec<Transaction> = Vec::new();

                    if let BlockTransactions::Full(txs) =
                        &block.transactions
                    {
                        db_transactions.extend(txs.iter().cloned());
                    }

                    let mut db_withdrawals: Vec<DatabaseWithdrawal> =
                        Vec::new();

                    if let Some(withdrawals) = &block.withdrawals {
                        for withdrawal in withdrawals {
                            let db_withdrawal =
                                DatabaseWithdrawal::from_rpc(
                                    withdrawal,
                                    self.chain_id,
                                    db_block.number,
                                    db_block.timestamp,
                                );

                            db_withdrawals.push(db_withdrawal)
                        }
                    }
                    let mut block_uncles = Vec::new();

                    if self.fetch_uncles {
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
                                    self.chain_id,
                                    true,
                                );
                                block_uncles.push(db_block)
                            }
                        }
                    } else if !block.uncles.is_empty() {
                        debug!(
                            "Skipping {} uncle blocks for block {} (fetch_uncles=false)",
                            block.uncles.len(),
                            block_number
                        );
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
                        DatabaseTrace::from_rpc(trace, self.chain_id);

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
                        DatabaseContract::from_rpc(&receipt, self.chain_id)
                    });
                }

                for log in receipt.inner.logs() {
                    let db_log = DatabaseLog::from_rpc(
                        log,
                        self.chain_id,
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
                            self.chain_id,
                        ));
                    }

                    for log in receipt.inner.logs() {
                        let db_log = DatabaseLog::from_rpc(
                            log,
                            self.chain_id,
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
