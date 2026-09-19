//! Who is allowed to press the buttons (docs/design.md section 15).
//!
//! Each piece below answers one threat, and says which. Nothing here is
//! clever; the point is that every check can be read in one sitting.
//!
//! | Threat | Answer |
//! |---|---|
//! | the password ends up in `ps`, a shell history, a compose file dump | it comes from the environment only, never from a flag ([`Password::from_env`]) |
//! | a memory dump or a core file hands out the password | only a salted SHA-256 of it is kept ([`Password`]) |
//! | timing tells an attacker how much of a guess was right | every comparison is constant time (`subtle`) |
//! | a guessable session token | 256 bits from the operating system's CSPRNG ([`random_token`]) |
//! | a stolen token replayed for ever | 12 h of idleness expires a session ([`Sessions`]) |
//! | a leaked token list (log, dump) replayed | sessions are stored as the HASH of the token, never the token |
//! | JavaScript on another page reads the cookie | `HttpOnly` |
//! | another site makes the browser send the cookie | `SameSite=Strict`, plus an `Origin` check on every state-changing request ([`same_origin`]) |
//! | the cookie travels in clear text | `Secure` when the panel is behind TLS |
//! | brute force over the network | 5 attempts a minute per address, then a doubling lock-out ([`RateLimiter`]) |
//! | an attacker fills memory with addresses or sessions | both maps are capped and pruned |

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

/// A session that has not been used for this long is gone
/// (docs/design.md section 15).
pub const IDLE_EXPIRY: Duration = Duration::from_secs(12 * 60 * 60);

/// Failed logins allowed per address before the lock-out starts.
pub const ATTEMPTS_PER_WINDOW: u32 = 5;

/// The window those attempts are counted in.
pub const ATTEMPT_WINDOW: Duration = Duration::from_secs(60);

/// First lock-out; it doubles with every further failure.
pub const FIRST_LOCKOUT: Duration = Duration::from_secs(30);

/// Longest lock-out. Long enough that guessing is hopeless, short enough
/// that the owner who fat-fingered their password is not locked out for
/// the evening.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Sessions kept at once. The owner is one person on a handful of devices;
/// anything beyond this is someone filling memory.
const MAX_SESSIONS: usize = 64;

/// Addresses tracked for rate limiting at once.
const MAX_TRACKED_ADDRESSES: usize = 4_096;

/// Reads 32 bytes from the operating system's CSPRNG.
///
/// Not a hash of the clock, not a `RandomState` hasher: a session token is
/// the only thing standing between the network and the Start/Stop buttons,
/// so it has to be unguessable even by someone who knows exactly when the
/// process started.
fn random_bytes() -> Result<[u8; 32], getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes)
}

/// A fresh session token, as the hex string that goes into the cookie.
pub fn random_token() -> Result<String, getrandom::Error> {
    Ok(hex::encode(random_bytes()?))
}

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// The panel's password, as a salted hash.
///
/// **Why SHA-256 and not argon2.** A password hash is slowed down to make
/// an OFFLINE attack on a stolen hash database expensive. There is no
/// database here: the hash exists for the length of one process, is never
/// written anywhere, and the only way to reach it is to already be reading
/// this process's memory - at which point the attacker can read the session
/// table and the database password too. Online guessing is answered by
/// [`RateLimiter`], not by the hash's cost. So the salt (which stops a
/// dumped hash from being recognised as a known password) is worth having,
/// and the extra dependency and tuning of argon2 are not.
pub struct Password {
    salt: [u8; 32],
    hash: [u8; 32],
}

impl Password {
    /// The password from the environment, or `None` when it is unset or
    /// blank - in which case the panel is not served at all.
    ///
    /// Environment only, and deliberately not a clap argument: a flag is
    /// visible to every user on the host through `ps`, ends up in shell
    /// history, and would be printed by `--help` as a default.
    pub fn from_env() -> Result<Option<Self>, getrandom::Error> {
        let Some(value) =
            std::env::var_os(crate::configs::ADMIN_PASSWORD_ENV)
        else {
            return Ok(None);
        };

        let Some(password) = value.to_str() else {
            return Ok(None);
        };

        if password.trim().is_empty() {
            return Ok(None);
        }

        Ok(Some(Self::new(password)?))
    }

