//! The one page the panel serves, and the policy that locks it down.
//!
//! `page.html` is compiled into the binary with `include_str!`: no build
//! step, no bundler, no CDN, and the page makes no request to anything but
//! this server. That is a security property as much as a packaging one -
//! there is no third party who can change what the owner's browser runs.
//!
//! Its script and style are inline, which `Content-Security-Policy` would
//! normally forbid. Rather than weaken the policy with `'unsafe-inline'`,
//! [`content_security_policy`] HASHES the page's own script and style at
//! start and allows exactly those two. Injected markup - from a chain's
//! error message, say - therefore cannot run, because its hash is not in
//! the policy.

use sha2::{Digest, Sha256};

/// The whole control panel.
pub const HTML: &str = include_str!("page.html");

/// `Content-Security-Policy` for every response.
///
/// * `default-src 'self'`: nothing is loaded from anywhere else.
/// * `script-src`/`style-src`: only the page's OWN inline block, by hash.
/// * `frame-ancestors 'none'`: the panel cannot be framed (clickjacking).
/// * `base-uri 'none'`: an injected `<base>` cannot redirect its requests.
/// * `form-action 'none'`: the page posts with `fetch`; nothing may submit
///   a form anywhere.
pub fn content_security_policy() -> String {
    let mut script = String::from("'self'");
    if let Some(hash) = inline_hash(HTML, "<script>", "</script>") {
        script.push_str(&format!(" '{hash}'"));
    }

    let mut style = String::from("'self'");
    if let Some(hash) = inline_hash(HTML, "<style>", "</style>") {
        style.push_str(&format!(" '{hash}'"));
    }

    format!(
        "default-src 'self'; script-src {script}; style-src {style}; \
         img-src 'self' data:; connect-src 'self'; font-src 'self'; \
         object-src 'none'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'"
    )
}

/// `sha256-<base64>` of what sits between `open` and `close`, which is
/// exactly what a browser hashes.
fn inline_hash(html: &str, open: &str, close: &str) -> Option<String> {
    let body = between(html, open, close)?;
    let digest: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    Some(format!("sha256-{}", base64(&digest)))
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

/// Standard base64 with padding. Twenty lines instead of a dependency, and
/// the only thing it ever encodes is a 32 byte digest.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16)
            | (u32::from(b[1]) << 8)
            | u32::from(b[2]);

        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// If the page ever grows a second `<script>` block, or loses the one
    /// it has, the policy silently stops matching and the panel goes blank
    /// in the browser. This is the test that notices.
    #[test]
    fn the_page_has_exactly_one_inline_script_and_one_inline_style() {
        assert_eq!(HTML.matches("<script").count(), 1, "{}", "one script");
        assert_eq!(HTML.matches("<style").count(), 1, "{}", "one style");
        assert!(HTML.contains("<script>"), "no attributes on the script");
        assert!(HTML.contains("<style>"), "no attributes on the style");
    }

    #[test]
    fn the_policy_names_the_hash_of_the_pages_own_script_and_style() {
        let csp = content_security_policy();

        let script =
            inline_hash(HTML, "<script>", "</script>").expect("a script");
        let style =
            inline_hash(HTML, "<style>", "</style>").expect("a style");

        assert!(csp.contains(&format!("'{script}'")), "{csp}");
        assert!(csp.contains(&format!("'{style}'")), "{csp}");
        assert!(csp.starts_with("default-src 'self'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("base-uri 'none'"), "{csp}");
        // The whole point: no blanket permission for inline code.
        assert!(!csp.contains("unsafe-inline"), "{csp}");
        assert!(!csp.contains("unsafe-eval"), "{csp}");
    }

    /// "No CDN, no external request" is a promise in the design; this is
    /// what keeps it true after an edit.
    #[test]
    fn the_page_asks_no_other_host_for_anything() {
        let lower = HTML.to_ascii_lowercase();

        for forbidden in [
            "http://",
            "https://",
            "//cdn",
            "integrity=",
            "<iframe",
            "srcdoc",
            "eval(",
        ] {
            assert!(
                !lower.contains(forbidden),
                "the page must not contain `{forbidden}`"
            );
        }
    }

    #[test]
    fn a_missing_block_degrades_to_self_rather_than_panicking() {
        assert_eq!(
            inline_hash("<p>nothing</p>", "<script>", "</script>"),
            None
        );
        assert_eq!(between("<a>x", "<a>", "</a>"), None);
    }
}
