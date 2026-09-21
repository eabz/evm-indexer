//! Token metadata fetching over JSON-RPC `eth_call`, batched through
//! Multicall3 with a fallback to individual calls.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use alloy::{
    primitives::{address, Address, Bytes, U256},
    sol,
    sol_types::{SolCall, SolValue},
};
use futures::{future::BoxFuture, stream, StreamExt};
use log::{debug, error, info, warn};
use tokio::time::Instant;

pub use super::http::HttpCaller;
use super::{
    breaker::CircuitBreaker, decode, redact::Redactor, TokenStandard,
};

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

    function getBlockNumber() external view returns (uint256 blockNumber);

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

/// Answer of [`EthCaller::confirm_empty`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmptyCheck {
    /// Enough independent backends agree: the call returns no data.
    Confirmed,
    /// Another backend returned data for the very same call (attached):
    /// the empty answer came from a lagging / broken node.
    Refuted(Bytes),
    /// Not enough backends could be asked to tell.
    Undecided,
}

/// Point in time health of an [`EthCaller`] (for metrics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallerHealth {
    pub endpoints_total: usize,
    /// Endpoints on the right chain whose last request succeeded.
    pub endpoints_healthy: usize,
    /// Endpoints set aside because they serve another chain.
    pub endpoints_wrong_chain: usize,
    /// Endpoints banned for contradicting the others.
    pub endpoints_distrusted: usize,
}

/// Which node answered, and whether its word is enough.
///
/// Endpoints the operator configured are *trusted*: one answer is enough
/// for anything positive. Endpoints discovered from a public list are
/// not: whatever would be persisted from them needs a second, matching
/// answer from another `group` (an independent host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Source {
    /// The endpoint (for pinning follow-up calls to the same node).
    pub id: u64,
    /// Endpoints of one provider share a group: they are one opinion.
    pub group: u64,
    pub trusted: bool,
}

impl Source {
    /// The only source of a plain, single node [`EthCaller`].
    pub const SINGLE: Source = Source { id: 0, group: 0, trusted: true };
}

/// Restricts which node may answer a [`EthCaller::call_routed`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    /// Only this endpoint ([`Source::id`]).
    pub only: Option<u64>,
    /// None of these groups ([`Source::group`]).
    pub exclude_groups: Vec<u64>,
    /// A failure says nothing about the node (the token is a known
    /// troublemaker): do not open circuit breakers over it.
    pub quiet: bool,
}

impl Route {
    pub fn admits(&self, source: &Source) -> bool {
        self.only.is_none_or(|id| id == source.id)
            && !self.exclude_groups.contains(&source.group)
    }
}

/// Answer of [`EthCaller::call_routed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routed {
    pub data: Bytes,
    pub source: Source,
}

/// Failure of [`EthCaller::call_routed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedError {
    pub error: CallError,
    /// The node that failed (always set for `Execution`).
    pub source: Option<Source>,
    /// No node matches the route at all: retrying is pointless.
    pub no_source: bool,
}

impl RoutedError {
    pub fn no_source(what: &str) -> Self {
        Self {
            error: CallError::Transient(what.to_string()),
            source: None,
            no_source: true,
        }
    }
}

/// The two block heights a node can be asked for. They are compared
/// within their kind only: on some L2s `block.number` inside the EVM is
/// not the number `eth_blockNumber` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightKind {
    /// `Multicall3.getBlockNumber()`: the state the answer was read from.
    Evm,
    /// `eth_blockNumber` (same numbering as the indexed blocks).
    Rpc,
}

/// Outcome of comparing the answers of several sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    /// Another source gave the same answer.
    Won,
    /// Two other sources agreed on a different answer.
    Lost,
}