    pub fn new(password: &str) -> Result<Self, getrandom::Error> {
        let salt = random_bytes()?;
        Ok(Self { hash: sha256(&[&salt, password.as_bytes()]), salt })
    }

    /// Constant time: the answer takes the same time whether the first
    /// byte is wrong or only the last one is.
    pub fn matches(&self, attempt: &str) -> bool {
        let attempt = sha256(&[&self.salt, attempt.as_bytes()]);
        self.hash.ct_eq(&attempt).into()
    }
}

/// Live sessions, keyed by the HASH of the token.
///
/// Hashing the key means a leak of this table (a dump, a debugger, a future
/// log line) hands out nothing that can be replayed: an attacker would have
/// to invert SHA-256 to get the cookie value back.
pub struct Sessions {
    live: Mutex<HashMap<[u8; 32], Instant>>,
    idle_expiry: Duration,
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new(IDLE_EXPIRY)
    }
}

impl Sessions {
    pub fn new(idle_expiry: Duration) -> Self {
        Self { live: Mutex::new(HashMap::new()), idle_expiry }
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Instant>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Remembers a token and returns it. Prunes expired sessions first, so
    /// the table cannot grow without bound.
    pub fn create(&self, token: &str) {
        self.create_at(token, Instant::now());
    }

    fn create_at(&self, token: &str, now: Instant) {
        let mut live = self.lock();
        live.retain(|_, last| {
            now.duration_since(*last) < self.idle_expiry
        });

        // Still full after pruning: someone is logging in in a loop. Drop
        // the least recently used one rather than growing.
        while live.len() >= MAX_SESSIONS {
            let Some(oldest) = live
                .iter()
                .min_by_key(|(_, last)| **last)
                .map(|(key, _)| *key)
            else {
                break;
            };
            live.remove(&oldest);
        }

        live.insert(sha256(&[token.as_bytes()]), now);
    }

    /// Is this cookie value a live session? Touches it, so the 12 hours are
    /// idle time and not absolute time.
    pub fn touch(&self, token: &str) -> bool {
        self.touch_at(token, Instant::now())
    }

    fn touch_at(&self, token: &str, now: Instant) -> bool {
        let key = sha256(&[token.as_bytes()]);
        let mut live = self.lock();

        let Some(last) = live.get(&key).copied() else {
            return false;
        };

        if now.duration_since(last) >= self.idle_expiry {
            live.remove(&key);
            return false;
        }

        live.insert(key, now);
        true
    }

    pub fn remove(&self, token: &str) {
        self.lock().remove(&sha256(&[token.as_bytes()]));
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy)]
struct Attempts {
    /// Failures inside the current window.
    failures: u32,
    /// Total failures since the last success, for the doubling lock-out.
    consecutive: u32,
    window_started: Instant,
    locked_until: Option<Instant>,
}

/// Per-address login throttle.
///
/// The threat is someone with the panel's address and a word list. Five
/// tries a minute makes even a small list take years, and the doubling
/// lock-out after that makes a distributed attempt expensive per address
/// while an honest owner who mistyped waits half a minute.
#[derive(Default)]
pub struct RateLimiter {
    per_address: Mutex<HashMap<IpAddr, Attempts>>,
}

/// What a login attempt is allowed to do right now.
#[derive(Debug, PartialEq, Eq)]
pub enum Allowed {
    Yes,
    /// Locked out; try again in this long.
    Wait(Duration),
}

impl RateLimiter {
    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<IpAddr, Attempts>> {
        self.per_address.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn check(&self, address: IpAddr) -> Allowed {
        self.check_at(address, Instant::now())
    }

    fn check_at(&self, address: IpAddr, now: Instant) -> Allowed {
        let attempts = self.lock();

        match attempts.get(&address) {
            Some(state) => match state.locked_until {
                Some(until) if until > now => Allowed::Wait(until - now),
                _ => Allowed::Yes,
            },
            None => Allowed::Yes,
        }
    }

    /// Records a failed attempt and returns the lock-out it caused, if any.
    pub fn failed(&self, address: IpAddr) -> Option<Duration> {
        self.failed_at(address, Instant::now())
    }

    fn failed_at(
        &self,
        address: IpAddr,
        now: Instant,
    ) -> Option<Duration> {
        let mut attempts = self.lock();
        prune(&mut attempts, now);

        let state = attempts.entry(address).or_insert(Attempts {
            failures: 0,
            consecutive: 0,
            window_started: now,
            locked_until: None,
        });

        if now.duration_since(state.window_started) >= ATTEMPT_WINDOW {
            state.window_started = now;
            state.failures = 0;
        }

        state.failures += 1;
        state.consecutive += 1;

        if state.failures < ATTEMPTS_PER_WINDOW {
            return None;
        }

        // Past the allowance: lock out, doubling with every further
        // failure, capped so an honest owner is never locked out for long.
        let over = state.consecutive.saturating_sub(ATTEMPTS_PER_WINDOW);
        let lockout = FIRST_LOCKOUT
            .saturating_mul(2u32.saturating_pow(over.min(16)))
            .min(MAX_LOCKOUT);

        state.locked_until = Some(now + lockout);
        Some(lockout)
    }

    /// A correct password clears the address.
    pub fn succeeded(&self, address: IpAddr) {
        self.lock().remove(&address);
    }
}

/// Forgets addresses whose window is long over, and caps the table.
fn prune(attempts: &mut HashMap<IpAddr, Attempts>, now: Instant) {
    attempts.retain(|_, state| {
        state.locked_until.is_some_and(|until| until > now)
            || now.duration_since(state.window_started) < ATTEMPT_WINDOW
    });

    while attempts.len() >= MAX_TRACKED_ADDRESSES {
        let Some(oldest) = attempts
            .iter()
            .min_by_key(|(_, state)| state.window_started)
            .map(|(address, _)| *address)
        else {
            break;
        };
        attempts.remove(&oldest);
    }
}

/// Does this request come from the panel's own page?
///
/// `SameSite=Strict` already stops a browser from sending the cookie on a
/// cross-site request, but it is one mechanism in one place. This is the
/// second: a state-changing request must carry an `Origin` header, and it
/// must be this server's own origin. A form on another site sends its OWN
/// origin, and a request with no `Origin` at all is refused rather than
/// trusted (every browser sends it on POST and PATCH).
pub fn same_origin(origin: Option<&str>, expected: &str) -> bool {
    origin.is_some_and(|origin| origin.eq_ignore_ascii_case(expected))
}

/// What the browser is told about a failed login. Never says whether the
/// password was close, and never distinguishes "no password set" from
/// "wrong password".
#[derive(Debug, Serialize)]
pub struct LoginRefused {
    pub error: String,
    /// Seconds to wait, when the address is locked out.
    pub retry_after_seconds: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn address(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    #[test]
    fn the_password_is_never_kept_in_the_clear() {
        let password = Password::new("hunter2").unwrap();

        // Nothing in the struct is the password.
        assert_ne!(&password.hash[..], b"hunter2");
        assert_ne!(&password.salt[..], b"hunter2");

        assert!(password.matches("hunter2"));
        assert!(!password.matches("hunter3"));
        assert!(!password.matches(""));
        assert!(!password.matches("hunter2 "));
    }

    #[test]
    fn the_same_password_hashes_differently_in_two_processes() {
        let a = Password::new("hunter2").unwrap();
        let b = Password::new("hunter2").unwrap();

        assert_ne!(a.salt, b.salt, "the salt must be random");
        assert_ne!(a.hash, b.hash, "so the hash must differ too");
        assert!(a.matches("hunter2") && b.matches("hunter2"));
    }

    #[test]
    fn tokens_are_unguessable_and_all_different() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            let token = random_token().unwrap();
            assert_eq!(token.len(), 64, "256 bits as hex");
            assert!(seen.insert(token), "a token repeated");
        }
    }

