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
        && trimmed.len() % 2 == 0
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

    let second = rest
        .split([',', '.'])
        .next()
        .unwrap_or_default()
        .trim();
    let first = first.trim();

    if first.is_empty()
        || second.is_empty()
        || first.len() > 128
        || second.len() > 128
    {
        return Vec::new();
    }

    vec![second.to_owned(), first.to_owned()]
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
        title: truncate(title.trim(), 512),
        description: truncate(rest[..end].trim(), MAX_DESCRIPTION),
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

        let hexed = hex::encode("title: Winner 2026, description: x, id: 1");
        assert_eq!(parse(hexed.as_bytes()).title, "Winner 2026");
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
