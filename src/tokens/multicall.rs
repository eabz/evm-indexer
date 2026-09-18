//! Token metadata fetching over JSON-RPC `eth_call`, batched through
//! Multicall3 with a fallback to individual calls.

use std::{
    collections::HashMap,
    future::{Future, IntoFuture},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
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
use log::{debug, error, info, warn};

use super::{decode, redact::Redactor, TokenStandard};

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

/// Why an RPC request failed. The messages never contain URLs / API keys:
/// implementations must redact them (see [`Redactor`]).
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

/// Minimal RPC backend so the fetcher can be tested without a node.
pub trait EthCaller: Send + Sync + 'static {
    /// `eth_call` at the latest block.
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>>;

    /// `eth_chainId`.
    fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>>;
}

/// [`EthCaller`] over an alloy HTTP provider.
pub struct AlloyCaller {
    provider: DynProvider,
    timeout: Duration,
    redactor: Redactor,
}

impl AlloyCaller {
    pub fn new(rpc_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        // `url::ParseError` does not echo the (possibly secret) input.
        let url = rpc_url
            .parse()
            .with_context(|| "invalid rpc url for token metadata")?;

        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(url)
            .erased();

        Ok(Self {
            provider,
            timeout,
            redactor: Redactor::for_url(rpc_url),
        })
    }

    /// Applies the timeout and turns the error into a redacted
    /// [`CallError`].
    async fn request<T, F>(
        &self,
        what: &str,
        request: F,
    ) -> Result<T, CallError>
    where
        F: Future<Output = Result<T, RpcError<TransportErrorKind>>>,
    {
        match tokio::time::timeout(self.timeout, request).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => {
                Err(classify_rpc_error(&error, &self.redactor))
            }
            Err(_) => Err(CallError::Transient(format!(
                "{what} timed out after {:?}",
                self.timeout
            ))),
        }
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

            self.request("eth_call", call.into_future()).await
        })
    }

    fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
        Box::pin(async move {
            self.request(
                "eth_chainId",
                self.provider.get_chain_id().into_future(),
            )
            .await
        })
    }
}

/// Decides whether an RPC error says something about the contract
/// (`Execution`) or only about the node / network (`Transient`).
///
/// Deliberately conservative: only JSON-RPC error *responses* that clearly
/// describe an EVM failure are `Execution`; anything ambiguous is
/// `Transient` so it can never poison the negative cache.
///
/// The error text goes through `redactor`: reqwest errors include the full
/// request URL, which commonly embeds an API key.
pub fn classify_rpc_error(
    error: &RpcError<TransportErrorKind>,
    redactor: &Redactor,
) -> CallError {
    match error {
        RpcError::ErrorResp(payload) if !payload.is_retry_err() => {
            let message = redactor.redact(&payload.to_string());
            if is_execution_error(payload.code, &payload.message) {
                CallError::Execution(message)
            } else {
                CallError::Transient(message)
            }
        }
        other => CallError::Transient(redactor.redact(&other.to_string())),
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

/// Result of [`MetadataFetcher::fetch`]. Tokens in neither collection
/// could not be fetched (RPC unreachable) and must not be cached at all.
#[derive(Debug, Default)]
pub struct FetchOutcome {
    /// Tokens whose calls were *executed*: decoded metadata, or empty
    /// fields for reverts / garbage. Definitive, safe to cache forever.
    pub resolved: HashMap<Address, TokenMetadata>,
    /// Tokens for which every call returned zero bytes: there is no code
    /// at the address as far as this node knows. That is NOT definitive
    /// (node lagging behind the indexed head, wrong network...), so these
    /// must never be persisted, only skipped for a short while.
    pub empty: Vec<Address>,
}

enum TokenResult {
    Resolved(TokenMetadata),
    Empty,
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
    /// Timeout of a single RPC request.
    pub call_timeout: Duration,
    /// After a fetch gives up on the RPC, it is not contacted at all for
    /// this long. Doubles on every consecutive failure.
    pub breaker_cooldown: Duration,
    /// Upper bound of the doubling cool-down.
    pub breaker_max_cooldown: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            chunk_size: MULTICALL_CHUNK_SIZE,
            chunk_concurrency: 4,
            individual_concurrency: 8,
            max_retries: 3,
            retry_backoff: Duration::from_millis(500),
            call_timeout: Duration::from_secs(10),
            breaker_cooldown: Duration::from_secs(30),
            breaker_max_cooldown: Duration::from_secs(300),
        }
    }
}

