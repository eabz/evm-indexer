//! The control panel of `indexer fleet` (docs/design.md section 15).
//!
//! One embedded HTML page and a small JSON API, served by `axum` on
//! `--admin-addr` (default `127.0.0.1:8090`). It is **off unless
//! `ADMIN_PASSWORD` is set**: with no password there is nothing to bind.
//!
//! # What it can and cannot do
//!
//! It can start a chain, stop it, restart it, add one, and change the
//! options a chain starts with. That is the whole list. There is **no
//! destructive endpoint of any kind** - no purge, no delete, no schema
//! change, no re-index - and there is nothing to add one to: the panel
//! only ever calls [`Supervisor`](crate::fleet::supervisor::Supervisor),
//! whose whole vocabulary is start / stop / restart / settings.
//!
//! # The shape of a request
//!
//! ```text
//!   TCP (loopback unless --admin-allow-remote)
//!     -> body size cap (MAX_BODY_BYTES)
//!     -> security headers on the way out (CSP, X-Frame-Options: DENY,
//!        nosniff, no-referrer, no-store)
//!     -> /api/*: session cookie required          -> else 401, no data
//!     -> POST / PATCH: Origin must be this server -> else 403
//!     -> settings validated by the CLI's own parser (src/configs)
//! ```
//!
//! Every check is in `auth.rs` with the threat it answers written next to
//! it. Nothing this module sends to the browser contains a secret: chain
//! settings go through `fleet::supervisor::redact_settings`, and the
//! database url, the HyperSync token and the Redis url are never part of a
//! response at all.

pub mod auth;
pub mod page;

#[cfg(test)]
mod tests;

use crate::{
    configs::{ChainSettings, CHAIN_SETTINGS},
    fleet::supervisor::{ChainView, CommandError, Supervisor},
};
use anyhow::{Context, Result};
use auth::{Allowed, LoginRefused, Password, RateLimiter, Sessions};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::future::BoxFuture;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};

/// Largest request body the panel reads. Every request it has is a small
/// JSON object; anything bigger is a mistake or an attempt to make the
/// process allocate.
const MAX_BODY_BYTES: usize = 16 * 1024;

/// The cookie the session lives in.
const COOKIE: &str = "session";

pub struct Admin {
    supervisor: Arc<Supervisor>,
    password: Password,
    sessions: Sessions,
    limiter: RateLimiter,
    /// Mark the cookie `Secure` unconditionally (`--admin-secure-cookie`).
    secure_cookie: bool,
    /// Believe `X-Forwarded-Proto: https`. Off by default: a header any
    /// client can set must not decide whether a cookie is protected.
    trust_forwarded_proto: bool,
    /// The Content-Security-Policy, with the hashes of the page's own
    /// inline script and style. Computed once at start.
    csp: String,
}

/// Binds and serves the panel. `Ok(None)` when no password is set: the
/// panel is then not merely closed, it does not exist.
pub async fn start(
    supervisor: Arc<Supervisor>,
    shutdown: BoxFuture<'static, ()>,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(password) = Password::from_env().map_err(|e| {
        anyhow::anyhow!(
            "could not read random bytes for the admin password salt: {e}"
        )
    })?
    else {
        info!(
            "The control panel is off: set {} to switch it on.",
            crate::configs::ADMIN_PASSWORD_ENV
        );
        return Ok(None);
    };

    let config = supervisor.config();
    let addr = config.admin_addr;
    let secure_cookie = config.admin_secure_cookie;
    let trust_forwarded_proto = config.admin_trust_forwarded_proto;

    let admin = Arc::new(Admin {
        supervisor,
        password,
        sessions: Sessions::default(),
        limiter: RateLimiter::default(),
        secure_cookie,
        trust_forwarded_proto,
        csp: page::content_security_policy(),
    });

    let listener =
        tokio::net::TcpListener::bind(addr).await.with_context(|| {
            format!(
                "cannot bind the control panel to {addr} (is the port \
                 already in use?)"
            )
        })?;

    let bound = listener.local_addr().unwrap_or(addr);
    info!("Control panel on http://{bound}/ (password protected).");
    if !bound.ip().is_loopback() {
        warn!(
            "The control panel is listening on {bound}, which is not \
             localhost. It speaks plain HTTP: put a TLS reverse proxy in \
             front of it, or use an SSH tunnel (README)."
        );
    }

    let app = router(admin);

    Ok(Some(tokio::spawn(async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await;

        if let Err(e) = served {
            warn!("The control panel stopped: {e:#}");
        }
    })))
}

