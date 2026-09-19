# admin

The control panel of `indexer fleet` (docs/design.md section 15): one
embedded HTML page and a small JSON API, served by `axum` on
`--admin-addr` (default `127.0.0.1:8090`).

**It is off unless `ADMIN_PASSWORD` is set.** With no password the port is
not even bound.

| File | What lives there |
|---|---|
| `mod.rs` | the router, the guard every `/api` handler starts with, the handlers, the security headers |
| `auth.rs` | the password, the sessions, the login throttle, the same-origin check - each with the threat it answers written next to it |
| `page.rs` | `include_str!` of the page, and the Content-Security-Policy built from its hashes |
| `page.html` | the whole panel: no build step, no bundler, no CDN, no request to any other host |
| `tests.rs` | every test speaks HTTP/1.1 to a real socket, so what is checked is what a browser receives |

## What it can do

Start a chain, stop it, restart it, add one, and change a short list of
options a chain starts with. That is the whole list.

There is **no destructive endpoint of any kind** - no purge, no delete, no
re-index, no schema change, no SQL - and there is nothing to add one to: the
panel only ever calls `fleet::supervisor::Supervisor`, whose entire
vocabulary is start / stop / restart / settings. A test asks for every
destructive shape anyone might add and expects nothing to answer.

**It also cannot change where a chain reads from.** The HyperSync endpoint
and token, the RPC endpoints, the database url and the Redis url come from
the fleet process's own flags and environment; the panel shows them as
"set" or "not set" and can change none of them. Before the security review
it could, and that was enough to send the owner's Envio token to any host
on the internet (`configs::fleet::CHAIN_SETTINGS` has the whole story).
The start block, the start DATE, the end block and `--new-blocks-only`
are out for the other reason: they decide the coverage floor, which design
section 16 fixes on a chain's first start. The panel shows that floor - the
same "gap-free from ... to ..." sentence `indexer verify` prints, once per
chain card - and shows it read-only. Moving it earlier is
`indexer backfill`; moving it later is refused everywhere, because data is
never dropped.

What IS editable: how far behind the head to stay, how deep a rollback may
go, how big and how frequent the writes are, and which decoders run.
`configs::fleet::NOT_PANEL_EDITABLE` names every other `run` option with
its reason, and a test walks the CLI and fails if a new flag is in neither
list.

| Method | Path | |
|---|---|---|
| `GET` | `/` | the page (a login form until you sign in) |
| `POST` | `/api/login` | `{"password": "..."}` |
| `POST` | `/api/logout` | |
| `GET` | `/api/chains` | every chain, its live numbers, and the settings form's fields |
| `POST` | `/api/chains` | `{"chain": "8453", "settings": {...}}` - `"solana"` works too |
| `PATCH` | `/api/chains/{id}` | `{"settings": {...}}`, applied at the next start |
| `POST` | `/api/chains/{id}/start` \| `/stop` \| `/restart` | |
| `GET` | `/api/chains/{id}/events` | recent errors, reorgs and restarts |

## The shape of a request

```text
  TCP (loopback unless --admin-allow-remote)
    -> connection cap (64), header-read 10s, request 30s, idle 60s
    -> Host is one we answer to        -> else 421, before any routing
    -> body size cap, 16 KiB
    -> security headers on EVERY response, from a layer
    -> /api/*: a live session cookie   -> else 401, and no data at all
    -> POST / PATCH: Origin is us      -> else 403
    -> settings judged by the CLI's own parser, from a short allow-list
```

Each connection is served in its own task, so a panic in a handler never
reaches the chain supervisors.

## Every check, and the threat it answers

