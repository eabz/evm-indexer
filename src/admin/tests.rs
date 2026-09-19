//! The control panel, over a real socket.
//!
//! Every test below speaks HTTP/1.1 to a listener the test itself bound, so
//! what is checked is what a browser would actually receive: the status
//! code, the `Set-Cookie` attributes, the security headers and the body.
//! Nothing is asserted about internal state that a client cannot see.

use super::*;
use crate::fleet::{
    fixtures::{config, until, Behaviour, FakeRunner, MemoryStore},
    supervisor::Supervisor,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const PASSWORD: &str = "a-very-good-password";

/// A panel on a real port, with fake chains behind it.
struct Panel {
    addr: SocketAddr,
    supervisor: Arc<Supervisor>,
    runner: Arc<FakeRunner>,
    server: tokio::task::JoinHandle<()>,
}

impl Panel {
    async fn start(chains: &[u64]) -> Self {
        Self::start_with(chains, |_| {}).await
    }

    async fn start_with(
        chains: &[u64],
        tweak: impl FnOnce(&mut Admin),
    ) -> Self {
        let runner = FakeRunner::new();
        let store = MemoryStore::new();
        let mut settings = config();
        settings.chains = chains.to_vec();

        let supervisor =
            Supervisor::new(settings, runner.clone(), store.clone());
        supervisor.load_and_start().await.unwrap();

        let mut admin = Admin {
            supervisor: supervisor.clone(),
            password: Password::new(PASSWORD).unwrap(),
            sessions: Sessions::default(),
            limiter: RateLimiter::default(),
            secure_cookie: false,
            trust_forwarded_proto: false,
            csp: page::content_security_policy(),
        };
        tweak(&mut admin);

        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(Arc::new(admin));

        let server = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await;
        });

        Self { addr, supervisor, runner, server }
    }

    fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn stop(self) {
        self.server.abort();
        self.supervisor.shutdown().await;
    }
}

// ------------------------------------------------------- a tiny client

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// One request, one connection, `Connection: close` so the body ends at
/// EOF and nothing has to parse a chunked encoding.
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n"
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    if let Some(body) = body {
        head.push_str(body);
    }

    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let raw = String::from_utf8_lossy(&raw).to_string();

    let (head, body) = raw
        .split_once("\r\n\r\n")
        .map(|(h, b)| (h.to_string(), b.to_string()))
        .unwrap_or((raw.clone(), String::new()));

    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| {
            (name.trim().to_string(), value.trim().to_string())
        })
        .collect();

    Reply { status, headers, body }
}

/// Signs in and returns the `Cookie` header value to send afterwards.
async fn sign_in(panel: &Panel) -> String {
    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[("Origin", &panel.origin())],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;

    assert_eq!(reply.status, 200, "{reply:?}");
    let cookie = reply.header("set-cookie").expect("a session cookie");
    cookie.split(';').next().unwrap().to_string()
}

// ------------------------------------------------------------- the tests

#[tokio::test]
async fn the_page_is_served_and_carries_the_security_headers() {
    let panel = Panel::start(&[1]).await;

    let reply = request(panel.addr, "GET", "/", &[], None).await;

    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("Indexer control panel"));
    assert_eq!(reply.header("x-frame-options"), Some("DENY"));
    assert_eq!(reply.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(reply.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));

    let csp = reply.header("content-security-policy").unwrap();
    assert!(csp.starts_with("default-src 'self'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "{csp}");

    // The page itself holds no data about any chain.
    assert!(!reply.body.contains("8453"));

    panel.stop().await;
}

#[tokio::test]
async fn every_api_route_refuses_a_request_without_a_session() {
    let panel = Panel::start(&[1]).await;
    let origin = panel.origin();

    let routes: Vec<(&str, String, Option<String>)> = vec![
        ("GET", "/api/chains".to_string(), None),
        (
            "POST",
            "/api/chains".to_string(),
            Some("{\"chain\":\"10\"}".to_string()),
        ),
        ("POST", "/api/chains/1/start".to_string(), None),
        ("POST", "/api/chains/1/stop".to_string(), None),
        ("POST", "/api/chains/1/restart".to_string(), None),
        (
            "PATCH",
            "/api/chains/1".to_string(),
            Some("{\"settings\":{}}".to_string()),
        ),
        ("GET", "/api/chains/1/events".to_string(), None),
    ];

    for (method, path, body) in routes {
        let reply = request(
            panel.addr,
            method,
            &path,
            &[("Origin", &origin)],
            body.as_deref(),
        )
        .await;

        assert_eq!(reply.status, 401, "{method} {path}: {reply:?}");

        // Nothing about any chain leaks in the refusal.
        assert!(!reply.body.contains("stored_head"), "{}", reply.body);
        assert!(!reply.body.contains("\"chains\""), "{}", reply.body);
        assert!(!reply.body.contains("8453"), "{}", reply.body);
        assert_eq!(reply.header("cache-control"), Some("no-store"));
    }

    // And nothing happened to the chain either.
    assert_eq!(panel.runner.starts(1), 1);
    assert!(panel.runner.is_live(1));

    panel.stop().await;
}

#[tokio::test]
async fn the_wrong_password_is_refused_and_sets_no_cookie() {
    let panel = Panel::start(&[]).await;

    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[("Origin", &panel.origin())],
        Some("{\"password\":\"not-it\"}"),
    )
    .await;

    assert_eq!(reply.status, 401);
    assert_eq!(reply.header("set-cookie"), None);
    assert!(reply.body.to_lowercase().contains("wrong password"));
    // The refusal never says how close the guess was.
    assert!(!reply.body.contains(PASSWORD));

    panel.stop().await;
}