/// Minimal RPC backend so the fetcher can be tested without a node.
///
/// Only `call` and `chain_id` are required. The provided methods describe
/// a backend made of several nodes; their defaults describe a single,
/// trusted one.
pub trait EthCaller: Send + Sync + 'static {
    /// `eth_call` at the latest block.
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>>;

    /// `eth_chainId`.
    fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>>;

    /// Called after [`call`](Self::call) answered `to` / `data` with zero
    /// bytes and the caller is about to draw a *lasting* conclusion from
    /// it ("there is no contract there"). A backend made of several
    /// independent nodes cross-checks the answer; the default (a single
    /// node) has nobody else to ask.
    fn confirm_empty(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, EmptyCheck> {
        let _ = (to, data);
        Box::pin(async { EmptyCheck::Confirmed })
    }

    /// Health of the backend, `None` when it does not track any.
    fn health(&self) -> Option<CallerHealth> {
        None
    }

    /// [`call`](Self::call) that tells who answered and can be told who
    /// may answer.
    fn call_routed<'a>(
        &'a self,
        to: Address,
        data: Bytes,
        route: &'a Route,
    ) -> BoxFuture<'a, Result<Routed, RoutedError>> {
        Box::pin(async move {
            if !route.admits(&Source::SINGLE) {
                return Err(RoutedError::no_source(
                    "there is no other RPC endpoint to ask",
                ));
            }

            match self.call(to, data).await {
                Ok(data) => Ok(Routed { data, source: Source::SINGLE }),
                Err(error) => Err(RoutedError {
                    error,
                    source: Some(Source::SINGLE),
                    no_source: false,
                }),
            }
        })
    }

    /// Number of independent sources (groups) that could answer.
    fn source_count(&self) -> usize {
        1
    }

    /// Reports the block height a source answered at. `false`: the source
    /// lags behind the others (it has been put in cool-down) and its
    /// answer must not be used.
    fn observe_height(
        &self,
        source: Source,
        kind: HeightKind,
        height: u64,
    ) -> bool {
        let _ = (source, kind, height);
        true
    }

    /// The height the indexer has reached: a node behind it cannot know
    /// the contracts being asked about.
    fn head_hint(&self, block: u64) {
        let _ = block;
    }

    /// Reports how a source fared against the others.
    fn report_vote(&self, source: Source, vote: Vote) {
        let _ = (source, vote);
    }

    /// `eth_blockNumber`, `None` when the backend cannot tell.
    fn block_number(
        &self,
    ) -> BoxFuture<'_, Result<Option<u64>, CallError>> {
        Box::pin(async { Ok(None) })
    }
}

/// An `eth_call` whose answer is safe to store, for callers outside of
/// the token fetcher (the DEX pool resolver's `token0()` / `token1()` /
/// `fee()`): the same rules as for token metadata.
///
/// * A trusted (configured) source is believed on a non-empty answer.
/// * Anything from an untrusted (discovered) source, and any negative
///   answer (no data, execution failure) whenever another source exists,
///   needs the same raw answer from a source of another provider; when
///   two disagree a third decides and the loser is reported.
/// * Without agreement the result is `Transient`: ask again later and
///   store nothing. `Execution` is only returned once it is agreed upon.
pub async fn call_confirmed(
    caller: &dyn EthCaller,
    to: Address,
    data: Bytes,
) -> Result<Bytes, CallError> {
    type Answer = Result<Bytes, String>;

    async fn ask(
        caller: &dyn EthCaller,
        to: Address,
        data: Bytes,
        route: &Route,
    ) -> Result<(Answer, Source), CallError> {
        match caller.call_routed(to, data, route).await {
            Ok(routed) => Ok((Ok(routed.data), routed.source)),
            Err(RoutedError {
                error: CallError::Execution(error),
                source: Some(source),
                ..
            }) => Ok((Err(error), source)),
            Err(failure) => Err(match failure.error {
                CallError::Transient(error)
                | CallError::Execution(error) => {
                    CallError::Transient(error)
                }
            }),
        }
    }

    // Two execution failures agree whatever their wording.
    fn same(a: &Answer, b: &Answer) -> bool {
        match (a, b) {
            (Ok(a), Ok(b)) => a == b,
            (Err(_), Err(_)) => true,
            _ => false,
        }
    }

    let finish = |answer: Answer| answer.map_err(CallError::Execution);
    let unconfirmed = |why: &str| {
        Err(CallError::Transient(format!(
            "the answer could not be confirmed by a second, independent \
             RPC endpoint ({why})"
        )))
    };

    let (first, first_source) =
        ask(caller, to, data.clone(), &Route::default()).await?;

    let negative = first.as_ref().map_or(true, |data| data.is_empty());
    if first_source.trusted && !(negative && caller.source_count() > 1) {
        return finish(first);
    }

    let route = Route {
        exclude_groups: vec![first_source.group],
        ..Route::default()
    };
    let Ok((second, second_source)) =
        ask(caller, to, data.clone(), &route).await
    else {
        return unconfirmed("none is reachable");
    };

    if same(&first, &second) {
        caller.report_vote(first_source, Vote::Won);
        caller.report_vote(second_source, Vote::Won);
        return finish(first);
    }

    let route = Route {
        exclude_groups: vec![first_source.group, second_source.group],
        ..Route::default()
    };
    match ask(caller, to, data, &route).await {
        Ok((third, _)) if same(&third, &first) => {
            caller.report_vote(second_source, Vote::Lost);
            finish(first)
        }
        Ok((third, _)) if same(&third, &second) => {
            caller.report_vote(first_source, Vote::Lost);
            finish(second)
        }
        _ => unconfirmed("the endpoints disagree"),
    }
}