| Threat | Answer |
|---|---|
| the password in `ps`, in a shell history, in `docker inspect` | `ADMIN_PASSWORD` is read from the environment only. It is not a clap argument, so it cannot reach a `--help` text or a `Debug` print either |
| a password short enough to guess | under 12 characters, one character repeated, or on a small built-in denylist, and the panel does not start. The log says why and prints `openssl rand -base64 18` |
| a memory dump hands out the password | only a salted SHA-256 of it is kept |
| timing says how much of a guess was right | the PASSWORD comparison is constant time (`subtle`) over two 32-byte digests. The session lookup is an ordinary hash-map probe, which is safe for a different reason: its key is a SHA-256 of the token, so a timing signal reveals nothing invertible |
| a guessable session token | 256 bits from the operating system's CSPRNG (`getrandom`) |
| a stolen token replayed for ever | 12 hours of idleness, and 7 days absolute, end a session |
| a leaked session table replayed | sessions are stored as the HASH of the token |
| JavaScript on another page reads the cookie | `HttpOnly` |
| a sibling subdomain shadows the cookie | `__Host-session` behind TLS (host-only, no `Domain`), and a cookie name seen twice is refused rather than resolved by taking the first |
| another site makes the browser act | `SameSite=Strict`, **and** an `Origin` check on every state-changing request - including sign in (login CSRF) and sign out. A request with no `Origin` is refused, not trusted |
| **DNS rebinding** (a page the owner visits, re-pointed at 127.0.0.1, talking to the panel from inside their machine) | `Host` is checked against a list the operator fixed - the bound address, the loopback names, and `--admin-host <name>` - **before any routing**. Anything else gets 421 and no content. Without this the origin check was only two client-supplied headers agreeing |
| the cookie in clear text | `Secure` when `--admin-secure-cookie`, or when a trusted proxy says `X-Forwarded-Proto: https` (`--admin-trust-forwarded-proto`, off by default: a header any client can set must not decide this) |
| brute force | 5 attempts a minute per address, then a lock-out. Checked BEFORE the password is looked at, so a lucky guess at the end of a run buys nothing |
| **an attacker locking the OWNER out** by failing five times every quarter of an hour | the lock-out decays with quiet time, and it never exceeds one minute unless the address is hammering (more than 20 failures inside one window). Behind a proxy, `--admin-trusted-proxy <ip>` lets the throttle tell clients apart by the right-most `X-Forwarded-For` hop; without that flag the header is ignored entirely |
| **an unauthenticated client holding connections open** until the indexer runs out of file descriptors | 64 connections, a 10 s header-read timeout, a 30 s request timeout and a 60 s idle timeout (`server.rs`). A connection with no permit is closed at once |
| a handler panic taking the indexing down | each connection is its own task |
| memory filled with sessions, addresses or chains | all three are capped (64 sessions, 4096 addresses, 256 chains) and pruned |
| one stuck chain hanging the process's shutdown | `Supervisor::shutdown` has a 60 s deadline; an abandoned chain's lease expires on its own |
| injected markup running as script | `Content-Security-Policy: default-src 'self'` with the page's own inline script and style allowed **by hash**, never `'unsafe-inline'` |
| clickjacking a Stop button | `X-Frame-Options: DENY` and `frame-ancestors 'none'` |
| JSON sniffed as HTML | `X-Content-Type-Options: nosniff` |
| a private panel's address in a referrer | `Referrer-Policy: no-referrer` |
| the panel's answers in a cache | `Cache-Control: no-store` |
| a refusal that never reached a handler going out bare | the headers are a layer around the whole router, so a 400, a 404, a 405 and a 413 carry them too. The chain id is parsed by us, so a bad one is answered with our sentence and not an echo of the input |
| a body that makes the process allocate | 16 KiB cap |
| **the panel redirecting an endpoint** and sending the HyperSync token to another host, reaching the host's private network, or feeding the indexer fabricated blocks | no endpoint and no credential is a panel-editable setting at all. `Source::verify_chain_id` also refuses an endpoint that cannot say which chain it serves, instead of warning and carrying on |
| a secret sent to the browser | a value the browser may not see is replaced by a fixed `<set>` marker, not run through a url redactor that only matches `scheme://` |
| the panel reachable from the network by accident | it binds loopback, and refuses anything else without `--admin-allow-remote` |

A refusal is the same 401 for "no cookie", "unknown cookie" and "expired
cookie", and it carries no data.

## The page

One file, compiled in with `include_str!`. It has one `<script>` and one
`<style>`, both inline and both named in the CSP by their SHA-256; a test
fails if a second one appears, if either grows an attribute, or if the page
ever mentions another host. Everything the page renders goes through
`textContent`, never `innerHTML`, so a chain's error message cannot become
markup.

It is built for a phone first: one card per chain with a status badge, how
far behind it is in blocks and in time, its speed, when it last wrote, how
long that write took, the rewinds it has seen, and the last problem in
plain words. Buttons: Start, Stop, Restart, Settings; and Add a chain.

It also shows, read-only, how the process itself was started: whether the
database url, the HyperSync token, the RPC endpoints and the metadata cache
are set, and the shared memory and query budgets. Never the values.

## Reaching it from somewhere else

The panel speaks plain HTTP and has no TLS of its own. Two supported ways,
both in the root README: an SSH tunnel (`ssh -L 8090:127.0.0.1:8090 host`,
nothing changes on the server), or a TLS reverse proxy in front of it with
`--admin-allow-remote` and `--admin-secure-cookie`.

Behind a proxy, two more flags matter:

- `--admin-host <name>` - the name the proxy serves the panel under. Without
  it every proxied request is refused with 421, because the panel only
  answers to names the operator fixed.
- `--admin-trusted-proxy <ip>` - the proxy's own address. Only then is
  `X-Forwarded-For` used to tell one sign-in attempt from another. Without
  it every client behind the proxy shares one throttle, so one attacker's
  lock-out falls on the owner too.

## Testing

```sh
cargo test admin::
```
