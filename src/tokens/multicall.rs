//! Token metadata fetching over JSON-RPC `eth_call`, batched through
//! Multicall3 with a fallback to individual calls.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{address, Address, Bytes},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    sol,
    sol_types::SolCall,
    transports::{RpcError, TransportErrorKind},
};
use anyhow::Context;
use futures::{future::BoxFuture, stream, StreamExt};
use log::{debug, warn};

use super::{decode, TokenStandard};

/// Multicall3, deployed at the same address on every chain.
pub const MULTICALL3_ADDRESS: Address =
    address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Tokens per `aggregate3` call (up to 3 sub-calls per token).
pub const MULTICALL_CHUNK_SIZE: usize = 50;

sol! {
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }

    struct Call3Result {
        bool success;
        bytes returnData;
    }

    function aggregate3(Call3[] calldata calls)
        external
        payable
        returns (Call3Result[] memory returnData);

    function name() external view returns (string);
    function symbol() external view returns (string);
    function decimals() external view returns (uint8);
}

/// Why an `eth_call` failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// The node could not be reached / was rate limited / timed out.
    /// Nothing can be concluded about the contract: retry later and never
    /// negatively cache.
    Transient(String),
    /// The node executed the call and the EVM failed (revert, out of gas,
    /// invalid opcode...). This is a property of the contract.
    Execution(String),
}

/// Minimal `eth_call` backend so the fetcher can be tested without a node.
pub trait EthCaller: Send + Sync + 'static {
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>>;
}

/// [`EthCaller`] over an alloy HTTP provider.
pub struct AlloyCaller {
    provider: DynProvider,
    timeout: Duration,
}

impl AlloyCaller {
    pub fn new(rpc_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        let url = rpc_url
            .parse()
            .with_context(|| "invalid rpc url for token metadata")?;

        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(url)
            .erased();

        Ok(Self { provider, timeout })
    }
}

impl EthCaller for AlloyCaller {
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>> {
        Box::pin(async move {
            let request =
                TransactionRequest::default().with_to(to).with_input(data);

            let call =
                self.provider.call(request).block(BlockId::latest());

            match tokio::time::timeout(self.timeout, call).await {
                Ok(Ok(bytes)) => Ok(bytes),
                Ok(Err(error)) => Err(classify_rpc_error(&error)),
                Err(_) => Err(CallError::Transient(format!(
                    "eth_call timed out after {:?}",
                    self.timeout
                ))),
            }
        })
    }
}

/// Decides whether an RPC error says something about the contract
/// (`Execution`) or only about the node / network (`Transient`).
///
/// Deliberately conservative: only JSON-RPC error *responses* that clearly
/// describe an EVM failure are `Execution`; anything ambiguous is
/// `Transient` so it can never poison the negative cache.
pub fn classify_rpc_error(
    error: &RpcError<TransportErrorKind>,
) -> CallError {
    match error {
        RpcError::ErrorResp(payload) if !payload.is_retry_err() => {
            if is_execution_error(payload.code, &payload.message) {
                CallError::Execution(payload.to_string())
            } else {
                CallError::Transient(payload.to_string())
            }
        }
        other => CallError::Transient(other.to_string()),
    }
}

fn is_execution_error(code: i64, message: &str) -> bool {
    // 3 is the geth/EIP-1474 code for "execution reverted".
    if code == 3 {
        return true;
    }

    let message = message.to_ascii_lowercase();

    [
        "revert",
        "out of gas",
        "invalid opcode",
        "invalid jump",
        "bad instruction",
        "bad jump",
        "stack underflow",
        "stack overflow",
        "stack limit",
        "vm execution error",
        "evm error",
        "execution error",
        "execution aborted",
        "gas required exceeds",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Sanitized metadata of one token. Fields that could not be read are
/// empty / zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenMetadata {
    pub name: String,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Symbol,
    Decimals,
}

impl Field {
    fn calldata(self) -> Bytes {
        match self {
            Field::Name => nameCall {}.abi_encode().into(),
            Field::Symbol => symbolCall {}.abi_encode().into(),
            Field::Decimals => decimalsCall {}.abi_encode().into(),
        }
    }
}

/// Which calls are worth making for a token standard. `decimals()` only
/// exists on ERC20.
fn fields_for(standard: TokenStandard) -> &'static [Field] {
    match standard {
        TokenStandard::Erc20 => {
            &[Field::Name, Field::Symbol, Field::Decimals]
        }
        TokenStandard::Erc721 | TokenStandard::Erc1155 => {
            &[Field::Name, Field::Symbol]
        }
    }
}