pub fn router(admin: Arc<Admin>) -> Router {
    Router::new()
        .route("/", get(serve_page))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/chains", get(list_chains).post(add_chain))
        .route("/api/chains/{chain}", axum::routing::patch(patch_chain))
        .route("/api/chains/{chain}/start", post(start_chain))
        .route("/api/chains/{chain}/stop", post(stop_chain))
        .route("/api/chains/{chain}/restart", post(restart_chain))
        .route("/api/chains/{chain}/events", get(chain_events))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(admin)
}

// --------------------------------------------------------- the responses

/// Every response leaves through here, so no route can forget a header.
///
/// * `Content-Security-Policy`: `default-src 'self'` plus the hashes of the
///   page's own inline script and style, so injected markup cannot run and
///   nothing can be loaded from another host.
/// * `X-Frame-Options: DENY` and `frame-ancestors 'none'`: the panel cannot
///   be framed, so it cannot be clickjacked into a Stop.
/// * `X-Content-Type-Options: nosniff`: JSON is never guessed to be HTML.
/// * `Referrer-Policy: no-referrer`: the address of a private panel never
///   leaks in a referrer.
/// * `Cache-Control: no-store`: nothing is written to a shared cache or to
///   the browser's disk cache.
fn secured(admin: &Admin, mut response: Response) -> Response {
    let headers = response.headers_mut();

    if let Ok(csp) = HeaderValue::from_str(&admin.csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers
        .insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );

    response
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    #[derive(Serialize)]
    struct Body {
        error: String,
    }

    (status, Json(Body { error: message.into() })).into_response()
}

// ------------------------------------------------------------- the guard

/// The cookie value, if the request carries one.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;

    cookies.split(';').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name.trim() == COOKIE).then(|| value.trim().to_string())
    })
}

/// The origin this server answers on, as a browser would write it.
///
/// Built from the request's own `Host`, so it works behind a proxy and on
/// whatever port the panel was given. `X-Forwarded-Proto` is only believed
/// when the operator said to: otherwise any client could claim `https` and
/// make the `Secure` decision for us.
fn expected_origin(admin: &Admin, headers: &HeaderMap) -> Option<String> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    Some(format!("{}://{host}", scheme(admin, headers)))
}

fn scheme(admin: &Admin, headers: &HeaderMap) -> &'static str {
    if admin.secure_cookie {
        return "https";
    }

    if admin.trust_forwarded_proto {
        let forwarded = headers
            .get("x-forwarded-proto")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();

        if forwarded.split(',').next().unwrap_or_default().trim()
            == "https"
        {
            return "https";
        }
    }

    "http"
}

/// Answers "may this request change anything", in this order:
///
/// 1. a live session (else 401 and NO data of any kind),
/// 2. for a state-changing method, an `Origin` that is this very server
///    (else 403).
///
/// `Some(response)` is the REFUSAL: every `/api` handler starts with one of
/// [`guard`] or [`guard_read`] and returns it as it is. A handler that
/// forgot would serve nothing useful anyway - the supervisor is only
/// reachable through them.
fn guard(
    admin: &Admin,
    headers: &HeaderMap,
    state_changing: bool,
) -> Option<Response> {
    // No cookie at all, an unknown one, or one that has expired: the same
    // refusal, and nothing else in the response.
    let allowed = cookie_token(headers)
        .is_some_and(|token| admin.sessions.touch(&token));

    if !allowed {
        return Some(unauthorized());
    }

    if !state_changing {
        return None;
    }

    let Some(expected) = expected_origin(admin, headers) else {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "This request did not say which page it came from.",
        ));
    };

    let origin =
        headers.get(header::ORIGIN).and_then(|value| value.to_str().ok());

    if !auth::same_origin(origin, &expected) {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "This request came from another page. Open the control panel \
             itself and try again.",
        ));
    }

    None
}

fn guard_read(admin: &Admin, headers: &HeaderMap) -> Option<Response> {
    guard(admin, headers, false)
}

/// The same answer for "no cookie", "unknown cookie" and "expired cookie":
/// nothing about the session table is revealed, and no data is attached.
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "Please sign in.",
            "authenticated": false
        })),
    )
        .into_response()
}

// ------------------------------------------------------------- the page

/// The page itself needs no session: it IS the login form, and it holds no
/// data about any chain - every number on it arrives later, from `/api`,
/// which does need one.
async fn serve_page(State(admin): State<Arc<Admin>>) -> Response {
    let response =
        ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page::HTML)
            .into_response();

    secured(&admin, response)
}

