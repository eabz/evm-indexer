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
    net::{IpAddr, SocketAddr},
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

/// Failures inside ONE window past which an address is treated as hammering
/// rather than as someone mistyping. Only then can the lock-out grow beyond
/// [`ATTEMPT_WINDOW`].
pub const LOUD_FAILURES: u32 = ATTEMPTS_PER_WINDOW * 4;

/// Quiet time that forgives one doubling of the lock-out, measured from the
/// moment the last lock-out ended.
///
/// This is what makes the penalty DECAY. A lock-out that only ever grew,
/// and was only ever cleared by a successful login, meant an attacker who
/// failed five times every fifteen minutes could keep the owner out of
/// their own panel for ever (review MAJOR 2).
pub const DECAY_AFTER: Duration = Duration::from_secs(5 * 60);

/// Shortest password the panel will run behind.
///
/// Not a style rule: the panel is the start/stop control of every chain in
/// the process, and [`RateLimiter`] allows five guesses a minute, which is
/// enough to walk a PIN or a short word (review MAJOR 5).
pub const MIN_PASSWORD_CHARS: usize = 12;

/// Is this password long enough to run a control plane behind? The message
/// completes the sentence "`ADMIN_PASSWORD` ...".
pub fn check_strength(password: &str) -> Result<(), String> {
    let length = password.chars().count();

    if length < MIN_PASSWORD_CHARS {
        return Err(format!(
            "is {length} characters long and the minimum is \
             {MIN_PASSWORD_CHARS}."
        ));
    }

    Ok(())
}

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
    /// The password from the environment, or `None` when it is unset,
    /// blank or too short - in which case the panel is not served at all.
    ///
    /// Environment only, and deliberately not a clap argument: a flag is
    /// visible to every user on the host through `ps`, ends up in shell
    /// history, and would be printed by `--help` as a default.
    ///
    /// **Too short is refused, loudly.** It used to accept anything that
    /// was not blank, so `ADMIN_PASSWORD=x` started a panel that any guess
    /// opened (review MAJOR 5). At five tries a minute a four-digit PIN
    /// falls in a day and a single character falls at once. The panel is
    /// the start/stop control of every chain this process indexes, so a
    /// password under [`MIN_PASSWORD_CHARS`] characters is treated as not
    /// having set one - the port is not bound, and the log says why.
    pub fn from_env() -> Result<Option<Self>, getrandom::Error> {
        let name = crate::configs::ADMIN_PASSWORD_ENV;

        let Some(value) = std::env::var_os(name) else {
            return Ok(None);
        };

        let Some(password) = value.to_str() else {
            log::error!(
                "{name} is not valid text, so the control panel is off."
            );
            return Ok(None);
        };

        if password.trim().is_empty() {
            return Ok(None);
        }

        if let Err(why) = check_strength(password) {
            log::error!(
                "The control panel is OFF: {name} {why} The panel can \
                 start and stop the indexing of every chain in this \
                 process, so it will not run behind a password that can be \
                 guessed. Set a longer one and start the process again."
            );
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
    /// Failures that earned a lock-out and have not decayed yet. What makes
    /// each lock-out longer than the last.
    consecutive: u32,
    window_started: Instant,
    locked_until: Option<Instant>,
    /// When the most recent lock-out ENDED. The penalty decays from here,
    /// so an attacker who comes back every fifteen minutes does not ratchet
    /// it up for ever.
    unlocked_at: Option<Instant>,
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
            unlocked_at: None,
        });

        // A new window: the allowance starts again.
        if now.duration_since(state.window_started) >= ATTEMPT_WINDOW {
            state.window_started = now;
            state.failures = 0;
        }

        // And the PENALTY decays. Without this, `consecutive` only ever
        // grew and was only ever cleared by a successful login - which is
        // impossible while locked out, so five wrong guesses every fifteen
        // minutes denied the owner their own panel for ever (review
        // MAJOR 2). One step of the penalty is forgiven for every
        // DECAY_AFTER of quiet since the last lock-out ended.
        if let Some(unlocked_at) = state.unlocked_at {
            let quiet = now.saturating_duration_since(unlocked_at);
            let forgiven = u32::try_from(
                quiet.as_secs() / DECAY_AFTER.as_secs().max(1),
            )
            .unwrap_or(u32::MAX);
            state.consecutive = state.consecutive.saturating_sub(forgiven);
        }

        state.failures += 1;
        state.consecutive += 1;

        if state.failures < ATTEMPTS_PER_WINDOW {
            return None;
        }

        // Past the allowance for THIS window: lock out, doubling with every
        // further lock-out that has not decayed, capped.
        let over = state.consecutive.saturating_sub(ATTEMPTS_PER_WINDOW);
        let doubled = FIRST_LOCKOUT
            .saturating_mul(2u32.saturating_pow(over.min(16)))
            .min(MAX_LOCKOUT);

        // THE OWNER ALWAYS GETS BACK IN WITHIN A MINUTE, unless the address
        // is failing loudly RIGHT NOW.
        //
        // A patient attacker - five wrong guesses, wait, five more - is
        // exactly what the owner's own address looks like from behind a
        // reverse proxy, or from any other process on a loopback-only box.
        // Letting that ratchet the lock-out to fifteen minutes handed
        // anyone a permanent denial of the control plane (review MAJOR 2),
        // and it bought nothing: the allowance itself already holds the
        // guess rate at five a minute whatever the lock-out length is.
        //
        // So the long cap is reserved for an address that is hammering -
        // more than [`LOUD_FAILURES`] failures inside ONE window - which no
        // owner mistyping a password ever does.
        let loud = state.failures > LOUD_FAILURES;
        let lockout =
            if loud { doubled } else { doubled.min(ATTEMPT_WINDOW) };

        state.locked_until = Some(now + lockout);
        state.unlocked_at = Some(now + lockout);
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