fn apply_field(metadata: &mut TokenMetadata, field: Field, data: &[u8]) {
    match field {
        Field::Name => {
            metadata.name = decode::decode_string(data).unwrap_or_default()
        }
        Field::Symbol => {
            metadata.symbol =
                decode::decode_string(data).unwrap_or_default()
        }
        Field::Decimals => {
            metadata.decimals =
                decode::decode_decimals(data).unwrap_or_default()
        }
    }
}

/// Tunables of the [`MetadataFetcher`].
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Tokens per `aggregate3` call.
    pub chunk_size: usize,
    /// `aggregate3` calls in flight at the same time.
    pub chunk_concurrency: usize,
    /// Tokens fetched concurrently when falling back to plain `eth_call`s.
    pub individual_concurrency: usize,
    /// Retries after a transient failure (so `max_retries + 1` attempts).
    pub max_retries: u32,
    /// First retry delay, doubled on every further retry.
    pub retry_backoff: Duration,
    /// Timeout of a single `eth_call`.
    pub call_timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            chunk_size: MULTICALL_CHUNK_SIZE,
            chunk_concurrency: 4,
            individual_concurrency: 8,
            max_retries: 3,
            retry_backoff: Duration::from_millis(500),
            call_timeout: Duration::from_secs(30),
        }
    }
}

/// Fetches token metadata through Multicall3 (or plain calls when the chain
/// has no Multicall3).
pub struct MetadataFetcher {
    caller: Arc<dyn EthCaller>,
    options: FetchOptions,
    multicall_missing: AtomicBool,
}

/// State shared by all chunks of one `fetch` call.
struct FetchRun {
    /// Set once a chunk exhausted its retries: the node is down, remaining
    /// chunks only get a single attempt instead of a full backoff cycle.
    gave_up: AtomicBool,
}

impl MetadataFetcher {
    pub fn new(caller: Arc<dyn EthCaller>, options: FetchOptions) -> Self {
        Self { caller, options, multicall_missing: AtomicBool::new(false) }
    }

    /// Fetches the metadata of `tokens`.
    ///
    /// A token is present in the result when its calls were *executed*,
    /// successfully or not (reverts / garbage yield empty fields: that is a
    /// definitive answer). A token is absent when the node could not be
    /// reached even after retries; the caller must not cache those.
    pub async fn fetch(
        &self,
        tokens: &[(Address, TokenStandard)],
    ) -> HashMap<Address, TokenMetadata> {
        let run = FetchRun { gave_up: AtomicBool::new(false) };
        let run = &run;

        let chunk_size = self.options.chunk_size.max(1);

        // Collected first: a lazily mapped stream of borrowing futures
        // makes the returned future not `Send` enough for `tokio::spawn`.
        let chunks: Vec<_> = tokens
            .chunks(chunk_size)
            .map(|chunk| self.fetch_chunk(chunk, run))
            .collect();

        let resolved: Vec<Vec<(Address, TokenMetadata)>> =
            stream::iter(chunks)
                .buffer_unordered(self.options.chunk_concurrency.max(1))
                .collect()
                .await;

        resolved.into_iter().flatten().collect()
    }

