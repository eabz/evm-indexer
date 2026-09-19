//! The only HTTP stack of the token module: JSON-RPC over `reqwest` with
//! the two properties a list of endpoints nobody vetted requires.
//!
//! * **Redirects are never followed.** A listed host answering `307` could
//!   otherwise point our JSON-RPC `POST` at `http://clickhouse:8123/...`
//!   or the cloud metadata address, past every URL filter.
//! * **Response bodies are capped**, so an endpoint cannot make the
//!   indexer buffer gigabytes.
//!
//! Public (discovered) endpoints additionally go through a DNS resolver
//! that refuses names resolving to loopback / private / link-local
//! addresses, on every resolution (nothing is pinned, so DNS rebinding
//! does not help either). Configured endpoints are the operator's
//! business and may well be private.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};

use alloy::primitives::{Address, Bytes};
use anyhow::{anyhow, bail, Context};
use futures::future::BoxFuture;
use reqwest::{
    dns::{Addrs, Name, Resolve, Resolving},
    redirect::Policy,
};
use serde::Deserialize;

use super::{
    multicall::{classify_error_response, CallError, EthCaller},
    redact::Redactor,
};

/// Cap of a JSON-RPC response. An `aggregate3` of 150 metadata calls is
/// a few tens of KiB.
pub const MAX_RPC_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str =
    concat!("evm-indexer/", env!("CARGO_PKG_VERSION"));

/// `true` for addresses that are reachable by anybody on the internet.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => is_public_ipv4(mapped),
            None => is_public_ipv6(ip),
        },
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();

    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        // 0.0.0.0/8, carrier grade NAT 100.64/10, benchmarking
        // 198.18/15, reserved 240/4.
        || a == 0
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (18..20).contains(&b))
        || a >= 240)
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];

    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // Unique local fc00::/7, link local fe80::/10, site local
        // fec0::/10, documentation 2001:db8::/32, NAT64 64:ff9b::/96.
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        || (first & 0xffc0) == 0xfec0
        || (first == 0x2001 && ip.segments()[1] == 0x0db8)
        || (first == 0x0064 && ip.segments()[1] == 0xff9b))
}

/// System DNS that only hands out public addresses.
struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let resolved: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0))
                    .await?
                    .collect();

            let public: Vec<SocketAddr> = resolved
                .into_iter()
                .filter(|address| is_public_ip(address.ip()))
                .collect();

            if public.is_empty() {
                return Err(
                    "the host does not resolve to a public address".into(),
                );
            }

            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

fn build_client(public_only: bool) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT);

    if public_only {
        builder = builder.dns_resolver(Arc::new(PublicOnlyResolver));
    }

    builder.build().map_err(|error| error.to_string())
}

/// The process wide HTTP client: one for configured endpoints, one (with
/// the public-only resolver) for everything that came from a public list,
/// including the list itself. Neither follows redirects.
pub fn client(public_only: bool) -> anyhow::Result<reqwest::Client> {
    static TRUSTED: OnceLock<Result<reqwest::Client, String>> =
        OnceLock::new();
    static PUBLIC: OnceLock<Result<reqwest::Client, String>> =
        OnceLock::new();

    let cell = if public_only { &PUBLIC } else { &TRUSTED };

    cell.get_or_init(|| build_client(public_only)).clone().map_err(
        |error| anyhow!("unable to create the http client: {error}"),
    )
}