/// Result of comparing the node's `eth_chainId` with the indexed chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainCheck {
    Verified,
    /// The node serves another chain (its id is attached).
    Mismatch(u64),
    /// The node could not be asked; checked again on the next fetch.
    Unavailable(String),
}

const CHAIN_UNCHECKED: u8 = 0;
const CHAIN_VERIFIED: u8 = 1;
const CHAIN_MISMATCH: u8 = 2;

struct Breaker {
    open_until: Option<Instant>,
    next_cooldown: Duration,
}

/// Fetches token metadata through Multicall3 (or plain calls when the chain
/// has no Multicall3).
///
/// A persistent circuit breaker protects the indexing pipeline from a dead
/// or hanging node: once a fetch gives up, the RPC is not contacted at all
/// during a (doubling) cool-down, and the first fetch after it is a single
/// attempt probe.
pub struct MetadataFetcher {
    caller: Arc<dyn EthCaller>,
    options: FetchOptions,
    multicall_missing: AtomicBool,
    expected_chain_id: Option<u64>,
    chain_state: AtomicU8,
    observed_chain_id: AtomicU64,
    breaker: Mutex<Breaker>,
    rpc_down: AtomicBool,
}

/// State shared by all the requests of one `fetch` call.
struct FetchRun {
    /// Set once a request exhausted its retries: the node is down, every
    /// remaining request of the run is skipped.
    gave_up: AtomicBool,
    /// The node answered at least one request.
    succeeded: AtomicBool,
    /// First fetch after an outage: no retries.
    probing: bool,
    last_error: Mutex<Option<String>>,
}

impl FetchRun {
    fn give_up(&self, error: &str) {
        self.gave_up.store(true, Ordering::Relaxed);
        let mut last_error =
            self.last_error.lock().unwrap_or_else(|e| e.into_inner());
        if last_error.is_none() {
            *last_error = Some(error.to_string());
        }
    }
}

impl MetadataFetcher {
    pub fn new(caller: Arc<dyn EthCaller>, options: FetchOptions) -> Self {
        let breaker = Breaker {
            open_until: None,
            next_cooldown: options.breaker_cooldown,
        };

        Self {
            caller,
            options,
            multicall_missing: AtomicBool::new(false),
            expected_chain_id: None,
            chain_state: AtomicU8::new(CHAIN_UNCHECKED),
            observed_chain_id: AtomicU64::new(0),
            breaker: Mutex::new(breaker),
            rpc_down: AtomicBool::new(false),
        }
    }

    /// Requires the node to serve `chain_id`: nothing is fetched until
    /// `eth_chainId` confirmed it, and never if it reports another chain.
    pub fn expect_chain_id(mut self, chain_id: u64) -> Self {
        self.expected_chain_id = Some(chain_id);
        self
    }