/// Is this request from the panel's own page? Used by the two routes that
/// change something WITHOUT needing a live session first (`login`, which
/// creates one, and `logout`, which destroys one).
fn from_our_page(admin: &Admin, headers: &HeaderMap) -> Option<Response> {
    let Some(expected) = expected_origin(admin, headers) else {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "This request did not say which page it came from.",
        ));
    };

    let origin =
        headers.get(header::ORIGIN).and_then(|value| value.to_str().ok());

    if !auth::same_origin(origin, &expected) {
        return Some(json_error(
            StatusCode::FORBIDDEN,
            "This request came from another page.",
        ));
    }

    None
}

// -------------------------------------------------------------- sign in

#[derive(Deserialize)]
struct LoginBody {
    password: String,
}

async fn login(
    State(admin): State<Arc<Admin>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<LoginBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    // Logging in is state-changing too: without this check another site
    // could silently sign the browser into an attacker's session.
    if let Some(denied) = from_our_page(&admin, &headers) {
        return secured(&admin, denied);
    }

    let address = peer.ip();

    // The throttle is checked BEFORE the password is even looked at, so a
    // locked-out address cannot use the comparison as an oracle.
    if let Allowed::Wait(left) = admin.limiter.check(address) {
        return secured(
            &admin,
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(LoginRefused {
                    error: format!(
                        "Too many attempts. Try again in {}.",
                        crate::fleet::status::human_duration(left)
                    ),
                    retry_after_seconds: Some(left.as_secs().max(1)),
                }),
            )
                .into_response(),
        );
    }

    let Ok(Json(body)) = body else {
        return secured(
            &admin,
            json_error(StatusCode::BAD_REQUEST, "Expected a password."),
        );
    };

    if !admin.password.matches(&body.password) {
        let lockout = admin.limiter.failed(address);
        warn!("Control panel: a sign-in from {address} was refused.");

        return secured(
            &admin,
            (
                StatusCode::UNAUTHORIZED,
                Json(LoginRefused {
                    error: match lockout {
                        Some(wait) => format!(
                            "Wrong password. Too many attempts: try again \
                             in {}.",
                            crate::fleet::status::human_duration(wait)
                        ),
                        None => "Wrong password.".to_string(),
                    },
                    retry_after_seconds: lockout.map(|w| w.as_secs()),
                }),
            )
                .into_response(),
        );
    }

    admin.limiter.succeeded(address);

    let Ok(token) = auth::random_token() else {
        return secured(
            &admin,
            json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not create a session.",
            ),
        );
    };
    admin.sessions.create(&token);

    let cookie = format!(
        "{COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{}",
        if scheme(&admin, &headers) == "https" { "; Secure" } else { "" }
    );

    let mut response =
        Json(serde_json::json!({ "authenticated": true })).into_response();

    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }

    info!("Control panel: signed in from {address}.");
    secured(&admin, response)
}