    async fn fetch_chunk(
        &self,
        chunk: &[(Address, TokenStandard)],
        run: &FetchRun,
    ) -> Vec<(Address, TokenMetadata)> {
        if self.multicall_missing.load(Ordering::Relaxed) {
            return self.fetch_individually(chunk, run).await;
        }

        let mut calls = Vec::with_capacity(chunk.len() * 3);
        for (token, standard) in chunk {
            for field in fields_for(*standard) {
                calls.push(Call3 {
                    target: *token,
                    allowFailure: true,
                    callData: field.calldata(),
                });
            }
        }
        let expected = calls.len();
        let calldata: Bytes = aggregate3Call { calls }.abi_encode().into();

        let returned = match self
            .call_with_retry(MULTICALL3_ADDRESS, calldata, run)
            .await
        {
            Ok(returned) => returned,
            Err(CallError::Transient(error)) => {
                warn!(
                    "Token metadata multicall failed for {} tokens, they \
                     will be retried when seen again: {error}",
                    chunk.len()
                );
                return Vec::new();
            }
            Err(CallError::Execution(error)) => {
                // allowFailure is set, so the aggregate itself failing
                // means a sub-call blew the gas / response limits.
                debug!(
                    "Token metadata multicall execution failed, fetching \
                     {} tokens individually: {error}",
                    chunk.len()
                );
                return self.fetch_individually(chunk, run).await;
            }
        };

        if returned.is_empty() {
            // eth_call to an address without code succeeds with no data.
            if !self.multicall_missing.swap(true, Ordering::Relaxed) {
                warn!(
                    "Multicall3 is not deployed at {MULTICALL3_ADDRESS} on \
                     this chain, falling back to individual eth_calls for \
                     token metadata"
                );
            }
            return self.fetch_individually(chunk, run).await;
        }

        let results = match aggregate3Call::abi_decode_returns(&returned) {
            Ok(results) if results.len() == expected => results,
            Ok(results) => {
                warn!(
                    "Multicall3 returned {} results for {expected} calls, \
                     fetching {} tokens individually",
                    results.len(),
                    chunk.len()
                );
                return self.fetch_individually(chunk, run).await;
            }
            Err(error) => {
                warn!(
                    "Unable to decode Multicall3 response, fetching {} \
                     tokens individually: {error}",
                    chunk.len()
                );
                return self.fetch_individually(chunk, run).await;
            }
        };

        let mut resolved = Vec::with_capacity(chunk.len());
        let mut suspicious = Vec::new();
        let mut results = results.iter();

        for (token, standard) in chunk {
            let mut metadata = TokenMetadata::default();
            let mut any_success = false;

            for (field, result) in
                fields_for(*standard).iter().zip(results.by_ref())
            {
                if result.success {
                    any_success = true;
                    apply_field(&mut metadata, *field, &result.returnData);
                }
            }

            // Every call of an ERC20/ERC721 failing is unusual: it may be
            // a victim of a gas-hungry neighbour in the same aggregate, so
            // double check it alone before it gets negatively cached.
            // (For ERC1155 it is the norm: name/symbol are not standard.)
            if !any_success && *standard != TokenStandard::Erc1155 {
                suspicious.push((*token, *standard));
            } else {
                resolved.push((*token, metadata));
            }
        }

        if !suspicious.is_empty() {
            resolved
                .extend(self.fetch_individually(&suspicious, run).await);
        }

        resolved
    }

    async fn fetch_individually(
        &self,
        tokens: &[(Address, TokenStandard)],
        run: &FetchRun,
    ) -> Vec<(Address, TokenMetadata)> {
        let calls: Vec<_> = tokens
            .iter()
            .map(|(token, standard)| async move {
                self.fetch_one(*token, *standard, run)
                    .await
                    .map(|metadata| (*token, metadata))
            })
            .collect();

        let resolved: Vec<Option<(Address, TokenMetadata)>> =
            stream::iter(calls)
                .buffer_unordered(
                    self.options.individual_concurrency.max(1),
                )
                .collect()
                .await;

        resolved.into_iter().flatten().collect()
    }

