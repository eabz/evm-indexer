//! Keeps secrets out of logs and error messages.
//!
//! RPC URLs commonly embed an API key in their path or query string, Redis
//! URLs may carry a password, and transport errors (reqwest in particular)
//! include the full URL they failed on.

const URL_PLACEHOLDER: &str = "<url>";
const SECRET_PLACEHOLDER: &str = "<redacted>";

/// Shortest URL path / query that is considered a potential secret. Avoids
/// mangling unrelated text for trivial components such as a Redis `/0`.
const MIN_SECRET_LEN: usize = 6;

/// Redacts URLs (any scheme) and the secret parts of one configured URL.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// A redactor that also knows the secret components (password, path,
    /// query) of `url`, so they are removed even when they show up outside
    /// of a well formed URL.
    pub fn for_url(url: &str) -> Self {
        let mut secrets = Vec::new();

        let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
        let (authority, tail) = match rest.find(['/', '?', '#']) {
            Some(index) => rest.split_at(index),
            None => (rest, ""),
        };

        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            let password =
                userinfo.split_once(':').map_or(userinfo, |(_, p)| p);
            if !password.is_empty() {
                secrets.push(password.to_string());
            }
        }

        let (path, query) = match tail.split_once('?') {
            Some((path, query)) => (path, query),
            None => (tail, ""),
        };
        let query = query.split('#').next().unwrap_or_default();

        // Whole path / query plus every path segment and query value: the
        // key may be echoed back on its own (e.g. in an HTTP error body).
        let parts = [path.trim_matches('/'), query]
            .into_iter()
            .chain(path.split('/'))
            .chain(query.split('&').filter_map(|pair| {
                pair.split_once('=').map(|(_, value)| value)
            }));
        for part in parts {
            if part.len() >= MIN_SECRET_LEN {
                secrets.push(part.to_string());
            }
        }

        // Longest first so a secret containing another is fully removed.
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        secrets.dedup();

        Self { secrets }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut text = redact_urls(text);
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), SECRET_PLACEHOLDER);
            }
        }
        text
    }
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

fn is_url_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ']' | '`')
}

/// Replaces everything that looks like `scheme://...` with a placeholder.
pub fn redact_urls(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(index) = rest.find("://") {
        let before = &rest[..index];
        let scheme_len = before
            .chars()
            .rev()
            .take_while(|c| is_scheme_char(*c))
            .count();

        let after = &rest[index + 3..];
        let url_len = after.find(is_url_end).unwrap_or(after.len());

        if scheme_len == 0 {
            // Not a URL: keep the text up to and including the "://".
            output.push_str(&rest[..index + 3]);
            rest = after;
            continue;
        }

        // Scheme chars are ASCII, so this is a char boundary.
        output.push_str(&before[..before.len() - scheme_len]);
        output.push_str(URL_PLACEHOLDER);
        rest = &after[url_len..];
    }

    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_reqwest_style_errors() {
        let error = "error sending request for url \
                     (https://eth-mainnet.g.alchemy.com/v2/SuPerSecretKey123): \
                     connection refused";
        let redacted = redact_urls(error);
        assert_eq!(
            redacted,
            "error sending request for url (<url>): connection refused"
        );
    }

    #[test]
    fn redacts_every_url_and_scheme() {
        let text = "a http://x/k1 b wss://y/k2?token=abc, c \
                    redis://:hunter2@cache:6379/0 d";
        let redacted = redact_urls(text);
        assert_eq!(redacted, "a <url> b <url> c <url> d");
        assert_eq!(redact_urls("https://only.example/KEY"), "<url>");
        assert_eq!(
            redact_urls("\"https://quoted/KEY\" end"),
            "\"<url>\" end"
        );
    }

    #[test]
    fn leaves_other_text_alone() {
        for text in [
            "",
            "execution reverted",
            "weird :// separator",
            "://",
            "ends with scheme http://",
            "unicode é://ü and 429 Too Many Requests",
        ] {
            let redacted = redact_urls(text);
            assert!(!redacted.contains("SECRET"));
            if !text.contains("http://") {
                assert_eq!(redacted, text);
            }
        }
        assert_eq!(
            redact_urls("ends with scheme http://"),
            "ends with scheme <url>"
        );
    }

    #[test]
    fn redacts_configured_secrets_outside_of_urls() {
        let redactor = Redactor::for_url(
            "https://user:p4ssw0rd@rpc.example.com/v2/SuPerSecretKey123?apikey=Query5ecret",
        );

        let redacted = redactor.redact(
            "HTTP error 401 with body: invalid key v2/SuPerSecretKey123 \
             (apikey=Query5ecret, p4ssw0rd) at \
             https://rpc.example.com/v2/SuPerSecretKey123",
        );

        assert!(!redacted.contains("SuPerSecretKey123"), "{redacted}");
        assert!(!redacted.contains("Query5ecret"), "{redacted}");
        assert!(!redacted.contains("p4ssw0rd"), "{redacted}");
        assert!(redacted.contains("HTTP error 401"));
    }

    #[test]
    fn redis_password_is_a_secret_but_the_db_number_is_not() {
        let redactor = Redactor::for_url("redis://:hunter2@cache:6379/0");
        assert_eq!(redactor.secrets, vec!["hunter2".to_string()]);
        assert_eq!(
            redactor.redact("AUTH failed for hunter2 on db 0"),
            "AUTH failed for <redacted> on db 0"
        );

        assert!(Redactor::for_url("redis://cache:6379")
            .secrets
            .is_empty());
        assert!(Redactor::for_url("not a url").secrets.is_empty());
        assert!(Redactor::for_url("").secrets.is_empty());
    }
}