/// Decides whether a JSON-RPC error *response* says something about the
/// contract (`Execution`) or only about the node / network (`Transient`).
///
/// Deliberately conservative: only errors that clearly describe an EVM
/// failure are `Execution`; anything ambiguous is `Transient` so it can
/// never poison the negative cache.
///
/// The error text goes through `redactor`: nodes echo URLs and API keys.
pub fn classify_error_response(
    code: i64,
    message: &str,
    redactor: &Redactor,
) -> CallError {
    let text = redactor.redact(&format!("error code {code}: {message}"));

    if !is_node_side_error(code, message)
        && is_execution_error(code, message)
    {
        CallError::Execution(text)
    } else {
        CallError::Transient(text)
    }
}

/// Rate limits, timeouts and resource caps: conditions of the node, even
/// when they are worded like an execution failure.
fn is_node_side_error(code: i64, message: &str) -> bool {
    if matches!(code, 429 | -32005 | -32007 | -32012 | -32016) {
        return true;
    }

    let message = message.to_ascii_lowercase();

    [
        "rate limit",
        "too many requests",
        "timeout",
        "timed out",
        "capacity",
        "limit exceeded",
        // "execution aborted (timeout = 5s)", "gas required exceeds
        // allowance": the node gave up, another node may not.
        "execution aborted",
        "gas required exceeds",
    ]
    .iter()
    .any(|needle| message.contains(needle))
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
    /// Tokens that were answered but not by enough independent sources
    /// (no second endpoint reachable, or the endpoints disagree). Nothing
    /// may be stored for them; they are worth trying again later.
    pub unconfirmed: Vec<Address>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenResult {
    Resolved(TokenMetadata),
    Empty,
}

impl TokenResult {
    /// "Nothing there": the kind of answer a stale or lying node gives,
    /// and the kind that is stored for good.
    fn is_negative(&self) -> bool {
        match self {
            TokenResult::Empty => true,
            TokenResult::Resolved(metadata) => {
                metadata.name.is_empty() && metadata.symbol.is_empty()
            }
        }
    }
}

/// What one source says about a set of tokens.
struct Opinion {
    source: Source,
    verdicts: HashMap<Address, TokenResult>,
}

#[derive(Default)]
struct ChunkOutcome {
    accepted: Vec<(Address, TokenResult)>,
    unconfirmed: Vec<Address>,
}

/// An answer is thrown away and asked again elsewhere at most this often
/// (stale node, node without Multicall3).
const MAX_REROUTES: usize = 3;

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
    /// A confirmed "Multicall3 is not deployed" is checked again after
    /// this long (it may get deployed, the nodes may all have lagged).
    pub multicall_recheck: Duration,
    /// How long individual calls are used when the absence of Multicall3
    /// could not be cross-checked (see [`EthCaller::confirm_empty`]).
    pub multicall_undecided_ttl: Duration,
    /// "The RPC serves another chain" is checked again after this long.
    pub chain_recheck: Duration,
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
            multicall_recheck: Duration::from_secs(3_600),
            multicall_undecided_ttl: Duration::from_secs(60),
            chain_recheck: Duration::from_secs(3_600),
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
    /// Individual calls are used instead of Multicall3 until then.
    multicall_missing_until: Mutex<Option<Instant>>,
    multicall_missing_logged: AtomicBool,
    expected_chain_id: Option<u64>,
    chain_state: AtomicU8,
    observed_chain_id: AtomicU64,
    /// When the chain id mismatch was seen (it is not a life sentence).
    mismatch_at: Mutex<Option<Instant>>,
    breaker: CircuitBreaker,
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
    /// Failures are expected (troublemaker token): no breaker over them.
    quiet: bool,
    /// Answers could not be confirmed for want of a second source.
    lacked_second: AtomicBool,
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
        let breaker = CircuitBreaker::new(
            options.breaker_cooldown,
            options.breaker_max_cooldown,
        );

        Self {
            caller,
            options,
            multicall_missing_until: Mutex::new(None),
            multicall_missing_logged: AtomicBool::new(false),
            expected_chain_id: None,
            chain_state: AtomicU8::new(CHAIN_UNCHECKED),
            observed_chain_id: AtomicU64::new(0),
            mismatch_at: Mutex::new(None),
            breaker,
            rpc_down: AtomicBool::new(false),
        }
    }

    /// The backend the metadata is fetched from.
    pub fn caller(&self) -> &Arc<dyn EthCaller> {
        &self.caller
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

        match self.chain_state() {
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
                *self
                    .mismatch_at
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) =
                    Some(Instant::now());
                if self.chain_state.swap(CHAIN_MISMATCH, Ordering::Relaxed)
                    != CHAIN_MISMATCH
                {
                    error!(
                        "Token metadata RPC serves chain {actual} but \
                         chain {expected} is being indexed: token metadata \
                         is disabled (checked again in {:?})",
                        self.options.chain_recheck
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
        self.chain_state() != CHAIN_MISMATCH && !self.breaker_open()
    }

    /// `true` while the RPC circuit breaker is open.
    pub fn breaker_open(&self) -> bool {
        self.breaker.is_open()
    }

    /// When the open circuit breaker lets the next probe through.
    pub fn breaker_open_until(&self) -> Option<Instant> {
        self.breaker.open_until()
    }

    /// `true` when the RPC is known to serve another chain (permanent).
    pub fn wrong_chain(&self) -> bool {
        self.chain_state() == CHAIN_MISMATCH
    }

    /// The chain check verdict; a mismatch expires so that a load
    /// balancer that briefly routed to the wrong network, or an operator
    /// fixing the node, does not need an indexer restart.
    fn chain_state(&self) -> u8 {
        let state = self.chain_state.load(Ordering::Relaxed);
        if state != CHAIN_MISMATCH {
            return state;
        }

        let expired = self
            .mismatch_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none_or(|at| at.elapsed() >= self.options.chain_recheck);

        if expired {
            self.chain_state.store(CHAIN_UNCHECKED, Ordering::Relaxed);
            return CHAIN_UNCHECKED;
        }
        state
    }

    /// When the wrong-chain verdict is looked at again.
    pub fn chain_recheck_at(&self) -> Option<Instant> {
        if self.chain_state() != CHAIN_MISMATCH {
            return None;
        }
        self.mismatch_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map(|at| at + self.options.chain_recheck)
    }

    /// Whether individual calls are currently used instead of Multicall3.
    pub fn multicall_missing(&self) -> bool {
        self.multicall_missing_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|until| Instant::now() < until)
    }

    fn set_multicall_missing(&self, ttl: Duration) {
        *self
            .multicall_missing_until
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() + ttl);
    }

    fn trip_breaker(&self, error: &str) {
        // `None`: a concurrent fetch already opened it for this failure.
        let Some(cooldown) = self.breaker.trip() else {
            return;
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
        self.breaker.reset();

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
        self.fetch_with(tokens, false).await
    }

    /// [`fetch`](Self::fetch); with `quiet` a failure is blamed on the
    /// tokens rather than on the RPC: no circuit breaker opens over it.
    pub async fn fetch_with(
        &self,
        tokens: &[(Address, TokenStandard)],
        quiet: bool,
    ) -> FetchOutcome {
        let mut outcome = FetchOutcome::default();

        if tokens.is_empty() || !self.is_available() {
            return outcome;
        }

        let run = FetchRun {
            gave_up: AtomicBool::new(false),
            succeeded: AtomicBool::new(false),
            probing: self.rpc_down.load(Ordering::Relaxed),
            quiet,
            lacked_second: AtomicBool::new(false),
            last_error: Mutex::new(None),
        };
        let run = &run;

        match self.check_chain_id().await {
            ChainCheck::Verified => {}
            ChainCheck::Mismatch(_) => return outcome,
            ChainCheck::Unavailable(error) => {
                if !quiet {
                    self.trip_breaker(&error);
                }
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

        let results: Vec<ChunkOutcome> = stream::iter(chunks)
            .buffer_unordered(self.options.chunk_concurrency.max(1))
            .collect()
            .await;

        for chunk in results {
            outcome.unconfirmed.extend(chunk.unconfirmed);
            for (token, result) in chunk.accepted {
                match result {
                    TokenResult::Resolved(metadata) => {
                        outcome.resolved.insert(token, metadata);
                    }
                    TokenResult::Empty => outcome.empty.push(token),
                }
            }
        }

        let accepted = outcome.resolved.len() + outcome.empty.len();

        if run.gave_up.load(Ordering::Relaxed) {
            let error = run
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
                .unwrap_or_else(|| "unknown error".to_string());
            self.trip_breaker(&error);
        } else if accepted == 0
            && !quiet
            && run.lacked_second.load(Ordering::Relaxed)
        {
            // Asking again right away would only burn requests on the
            // one endpoint that works.
            self.trip_breaker(
                "answers of a public RPC endpoint must be confirmed by a \
                 second, independent endpoint and none is reachable",
            );
        } else if run.succeeded.load(Ordering::Relaxed) {
            self.reset_breaker();
        }

        outcome
    }

    /// Resolves one chunk, asking as many independent sources as the
    /// answers need before they may be stored:
    ///
    /// * a trusted (configured) source is believed on anything positive;
    /// * everything from an untrusted (discovered) source, and every
    ///   negative answer whenever somebody else could be asked, needs the
    ///   same decoded answer from a source of another group;
    /// * when two sources disagree a third one decides, and the loser is
    ///   reported;
    /// * whatever is left without agreement is `unconfirmed`: no row.
    async fn fetch_chunk(
        &self,
        chunk: &[(Address, TokenStandard)],
        run: &FetchRun,
    ) -> ChunkOutcome {
        let mut outcome = ChunkOutcome::default();

        let route = Route { quiet: run.quiet, ..Route::default() };
        let Some(first) = self.opinion(chunk, route, run, false).await
        else {
            return outcome;
        };

        let others = self.caller.source_count() > 1;
        let mut contested = Vec::new();

        for (token, standard) in chunk {
            let Some(verdict) = first.verdicts.get(token) else {
                continue;
            };

            if first.source.trusted && !(others && verdict.is_negative()) {
                outcome.accepted.push((*token, verdict.clone()));
            } else {
                contested.push((*token, *standard));
            }
        }

        if contested.is_empty() {
            return outcome;
        }

        let route = Route {
            only: None,
            exclude_groups: vec![first.source.group],
            quiet: run.quiet,
        };
        let Some(second) =
            self.opinion(&contested, route, run, true).await
        else {
            run.lacked_second.store(true, Ordering::Relaxed);
            debug!(
                "No second RPC endpoint could confirm {} token answers, \
                 nothing is stored for them",
                contested.len()
            );
            outcome.unconfirmed.extend(contested.iter().map(|(t, _)| *t));
            return outcome;
        };

        let mut disputed = Vec::new();
        let mut agreed = false;

        for (token, standard) in &contested {
            match (first.verdicts.get(token), second.verdicts.get(token)) {
                (Some(a), Some(b)) if a == b => {
                    agreed = true;
                    outcome.accepted.push((*token, a.clone()));
                }
                (Some(_), Some(_)) => disputed.push((*token, *standard)),
                _ => outcome.unconfirmed.push(*token),
            }
        }

        if agreed {
            self.caller.report_vote(first.source, Vote::Won);
            self.caller.report_vote(second.source, Vote::Won);
        }

        if disputed.is_empty() {
            return outcome;
        }

        let route = Route {
            only: None,
            exclude_groups: vec![first.source.group, second.source.group],
            quiet: run.quiet,
        };
        let third = self.opinion(&disputed, route, run, true).await;
        let (mut first_lost, mut second_lost) = (false, false);

        for (token, _) in &disputed {
            let a = first.verdicts.get(token);
            let b = second.verdicts.get(token);
            let c = third.as_ref().and_then(|t| t.verdicts.get(token));

            match c {
                Some(c) if Some(c) == a => {
                    second_lost = true;
                    outcome.accepted.push((*token, c.clone()));
                }
                Some(c) if Some(c) == b => {
                    first_lost = true;
                    outcome.accepted.push((*token, c.clone()));
                }
                _ => outcome.unconfirmed.push(*token),
            }
        }

        debug!(
            "RPC endpoints disagree about {} tokens ({} left unconfirmed)",
            disputed.len(),
            outcome.unconfirmed.len()
        );

        if first_lost {
            self.caller.report_vote(first.source, Vote::Lost);
        }
        if second_lost {
            self.caller.report_vote(second.source, Vote::Lost);
        }

        outcome
    }

    /// What one source (the first the route admits that answers) says
    /// about `tokens`. `None` when nobody could be asked. `auxiliary`
    /// opinions (second, third) never abort the run: the RPC as a whole
    /// is not down because a second endpoint is.
    async fn opinion(
        &self,
        tokens: &[(Address, TokenStandard)],
        mut route: Route,
        run: &FetchRun,
        auxiliary: bool,
    ) -> Option<Opinion> {
        for _ in 0..MAX_REROUTES {
            if run.gave_up.load(Ordering::Relaxed) {
                return None;
            }

            if self.multicall_missing() {
                return self
                    .opinion_individually(tokens, route, run, auxiliary)
                    .await;
            }

            // The height first: it tells whether the node answering has
            // seen the blocks these tokens come from, in the very same
            // state the metadata is read from and at no extra cost.
            let mut calls = Vec::with_capacity(tokens.len() * 3 + 1);
            calls.push(Call3 {
                target: MULTICALL3_ADDRESS,
                allowFailure: true,
                callData: getBlockNumberCall {}.abi_encode().into(),
            });
            for (token, standard) in tokens {
                for field in fields_for(*standard) {
                    calls.push(Call3 {
                        target: *token,
                        allowFailure: true,
                        callData: field.calldata(),
                    });
                }
            }
            let expected = calls.len();
            let calldata: Bytes =
                aggregate3Call { calls }.abi_encode().into();

            let routed = match self
                .call_with_retry(
                    MULTICALL3_ADDRESS,
                    calldata.clone(),
                    &route,
                    run,
                    auxiliary,
                )
                .await
            {
                Ok(routed) => routed,
                Err(RoutedError {
                    error: CallError::Transient(error),
                    ..
                }) => {
                    // The outage itself is reported once by the breaker.
                    debug!(
                        "Token metadata multicall failed for {} tokens, \
                         they will be retried later: {error}",
                        tokens.len()
                    );
                    return None;
                }
                Err(RoutedError {
                    error: CallError::Execution(error),
                    source,
                    ..
                }) => {
                    // allowFailure is set, so the aggregate itself failing
                    // means a sub-call blew the gas / response limits.
                    debug!(
                        "Token metadata multicall execution failed, \
                         fetching {} tokens individually: {error}",
                        tokens.len()
                    );
                    route.only = source.map(|source| source.id);
                    return self
                        .opinion_individually(
                            tokens, route, run, auxiliary,
                        )
                        .await;
                }
            };

            let source = routed.source;
            let pinned = Route { only: Some(source.id), ..route.clone() };

            if routed.data.is_empty() {
                // eth_call to an address without code succeeds with no
                // data. Multicall3 presence is a property of the chain,
                // not of the node that answered: a lagging / pruned /
                // broken endpoint must not switch everybody to individual
                // calls, so the backend cross-checks before it is
                // remembered.
                match self
                    .caller
                    .confirm_empty(MULTICALL3_ADDRESS, calldata)
                    .await
                {
                    EmptyCheck::Refuted(_) => {
                        debug!(
                            "An RPC endpoint answered empty for \
                             Multicall3 while another one has it, asking \
                             somebody else"
                        );
                        route.exclude_groups.push(source.group);
                        continue;
                    }
                    EmptyCheck::Confirmed => {
                        self.set_multicall_missing(
                            self.options.multicall_recheck,
                        );
                        if !self
                            .multicall_missing_logged
                            .swap(true, Ordering::Relaxed)
                        {
                            warn!(
                                "Multicall3 is not deployed at \
                                 {MULTICALL3_ADDRESS} on this chain, \
                                 falling back to individual eth_calls \
                                 for token metadata"
                            );
                        }
                    }
                    EmptyCheck::Undecided => {
                        debug!(
                            "Unable to cross-check whether Multicall3 is \
                             deployed, using individual eth_calls for a \
                             while"
                        );
                        self.set_multicall_missing(
                            self.options.multicall_undecided_ttl,
                        );
                    }
                }
                return self
                    .opinion_individually(tokens, pinned, run, auxiliary)
                    .await;
            }

            let results =
                match aggregate3Call::abi_decode_returns(&routed.data) {
                    Ok(results) if results.len() == expected => results,
                    other => {
                        debug!(
                            "Unusable Multicall3 response ({}), fetching \
                             {} tokens individually",
                            match other {
                                Ok(results) => format!(
                                    "{} results for {expected} calls",
                                    results.len()
                                ),
                                Err(error) => error.to_string(),
                            },
                            tokens.len()
                        );
                        return self
                            .opinion_individually(
                                tokens, pinned, run, auxiliary,
                            )
                            .await;
                    }
                };

            let mut results = results.iter();

            if let Some(height) = results
                .next()
                .filter(|result| result.success)
                .and_then(|result| {
                    U256::abi_decode(&result.returnData).ok()
                })
                .and_then(|height| u64::try_from(height).ok())
            {
                if !self.caller.observe_height(
                    source,
                    HeightKind::Evm,
                    height,
                ) {
                    debug!(
                        "An RPC endpoint answered from block {height}, \
                         behind the others: asking somebody else"
                    );
                    route.exclude_groups.push(source.group);
                    continue;
                }
            }

            let mut verdicts = HashMap::with_capacity(tokens.len());
            let mut suspicious = Vec::new();

            for (token, standard) in tokens {
                let mut metadata = TokenMetadata::default();
                let mut any_failed = false;
                let mut all_empty = true;

                for (field, result) in
                    fields_for(*standard).iter().zip(results.by_ref())
                {
                    all_empty &=
                        result.success && result.returnData.is_empty();
                    any_failed |= !result.success;

                    if result.success {
                        apply_field(
                            &mut metadata,
                            *field,
                            &result.returnData,
                        );
                    }
                }

                if all_empty {
                    // No code at the address (yet): not definitive.
                    verdicts.insert(*token, TokenResult::Empty);
                } else if any_failed && *standard != TokenStandard::Erc1155
                {
                    // A failed call of an ERC20/ERC721 may be the doing
                    // of a gas-hungry neighbour in the same aggregate
                    // (`decimals()` failing next to a working `name()`
                    // would be stored as 0 decimals): ask again, alone,
                    // before anything is concluded. (For ERC1155 failing
                    // is the norm: name/symbol are not in the standard.)
                    suspicious.push((*token, *standard));
                } else {
                    verdicts
                        .insert(*token, TokenResult::Resolved(metadata));
                }
            }

            if !suspicious.is_empty() {
                if let Some(alone) = self
                    .opinion_individually(
                        &suspicious,
                        pinned,
                        run,
                        auxiliary,
                    )
                    .await
                {
                    verdicts.extend(alone.verdicts);
                }
            }

            return Some(Opinion { source, verdicts });
        }

        None
    }

    /// [`opinion`](Self::opinion) with plain `eth_call`s, all answered by
    /// the same endpoint: the first one that answers within the route.
    async fn opinion_individually(
        &self,
        tokens: &[(Address, TokenStandard)],
        mut route: Route,
        run: &FetchRun,
        auxiliary: bool,
    ) -> Option<Opinion> {
        let mut verdicts = HashMap::with_capacity(tokens.len());
        let mut source = None;
        let mut rest = tokens.iter();

        // One at a time until somebody answers, then pin to that node.
        for (token, standard) in rest.by_ref() {
            if let Some((result, answered_by)) = self
                .fetch_one(*token, *standard, &route, run, auxiliary)
                .await
            {
                verdicts.insert(*token, result);
                route.only = Some(answered_by.id);
                source = Some(answered_by);
                break;
            }
        }

        let source = source?;
        let route = &route;

        let calls: Vec<_> = rest
            .map(|(token, standard)| async move {
                self.fetch_one(*token, *standard, route, run, auxiliary)
                    .await
                    .map(|(result, _)| (*token, result))
            })
            .collect();

        let resolved: Vec<Option<(Address, TokenResult)>> =
            stream::iter(calls)
                .buffer_unordered(
                    self.options.individual_concurrency.max(1),
                )
                .collect()
                .await;

        verdicts.extend(resolved.into_iter().flatten());

        Some(Opinion { source, verdicts })
    }

    /// `None` when the node could not be reached for any of the calls.
    async fn fetch_one(
        &self,
        token: Address,
        standard: TokenStandard,
        route: &Route,
        run: &FetchRun,
        auxiliary: bool,
    ) -> Option<(TokenResult, Source)> {
        let mut metadata = TokenMetadata::default();
        let mut all_empty = true;
        let mut route = route.clone();
        let mut source = None;

        for field in fields_for(standard) {
            let answered_by = match self
                .call_with_retry(
                    token,
                    field.calldata(),
                    &route,
                    run,
                    auxiliary,
                )
                .await
            {
                Ok(routed) => {
                    all_empty &= routed.data.is_empty();
                    apply_field(&mut metadata, *field, &routed.data);
                    Some(routed.source)
                }
                Err(RoutedError {
                    error: CallError::Execution(_),
                    source,
                    ..
                }) => {
                    all_empty = false;
                    source
                }
                Err(RoutedError {
                    error: CallError::Transient(error),
                    ..
                }) => {
                    debug!(
                        "Token metadata call failed for {token}, it will \
                         be retried later: {error}"
                    );
                    return None;
                }
            };

            // Every field of a token from the same node.
            if let Some(answered_by) = answered_by {
                route.only = Some(answered_by.id);
                source = Some(answered_by);
            }
        }

        let result = if all_empty {
            TokenResult::Empty
        } else {
            TokenResult::Resolved(metadata)
        };

        Some((result, source?))
    }

    async fn call_with_retry(
        &self,
        to: Address,
        data: Bytes,
        route: &Route,
        run: &FetchRun,
        auxiliary: bool,
    ) -> Result<Routed, RoutedError> {
        let max_retries = if run.probing {
            0
        } else if auxiliary {
            self.options.max_retries.min(1)
        } else {
            self.options.max_retries
        };
        let mut attempt: u32 = 0;

        loop {
            // Another request of this run already gave up on the node.
            if run.gave_up.load(Ordering::Relaxed) {
                return Err(RoutedError {
                    error: CallError::Transient(
                        "skipped, the RPC is unavailable".to_string(),
                    ),
                    source: None,
                    no_source: false,
                });
            }

            match self.caller.call_routed(to, data.clone(), route).await {
                Err(failure)
                    if matches!(
                        failure.error,
                        CallError::Transient(_)
                    ) =>
                {
                    if failure.no_source {
                        return Err(failure);
                    }

                    if attempt >= max_retries {
                        if let CallError::Transient(error) = &failure.error
                        {
                            if !auxiliary && !run.quiet {
                                run.give_up(error);
                            }
                        }
                        return Err(failure);
                    }

                    let delay = self
                        .options
                        .retry_backoff
                        .saturating_mul(2u32.saturating_pow(attempt));
                    debug!(
                        "Token metadata eth_call failed (attempt {}), \
                         retrying in {delay:?}",
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
        /// Block height of the node (`getBlockNumber`, `eth_blockNumber`).
        pub height: AtomicU64,
        pub block_number_calls: AtomicUsize,
        /// Sub-calls of an aggregate from this index on fail (0 = off):
        /// a neighbour ate the gas half way through a token.
        pub fail_subcalls_from: AtomicUsize,
    }

    impl FakeChain {
        pub fn new() -> Arc<Self> {
            let chain = Self::default();
            chain.multicall_deployed.store(true, Ordering::SeqCst);
            chain.chain_id.store(1, Ordering::SeqCst);
            chain.height.store(1_000, Ordering::SeqCst);
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

                let fail_from =
                    self.fail_subcalls_from.load(Ordering::SeqCst);

                for (index, call) in decoded.calls.iter().enumerate() {
                    exploded |= fail_from > 0 && index >= fail_from;

                    if call.target == MULTICALL3_ADDRESS
                        && call
                            .callData
                            .starts_with(&getBlockNumberCall::SELECTOR)
                    {
                        let height = self.height.load(Ordering::SeqCst);
                        results.push(Call3Result {
                            success: true,
                            returnData: U256::from(height)
                                .abi_encode()
                                .into(),
                        });
                        continue;
                    }

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

        fn block_number(
            &self,
        ) -> BoxFuture<'_, Result<Option<u64>, CallError>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                self.block_number_calls.fetch_add(1, Ordering::SeqCst);

                if self.offline.load(Ordering::SeqCst) {
                    return Err(CallError::Transient("offline".into()));
                }

                Ok(Some(self.height.load(Ordering::SeqCst)))
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
    fn classifies_error_responses() {
        let classify = |code: i64, message: &str| {
            classify_error_response(code, message, &Redactor::default())
        };

        let execution = [
            (3, "execution reverted"),
            (-32000, "execution reverted: nope"),
            (-32000, "out of gas"),
            (-32015, "VM execution error."),
            (-32000, "invalid opcode: INVALID"),
        ];
        for (code, message) in execution {
            assert!(
                matches!(classify(code, message), CallError::Execution(_)),
                "{code} {message}"
            );
        }

        let transient = [
            (429, "Too many requests"),
            (-32005, "project rate limit exceeded"),
            (-32000, "header not found"),
            (-32603, "internal error"),
            (-32000, "something unexpected"),
            // Conditions of the node, however much they sound like the
            // contract's fault: another node may well answer.
            (-32000, "execution aborted (timeout = 5s)"),
            (-32000, "gas required exceeds allowance (50000000)"),
            (3, "execution reverted: rate limit exceeded"),
            (-32016, "execution reverted"),
            (-32000, "evm timeout"),
        ];
        for (code, message) in transient {
            assert!(
                matches!(classify(code, message), CallError::Transient(_)),
                "{code} {message}"
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
        assert!(!fetcher.multicall_missing());

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
        assert_eq!(fetcher.breaker.next_cooldown(), cooldown);
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
        assert_eq!(fetcher.breaker.next_cooldown(), cooldown * 2);

        // Following probes: a single attempt each, cool-down doubles up
        // to the maximum.
        let mut expected_attempts = 4;
        for expected_next in [cooldown * 4, cooldown * 4, cooldown * 4] {
            let wait = fetcher.breaker.next_cooldown();
            tokio::time::sleep(wait + Duration::from_millis(20)).await;
            fetcher.fetch(&tokens).await;
            expected_attempts += 1;
            assert_eq!(
                chain.attempts.load(Ordering::SeqCst),
                expected_attempts
            );
            assert_eq!(fetcher.breaker.next_cooldown(), expected_next);
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

    #[tokio::test(start_paused = true)]
    async fn a_chain_mismatch_is_looked_at_again_after_an_hour() {
        let chain = FakeChain::new();
        chain.add(addr(1), FakeToken::erc20("Token", "TKN", 8));
        chain.chain_id.store(56, Ordering::SeqCst);
        let tokens = [(addr(1), TokenStandard::Erc20)];
        let fetcher = fetcher(&chain).expect_chain_id(1);

        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        assert!(fetcher.wrong_chain());
        assert!(fetcher.chain_recheck_at().is_some());

        // The operator points the URL at the right network. Nothing
        // happens for an hour (no hammering), then it just works.
        chain.chain_id.store(1, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(3_000)).await;
        assert!(fetcher.fetch(&tokens).await.resolved.is_empty());
        assert_eq!(chain.chain_id_calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(700)).await;
        assert!(!fetcher.wrong_chain());
        assert_eq!(fetcher.fetch(&tokens).await.resolved.len(), 1);
        assert_eq!(fetcher.chain_recheck_at(), None);
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
            (401, "unknown api key SuPerSecretKey123".to_string()),
            (-32000, format!("execution reverted at {url}")),
        ];

        for (code, message) in &errors {
            let (CallError::Transient(message)
            | CallError::Execution(message)) =
                classify_error_response(*code, message, &redactor);
            assert!(!message.contains("SuPerSecretKey123"), "{message}");
            assert!(!message.contains("alchemy"), "{message}");
        }
    }
}