/// The `Host` values this panel answers to.
///
/// # The attack this answers
///
/// Without it, the same-origin check compared two headers the CLIENT sends:
/// `expected_origin` was built from the request's own `Host`, and `Origin`
/// was compared to that. Two headers agreeing says nothing about which
/// server the request was aimed at.
///
/// The practical consequence is DNS rebinding. A page on `evil.example`
/// whose name is re-pointed at `127.0.0.1` is, to the browser, same-origin
/// with the panel: the browser sends `Host: evil.example` and
/// `Origin: http://evil.example`, they match, and a website the owner
/// merely VISITED is now talking to the panel from inside their machine. It
/// still needs the password - but "it only listens on localhost", which is
/// this panel's primary control, stops meaning anything.
///
/// So `Host` is checked against a list the OPERATOR fixed before any
/// routing happens: the address the panel was bound to, the loopback names,
/// and whatever `--admin-host` adds for a reverse proxy.
#[derive(Debug, Clone)]
pub struct AllowedHosts {
    /// Lowercased, each either `name` or `name:port`.
    entries: Vec<String>,
    /// The port the panel was bound to, so a `--admin-host` written without
    /// one still matches a browser that sends one.
    port: u16,
}

impl AllowedHosts {
    /// The default list for a panel bound to `addr`, plus the operator's
    /// own names.
    pub fn new(addr: SocketAddr, extra: &[String]) -> Self {
        let port = addr.port();
        let mut entries = vec![
            addr.to_string(),
            format!("localhost:{port}"),
            "localhost".to_string(),
            format!("127.0.0.1:{port}"),
            "127.0.0.1".to_string(),
            format!("[::1]:{port}"),
            "[::1]".to_string(),
        ];

        entries.extend(
            extra
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .filter(|name| !name.is_empty()),
        );

        entries.sort();
        entries.dedup();

        Self { entries, port }
    }

    /// Is this `Host` header one we answer to?
    ///
    /// An entry written WITHOUT a port also matches that name on the
    /// panel's own port, because a browser writes the port whenever it is
    /// not the scheme's default. An entry written WITH one must match
    /// exactly.
    pub fn accepts(&self, host: &str) -> bool {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }

        self.entries.iter().any(|entry| {
            *entry == host
                || (!entry.contains(':')
                    && host == format!("{entry}:{}", self.port))
                // An IPv6 literal is bracketed, so the only colon that can
                // introduce a port comes after the closing bracket.
                || (entry.starts_with('[')
                    && entry.ends_with(']')
                    && host == format!("{entry}:{}", self.port))
        })
    }

    /// For the message an operator sees when a request is refused.
    pub fn known(&self) -> &[String] {
        &self.entries
    }
}