#[tokio::test]
async fn the_session_cookie_is_http_only_and_same_site_strict() {
    let panel = Panel::start(&[]).await;

    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[("Origin", &panel.origin())],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;

    let cookie = reply.header("set-cookie").unwrap();
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    // Plain HTTP on loopback: `Secure` would make the cookie unusable.
    assert!(!cookie.contains("Secure"), "{cookie}");

    // 256 bits as hex.
    let value =
        cookie.split(';').next().unwrap().trim_start_matches("session=");
    assert_eq!(value.len(), 64, "{value}");
    assert!(value.chars().all(|c| c.is_ascii_hexdigit()));

    panel.stop().await;
}

#[tokio::test]
async fn behind_tls_the_cookie_is_marked_secure() {
    let panel =
        Panel::start_with(&[], |admin| admin.secure_cookie = true).await;

    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[("Origin", &format!("https://{}", panel.addr))],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;

    assert_eq!(reply.status, 200, "{reply:?}");
    assert!(reply.header("set-cookie").unwrap().contains("Secure"));

    panel.stop().await;
}

#[tokio::test]
async fn a_forwarded_proto_header_is_ignored_unless_it_is_trusted() {
    // Off by default: a client that claims https is not believed, so the
    // expected origin stays http and an https Origin does not match.
    let panel = Panel::start(&[]).await;
    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[
            ("Origin", &format!("https://{}", panel.addr)),
            ("X-Forwarded-Proto", "https"),
        ],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;
    assert_eq!(reply.status, 403, "{reply:?}");
    panel.stop().await;

    // Switched on by the operator: now the proxy's header decides.
    let panel = Panel::start_with(&[], |admin| {
        admin.trust_forwarded_proto = true;
    })
    .await;
    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[
            ("Origin", &format!("https://{}", panel.addr)),
            ("X-Forwarded-Proto", "https"),
        ],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");
    assert!(reply.header("set-cookie").unwrap().contains("Secure"));
    panel.stop().await;
}

#[tokio::test]
async fn guessing_is_throttled_per_address() {
    let panel = Panel::start(&[]).await;

    let mut last = 0;
    for _ in 0..(auth::ATTEMPTS_PER_WINDOW + 1) {
        let reply = request(
            panel.addr,
            "POST",
            "/api/login",
            &[("Origin", &panel.origin())],
            Some("{\"password\":\"guess\"}"),
        )
        .await;
        last = reply.status;
    }

    assert_eq!(last, 429, "the sixth guess was not throttled");

    // The RIGHT password is refused too while the address is locked out,
    // so a lucky guess at the end of a run buys nothing.
    let reply = request(
        panel.addr,
        "POST",
        "/api/login",
        &[("Origin", &panel.origin())],
        Some(&format!("{{\"password\":\"{PASSWORD}\"}}")),
    )
    .await;
    assert_eq!(reply.status, 429, "{reply:?}");
    assert_eq!(reply.header("set-cookie"), None);
    assert!(reply.json()["retry_after_seconds"].as_u64().unwrap() > 0);

    panel.stop().await;
}

#[tokio::test]
async fn a_request_from_another_page_can_not_change_anything() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;

    for origin in [
        Some("https://evil.example"),
        Some("http://127.0.0.1:1"),
        Some("null"),
        // No Origin at all is refused, not trusted.
        None,
    ] {
        let mut headers = vec![("Cookie", cookie.as_str())];
        if let Some(origin) = origin {
            headers.push(("Origin", origin));
        }

        let reply = request(
            panel.addr,
            "POST",
            "/api/chains/1/stop",
            &headers,
            None,
        )
        .await;

        assert_eq!(reply.status, 403, "{origin:?}: {reply:?}");
    }

    // The chain is untouched.
    assert!(panel.runner.is_live(1));

    // Reading is not state-changing, so it works without an Origin.
    let reply = request(
        panel.addr,
        "GET",
        "/api/chains",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(reply.status, 200);

    panel.stop().await;
}

