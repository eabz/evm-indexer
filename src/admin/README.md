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

Start a chain, stop it, restart it, add one, change the options a chain
starts with. That is the whole list.

There is **no destructive endpoint of any kind** - no purge, no delete, no
re-index, no schema change, no SQL - and there is nothing to add one to: the
panel only ever calls `fleet::supervisor::Supervisor`, whose entire
vocabulary is start / stop / restart / settings. A test asks for every
destructive shape anyone might add and expects nothing to answer.

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
    -> body size cap, 16 KiB
    -> security headers on the way out
    -> /api/*: a live session cookie   -> else 401, and no data at all
    -> POST / PATCH: Origin is us      -> else 403
    -> settings judged by the CLI's own parser (src/configs)
```

## Every check, and the threat it answers

| Threat | Answer |
|---|---|
| the password in `ps`, in a shell history, in `docker inspect` | `ADMIN_PASSWORD` is read from the environment only. It is not a clap argument, so it cannot reach a `--help` text or a `Debug` print either |
| a memory dump hands out the password | only a salted SHA-256 of it is kept |
| timing says how much of a guess was right | constant-time comparison (`subtle`) |
| a guessable session token | 256 bits from the operating system's CSPRNG (`getrandom`) |
| a stolen token replayed for ever | 12 hours of idleness expires a session |
| a leaked session table replayed | sessions are stored as the HASH of the token |
| JavaScript on another page reads the cookie | `HttpOnly` |
| another site makes the browser act | `SameSite=Strict`, **and** an `Origin` check on every state-changing request - including sign in (login CSRF) and sign out. A request with no `Origin` is refused, not trusted |
| the cookie in clear text | `Secure` when `--admin-secure-cookie`, or when a trusted proxy says `X-Forwarded-Proto: https` (`--admin-trust-forwarded-proto`, off by default: a header any client can set must not decide this) |
| brute force | 5 attempts a minute per address, then a lock-out doubling from 30 s to 15 min. Checked BEFORE the password is looked at, so a lucky guess at the end of a run buys nothing |
| memory filled with sessions or addresses | both tables are pruned and capped |
| injected markup running as script | `Content-Security-Policy: default-src 'self'` with the page's own inline script and style allowed **by hash**, never `'unsafe-inline'` |
| clickjacking a Stop button | `X-Frame-Options: DENY` and `frame-ancestors 'none'` |
| JSON sniffed as HTML | `X-Content-Type-Options: nosniff` |
| a private panel's address in a referrer | `Referrer-Policy: no-referrer` |
| the panel's answers in a cache | `Cache-Control: no-store` |
| a body that makes the process allocate | 16 KiB cap |
| a secret sent to the browser | settings go through `fleet::supervisor::redact_settings` (`tokens::redact`); the database url, the HyperSync token and the Redis url are never part of a response at all |
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

A secret setting is shown redacted and its box starts empty: leaving it
empty keeps what is stored, because the page was never shown the real value
and must not send the redacted one back.

## Reaching it from somewhere else

The panel speaks plain HTTP and has no TLS of its own. Two supported ways,
both in the root README: an SSH tunnel (`ssh -L 8090:127.0.0.1:8090 host`,
nothing changes on the server), or a TLS reverse proxy in front of it with
`--admin-allow-remote` and `--admin-secure-cookie`.

## Testing

```sh
cargo test admin::
```
