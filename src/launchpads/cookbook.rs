//! The query cookbook: ONE query per screen of a launchpad UI, exactly as
//! a frontend would send it (docs/design.md §11). `README.md` §4 prints
//! every query (a unit test keeps the two identical) and the ClickHouse
//! integration tests run every one of them against the real fixtures with
//! hand-computed assertions.
//!
//! # Never splice a request value into the SQL
//!
//! Every `{name:Type}` here is a ClickHouse **bound parameter**, sent
//! beside the query (`param_name=...` over HTTP, `.param(..)` with the
//! `clickhouse` crate), never pasted into its text. `{token}` and
//! `{creator}` are whatever the URL carried: string-formatting those into
//! the statement is SQL injection, and it is why no placeholder below
//! sits inside quotes. The queries are the literal text to send.
//!
//! The ids: **plain hex, no `0x`**. Identity columns are chain neutral
//! `FixedString(32)` (docs/design.md §13), so an id is 64 hex characters;
//! a UI holding a 20 byte EVM address passes its 40 characters unchanged
//! and the parameterized views left pad them with 12 zero bytes
//! themselves (the padding is a constant expression, so the primary key
//! range read survives it). Anything else can only fail to match, except
//! an EMPTY string, which pads to the 32 zero bytes - in this module a
//! real bucket, the trades whose token leg stayed unverified. `{now}` /
//! `{since}` are unix seconds, and `tx_id` comes back as the raw
//! transaction bytes (`hex(tx_id)` to print it).
//!
//! # Trust, and the `_all_v` twins
//!
//! Anyone can emit a `TokenLaunched` or a `CurveBuy`, so every recipe
//! here reads a view that counts only emitters an operator listed in
//! `launchpad_trusted_emitters`. **Picking a token is not a trust
//! decision**: a forged curve can name a real token, and a forged launch
//! emitted earlier than the real one can claim it, so the token page, the
//! chart, the tape, the snipers and the holders are filtered too.
//!
//! **Picking a creator is not a trust decision either.** A launch names
//! its creator in the event, so a forger can hang a launch that never
//! graduates on any wallet it likes - inflating that wallet's launch
//! count, tanking its graduation rate and manufacturing the very
//! serial-rugger signal the creator page reports - and a forged fee sweep
//! can name it as the recipient of fees it never earned. The creator
//! screens therefore take their launches, graduations, trades and fees
//! from trusted curves only.
//!
//! Each filtered screen has an `*_all_v` twin that counts everything -
//! that is the tool for deciding what to trust, never the screen.
//!
//! # The text these queries return is HOSTILE
//!
//! `name` and `symbol` come from logs anyone can emit.
//! `decode::sanitize` has already removed the control, bidi, zero width
//! and tag characters, but the strings are stored as TEXT: **the UI
//! escapes them** for whatever it renders into (HTML, a terminal, a CSV).
//! This module never escapes for a target it cannot see.

/// A screen and the query that serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub screen: &'static str,
    pub sql: &'static str,
}

/// The new-launch feed: newest first, with the first minute of the curve.
pub const NEW_LAUNCHES: Recipe = Recipe {
    screen: "New launch feed",
    sql: "\
SELECT launch_time, token, family, emitter, creator, name, symbol,
       quote_token, initial_price_raw, first_minute_trades,
       first_minute_buys, first_minute_volume_raw, first_minute_traders,
       graduation_threshold_raw
FROM launchpad_new_launches_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50",
};

/// The same feed without the trust filter, for deciding what to trust.
pub const NEW_LAUNCHES_ALL: Recipe = Recipe {
    screen: "New launch feed (untrusted included)",
    sql: "\
SELECT launch_time, token, family, emitter, trusted, name, symbol,
       first_minute_trades, first_minute_volume_raw
FROM launchpad_new_launches_all_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50",
};

/// Token page header: curve progress, price, volume, graduation.
pub const TOKEN_PAGE: Recipe = Recipe {
    screen: "Token page header",
    sql: "\
SELECT *
FROM launchpad_token_v(chain = {chain:UInt64}, token = {token:String})",
};

