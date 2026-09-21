//! The human readable part of on chain question payloads.
//!
//! Observed on Polygon (see `fixtures_data.rs`):
//!
//! * UmaCtfAdapter `QuestionInitialized.ancillaryData`:
//!   `q: title: <title>, description: <text> ... market_id: <n> res_data:
//!   p1: 0, p2: 1, p3: 0.5. Where p1 corresponds to <B>, p2 to <A>, p3 to
//!   unknown/50-50. ...,initializer:<hex>`
//! * NegRiskAdapter `MarketPrepared.data`: `title: <title>, description:
//!   <text>, id: <n>`; `QuestionPrepared.data`: `question: <title>,
//!   description: <text>, id: <n>`
//! * the second generation NegRisk adapter stores the same kind of text
//!   hex encoded once more.
//!
//! Nothing here is guaranteed by a contract: whatever does not look like
//! the above yields empty strings, never an error. The raw payload is
//! stored next to the parsed fields.
//!
//! # This text is HOSTILE
//!
//! `QuestionInitialized` / `MarketPrepared` are permissionless: the bytes
//! are chosen by whoever emitted the log, and they end up in
//! `prediction_markets_v.title` / `.description` / `.outcomes`, i.e. on a
//! screen. [`sanitize`] therefore strips, from every parsed field:
//!
//! * C0 controls including newline and tab (a title is one line - a
//!   newline in a log line or a CSV export is a forged second row),
//! * C1 controls and the Unicode line / paragraph separators,
//! * the bidirectional overrides and isolates (`U+202A..U+202E`,
//!   `U+2066..U+2069`) - the "Trojan Source" class, which makes a title
//!   render as text it does not contain,
//! * the zero width characters and `U+FEFF`,
//! * `U+0000`,
//!
//! and caps the length. It does NOT escape HTML: the strings are stored as
//! text, so **the UI must escape them** (the README says so next to the
//! cookbook). A `<script>` in a title is data here and must stay data
//! there.

/// What could be read out of a payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuestionText {
    pub title: String,
    pub description: String,
    /// Labels by OUTCOME INDEX, empty when the text does not name them.
    pub outcomes: Vec<String>,
}

/// Longest description kept (the raw payload keeps everything).
const MAX_DESCRIPTION: usize = 4_096;

const DESCRIPTION: &str = ", description: ";

/// The payload as text; a payload that is the hex encoding of text is
/// decoded once more.
fn to_text(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let trimmed = text.trim_matches(char::from(0)).trim();

    let looks_hex = trimmed.len() >= 8
        && trimmed.len().is_multiple_of(2)
        && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit());

    if looks_hex {
        if let Ok(inner) = hex::decode(trimmed) {
            if let Ok(inner) = String::from_utf8(inner) {
                return inner;
            }
        }
    }

    trimmed.to_owned()
}