/// Reads a response body, refusing to buffer more than `max_bytes`.
pub async fn read_capped(
    response: &mut reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!(
            "the response is larger than {max_bytes} bytes"
        ));
    }

    let mut body = Vec::new();
    while let Some(chunk) =
        response.chunk().await.map_err(|error| error.to_string())?
    {
        if body.len() + chunk.len() > max_bytes {
            return Err(format!(
                "the response is larger than {max_bytes} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(body)
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<serde_json::Value>,
    error: Option<RpcErrorBody>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

/// [`EthCaller`] over one HTTP(S) JSON-RPC endpoint.
pub struct HttpCaller {
    client: reqwest::Client,
    url: reqwest::Url,
    timeout: Duration,
    max_response_bytes: usize,
    redactor: Redactor,
    next_id: AtomicU64,
}

impl HttpCaller {
    /// A configured endpoint (may be private, plain http...).
    pub fn new(rpc_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        Self::build(rpc_url, timeout, false)
    }

    /// An endpoint taken from a public list: https only, a domain name,
    /// and only ever connected to on public addresses.
    pub fn public(
        rpc_url: &str,
        timeout: Duration,
    ) -> anyhow::Result<Self> {
        Self::build(rpc_url, timeout, true)
    }

    fn build(
        rpc_url: &str,
        timeout: Duration,
        public_only: bool,
    ) -> anyhow::Result<Self> {
        // `url::ParseError` does not echo the (possibly secret) input.
        let url: reqwest::Url = rpc_url
            .trim()
            .parse()
            .with_context(|| "invalid rpc url for token metadata")?;

        match url.scheme() {
            "https" => {}
            "http" if !public_only => {}
            _ => bail!("unsupported rpc url scheme for token metadata"),
        }

        if public_only && !matches!(url.host(), Some(url::Host::Domain(_)))
        {
            bail!("a public rpc url must name a host");
        }

        Ok(Self {
            client: client(public_only)?,
            url,
            timeout,
            max_response_bytes: MAX_RPC_RESPONSE_BYTES,
            redactor: Redactor::for_url(rpc_url),
            next_id: AtomicU64::new(1),
        })
    }

    /// Overrides the response size cap (tests).
    pub fn with_max_response_bytes(mut self, max_bytes: usize) -> Self {
        self.max_response_bytes = max_bytes;
        self
    }

    fn transient(&self, what: &str, error: &str) -> CallError {
        CallError::Transient(
            self.redactor.redact(&format!("{what}: {error}")),
        )
    }

    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, CallError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "method": method,
            "params": params,
        })
        .to_string();

        let exchange = async {
            let mut response = self
                .client
                .post(self.url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| self.transient(method, &e.to_string()))?;

            let status = response.status();
            if status.is_redirection() {
                return Err(self.transient(
                    method,
                    &format!(
                        "the endpoint answered with a redirect ({}), \
                         which is never followed",
                        status.as_u16()
                    ),
                ));
            }

            let bytes =
                read_capped(&mut response, self.max_response_bytes)
                    .await
                    .map_err(|e| self.transient(method, &e))?;

            Ok((status, bytes))
        };

        // One deadline for the whole exchange, body included.
        let (status, bytes) =
            match tokio::time::timeout(self.timeout, exchange).await {
                Ok(result) => result?,
                Err(_) => {
                    return Err(CallError::Transient(format!(
                        "{method} timed out after {:?}",
                        self.timeout
                    )))
                }
            };

        // Nodes answer JSON-RPC errors with any HTTP status: the body
        // wins when it is a JSON-RPC response.
        let parsed: Option<RpcResponse> =
            serde_json::from_slice(&bytes).ok();

        match parsed {
            Some(RpcResponse { error: Some(error), .. }) => {
                Err(classify_error_response(
                    error.code,
                    &error.message,
                    &self.redactor,
                ))
            }
            Some(RpcResponse { result: Some(result), .. })
                if status.is_success() =>
            {
                Ok(result)
            }
            _ if !status.is_success() => Err(self.transient(
                method,
                &format!("http status {}", status.as_u16()),
            )),
            _ => Err(self.transient(method, "not a JSON-RPC response")),
        }
    }

    async fn quantity(&self, method: &str) -> Result<u64, CallError> {
        let result = self.request(method, serde_json::json!([])).await?;

        result
            .as_str()
            .and_then(|hex| hex.strip_prefix("0x"))
            .and_then(|hex| u64::from_str_radix(hex, 16).ok())
            .ok_or_else(|| self.transient(method, "not a hex quantity"))
    }
}

impl EthCaller for HttpCaller {
    fn call(
        &self,
        to: Address,
        data: Bytes,
    ) -> BoxFuture<'_, Result<Bytes, CallError>> {
        Box::pin(async move {
            // Both spellings of the calldata field: old nodes only know
            // `data`, and they must carry the same value when both are set.
            let params = serde_json::json!([
                {
                    "to": to.to_string(),
                    "data": data.to_string(),
                    "input": data.to_string(),
                },
                "latest"
            ]);

            let result = self.request("eth_call", params).await?;

            result
                .as_str()
                .and_then(|hex| hex.strip_prefix("0x"))
                .and_then(|hex| hex::decode(hex).ok())
                .map(Bytes::from)
                .ok_or_else(|| self.transient("eth_call", "not hex data"))
        })
    }

    fn chain_id(&self) -> BoxFuture<'_, Result<u64, CallError>> {
        Box::pin(self.quantity("eth_chainId"))
    }

    fn block_number(
        &self,
    ) -> BoxFuture<'_, Result<Option<u64>, CallError>> {
        Box::pin(async move {
            self.quantity("eth_blockNumber").await.map(Some)
        })
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A tiny scripted HTTP/1.1 server.

    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    pub struct Server {
        pub url: String,
        pub requests: Arc<AtomicUsize>,
    }

    /// Serves `respond(request_number, request_head)` raw HTTP responses
    /// (the head, request line and headers, is lowercased).
    pub async fn serve<F>(respond: F) -> Server
    where
        F: Fn(usize, &str) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let respond = Arc::new(respond);

        let count = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let count = count.clone();
                let respond = respond.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 64 * 1024];
                    let mut read = 0;
                    // Good enough: requests here fit one segment or two.
                    loop {
                        let Ok(n) = socket.read(&mut buffer[read..]).await
                        else {
                            return;
                        };
                        read += n;
                        let text =
                            String::from_utf8_lossy(&buffer[..read]);
                        if n == 0 || text.contains("\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer[..read])
                        .to_ascii_lowercase();
                    let number = count.fetch_add(1, Ordering::SeqCst);
                    let _ =
                        socket.write_all(&respond(number, &head)).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        Server { url, requests }
    }

    pub fn response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: \
             close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        raw.extend_from_slice(body);
        raw
    }

    pub fn json_rpc(result: &str) -> Vec<u8> {
        response(
            "200 OK",
            "content-type: application/json\r\n",
            format!(r#"{{"jsonrpc":"2.0","id":1,"result":{result}}}"#)
                .as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn only_public_addresses_are_public() {
        let private = [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "255.255.255.255",
            "198.18.0.1",
            "240.0.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
        ];
        for ip in private {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }

        for ip in
            ["1.1.1.1", "8.8.8.8", "2606:4700::1111", "::ffff:1.1.1.1"]
        {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn public_client_refuses_names_resolving_to_private_addresses() {
        let resolver = PublicOnlyResolver;
        let name: Name = "localhost".parse().unwrap();
        assert!(resolver.resolve(name).await.is_err());

        // ...and IP literals / non-https never get that far.
        assert!(
            HttpCaller::public("https://127.0.0.1:8545", TIMEOUT).is_err()
        );
        assert!(
            HttpCaller::public("http://rpc.example.org", TIMEOUT).is_err()
        );
        assert!(
            HttpCaller::public("https://rpc.example.org", TIMEOUT).is_ok()
        );
        assert!(HttpCaller::new("ftp://rpc.example.org", TIMEOUT).is_err());
        assert!(HttpCaller::new("http://127.0.0.1:8545", TIMEOUT).is_ok());
    }

    #[tokio::test]
    async fn speaks_json_rpc() {
        let server = serve(|_, head| {
            assert!(head.starts_with("post / http/1.1"), "{head}");
            json_rpc("\"0x2105\"")
        })
        .await;
        let caller = HttpCaller::new(&server.url, TIMEOUT).unwrap();

        assert_eq!(caller.chain_id().await, Ok(0x2105));
        assert_eq!(caller.block_number().await, Ok(Some(0x2105)));
        assert_eq!(
            caller.call(Address::ZERO, Bytes::new()).await,
            Ok(Bytes::from(vec![0x21, 0x05]))
        );
    }

    #[tokio::test]
    async fn classifies_error_responses_and_statuses() {
        let server = serve(|number, _| match number {
            0 => response(
                "200 OK",
                "",
                br#"{"jsonrpc":"2.0","id":1,"error":{"code":3,"message":"execution reverted"}}"#,
            ),
            // A JSON-RPC error wins over the HTTP status.
            1 => response(
                "500 Internal Server Error",
                "",
                br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution reverted: nope"}}"#,
            ),
            2 => response(
                "429 Too Many Requests",
                "",
                br#"{"jsonrpc":"2.0","id":1,"error":{"code":429,"message":"slow down"}}"#,
            ),
            3 => response("502 Bad Gateway", "", b"<html>bad</html>"),
            4 => response("200 OK", "", b"<html>captive portal</html>"),
            _ => json_rpc("\"zz\""),
        })
        .await;
        let caller = HttpCaller::new(&server.url, TIMEOUT).unwrap();
        let call = || caller.call(Address::ZERO, Bytes::new());

        assert!(matches!(call().await, Err(CallError::Execution(_))));
        assert!(matches!(call().await, Err(CallError::Execution(_))));
        assert!(matches!(call().await, Err(CallError::Transient(_))));
        assert!(matches!(call().await, Err(CallError::Transient(_))));
        assert!(matches!(call().await, Err(CallError::Transient(_))));
        assert!(matches!(call().await, Err(CallError::Transient(_))));
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        // Where a malicious endpoint would like our POST to go.
        let internal = serve(|_, _| json_rpc("\"0x1\"")).await;
        let target = internal.url.clone();

        for status in
            ["301 Moved", "302 Found", "307 Temporary", "308 Perm"]
        {
            let location = format!("location: {target}/secret\r\n");
            let server =
                serve(move |_, _| response(status, &location, b"")).await;
            let caller = HttpCaller::new(&server.url, TIMEOUT).unwrap();

            let Err(CallError::Transient(error)) = caller.chain_id().await
            else {
                panic!("{status}: the redirect was followed");
            };
            assert!(error.contains("redirect"), "{error}");
            assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        }

        assert_eq!(internal.requests.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn response_bodies_are_capped() {
        // Declared too large: refused without reading.
        let declared = serve(|_, _| {
            let mut raw = b"HTTP/1.1 200 OK\r\ncontent-length: \
                            999999999\r\n\r\n"
                .to_vec();
            raw.extend_from_slice(&[b'a'; 1024]);
            raw
        })
        .await;
        // Not declared (close delimited): refused while streaming.
        let streamed = serve(|_, _| {
            let mut raw =
                b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n".to_vec();
            raw.extend_from_slice(&vec![b'a'; 64 * 1024]);
            raw
        })
        .await;

        for server in [&declared, &streamed] {
            let caller = HttpCaller::new(&server.url, TIMEOUT)
                .unwrap()
                .with_max_response_bytes(16 * 1024);
            let Err(CallError::Transient(error)) = caller.chain_id().await
            else {
                panic!("an oversized body was accepted");
            };
            assert!(error.contains("larger than"), "{error}");
        }

        // Within the cap it is fine.
        let small = serve(|_, _| json_rpc("\"0x1\"")).await;
        let caller = HttpCaller::new(&small.url, TIMEOUT)
            .unwrap()
            .with_max_response_bytes(16 * 1024);
        assert_eq!(caller.chain_id().await, Ok(1));
    }

    #[tokio::test]
    async fn errors_do_not_leak_the_url() {
        // Closed local port: reqwest reports the full URL in its error.
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let caller = HttpCaller::new(
            &format!("http://127.0.0.1:{port}/v2/SuPerSecretKey123"),
            TIMEOUT,
        )
        .unwrap();

        let Err(CallError::Transient(message)) = caller.chain_id().await
        else {
            panic!("expected a transport error");
        };
        assert!(!message.contains("SuPerSecretKey123"), "{message}");

        let Err(CallError::Transient(message)) =
            caller.call(Address::ZERO, Bytes::new()).await
        else {
            panic!("expected a transport error");
        };
        assert!(!message.contains("SuPerSecretKey123"), "{message}");

        // A node echoing the key back in its error body.
        let server = serve(|_, _| {
            response(
                "200 OK",
                "",
                br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"bad key SuPerSecretKey123"}}"#,
            )
        })
        .await;
        let caller = HttpCaller::new(
            &format!("{}/v2/SuPerSecretKey123", server.url),
            TIMEOUT,
        )
        .unwrap();
        let Err(CallError::Transient(message)) = caller.chain_id().await
        else {
            panic!("expected an error");
        };
        assert!(!message.contains("SuPerSecretKey123"), "{message}");
    }
}