/// Which address the login throttle counts against.
///
/// **`X-Forwarded-For` is a header, so by default it is a lie.** Anyone can
/// send one, and believing it would let a single attacker spread their
/// guesses over as many made-up addresses as they like - the throttle would
/// stop existing. So the peer address of the TCP connection is the key, and
/// nothing else, unless the operator has named the proxy that sits in front
/// of the panel (`--admin-trusted-proxy <ip>`).
///
/// When they have, and the connection really does come from that proxy, the
/// key is the RIGHT-MOST entry of `X-Forwarded-For` that is not the proxy
/// itself. Right-most, because a forwarding hop APPENDS: everything to the
/// left of it was written by someone further away and can be forged, while
/// the right-most entry is what the trusted proxy itself observed.
///
/// Without this, a panel behind a proxy sees every client as one address
/// and one attacker's lock-out falls on the owner too (review MAJOR 2).
pub fn throttle_key(
    peer: IpAddr,
    forwarded: Option<&str>,
    trusted_proxy: Option<IpAddr>,
) -> IpAddr {
    // Not configured, or this connection is not from the trusted proxy:
    // the header is ignored entirely.
    let Some(proxy) = trusted_proxy.filter(|proxy| *proxy == peer) else {
        return peer;
    };

    forwarded
        .unwrap_or_default()
        .rsplit(',')
        .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
        .find(|hop| *hop != proxy)
        .unwrap_or(peer)
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

    /// Review MAJOR 5: `ADMIN_PASSWORD=x` used to start a panel that any
    /// guess opened, with no warning at all.
    #[test]
    fn a_password_short_enough_to_guess_is_refused() {
        for weak in ["x", "1234", "hunter2", "admin", "0123456789 "] {
            assert!(
                check_strength(weak).is_err(),
                "{weak:?} was accepted as a password"
            );
        }

        // The message says what is wrong and what to do.
        let why = check_strength("short").unwrap_err();
        assert!(why.contains("12"), "{why}");
        assert!(why.contains("minimum"), "{why}");

        for good in [
            "a-very-good-password",
            "correct horse battery staple",
            "123456789012",
        ] {
            check_strength(good).unwrap_or_else(|why| {
                panic!("{good:?} was refused: {why}")
            });
        }
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

        // ... and it grows to the cap only while the address keeps
        // hammering INSIDE one window (see
        // `a_patient_attacker_can_not_lock_the_owner_out_for_longer_than_a_minute`
        // for why a slow one must not).
        let mut last = second;
        for _ in 0..30 {
            last = limiter
                .failed_at(who, now + FIRST_LOCKOUT)
                .expect("still locked out");
            assert!(last <= MAX_LOCKOUT, "{last:?}");
        }
        assert_eq!(last, MAX_LOCKOUT);
    }

    /// The lock-out is not a punishment for ever: an address that stops
    /// guessing for longer than the window (and is no longer locked out) is
    /// forgotten, so the owner who mistyped their password in the morning
    /// is not locked out in the afternoon.
    #[test]
    fn an_address_that_gives_up_is_forgotten() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let who = address(1);

        for _ in 0..(ATTEMPTS_PER_WINDOW + 1) {
            limiter.failed_at(who, now);
        }
        assert!(matches!(limiter.check_at(who, now), Allowed::Wait(_)));

        let much_later = now + MAX_LOCKOUT + ATTEMPT_WINDOW;
        assert_eq!(limiter.check_at(who, much_later), Allowed::Yes);

        // And the count starts again rather than locking out at once.
        assert_eq!(limiter.failed_at(who, much_later), None);
    }

    /// Review MAJOR 2. A patient attacker - five wrong guesses, wait, five
    /// more - must never be able to keep the owner out for more than a
    /// minute at a time. This is also exactly what the owner's OWN address
    /// looks like from behind a reverse proxy.
    #[test]
    fn a_patient_attacker_can_not_lock_the_owner_out_for_longer_than_a_minute(
    ) {
        let limiter = RateLimiter::default();
        let who = address(1);
        let mut at = Instant::now();

        for round in 0..40 {
            for _ in 0..ATTEMPTS_PER_WINDOW {
                let lockout = limiter.failed_at(who, at);
                if let Some(lockout) = lockout {
                    assert!(
                        lockout <= ATTEMPT_WINDOW,
                        "round {round}: locked out for {lockout:?}"
                    );
                }
            }

            // The attacker comes straight back the moment it lifts.
            at += ATTEMPT_WINDOW;

            // And whenever it has lifted, the owner gets in.
            assert_eq!(
                limiter.check_at(who, at),
                Allowed::Yes,
                "round {round}: the owner is still locked out"
            );
        }
    }

    /// The penalty is not permanent: quiet time forgives it, so a slow
    /// attacker cannot ratchet it up over an afternoon.
    #[test]
    fn the_penalty_decays_with_quiet_time() {
        let limiter = RateLimiter::default();
        let who = address(1);
        let mut at = Instant::now();

        // Hammer hard enough to earn a real lock-out.
        for _ in 0..(LOUD_FAILURES + 2) {
            limiter.failed_at(who, at);
        }
        let hammered = limiter
            .failed_at(who, at)
            .expect("a loud address is locked out");
        assert!(hammered > ATTEMPT_WINDOW, "{hammered:?}");

        // Now go quiet for a long time, then fail the allowance again.
        at += MAX_LOCKOUT + DECAY_AFTER * 20;
        let mut after = None;
        for _ in 0..ATTEMPTS_PER_WINDOW {
            after = limiter.failed_at(who, at);
        }

        assert!(
            after.is_none_or(|wait| wait <= ATTEMPT_WINDOW),
            "the penalty did not decay: {after:?}"
        );
    }

    /// The other half: someone actually hammering still earns the long cap.
    #[test]
    fn a_loud_attacker_still_earns_the_long_lock_out() {
        let limiter = RateLimiter::default();
        let who = address(1);
        let now = Instant::now();

        let mut longest = Duration::ZERO;
        for _ in 0..(LOUD_FAILURES * 3) {
            if let Some(lockout) = limiter.failed_at(who, now) {
                longest = longest.max(lockout);
            }
        }

        assert_eq!(longest, MAX_LOCKOUT, "a flood was not locked out");
    }

    /// `X-Forwarded-For` is a header: believing it by default would let one
    /// attacker spread their guesses over as many invented addresses as
    /// they like, and the throttle would stop existing.
    #[test]
    fn the_forwarded_address_is_ignored_unless_a_proxy_was_named() {
        let peer = address(1);
        let client: IpAddr = "203.0.113.9".parse().unwrap();

        // Nothing configured: the header is not read at all.
        assert_eq!(throttle_key(peer, Some("203.0.113.9"), None), peer);

        // Configured, but this connection is NOT from the proxy: ignored.
        let proxy: IpAddr = "10.0.0.7".parse().unwrap();
        assert_eq!(
            throttle_key(peer, Some("203.0.113.9"), Some(proxy)),
            peer
        );

        // From the proxy: the right-most hop that is not the proxy wins,
        // because a forwarding hop APPENDS and everything to its left can
        // be forged by the client.
        assert_eq!(
            throttle_key(
                proxy,
                Some("198.51.100.1, 203.0.113.9"),
                Some(proxy)
            ),
            client
        );
        assert_eq!(
            throttle_key(
                proxy,
                Some("203.0.113.9, 10.0.0.7"),
                Some(proxy)
            ),
            client
        );

        // Garbage, or no header at all, falls back to the peer.
        assert_eq!(
            throttle_key(proxy, Some("not-an-ip"), Some(proxy)),
            proxy
        );
        assert_eq!(throttle_key(proxy, None, Some(proxy)), proxy);
        assert_eq!(throttle_key(proxy, Some(""), Some(proxy)), proxy);
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

    /// Review MAJOR 3. `Host` decides which server a browser thinks it is
    /// talking to; if anything is accepted, the same-origin check is just
    /// two attacker-supplied headers agreeing with each other.
    #[test]
    fn only_the_hosts_the_operator_fixed_are_answered() {
        let addr: SocketAddr = "127.0.0.1:8090".parse().unwrap();
        let hosts = AllowedHosts::new(addr, &[]);

        // What a browser actually sends to a loopback panel.
        assert!(hosts.accepts("127.0.0.1:8090"));
        assert!(hosts.accepts("localhost:8090"));
        assert!(hosts.accepts("LOCALHOST:8090"));
        assert!(hosts.accepts("[::1]:8090"));
        assert!(hosts.accepts(" localhost:8090 "));

        // DNS rebinding: a name that resolves to 127.0.0.1 but is not ours.
        assert!(!hosts.accepts("evil.example"));
        assert!(!hosts.accepts("panel.evil.example"));
        assert!(!hosts.accepts("localhost.evil.example"));
        assert!(!hosts.accepts("evil.example:8090"));
        // Our own name on somebody else's port is a different origin.
        assert!(!hosts.accepts("127.0.0.1:9999"));
        assert!(!hosts.accepts(""));
        assert!(!hosts.accepts("   "));
    }

    #[test]
    fn a_reverse_proxy_name_is_added_by_the_operator_and_nobody_else() {
        let addr: SocketAddr = "127.0.0.1:8090".parse().unwrap();
        let hosts =
            AllowedHosts::new(addr, &["indexer.example.com".to_string()]);

        // As a browser writes it on 443 (no port) and on our own port.
        assert!(hosts.accepts("indexer.example.com"));
        assert!(hosts.accepts("indexer.example.com:8090"));
        assert!(hosts.accepts("INDEXER.EXAMPLE.COM"));

        // A neighbour is still not us.
        assert!(!hosts.accepts("other.example.com"));
        assert!(!hosts.accepts("indexer.example.com.evil.example"));
        // ... and neither is our name on an arbitrary port.
        assert!(!hosts.accepts("indexer.example.com:9999"));
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