#[tokio::test]
async fn an_expired_session_is_refused_like_no_session_at_all() {
    let panel = Panel::start_with(&[1], |admin| {
        // A session that is idle for a millisecond is already stale.
        admin.sessions = Sessions::new(Duration::from_millis(1));
    })
    .await;

    let cookie = sign_in(&panel).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let reply = request(
        panel.addr,
        "GET",
        "/api/chains",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;

    assert_eq!(reply.status, 401);
    assert!(!reply.body.contains("stored_head"));

    panel.stop().await;
}

#[tokio::test]
async fn a_made_up_cookie_is_refused() {
    let panel = Panel::start(&[1]).await;

    for cookie in [
        "session=deadbeef",
        "session=",
        "session=00000000000000000000000000000000000000000000000000000000000000",
        "other=x",
    ] {
        let reply = request(
            panel.addr,
            "GET",
            "/api/chains",
            &[("Cookie", cookie)],
            None,
        )
        .await;
        assert_eq!(reply.status, 401, "{cookie}");
    }

    panel.stop().await;
}

#[tokio::test]
async fn signing_out_ends_the_session_for_that_cookie() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();

    let reply = request(
        panel.addr,
        "POST",
        "/api/logout",
        &[("Cookie", cookie.as_str()), ("Origin", &origin)],
        None,
    )
    .await;
    assert_eq!(reply.status, 200);
    assert!(reply.header("set-cookie").unwrap().contains("Max-Age=0"));

    let reply = request(
        panel.addr,
        "GET",
        "/api/chains",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(reply.status, 401);

    panel.stop().await;
}

#[tokio::test]
async fn a_signed_in_owner_sees_the_chains_and_the_settings_form() {
    let panel = Panel::start(&[1, 8453]).await;
    let cookie = sign_in(&panel).await;

    let reply = request(
        panel.addr,
        "GET",
        "/api/chains",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;

    assert_eq!(reply.status, 200);
    let body = reply.json();

    let chains = body["chains"].as_array().unwrap();
    assert_eq!(chains.len(), 2);
    assert!(chains.iter().all(|chain| {
        chain["state"].is_string() && chain["state_text"].is_string()
    }));

    // The form is built from the same table the CLI validates against.
    let settings = body["settings"].as_array().unwrap();
    assert_eq!(settings.len(), crate::configs::CHAIN_SETTINGS.len());
    assert!(settings
        .iter()
        .any(|setting| setting["name"] == "start-block"));

    panel.stop().await;
}

#[tokio::test]
async fn the_buttons_start_stop_and_restart_one_chain() {
    let panel = Panel::start(&[1, 8453]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();
    let headers =
        [("Cookie", cookie.as_str()), ("Origin", origin.as_str())];

    until("both running", || {
        panel.runner.is_live(1) && panel.runner.is_live(8453)
    })
    .await;

    // Stop one ...
    let reply =
        request(panel.addr, "POST", "/api/chains/1/stop", &headers, None)
            .await;
    assert_eq!(reply.status, 200);
    until("chain 1 stopped", || !panel.runner.is_live(1)).await;

    // ... the other keeps indexing.
    assert!(panel.runner.is_live(8453));
    assert_eq!(panel.runner.starts(8453), 1);

    // Start it again.
    let reply =
        request(panel.addr, "POST", "/api/chains/1/start", &headers, None)
            .await;
    assert_eq!(reply.status, 200);
    until("chain 1 running again", || panel.runner.is_live(1)).await;

    // Restart.
    let reply = request(
        panel.addr,
        "POST",
        "/api/chains/8453/restart",
        &headers,
        None,
    )
    .await;
    assert_eq!(reply.status, 200);
    until("8453 restarted", || panel.runner.starts(8453) == 2).await;

    panel.stop().await;
}

#[tokio::test]
async fn a_chain_can_be_added_by_id_or_by_name() {
    let panel = Panel::start(&[]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();
    let headers =
        [("Cookie", cookie.as_str()), ("Origin", origin.as_str())];

    let reply = request(
        panel.addr,
        "POST",
        "/api/chains",
        &headers,
        Some("{\"chain\":\"8453\"}"),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");
    until("8453 running", || panel.runner.is_live(8453)).await;

    // The same name the command line takes.
    let reply = request(
        panel.addr,
        "POST",
        "/api/chains",
        &headers,
        Some("{\"chain\":\"solana\"}"),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");
    until("solana running", || {
        panel.runner.is_live(crate::pipeline::solana::SOLANA_CHAIN_ID)
    })
    .await;

    // Nonsense is refused with a message that names the mistake.
    let reply = request(
        panel.addr,
        "POST",
        "/api/chains",
        &headers,
        Some("{\"chain\":\"mainnet\"}"),
    )
    .await;
    assert_eq!(reply.status, 400);
    assert!(reply.body.contains("invalid chain"), "{}", reply.body);

    panel.stop().await;
}

#[tokio::test]
async fn a_setting_the_command_line_refuses_is_refused_here_too() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();
    let headers =
        [("Cookie", cookie.as_str()), ("Origin", origin.as_str())];

    for body in [
        "{\"settings\":{\"start-block\":\"soon\"}}",
        "{\"settings\":{\"no-dex\":\"maybe\"}}",
        "{\"settings\":{\"delete-everything\":\"yes\"}}",
        "{\"settings\":{\"rpc\":\"--database\"}}",
    ] {
        let reply = request(
            panel.addr,
            "PATCH",
            "/api/chains/1",
            &headers,
            Some(body),
        )
        .await;

        assert_eq!(reply.status, 400, "{body}: {reply:?}");
    }

    // A good one is accepted.
    let reply = request(
        panel.addr,
        "PATCH",
        "/api/chains/1",
        &headers,
        Some("{\"settings\":{\"confirmations\":\"12\"}}"),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");

    panel.stop().await;
}

#[tokio::test]
async fn a_secret_setting_is_shown_redacted_and_never_in_full() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();
    let headers =
        [("Cookie", cookie.as_str()), ("Origin", origin.as_str())];

    let reply = request(
        panel.addr,
        "PATCH",
        "/api/chains/1",
        &headers,
        Some(
            "{\"settings\":{\"rpc\":\"https://eth.example/v2/hunter2secret\"}}",
        ),
    )
    .await;
    assert_eq!(reply.status, 200, "{reply:?}");

    let reply = request(
        panel.addr,
        "GET",
        "/api/chains",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;

    assert!(
        !reply.body.contains("hunter2secret"),
        "the API sent an API key to the browser: {}",
        reply.body
    );
    // The database url and the HyperSync token are never in a response at
    // all.
    assert!(!reply.body.contains("8123"), "{}", reply.body);
    assert!(
        !reply.body.contains("00000000-0000-0000-0000-000000000000"),
        "{}",
        reply.body
    );

    panel.stop().await;
}

#[tokio::test]
async fn the_events_of_a_chain_are_readable_and_scoped() {
    let panel = Panel::start(&[1]).await;
    panel.runner.behave(1, Behaviour::Fails);
    panel.supervisor.restart(1).await.unwrap();

    let cookie = sign_in(&panel).await;

    until("an error was recorded", || {
        panel
            .supervisor
            .events(1)
            .is_some_and(|events| events.iter().any(|e| e.kind == "error"))
    })
    .await;

    let reply = request(
        panel.addr,
        "GET",
        "/api/chains/1/events",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(reply.status, 200);
    assert!(!reply.json()["events"].as_array().unwrap().is_empty());

    // A chain that is not here is a 404, not an empty list.
    let reply = request(
        panel.addr,
        "GET",
        "/api/chains/99/events",
        &[("Cookie", cookie.as_str())],
        None,
    )
    .await;
    assert_eq!(reply.status, 404);

    panel.stop().await;
}

#[tokio::test]
async fn a_huge_body_is_refused_instead_of_being_buffered() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();

    let huge = format!(
        "{{\"settings\":{{\"rpc\":\"{}\"}}}}",
        "x".repeat(MAX_BODY_BYTES * 2)
    );

    let reply = request(
        panel.addr,
        "PATCH",
        "/api/chains/1",
        &[("Cookie", cookie.as_str()), ("Origin", origin.as_str())],
        Some(&huge),
    )
    .await;

    assert!(
        reply.status == 400 || reply.status == 413,
        "a {} byte body got {}",
        huge.len(),
        reply.status
    );

    panel.stop().await;
}

/// There is no endpoint that removes, purges or rewrites anything, and this
/// is the test that keeps it that way: it asks for every destructive shape
/// anyone might add and expects nothing to answer.
#[tokio::test]
async fn the_panel_has_no_destructive_endpoint() {
    let panel = Panel::start(&[1]).await;
    let cookie = sign_in(&panel).await;
    let origin = panel.origin();
    let headers =
        [("Cookie", cookie.as_str()), ("Origin", origin.as_str())];

    for (method, path) in [
        ("DELETE", "/api/chains/1"),
        ("DELETE", "/api/chains"),
        ("POST", "/api/chains/1/purge"),
        ("POST", "/api/chains/1/delete"),
        ("POST", "/api/chains/1/reindex"),
        ("POST", "/api/chains/1/reset"),
        ("POST", "/api/migrate"),
        ("POST", "/api/sql"),
    ] {
        let reply =
            request(panel.addr, method, path, &headers, None).await;

        assert!(
            reply.status == 404 || reply.status == 405,
            "{method} {path} answered {}",
            reply.status
        );
    }

    panel.stop().await;
}