/// Signing out needs no live session (an expired cookie must still be
/// cleared), but it does need to come from this page: otherwise any site
/// could sign the owner out whenever they visited it.
async fn logout(
    State(admin): State<Arc<Admin>>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = from_our_page(&admin, &headers) {
        return secured(&admin, denied);
    }

    if let Some(token) = cookie_token(&headers) {
        admin.sessions.remove(&token);
    }

    let mut response = Json(serde_json::json!({ "authenticated": false }))
        .into_response();

    // Max-Age=0 removes it in the browser too.
    if let Ok(value) = HeaderValue::from_str(&format!(
        "{COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    )) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }

    secured(&admin, response)
}

// ------------------------------------------------------------- the API

#[derive(Serialize)]
struct SettingSchema {
    name: &'static str,
    kind: &'static str,
    label: &'static str,
    help: &'static str,
    secret: bool,
}

#[derive(Serialize)]
struct ChainsBody {
    chains: Vec<ChainView>,
    /// What the settings form is built from, so the page and the command
    /// line can never disagree about which options exist.
    settings: Vec<SettingSchema>,
}

async fn list_chains(
    State(admin): State<Arc<Admin>>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = guard_read(&admin, &headers) {
        return secured(&admin, denied);
    }

    let body = ChainsBody {
        chains: admin.supervisor.views().await,
        settings: CHAIN_SETTINGS
            .iter()
            .map(|setting| SettingSchema {
                name: setting.name,
                kind: match setting.kind {
                    crate::configs::SettingKind::Flag => "flag",
                    crate::configs::SettingKind::Number => "number",
                    crate::configs::SettingKind::Text => "text",
                },
                label: setting.label,
                help: setting.help,
                secret: setting.secret,
            })
            .collect(),
    };

    secured(&admin, Json(body).into_response())
}

#[derive(Deserialize)]
struct AddBody {
    /// A chain id, or a name the CLI knows (`solana`).
    chain: String,
    #[serde(default)]
    settings: ChainSettings,
}

async fn add_chain(
    State(admin): State<Arc<Admin>>,
    headers: HeaderMap,
    body: Result<Json<AddBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Some(denied) = guard(&admin, &headers, true) {
        return secured(&admin, denied);
    }

    let Ok(Json(body)) = body else {
        return secured(
            &admin,
            json_error(
                StatusCode::BAD_REQUEST,
                "Expected a chain and its settings.",
            ),
        );
    };

    // The SAME parser `--chain` uses, so `solana` works here exactly as it
    // does on the command line and nothing else is invented.
    let chain = match crate::configs::parse_chain_argument(&body.chain) {
        Ok(chain) => chain,
        Err(message) => {
            return secured(
                &admin,
                json_error(StatusCode::BAD_REQUEST, message),
            )
        }
    };

    match admin.supervisor.add(chain, body.settings).await {
        Ok(()) => secured(&admin, ok()),
        Err(e) => secured(&admin, command_error(e)),
    }
}

#[derive(Deserialize)]
struct SettingsBody {
    settings: ChainSettings,
}

async fn patch_chain(
    State(admin): State<Arc<Admin>>,
    Path(chain): Path<u64>,
    headers: HeaderMap,
    body: Result<
        Json<SettingsBody>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Response {
    if let Some(denied) = guard(&admin, &headers, true) {
        return secured(&admin, denied);
    }

    let Ok(Json(body)) = body else {
        return secured(
            &admin,
            json_error(StatusCode::BAD_REQUEST, "Expected settings."),
        );
    };

    match admin.supervisor.update_settings(chain, body.settings).await {
        Ok(()) => secured(&admin, ok()),
        Err(e) => secured(&admin, command_error(e)),
    }
}

async fn start_chain(
    State(admin): State<Arc<Admin>>,
    Path(chain): Path<u64>,
    headers: HeaderMap,
) -> Response {
    command(admin, chain, headers, Verb::Start).await
}

async fn stop_chain(
    State(admin): State<Arc<Admin>>,
    Path(chain): Path<u64>,
    headers: HeaderMap,
) -> Response {
    command(admin, chain, headers, Verb::Stop).await
}

async fn restart_chain(
    State(admin): State<Arc<Admin>>,
    Path(chain): Path<u64>,
    headers: HeaderMap,
) -> Response {
    command(admin, chain, headers, Verb::Restart).await
}

/// The whole vocabulary of the panel. There is no fourth verb, and none of
/// these three touches stored data.
enum Verb {
    Start,
    Stop,
    Restart,
}

async fn command(
    admin: Arc<Admin>,
    chain: u64,
    headers: HeaderMap,
    verb: Verb,
) -> Response {
    if let Some(denied) = guard(&admin, &headers, true) {
        return secured(&admin, denied);
    }

    let result = match verb {
        Verb::Start => admin.supervisor.start(chain).await,
        Verb::Stop => admin.supervisor.stop(chain).await,
        Verb::Restart => admin.supervisor.restart(chain).await,
    };

    match result {
        Ok(()) => secured(&admin, ok()),
        Err(e) => secured(&admin, command_error(e)),
    }
}

/// `GET /api/chains/{id}/events`: the recent errors, reorgs and restarts of
/// one chain.
async fn chain_events(
    State(admin): State<Arc<Admin>>,
    Path(chain): Path<u64>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = guard_read(&admin, &headers) {
        return secured(&admin, denied);
    }

    match admin.supervisor.events(chain) {
        Some(events) => secured(
            &admin,
            Json(serde_json::json!({ "events": events })).into_response(),
        ),
        None => secured(
            &admin,
            json_error(
                StatusCode::NOT_FOUND,
                format!("Chain {chain} is not in this fleet."),
            ),
        ),
    }
}

fn ok() -> Response {
    Json(serde_json::json!({ "ok": true })).into_response()
}

fn command_error(error: CommandError) -> Response {
    let status = match error {
        CommandError::NoSuchChain(_) => StatusCode::NOT_FOUND,
        CommandError::AlreadyThere(_) => StatusCode::CONFLICT,
        CommandError::Settings(_) => StatusCode::BAD_REQUEST,
    };

    json_error(status, error.to_string())
}