/// Price chart of one token (1m; the 1h view takes the same parameters).
pub const PRICE_CHART: Recipe = Recipe {
    screen: "Price chart",
    sql: "\
SELECT bucket, open_raw, high_raw, low_raw, close_raw, volume_quote_raw,
       volume_token_raw, trades, unique_traders, curve_progress
FROM launchpad_candles_1m_v(chain = {chain:UInt64}, token = {token:String})
ORDER BY bucket",
};

/// Trades tape of one token, newest first.
pub const TRADES_TAPE: Recipe = Recipe {
    screen: "Trades tape",
    sql: "\
SELECT timestamp, side, trader, caller, token_amount_raw, quote_amount_raw,
       price_raw, fee_amount_raw, token_verified, quote_verified,
       tx_id
FROM launchpad_token_trades_v(chain = {chain:UInt64}, token = {token:String},
                              from_block = {from_block:UInt64})
LIMIT 50",
};

/// Top holders of one token (`as_of_block` = the graduation block for the
/// concentration at graduation).
pub const HOLDERS: Recipe = Recipe {
    screen: "Top holders",
    sql: "\
SELECT account, balance_raw, share_of_initial_supply, received, sent
FROM launchpad_token_holders_v(chain = {chain:UInt64}, token = {token:String},
                               as_of_block = {as_of_block:UInt64})
LIMIT 50",
};

/// Graduation feed, with the destination pool the DEX module knows.
pub const GRADUATIONS: Recipe = Recipe {
    screen: "Graduation feed",
    sql: "\
SELECT graduation_time, token, family, pool_id, pool_kind, pool_status,
       pool_protocol, token_amount_raw, quote_amount_raw, graduation_tx
FROM launchpad_graduations_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50",
};

/// Creator page header: the serial-rugger signal.
pub const CREATOR_PAGE: Recipe = Recipe {
    screen: "Creator page header",
    sql: "\
SELECT launches, graduated, died, graduation_rate, first_launch,
       last_launch, volume_quote_raw, realised_creator_fees_raw
FROM launchpad_creator_v(chain = {chain:UInt64}, creator = {creator:String},
                         as_of = {now:UInt32}, dead_after = {dead_after:UInt32})",
};

/// Every launch of one creator.
pub const CREATOR_TOKENS: Recipe = Recipe {
    screen: "Creator launches",
    sql: "\
SELECT launch_time, token, symbol, graduated, died, trades,
       volume_quote_raw, last_trade_time, pool_id
FROM launchpad_creator_tokens_v(chain = {chain:UInt64}, creator = {creator:String},
                                as_of = {now:UInt32}, dead_after = {dead_after:UInt32})
LIMIT 200",
};

/// Sniper view: who bought in the launch block and just after it.
pub const SNIPERS: Recipe = Recipe {
    screen: "Sniper view",
    sql: "\
SELECT trader, blocks_after_launch, buys, token_amount_raw,
       quote_amount_raw, share_of_initial_supply, funder, bundle_size,
       is_creator
FROM launchpad_snipers_v(chain = {chain:UInt64}, token = {token:String},
                         blocks = {blocks:UInt64})
LIMIT 100",
};

/// Venue leaderboard for a day.
pub const VENUES: Recipe = Recipe {
    screen: "Venue stats",
    sql: "\
SELECT family, bucket, launches, graduations, graduation_rate,
       trades, volume_quote_raw, volume_quote_verified_raw, fees_raw,
       unique_traders, unique_creators
FROM launchpad_venues_1d_v(chain = {chain:UInt64})
LIMIT 100",
};

/// A venue's volume split by front end - never added to venue volume.
pub const FRONTENDS: Recipe = Recipe {
    screen: "Front end attribution",
    sql: "\
SELECT family, emitter, frontend, trades, volume_quote_raw,
       volume_quote_verified_raw, unique_traders
FROM launchpad_frontend_volume_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 100",
};