    /// Asks the node for its chain id (single attempt) and remembers a
    /// definitive answer.
    pub async fn check_chain_id(&self) -> ChainCheck {
        let Some(expected) = self.expected_chain_id else {
            return ChainCheck::Verified;
        };

        match self.chain_state.load(Ordering::Relaxed) {
            CHAIN_VERIFIED => return ChainCheck::Verified,
            CHAIN_UNCHECKED => {}
            _ => {
                return ChainCheck::Mismatch(
                    self.observed_chain_id.load(Ordering::Relaxed),
                )
            }
        }

        match self.caller.chain_id().await {
            Ok(actual) if actual == expected => {
                self.chain_state.store(CHAIN_VERIFIED, Ordering::Relaxed);
                ChainCheck::Verified
            }
            Ok(actual) => {
                self.observed_chain_id.store(actual, Ordering::Relaxed);
                if self.chain_state.swap(CHAIN_MISMATCH, Ordering::Relaxed)
                    != CHAIN_MISMATCH
                {
                    error!(
                        "Token metadata RPC serves chain {actual} but \
                         chain {expected} is being indexed: token metadata \
                         is disabled"
                    );
                }
                ChainCheck::Mismatch(actual)
            }
            Err(
                CallError::Transient(error) | CallError::Execution(error),
            ) => ChainCheck::Unavailable(error),
        }
    }

    /// `false` while the circuit breaker is open or the node is known to
    /// serve another chain: `fetch` would return nothing without any I/O.
    pub fn is_available(&self) -> bool {
        self.chain_state.load(Ordering::Relaxed) != CHAIN_MISMATCH
            && !self.breaker_open()
    }

    fn breaker(&self) -> MutexGuard<'_, Breaker> {
        self.breaker.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn breaker_open(&self) -> bool {
        self.breaker()
            .open_until
            .is_some_and(|until| Instant::now() < until)
    }

    fn trip_breaker(&self, error: &str) {
        let cooldown = {
            let mut breaker = self.breaker();
            let now = Instant::now();

            // A concurrent fetch already opened it for this failure.
            if breaker.open_until.is_some_and(|until| now < until) {
                return;
            }

            let cooldown = breaker.next_cooldown;
            breaker.open_until = Some(now + cooldown);
            breaker.next_cooldown = cooldown
                .saturating_mul(2)
                .min(self.options.breaker_max_cooldown)
                .max(self.options.breaker_cooldown);
            cooldown
        };

        if !self.rpc_down.swap(true, Ordering::Relaxed) {
            warn!(
                "Token metadata RPC is unavailable, token metadata is \
                 paused (next attempt in {cooldown:?}, affected tokens \
                 are retried when seen again): {error}"
            );
        } else {
            debug!(
                "Token metadata RPC still unavailable, next attempt in \
                 {cooldown:?}: {error}"
            );
        }
    }

    fn reset_breaker(&self) {
        {
            let mut breaker = self.breaker();
            breaker.open_until = None;
            breaker.next_cooldown = self.options.breaker_cooldown;
        }

        if self.rpc_down.swap(false, Ordering::Relaxed) {
            info!("Token metadata RPC recovered");
        }
    }

    /// Fetches the metadata of `tokens`, see [`FetchOutcome`].
    ///
    /// Never blocks for long on a dead node: requests are bounded by
    /// `call_timeout`, the first request exhausting its retries aborts the
    /// whole run, and the circuit breaker then short-circuits later runs.
    pub async fn fetch(
        &self,
        tokens: &[(Address, TokenStandard)],
    ) -> FetchOutcome {
        let mut outcome = FetchOutcome::default();

        if tokens.is_empty() || !self.is_available() {
            return outcome;
        }

        let run = FetchRun {
            gave_up: AtomicBool::new(false),
            succeeded: AtomicBool::new(false),
            probing: self.rpc_down.load(Ordering::Relaxed),
            last_error: Mutex::new(None),
        };
        let run = &run;

        match self.check_chain_id().await {
            ChainCheck::Verified => {}
            ChainCheck::Mismatch(_) => return outcome,
            ChainCheck::Unavailable(error) => {
                self.trip_breaker(&error);
                return outcome;
            }
        }

        let chunk_size = self.options.chunk_size.max(1);

        // Collected first: a lazily mapped stream of borrowing futures
        // makes the returned future not `Send` enough for `tokio::spawn`.
        let chunks: Vec<_> = tokens
            .chunks(chunk_size)
            .map(|chunk| self.fetch_chunk(chunk, run))
            .collect();

        let results: Vec<Vec<(Address, TokenResult)>> =
            stream::iter(chunks)
                .buffer_unordered(self.options.chunk_concurrency.max(1))
                .collect()
                .await;

        for (token, result) in results.into_iter().flatten() {
            match result {
                TokenResult::Resolved(metadata) => {
                    outcome.resolved.insert(token, metadata);
                }
                TokenResult::Empty => outcome.empty.push(token),
            }
        }

        if run.gave_up.load(Ordering::Relaxed) {
            let error = run
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| "unknown error".to_string());
            self.trip_breaker(&error);
        } else if run.succeeded.load(Ordering::Relaxed) {
            self.reset_breaker();
        }