    /// `None` when the node could not be reached for any of the calls.
    async fn fetch_one(
        &self,
        token: Address,
        standard: TokenStandard,
        run: &FetchRun,
    ) -> Option<TokenMetadata> {
        let mut metadata = TokenMetadata::default();

        for field in fields_for(standard) {
            match self.call_with_retry(token, field.calldata(), run).await
            {
                Ok(data) => apply_field(&mut metadata, *field, &data),
                Err(CallError::Execution(_)) => {}
                Err(CallError::Transient(error)) => {
                    warn!(
                        "Token metadata call failed for {token}, it will \
                         be retried when seen again: {error}"
                    );
                    return None;
                }
            }
        }

        Some(metadata)
    }

    async fn call_with_retry(
        &self,
        to: Address,
        data: Bytes,
        run: &FetchRun,
    ) -> Result<Bytes, CallError> {
        let mut attempt: u32 = 0;

        loop {
            match self.caller.call(to, data.clone()).await {
                Err(CallError::Transient(error)) => {
                    if attempt >= self.options.max_retries
                        || run.gave_up.load(Ordering::Relaxed)
                    {
                        run.gave_up.store(true, Ordering::Relaxed);
                        return Err(CallError::Transient(error));
                    }

                    let delay = self
                        .options
                        .retry_backoff
                        .saturating_mul(2u32.saturating_pow(attempt));
                    debug!(
                        "Token metadata eth_call failed (attempt {}), \
                         retrying in {delay:?}: {error}",
                        attempt + 1
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! In-memory fake chain implementing [`EthCaller`], including a
    //! functional Multicall3.

    use super::*;
    use alloy::{primitives::U256, sol_types::SolValue};
    use std::sync::{atomic::AtomicUsize, Mutex};

    /// Raw return data of each metadata call, `None` = revert.
    #[derive(Debug, Clone, Default)]
    pub struct FakeToken {
        pub name: Option<Vec<u8>>,
        pub symbol: Option<Vec<u8>>,
        pub decimals: Option<Vec<u8>>,
    }

    impl FakeToken {
        pub fn erc20(name: &str, symbol: &str, decimals: u8) -> Self {
            Self {
                name: Some(name.to_string().abi_encode()),
                symbol: Some(symbol.to_string().abi_encode()),
                decimals: Some(U256::from(decimals).abi_encode()),
            }
        }

        pub fn nft(name: &str, symbol: &str) -> Self {
            Self {
                name: Some(name.to_string().abi_encode()),
                symbol: Some(symbol.to_string().abi_encode()),
                decimals: None,
            }
        }

        pub fn reverting() -> Self {
            Self::default()
        }
    }

    #[derive(Default)]
    pub struct FakeChain {
        pub tokens: Mutex<HashMap<Address, FakeToken>>,
        pub multicall_deployed: AtomicBool,
        /// Tokens whose presence makes the whole aggregate3 fail and that
        /// make every later sub-call fail too (gas bomb).
        pub gas_bombs: Mutex<Vec<Address>>,
        /// Number of upcoming calls that fail with a transient error.
        pub transient_failures: AtomicUsize,
        /// Every call fails with a transient error while set.
        pub offline: AtomicBool,
        pub multicall_calls: AtomicUsize,
        pub direct_calls: AtomicUsize,
        pub calls_per_token: Mutex<HashMap<Address, usize>>,
    }

    impl FakeChain {
        pub fn new() -> Arc<Self> {
            let chain = Self::default();
            chain.multicall_deployed.store(true, Ordering::SeqCst);
            Arc::new(chain)
        }

        pub fn add(&self, address: Address, token: FakeToken) {
            self.tokens.lock().unwrap().insert(address, token);
        }

        pub fn token_calls(&self, address: &Address) -> usize {
            self.calls_per_token
                .lock()
                .unwrap()
                .get(address)
                .copied()
                .unwrap_or(0)
        }

        fn execute(&self, to: Address, data: &[u8]) -> Option<Vec<u8>> {
            *self
                .calls_per_token
                .lock()
                .unwrap()
                .entry(to)
                .or_default() += 1;

            let tokens = self.tokens.lock().unwrap();
            // No code at the address: success with empty return data.
            let Some(token) = tokens.get(&to) else {
                return Some(Vec::new());
            };

            let selector: [u8; 4] = data.get(..4)?.try_into().ok()?;
            match selector {
                nameCall::SELECTOR => token.name.clone(),
                symbolCall::SELECTOR => token.symbol.clone(),
                decimalsCall::SELECTOR => token.decimals.clone(),
                _ => None,
            }
        }
    }

    impl EthCaller for FakeChain {
        fn call(
            &self,
            to: Address,
            data: Bytes,
        ) -> BoxFuture<'_, Result<Bytes, CallError>> {
            Box::pin(async move {
                // Yield so concurrent callers really interleave.
                tokio::task::yield_now().await;

                if self.offline.load(Ordering::SeqCst) {
                    return Err(CallError::Transient("offline".into()));
                }

                let failing = self
                    .transient_failures
                    .fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |n| n.checked_sub(1),
                    )
                    .is_ok();
                if failing {
                    return Err(CallError::Transient(
                        "rate limited".into(),
                    ));
                }

                if to != MULTICALL3_ADDRESS {
                    self.direct_calls.fetch_add(1, Ordering::SeqCst);
                    return self
                        .execute(to, &data)
                        .map(Bytes::from)
                        .ok_or_else(|| {
                            CallError::Execution(
                                "execution reverted".into(),
                            )
                        });
                }

                self.multicall_calls.fetch_add(1, Ordering::SeqCst);

                if !self.multicall_deployed.load(Ordering::SeqCst) {
                    return Ok(Bytes::new());
                }

                let decoded = aggregate3Call::abi_decode(&data)
                    .map_err(|e| CallError::Execution(e.to_string()))?;

                let bombs = self.gas_bombs.lock().unwrap().clone();
                let mut exploded = false;
                let mut results = Vec::with_capacity(decoded.calls.len());

                for call in &decoded.calls {
                    exploded |= bombs.contains(&call.target);
                    let output = if exploded {
                        None
                    } else {
                        self.execute(call.target, &call.callData)
                    };
                    results.push(Call3Result {
                        success: output.is_some(),
                        returnData: output.unwrap_or_default().into(),
                    });
                }

                Ok(aggregate3Call::abi_encode_returns(&results).into())
            })
        }
    }

    pub fn fast_options() -> FetchOptions {
        FetchOptions {
            retry_backoff: Duration::from_millis(1),
            ..FetchOptions::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use alloy::{primitives::U256, sol_types::SolValue};

    fn addr(n: u64) -> Address {
        Address::left_padding_from(&n.to_be_bytes())
    }

    fn fetcher(chain: &Arc<FakeChain>) -> MetadataFetcher {
        MetadataFetcher::new(chain.clone(), fast_options())
    }

    #[test]
    fn multicall_address_is_canonical() {
        assert_eq!(
            MULTICALL3_ADDRESS.to_checksum(None),
            "0xcA11bde05977b3631167028862bE2a173976CA11"
        );
    }

    #[test]
    fn selectors_match_the_standards() {
        assert_eq!(nameCall::SELECTOR, [0x06, 0xfd, 0xde, 0x03]);
        assert_eq!(symbolCall::SELECTOR, [0x95, 0xd8, 0x9b, 0x41]);
        assert_eq!(decimalsCall::SELECTOR, [0x31, 0x3c, 0xe5, 0x67]);
        assert_eq!(aggregate3Call::SELECTOR, [0x82, 0xad, 0x56, 0xcb]);
    }

    #[test]
    fn classifies_rpc_errors() {
        let resp = |code: i64, message: &'static str| {
            RpcError::<TransportErrorKind>::ErrorResp(
                serde_json::from_value(
                    serde_json::json!({ "code": code, "message": message }),
                )
                .unwrap(),
            )
        };

        let execution = [
            resp(3, "execution reverted"),
            resp(-32000, "execution reverted: nope"),
            resp(-32000, "out of gas"),
            resp(-32015, "VM execution error."),
            resp(-32000, "invalid opcode: INVALID"),
        ];
        for error in &execution {
            assert!(
                matches!(
                    classify_rpc_error(error),
                    CallError::Execution(_)
                ),
                "{error}"
            );
        }

        let transient = [
            resp(429, "Too many requests"),
            resp(-32005, "project rate limit exceeded"),
            resp(-32000, "header not found"),
            resp(-32603, "internal error"),
            resp(-32000, "something unexpected"),
            TransportErrorKind::http_error(502, "bad gateway".into()),
            TransportErrorKind::http_error(429, String::new()),
            TransportErrorKind::backend_gone(),
            TransportErrorKind::custom_str("connection refused"),
        ];
        for error in &transient {
            assert!(
                matches!(
                    classify_rpc_error(error),
                    CallError::Transient(_)
                ),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn fetches_through_multicall_in_chunks() {
        let chain = FakeChain::new();
        let mut tokens = Vec::new();
        for n in 0..120u64 {
            chain.add(
                addr(n),
                FakeToken::erc20(
                    &format!("Token {n}"),
                    &format!("T{n}"),
                    18,
                ),
            );
            tokens.push((addr(n), TokenStandard::Erc20));
        }

        let resolved = fetcher(&chain).fetch(&tokens).await;

        assert_eq!(resolved.len(), 120);
        assert_eq!(resolved[&addr(77)].name, "Token 77");
        assert_eq!(resolved[&addr(77)].symbol, "T77");
        assert_eq!(resolved[&addr(77)].decimals, 18);
        // 120 tokens / 50 per chunk.
        assert_eq!(chain.multicall_calls.load(Ordering::SeqCst), 3);
        assert_eq!(chain.direct_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn only_erc20_is_asked_for_decimals() {
        let chain = FakeChain::new();
        // An NFT that (wrongly) answers decimals(): must be ignored.
        chain.add(addr(1), FakeToken::erc20("Punks", "PUNK", 9));
        chain.add(addr(2), FakeToken::erc20("Items", "ITM", 9));
        chain.add(addr(3), FakeToken::erc20("Coin", "COIN", 9));

        let resolved = fetcher(&chain)
            .fetch(&[
                (addr(1), TokenStandard::Erc721),
                (addr(2), TokenStandard::Erc1155),
                (addr(3), TokenStandard::Erc20),
            ])
            .await;

        assert_eq!(resolved[&addr(1)].decimals, 0);
        assert_eq!(resolved[&addr(1)].name, "Punks");
        assert_eq!(resolved[&addr(2)].decimals, 0);
        assert_eq!(resolved[&addr(3)].decimals, 9);
        assert_eq!(chain.token_calls(&addr(1)), 2);
        assert_eq!(chain.token_calls(&addr(2)), 2);
        assert_eq!(chain.token_calls(&addr(3)), 3);
    }

    #[tokio::test]
    async fn decodes_bytes32_and_garbage_tokens() {
        let chain = FakeChain::new();

        let mut mkr = [0u8; 32];
        mkr[..3].copy_from_slice(b"MKR");
        let mut maker = [0u8; 32];
        maker[..5].copy_from_slice(b"Maker");
        chain.add(
            addr(1),
            FakeToken {
                name: Some(maker.to_vec()),
                symbol: Some(mkr.to_vec()),
                decimals: Some(U256::from(18u8).abi_encode()),
            },
        );
        chain.add(
            addr(2),
            FakeToken {
                name: Some(vec![0xde, 0xad]),
                symbol: Some("NUL\0SYM".to_string().abi_encode()),
                decimals: Some(U256::MAX.abi_encode()),
            },
        );

        let resolved = fetcher(&chain)
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(2), TokenStandard::Erc20),
            ])
            .await;

        assert_eq!(
            resolved[&addr(1)],
            TokenMetadata {
                name: "Maker".into(),
                symbol: "MKR".into(),
                decimals: 18
            }
        );
        assert_eq!(
            resolved[&addr(2)],
            TokenMetadata {
                name: String::new(),
                symbol: "NULSYM".into(),
                decimals: 0
            }
        );
    }

    #[tokio::test]
    async fn reverting_and_codeless_tokens_resolve_to_empty_metadata() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::reverting());
        // addr(2) has no code at all.
        chain.add(addr(3), FakeToken::reverting());

        let resolved = fetcher(&chain)
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(2), TokenStandard::Erc20),
                (addr(3), TokenStandard::Erc1155),
            ])
            .await;

        assert_eq!(resolved.len(), 3);
        assert!(resolved.values().all(|m| *m == TokenMetadata::default()));
        // The fully reverting ERC20 was double checked individually, the
        // ERC1155 (where that is expected) was not.
        assert_eq!(chain.token_calls(&addr(1)), 6);
        assert_eq!(chain.token_calls(&addr(2)), 3);
        assert_eq!(chain.token_calls(&addr(3)), 2);
    }

    #[tokio::test]
    async fn gas_bomb_neighbours_are_not_negatively_resolved() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::reverting());
        chain.add(addr(2), FakeToken::erc20("Victim", "VIC", 6));
        chain.gas_bombs.lock().unwrap().push(addr(1));

