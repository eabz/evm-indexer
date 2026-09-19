//! The query cookbook: ONE query per screen of a trading UI, exactly as a
//! frontend would send it (docs/design.md §10). `README.md` prints every
//! query (a unit test keeps the two identical) and the ClickHouse
//! integration tests run every one of them against real Polymarket data.
//!
//! # Never splice a request value into the SQL
//!
//! Every `{name:Type}` here is a ClickHouse **bound parameter**, sent
//! beside the query (`param_name=...` over HTTP, `.param(..)` with the
//! `clickhouse` crate), never pasted into its text. `{text:String}` of the
//! search screen is the user's search box, and `{market_id:String}` is
//! whatever the URL carried: string-formatting those into the statement is
//! SQL injection, and it is the reason these constants carry no `'` around
//! a placeholder. The queries below are the literal text to send.
//!
//! The ids: **plain hex, no `0x`**, token ids as a decimal string.
//! Identity columns are chain neutral `FixedString(32)` (docs/design.md
//! §13), so an id is 64 hex characters. A UI holding a 20 byte EVM address
//! passes its 40 characters unchanged: the PARAMETERIZED views
//! (`prediction_candles_*_v`, `prediction_trades_v`,
//! `prediction_holders_v`, `prediction_positions_v`,
//! `prediction_activity_v`) left pad it with 12 zero bytes themselves, and
//! the primary key range read survives the padding (it is a constant
//! expression, verified with `EXPLAIN indexes = 1`). Queries against
//! `prediction_markets_v` take ids that are 32 bytes on every chain
//! (`market_id`, `event_id`), so they `unhex()` the same hex string.
//!
//! # The text these queries return is HOSTILE
//!
//! `title`, `description` and `outcomes` come from on chain payloads
//! anyone can emit. `text::sanitize` has already removed the control,
//! bidi and zero width characters at decode time, but the strings are
//! stored as TEXT: **the UI escapes them for whatever it renders into**
//! (HTML, a terminal, a CSV). This module never escapes for a target it
//! cannot see.

/// A screen and the query that serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Recipe {
    pub screen: &'static str,
    pub sql: &'static str,
}

/// Market list: open markets by 24h volume.
pub const MARKET_LIST: Recipe = Recipe {
    screen: "Market list",
    sql: "\
SELECT market_id, registry, venue, title, event_title, category, tags,
       outcomes, outcome_prices, volume_24h, volume_total, open_interest,
       traders, end_date, status
FROM prediction_markets_v
WHERE chain = {chain:UInt64} AND status = 'open'
ORDER BY volume_24h DESC NULLS LAST, volume_total DESC NULLS LAST
LIMIT 50",
};

/// Market search by title.
pub const MARKET_SEARCH: Recipe = Recipe {
    screen: "Market search",
    sql: "\
SELECT market_id, registry, venue, title, event_title, outcomes,
       outcome_prices, volume_total, status
FROM prediction_markets_v
WHERE chain = {chain:UInt64}
  AND positionCaseInsensitiveUTF8(coalesce(title, ''), {text:String}) > 0
ORDER BY volume_total DESC NULLS LAST
LIMIT 20",
};

/// Market page header: everything about one market.
pub const MARKET_HEADER: Recipe = Recipe {
    screen: "Market page header",
    sql: "\
SELECT *
FROM prediction_markets_v
WHERE chain = {chain:UInt64} AND market_id = unhex({market_id:String})",
};

/// The markets of a multi outcome event, most likely first.
pub const EVENT_MARKETS: Recipe = Recipe {
    screen: "Multi outcome event",
    sql: "\
SELECT market_id, title, event_title, event_index,
       outcome_prices[1] AS yes_price, volume_total, status
FROM prediction_markets_v
WHERE chain = {chain:UInt64} AND event_id = unhex({event_id:String})
ORDER BY yes_price DESC NULLS LAST, event_index",
};