        outcome
    }

    async fn fetch_chunk(
        &self,
        chunk: &[(Address, TokenStandard)],
        run: &FetchRun,
    ) -> Vec<(Address, TokenResult)> {
        if run.gave_up.load(Ordering::Relaxed) {
            return Vec::new();
        }

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
                // The outage itself is reported once by the breaker.
                debug!(
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
            let mut all_empty = true;

            for (field, result) in
                fields_for(*standard).iter().zip(results.by_ref())
            {
                all_empty &=
                    result.success && result.returnData.is_empty();

                if result.success {
                    any_success = true;
                    apply_field(&mut metadata, *field, &result.returnData);
                }
            }

            if all_empty {
                // No code at the address (yet): not a definitive answer.
                resolved.push((*token, TokenResult::Empty));
            } else if !any_success && *standard != TokenStandard::Erc1155 {
                // Every call of an ERC20/ERC721 failing is unusual: it may
                // be a victim of a gas-hungry neighbour in the same
                // aggregate, so double check it alone before it gets
                // negatively cached. (For ERC1155 it is the norm:
                // name/symbol are not part of the standard.)
                suspicious.push((*token, *standard));
            } else {
                resolved.push((*token, TokenResult::Resolved(metadata)));
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
    ) -> Vec<(Address, TokenResult)> {
        let calls: Vec<_> = tokens
            .iter()
            .map(|(token, standard)| async move {
                self.fetch_one(*token, *standard, run)
                    .await
                    .map(|result| (*token, result))
            })
            .collect();

        let resolved: Vec<Option<(Address, TokenResult)>> =
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
    ) -> Option<TokenResult> {
        let mut metadata = TokenMetadata::default();
        let mut all_empty = true;

        for field in fields_for(standard) {
            match self.call_with_retry(token, field.calldata(), run).await
            {
                Ok(data) => {
                    all_empty &= data.is_empty();
                    apply_field(&mut metadata, *field, &data);
                }
                Err(CallError::Execution(_)) => all_empty = false,
                Err(CallError::Transient(error)) => {
                    debug!(
                        "Token metadata call failed for {token}, it will \
                         be retried when seen again: {error}"
                    );
                    return None;
                }
            }
        }

        Some(if all_empty {
            TokenResult::Empty
        } else {
            TokenResult::Resolved(metadata)
        })
    }

    async fn call_with_retry(
        &self,
        to: Address,
        data: Bytes,
        run: &FetchRun,
    ) -> Result<Bytes, CallError> {
        let max_retries =
            if run.probing { 0 } else { self.options.max_retries };
        let mut attempt: u32 = 0;

        loop {
            // Another request of this run already gave up on the node.
            if run.gave_up.load(Ordering::Relaxed) {
                return Err(CallError::Transient(
                    "skipped, the RPC is unavailable".to_string(),
                ));
            }

            match self.caller.call(to, data.clone()).await {
                Err(CallError::Transient(error)) => {
                    if attempt >= max_retries {
                        run.give_up(&error);
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
                // Both mean the node is alive and executed the call.
                other => {
                    run.succeeded.store(true, Ordering::Relaxed);
                    return other;
                }
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
    use std::sync::atomic::AtomicUsize;

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
        /// Successful (executed) requests.
        pub multicall_calls: AtomicUsize,
        pub direct_calls: AtomicUsize,
        pub calls_per_token: Mutex<HashMap<Address, usize>>,
        /// Every `eth_call` attempt, including the failed ones.
        pub attempts: AtomicUsize,
        pub chain_id: AtomicU64,
        pub chain_id_calls: AtomicUsize,
    }

    impl FakeChain {
        pub fn new() -> Arc<Self> {
            let chain = Self::default();
            chain.multicall_deployed.store(true, Ordering::SeqCst);
            chain.chain_id.store(1, Ordering::SeqCst);
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
                self.attempts.fetch_add(1, Ordering::SeqCst);

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

        fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.chain_id_calls.fetch_add(1, Ordering::SeqCst);

                if self.offline.load(Ordering::SeqCst) {
                    return Err(CallError::Transient("offline".into()));
                }

                Ok(self.chain_id.load(Ordering::SeqCst))
            })
        }
    }

    /// No real waiting: tiny backoff and a circuit breaker that closes
    /// again immediately (breaker tests set their own cool-down).
    pub fn fast_options() -> FetchOptions {
        FetchOptions {
            retry_backoff: Duration::from_millis(1),
            breaker_cooldown: Duration::ZERO,
            breaker_max_cooldown: Duration::ZERO,
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
                    classify_rpc_error(error, &Redactor::default()),
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
                    classify_rpc_error(error, &Redactor::default()),
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

        let resolved = fetcher(&chain).fetch(&tokens).await.resolved;

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
            .await
            .resolved;

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
            .await
            .resolved;

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
    async fn reverting_tokens_resolve_to_empty_metadata() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::reverting());
        chain.add(addr(3), FakeToken::reverting());

        let outcome = fetcher(&chain)
            .fetch(&[
                (addr(1), TokenStandard::Erc20),
                (addr(3), TokenStandard::Erc1155),
            ])
            .await;

        assert!(outcome.empty.is_empty());
        assert_eq!(outcome.resolved.len(), 2);
        assert!(outcome
            .resolved
            .values()
            .all(|m| *m == TokenMetadata::default()));
        // The fully reverting ERC20 was double checked individually, the
        // ERC1155 (where that is expected) was not.
        assert_eq!(chain.token_calls(&addr(1)), 6);
        assert_eq!(chain.token_calls(&addr(3)), 2);
    }

    #[tokio::test]
    async fn codeless_addresses_are_unresolved_not_blank() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        // Non-empty garbage / partially empty answers stay definitive.
        chain.add(
            addr(5),
            FakeToken {
                name: Some(Vec::new()),
                symbol: Some(vec![0xde, 0xad]),
                decimals: None,
            },
        );
        // addr(2), addr(3), addr(4) have no code at all.
        let tokens = [
            (addr(1), TokenStandard::Erc20),
            (addr(2), TokenStandard::Erc20),
            (addr(3), TokenStandard::Erc721),
            (addr(4), TokenStandard::Erc1155),
            (addr(5), TokenStandard::Erc20),
        ];

        for multicall in [true, false] {
            chain.multicall_deployed.store(multicall, Ordering::SeqCst);

            let mut outcome = fetcher(&chain).fetch(&tokens).await;
            outcome.empty.sort();

            assert_eq!(outcome.empty, vec![addr(2), addr(3), addr(4)]);
            assert_eq!(outcome.resolved.len(), 2);
            assert_eq!(outcome.resolved[&addr(1)].symbol, "TKN");
            assert_eq!(
                outcome.resolved[&addr(5)],
                TokenMetadata::default()
            );
        }
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

        let resolved = fetcher(&chain).fetch(&tokens).await.resolved;

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

        let resolved = fetcher.fetch(&tokens).await.resolved;
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
            .await
            .resolved;

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
        let outcome = fetcher.fetch(&tokens).await;
        assert!(outcome.resolved.is_empty());
        assert!(outcome.empty.is_empty());
        // No multicall-missing misdetection because of the outage.
        assert!(!fetcher.multicall_missing.load(Ordering::SeqCst));

        chain.offline.store(false, Ordering::SeqCst);
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 500);
    }

    #[tokio::test]
    async fn remaining_chunks_are_skipped_once_a_run_gave_up() {
        let chain = FakeChain::new();
        let tokens: Vec<_> =
            (0..500u64).map(|n| (addr(n), TokenStandard::Erc20)).collect();
        chain.offline.store(true, Ordering::SeqCst);

        // 10 chunks, one at a time: only the first one may hit the node.
        let options =
            FetchOptions { chunk_concurrency: 1, ..fast_options() };
        let attempts_per_request = options.max_retries as usize + 1;
        let fetcher = MetadataFetcher::new(chain.clone(), options);

        fetcher.fetch(&tokens).await;
        assert_eq!(
            chain.attempts.load(Ordering::SeqCst),
            attempts_per_request
        );

        // With concurrency the chunks already in flight stop retrying.
        let chain = FakeChain::new();
        chain.offline.store(true, Ordering::SeqCst);
        let fetcher = MetadataFetcher::new(chain.clone(), fast_options());
        fetcher.fetch(&tokens).await;
        assert!(
            chain.attempts.load(Ordering::SeqCst)
                <= 4 * attempts_per_request
        );
    }

    fn breaker_options(cooldown: Duration) -> FetchOptions {
        FetchOptions {
            breaker_cooldown: cooldown,
            breaker_max_cooldown: cooldown * 4,
            ..fast_options()
        }
    }

    #[tokio::test]
    async fn circuit_breaker_skips_the_rpc_during_the_cooldown() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        let tokens = [(addr(1), TokenStandard::Erc20)];
        let cooldown = Duration::from_millis(60);
        let fetcher =
            MetadataFetcher::new(chain.clone(), breaker_options(cooldown));

        chain.offline.store(true, Ordering::SeqCst);
        assert!(fetcher.is_available());
        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        assert!(!fetcher.is_available());
        let attempts = chain.attempts.load(Ordering::SeqCst);
        assert_eq!(attempts, 4);

        // Open: no I/O at all, even though the node is back.
        chain.offline.store(false, Ordering::SeqCst);
        for _ in 0..10 {
            assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        }
        assert_eq!(chain.attempts.load(Ordering::SeqCst), attempts);

        // Cool-down over: the next fetch goes through and closes it.
        tokio::time::sleep(cooldown + Duration::from_millis(20)).await;
        assert!(fetcher.is_available());
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 1);
        assert!(!fetcher.rpc_down.load(Ordering::SeqCst));
        assert_eq!(fetcher.breaker().next_cooldown, cooldown);
    }

    #[tokio::test]
    async fn circuit_breaker_probes_once_and_doubles_the_cooldown() {
        let chain = FakeChain::new();
        chain.offline.store(true, Ordering::SeqCst);
        let tokens: Vec<_> =
            (0..200u64).map(|n| (addr(n), TokenStandard::Erc20)).collect();
        let cooldown = Duration::from_millis(20);
        let fetcher = MetadataFetcher::new(
            chain.clone(),
            FetchOptions {
                chunk_concurrency: 1,
                ..breaker_options(cooldown)
            },
        );

        // First failure: full retry cycle, breaker opens for `cooldown`.
        fetcher.fetch(&tokens).await;
        assert_eq!(chain.attempts.load(Ordering::SeqCst), 4);
        assert_eq!(fetcher.breaker().next_cooldown, cooldown * 2);

        // Following probes: a single attempt each, cool-down doubles up
        // to the maximum.
        let mut expected_attempts = 4;
        for expected_next in [cooldown * 4, cooldown * 4, cooldown * 4] {
            let wait = fetcher.breaker().next_cooldown;
            tokio::time::sleep(wait + Duration::from_millis(20)).await;
            fetcher.fetch(&tokens).await;
            expected_attempts += 1;
            assert_eq!(
                chain.attempts.load(Ordering::SeqCst),
                expected_attempts
            );
            assert_eq!(fetcher.breaker().next_cooldown, expected_next);
        }
    }

    #[tokio::test]
    async fn chain_id_is_verified_before_fetching() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        let tokens = [(addr(1), TokenStandard::Erc20)];

        // Matching chain: verified once, then never asked again.
        let fetcher = fetcher(&chain).expect_chain_id(1);
        assert_eq!(fetcher.check_chain_id().await, ChainCheck::Verified);
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 1);
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 1);
        assert_eq!(chain.chain_id_calls.load(Ordering::SeqCst), 1);

        // Wrong chain: nothing is ever fetched.
        let wrong = super::tests::fetcher(&chain).expect_chain_id(56);
        assert_eq!(wrong.check_chain_id().await, ChainCheck::Mismatch(1));
        let attempts = chain.attempts.load(Ordering::SeqCst);
        let outcome = wrong.fetch(&tokens).await;
        assert!(outcome.resolved.is_empty() && outcome.empty.is_empty());
        assert!(!wrong.is_available());
        assert_eq!(chain.attempts.load(Ordering::SeqCst), attempts);
        assert_eq!(wrong.check_chain_id().await, ChainCheck::Mismatch(1));
    }

    #[tokio::test]
    async fn chain_id_is_checked_lazily_when_the_node_was_down_at_startup()
    {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        let tokens = [(addr(1), TokenStandard::Erc20)];
        chain.offline.store(true, Ordering::SeqCst);

        let fetcher = fetcher(&chain).expect_chain_id(56);
        assert!(matches!(
            fetcher.check_chain_id().await,
            ChainCheck::Unavailable(_)
        ));

        // Still down: no eth_call before the chain is verified.
        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        assert_eq!(chain.attempts.load(Ordering::SeqCst), 0);

        // Back, but it is the wrong chain: found out on first use.
        chain.offline.store(false, Ordering::SeqCst);
        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        assert_eq!(chain.attempts.load(Ordering::SeqCst), 0);
        assert!(!fetcher.is_available());

        // Same story with the right chain: works once the node is back.
        let fetcher = super::tests::fetcher(&chain).expect_chain_id(1);
        chain.offline.store(true, Ordering::SeqCst);
        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        chain.offline.store(false, Ordering::SeqCst);
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 1);
    }

    #[test]
    fn rpc_errors_are_redacted() {
        let url = "https://eth-mainnet.g.alchemy.com/v2/SuPerSecretKey123";
        let redactor = Redactor::for_url(url);

        let errors = [
            TransportErrorKind::custom_str(&format!(
                "error sending request for url ({url}): connection refused"
            )),
            TransportErrorKind::http_error(
                401,
                "unknown api key SuPerSecretKey123".into(),
            ),
            RpcError::<TransportErrorKind>::ErrorResp(
                serde_json::from_value(serde_json::json!({
                    "code": -32000,
                    "message": format!("execution reverted at {url}"),
                }))
                .unwrap(),
            ),
        ];

        for error in &errors {
            assert!(error.to_string().contains("SuPerSecretKey123"));
            let (CallError::Transient(message)
            | CallError::Execution(message)) =
                classify_rpc_error(error, &redactor);
            assert!(!message.contains("SuPerSecretKey123"), "{message}");
            assert!(!message.contains("alchemy"), "{message}");
        }
    }

    #[tokio::test]
    async fn alloy_caller_errors_do_not_leak_the_url() {
        // Closed local port: reqwest reports the full URL in its error.
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let caller = AlloyCaller::new(
            &format!("http://127.0.0.1:{port}/v2/SuPerSecretKey123"),
            Duration::from_secs(5),
        )
        .unwrap();

        let Err(CallError::Transient(message)) = caller.chain_id().await
        else {
            panic!("expected a transport error");
        };
        assert!(!message.contains("SuPerSecretKey123"), "{message}");

        let Err(CallError::Transient(message)) =
            caller.call(addr(1), Bytes::new()).await
        else {
            panic!("expected a transport error");
        };
        assert!(!message.contains("SuPerSecretKey123"), "{message}");
    }
}