/// Drops the characters that let on chain text lie about what it is
/// (see the module docs). Collapses the runs of whitespace a stripped
/// control leaves behind, so a title stays one readable line.
pub fn sanitize(text: &str) -> String {
    let kept: String = text
        .chars()
        .map(|c| {
            if c.is_whitespace() {
                // Every kind of space, including the separators, becomes
                // a plain one.
                ' '
            } else {
                c
            }
        })
        .filter(|c| {
            let code = u32::from(*c);
            let control = code < 0x20 || (0x7f..=0x9f).contains(&code);
            let bidi = (0x202a..=0x202e).contains(&code)
                || (0x2066..=0x2069).contains(&code)
                || code == 0x200f
                || code == 0x200e;
            let invisible = (0x200b..=0x200d).contains(&code)
                || code == 0xfeff
                || code == 0x2060;

            !(control || bidi || invisible)
        })
        .collect();

    // A run of spaces where controls were removed reads as a gap.
    let mut out = String::with_capacity(kept.len());
    let mut space = false;
    for c in kept.chars() {
        if c == ' ' {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }

    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }

    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// `Where p1 corresponds to B, p2 to A, ...`: p2 is price 1, which the
/// UmaCtfAdapter reports as payouts `[1, 0]` - outcome 0. So `[A, B]`.
fn outcome_labels(text: &str) -> Vec<String> {
    let Some((_, rest)) = text.split_once("p1 corresponds to ") else {
        return Vec::new();
    };
    let Some((first, rest)) = rest.split_once(", p2 to ") else {
        return Vec::new();
    };

    let second = rest.split([',', '.']).next().unwrap_or_default().trim();
    let first = first.trim();

    if first.is_empty()
        || second.is_empty()
        || first.len() > 128
        || second.len() > 128
    {
        return Vec::new();
    }

    let (first, second) = (sanitize(first), sanitize(second));
    if first.is_empty() || second.is_empty() {
        return Vec::new();
    }

    vec![second, first]
}

pub fn parse(data: &[u8]) -> QuestionText {
    let text = to_text(data);
    let body = text.strip_prefix("q: ").unwrap_or(&text);

    let Some(body) = body
        .strip_prefix("title: ")
        .or_else(|| body.strip_prefix("question: "))
    else {
        return QuestionText::default();
    };

    let (title, rest) = body.split_once(DESCRIPTION).unwrap_or((body, ""));

    // The description ends where the machine readable tail starts.
    let end = [" market_id: ", " res_data: ", ", id: ", ",initializer:"]
        .iter()
        .filter_map(|marker| rest.rfind(marker))
        .min()
        .unwrap_or(rest.len());

    QuestionText {
        title: truncate(&sanitize(title), 512),
        description: truncate(&sanitize(&rest[..end]), MAX_DESCRIPTION),
        outcomes: outcome_labels(rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uma_ancillary_data() {
        let parsed = parse(
            b"q: title: Will it rain?, description: Resolves Yes if it \
              rains. market_id: 12 res_data: p1: 0, p2: 1, p3: 0.5. Where \
              p1 corresponds to No, p2 to Yes, p3 to unknown/50-50. \
              Updates ...,initializer:ab",
        );

        assert_eq!(parsed.title, "Will it rain?");
        assert_eq!(parsed.description, "Resolves Yes if it rains.");
        assert_eq!(parsed.outcomes, vec!["Yes", "No"]);
    }

    #[test]
    fn neg_risk_payloads_plain_and_hex() {
        let parsed =
            parse(b"question: Will A win?, description: About A., id: 7");
        assert_eq!(parsed.title, "Will A win?");
        assert_eq!(parsed.description, "About A.");
        assert!(parsed.outcomes.is_empty());

        let hexed =
            hex::encode("title: Winner 2026, description: x, id: 1");
        assert_eq!(parse(hexed.as_bytes()).title, "Winner 2026");
    }

    /// On chain text is chosen by whoever emitted the log. None of it
    /// may reach a screen as anything but one line of plain characters.
    #[test]
    fn hostile_text_is_stripped_of_controls_and_bidi_overrides() {
        // A newline would forge a second line in a log or a CSV export.
        let parsed = parse(
            "title: Real\nFAKE: resolved YES, description: a\tb\u{0}c, id: 1"
                .as_bytes(),
        );
        assert_eq!(parsed.title, "Real FAKE: resolved YES");
        assert_eq!(parsed.description, "a bc");
        assert!(!parsed.title.contains('\n'));

        // Trojan Source: the override makes the rendering lie.
        let parsed = parse(
            "title: Will \u{202e}SEY evloser\u{202c} happen?, description: d, id: 1"
                .as_bytes(),
        );
        assert_eq!(parsed.title, "Will SEY evloser happen?");
        for c in parsed.title.chars() {
            assert!(!(0x202a..=0x202e).contains(&u32::from(c)));
        }

        // Zero width characters cannot smuggle a different word past a
        // human reader or a search.
        assert_eq!(
            parse(
                "title: Pol\u{200b}ymarket, description: d, id: 1"
                    .as_bytes()
            )
            .title,
            "Polymarket"
        );

        // Outcome labels go through the same door.
        let parsed = parse(
            "q: title: t, description: d res_data: Where p1 corresponds \
             to N\u{0}o, p2 to Y\u{202e}es, p3 to x."
                .as_bytes(),
        );
        assert_eq!(parsed.outcomes, vec!["Yes", "No"]);

        // HTML is NOT escaped here - it is data, and the UI escapes it.
        assert_eq!(
            parse(
                "title: <script>alert(1)</script>, description: d, id: 1"
                    .as_bytes()
            )
            .title,
            "<script>alert(1)</script>"
        );

        // Nothing but controls leaves nothing.
        assert_eq!(sanitize("\u{202e}\u{200b}\n\t "), "");
    }

    #[test]
    fn anything_else_is_empty_never_an_error() {
        assert_eq!(parse(b""), QuestionText::default());
        assert_eq!(parse(&[0xff, 0xfe, 0x00]), QuestionText::default());
        assert_eq!(parse(b"deadbeef"), QuestionText::default());
        assert_eq!(parse(b"title: only a title").title, "only a title");

        // Multi byte characters survive truncation.
        let long = format!("title: {}", "é".repeat(600));
        assert!(parse(long.as_bytes()).title.len() <= 512);
    }
}