pub const COOKBOOK: &[Recipe] = &[
    NEW_LAUNCHES,
    NEW_LAUNCHES_ALL,
    TOKEN_PAGE,
    PRICE_CHART,
    TRADES_TAPE,
    HOLDERS,
    GRADUATIONS,
    CREATOR_PAGE,
    CREATOR_TOKENS,
    SNIPERS,
    VENUES,
    FRONTENDS,
];

impl Recipe {
    /// The bound parameter names this query needs, in first-seen order.
    pub fn parameters(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        let mut rest = self.sql;

        while let Some(at) = rest.find('{') {
            rest = &rest[at + 1..];
            let Some(end) = rest.find('}') else { break };
            let Some((name, _type)) = rest[..end].split_once(':') else {
                continue;
            };
            if !names.contains(&name) {
                names.push(name);
            }
        }

        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = include_str!("README.md");

    #[test]
    fn the_readme_prints_every_recipe_verbatim() {
        for recipe in COOKBOOK {
            assert!(README.contains(recipe.sql), "{}", recipe.screen);
            assert!(README.contains(recipe.screen), "{}", recipe.screen);
        }
    }

    #[test]
    fn recipes_are_one_statement_against_a_view() {
        for recipe in COOKBOOK {
            assert!(!recipe.sql.contains(';'), "{}", recipe.screen);
            assert_eq!(recipe.sql.matches("FROM ").count(), 1);
            assert!(!recipe.sql.contains("JOIN"), "{}", recipe.screen);
            assert!(recipe.sql.contains("_v"), "{}", recipe.screen);
            assert!(
                recipe.sql.contains("{chain:UInt64}"),
                "{}",
                recipe.screen
            );
        }
    }

    /// Request values are BOUND, never spliced: no placeholder may be
    /// typeless, and none may sit inside quotes (which is what a splice
    /// looks like, and what makes it injectable).
    #[test]
    fn every_placeholder_is_a_typed_bound_parameter() {
        for recipe in COOKBOOK {
            let names = recipe.parameters();
            assert!(names.contains(&"chain"), "{}", recipe.screen);
            // `unhex('...')` around an id was the old splice.
            assert!(!recipe.sql.contains("unhex("), "{}", recipe.screen);

            let mut rest = recipe.sql;
            let mut found = 0;
            while let Some(at) = rest.find('{') {
                let before = &rest[..at];
                rest = &rest[at + 1..];
                let end = rest.find('}').unwrap_or_else(|| {
                    panic!("{}: unclosed placeholder", recipe.screen)
                });
                let inside = &rest[..end];

                // Typed: `{name:Type}`, never a bare `{name}`.
                let (name, kind) =
                    inside.split_once(':').unwrap_or_else(|| {
                        panic!("{}: untyped {{{inside}}}", recipe.screen)
                    });
                assert!(!name.is_empty() && !kind.is_empty());
                assert!(names.contains(&name), "{}", recipe.screen);

                // Not quoted: `'{x:String}'` would be a splice.
                assert!(
                    !before.ends_with('\''),
                    "{}: quoted {{{inside}}}",
                    recipe.screen
                );
                assert!(
                    !rest[end + 1..].starts_with('\''),
                    "{}: quoted {{{inside}}}",
                    recipe.screen
                );

                rest = &rest[end + 1..];
                found += 1;
            }
            assert!(found > 0, "{}", recipe.screen);
        }

        assert_eq!(TOKEN_PAGE.parameters(), vec!["chain", "token"]);
        assert_eq!(PRICE_CHART.parameters(), vec!["chain", "token"]);
        assert_eq!(
            CREATOR_PAGE.parameters(),
            vec!["chain", "creator", "now", "dead_after"]
        );
    }

    /// Every screen reads a trust-filtered view. The `_all_v` twins exist
    /// for exploration and only the launch feed ships one as a recipe,
    /// clearly labelled.
    #[test]
    fn only_the_labelled_recipe_reads_an_all_v_view() {
        for recipe in COOKBOOK {
            if recipe.sql.contains("_all_v") {
                assert_eq!(recipe.screen, NEW_LAUNCHES_ALL.screen);
            }
        }
        assert!(NEW_LAUNCHES_ALL.screen.contains("untrusted"));
    }
}