        let mut tokens = vec![
            (addr(1), TokenStandard::Erc20),
            (addr(2), TokenStandard::Erc20),
        ];
        tokens.sort();

        let resolved = fetcher(&chain).fetch(&tokens).await;

        assert_eq!(resolved[&addr(1)], TokenMetadata::default());
        assert_eq!(resolved[&addr(2)].name, "Victim");
        assert_eq!(resolved[&addr(2)].decimals, 6);
    }

    #[tokio::test]
    async fn falls_back_when_multicall_is_not_deployed() {
        let chain = FakeChain::new();
        chain.multicall_deployed.store(false, Ordering::SeqCst);

        let mut tokens = Vec::new();
        for n in 0..120u64 {
            chain.add(addr(n), FakeToken::erc20("Token", "TKN", 8));
            tokens.push((addr(n), TokenStandard::Erc20));
        }

        let fetcher = MetadataFetcher::new(
            chain.clone(),
            FetchOptions { chunk_concurrency: 1, ..fast_options() },
        );

        let resolved = fetcher.fetch(&tokens).await;
        assert_eq!(resolved.len(), 120);
        assert!(resolved.values().all(|m| m.decimals == 8));
        // Detected once, never tried again.
        assert_eq!(chain.multicall_calls.load(Ordering::SeqCst), 1);
        assert_eq!(chain.direct_calls.load(Ordering::SeqCst), 360);

        fetcher.fetch(&tokens).await;
        assert_eq!(chain.multicall_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_transient_failures_with_backoff() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        chain.transient_failures.store(3, Ordering::SeqCst);

        let resolved = fetcher(&chain)
            .fetch(&[(addr(1), TokenStandard::Erc20)])
            .await;

        assert_eq!(resolved[&addr(1)].symbol, "TKN");
        assert_eq!(chain.multicall_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gives_up_without_resolving_when_node_is_down() {
        let chain = FakeChain::new();
        let mut tokens = Vec::new();
        for n in 0..500u64 {
            chain.add(addr(n), FakeToken::erc20("Token", "TKN", 8));
            tokens.push((addr(n), TokenStandard::Erc20));
        }
        chain.offline.store(true, Ordering::SeqCst);

        let fetcher = fetcher(&chain);
        assert!(fetcher.fetch(&tokens).await.is_empty());
        // No multicall-missing misdetection because of the outage.
        assert!(!fetcher.multicall_missing.load(Ordering::SeqCst));

        chain.offline.store(false, Ordering::SeqCst);
        assert_eq!(fetcher.fetch(&tokens).await.len(), 500);
    }
}