    #[test]
    fn a_session_lives_until_it_is_idle_for_twelve_hours() {
        let sessions = Sessions::new(Duration::from_secs(3_600));
        let start = Instant::now();
        let token = random_token().unwrap();

        sessions.create_at(&token, start);
        assert!(sessions.touch_at(&token, start));

        // Used inside the window: still alive an hour after THAT.
        let later = start + Duration::from_secs(3_000);
        assert!(sessions.touch_at(&token, later));
        assert!(
            sessions.touch_at(&token, later + Duration::from_secs(3_000))
        );

        // Left alone for longer than the idle time: gone, and forgotten.
        let much_later = later + Duration::from_secs(10_000);
        assert!(!sessions.touch_at(&token, much_later));
        assert!(sessions.is_empty());
    }

    #[test]
    fn an_unknown_token_is_never_a_session() {
        let sessions = Sessions::default();
        sessions.create(&random_token().unwrap());

        assert!(!sessions.touch(&random_token().unwrap()));
        assert!(!sessions.touch(""));
        assert!(!sessions.touch("../../etc/passwd"));
    }

    #[test]
    fn logging_out_ends_the_session_at_once() {
        let sessions = Sessions::default();
        let token = random_token().unwrap();
        sessions.create(&token);
        assert!(sessions.touch(&token));

        sessions.remove(&token);
        assert!(!sessions.touch(&token));
    }