/// Price chart of one outcome (1m / 1h / 1d: same query, other view).
pub const PRICE_CHART: Recipe = Recipe {
    screen: "Price chart",
    sql: "\
SELECT bucket, open, high, low, close, volume, shares, trades, traders
FROM prediction_candles_1h_v(chain = {chain:UInt64}, registry = {registry:String},
                             outcome_token_id = {token:UInt256})
WHERE bucket >= now() - INTERVAL 30 DAY
ORDER BY bucket",
};

/// Trades tape of a market, newest first.
pub const TRADES_TAPE: Recipe = Recipe {
    screen: "Trades tape",
    sql: "\
SELECT timestamp, outcome_index, outcome, side, price, shares, collateral,
       trader, tx_id
FROM prediction_trades_v(chain = {chain:UInt64}, market_id = {market_id:String})
ORDER BY block_number DESC, tx_index DESC, ordinal DESC
LIMIT 50",
};

/// Top holders of a market, per outcome.
pub const HOLDERS: Recipe = Recipe {
    screen: "Holders",
    sql: "\
SELECT outcome_index, outcome, holder, shares, avg_entry_price,
       current_price, value
FROM prediction_holders_v(chain = {chain:UInt64}, market_id = {market_id:String})
ORDER BY outcome_index, shares DESC
LIMIT 100",
};

/// Portfolio of a wallet: open positions and what they are worth, realized
/// profit of the closed ones, winnings waiting to be redeemed.
pub const PORTFOLIO: Recipe = Recipe {
    screen: "Portfolio",
    sql: "\
SELECT market_id, title, outcome, status, shares, avg_entry_price,
       current_price, value, unrealized_pnl, realized_pnl, redeemable,
       unpriced_shares
FROM prediction_positions_v(chain = {chain:UInt64}, holder = {holder:String})
ORDER BY value DESC NULLS LAST, realized_pnl DESC NULLS LAST",
};

/// Trade history of a wallet, newest first.
pub const WALLET_TRADES: Recipe = Recipe {
    screen: "Wallet trade history",
    sql: "\
SELECT timestamp, title, outcome, action, role, price, shares, collateral,
       fee, tx_id
FROM prediction_activity_v(chain = {chain:UInt64}, holder = {holder:String})
WHERE action IN ('buy', 'sell')
ORDER BY block_number DESC, tx_index DESC, ordinal DESC
LIMIT 50",
};

/// Leaderboard of a period.
pub const LEADERBOARD: Recipe = Recipe {
    screen: "Leaderboard",
    sql: "\
SELECT trader, collateral_token, volume, net_cash_flow, fees, trades,
       unpriced_trades, outcome_tokens_traded
FROM prediction_leaderboard_v(chain = {chain:UInt64}, from_day = {from_day:Date},
                              to_day = {to_day:Date})
ORDER BY volume DESC NULLS LAST
LIMIT 100",
};

pub const COOKBOOK: &[Recipe] = &[
    MARKET_LIST,
    MARKET_SEARCH,
    MARKET_HEADER,
    EVENT_MARKETS,
    PRICE_CHART,
    TRADES_TAPE,
    HOLDERS,
    PORTFOLIO,
    WALLET_TRADES,
    LEADERBOARD,
];

impl Recipe {
    /// Names of the bound parameters the query needs, in first use order.
    /// The caller binds each one - it never substitutes them into
    /// [`Recipe::sql`], see the module docs.
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
    /// looks like and what makes it injectable).
    #[test]
    fn every_placeholder_is_a_typed_bound_parameter() {
        for recipe in COOKBOOK {
            let names = recipe.parameters();
            assert!(names.contains(&"chain"), "{}", recipe.screen);

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

        assert_eq!(MARKET_HEADER.parameters(), vec!["chain", "market_id"]);
        assert_eq!(
            PRICE_CHART.parameters(),
            vec!["chain", "registry", "token"]
        );
    }
}