    #[test]
    fn a_flood_of_logins_can_not_grow_the_session_table() {
        let sessions = Sessions::default();
        for _ in 0..(MAX_SESSIONS * 10) {
            sessions.create(&random_token().unwrap());
        }
        assert!(sessions.len() <= MAX_SESSIONS);
    }

    #[test]
    fn five_wrong_guesses_a_minute_then_a_doubling_lock_out() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let who = address(1);

        for _ in 0..(ATTEMPTS_PER_WINDOW - 1) {
            assert_eq!(limiter.failed_at(who, now), None);
            assert_eq!(limiter.check_at(who, now), Allowed::Yes);
        }

        let first = limiter.failed_at(who, now).expect("locked out");
        assert_eq!(first, FIRST_LOCKOUT);
        assert!(matches!(
            limiter.check_at(who, now),
            Allowed::Wait(left) if left <= FIRST_LOCKOUT
        ));

        // Another failure doubles it ...
        let second = limiter
            .failed_at(who, now + FIRST_LOCKOUT)
            .expect("locked out again");
        assert_eq!(second, FIRST_LOCKOUT * 2);

        // ... and it never grows past the cap.
        let mut at = now + FIRST_LOCKOUT;
        for _ in 0..30 {
            at += MAX_LOCKOUT;
            let lockout = limiter.failed_at(who, at).expect("locked out");
            assert!(lockout <= MAX_LOCKOUT, "{lockout:?}");
        }
    }

    #[test]
    fn one_addresss_lock_out_never_touches_another() {
        let limiter = RateLimiter::default();
        let now = Instant::now();

        for _ in 0..(ATTEMPTS_PER_WINDOW + 2) {
            limiter.failed_at(address(1), now);
        }

        assert!(matches!(
            limiter.check_at(address(1), now),
            Allowed::Wait(_)
        ));
        assert_eq!(limiter.check_at(address(2), now), Allowed::Yes);
    }

    #[test]
    fn the_right_password_clears_the_address() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let who = address(1);

        for _ in 0..(ATTEMPTS_PER_WINDOW + 1) {
            limiter.failed_at(who, now);
        }
        assert!(matches!(limiter.check_at(who, now), Allowed::Wait(_)));

        limiter.succeeded(who);
        assert_eq!(limiter.check_at(who, now), Allowed::Yes);
    }

    #[test]
    fn a_flood_of_addresses_can_not_grow_the_table() {
        let limiter = RateLimiter::default();
        let now = Instant::now();

        for i in 0..(MAX_TRACKED_ADDRESSES + 500) {
            let octets = (i as u32).to_be_bytes();
            limiter.failed_at(
                IpAddr::V4(Ipv4Addr::from(octets)),
                now + Duration::from_secs(i as u64),
            );
        }

        assert!(limiter.lock().len() <= MAX_TRACKED_ADDRESSES);
    }

    #[test]
    fn only_this_servers_own_origin_may_change_anything() {
        let expected = "http://127.0.0.1:8090";

        assert!(same_origin(Some(expected), expected));
        assert!(same_origin(Some("HTTP://127.0.0.1:8090"), expected));

        // Missing entirely: refused, not trusted.
        assert!(!same_origin(None, expected));
        // Another site.
        assert!(!same_origin(Some("https://evil.example"), expected));
        // The right host on another port or scheme is another origin.
        assert!(!same_origin(Some("http://127.0.0.1:9090"), expected));
        assert!(!same_origin(Some("https://127.0.0.1:8090"), expected));
        // A prefix is not a match.
        assert!(!same_origin(
            Some("http://127.0.0.1:8090.evil.example"),
            expected
        ));
        assert!(!same_origin(Some("null"), expected));
    }
}
