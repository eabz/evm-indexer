//! The query cookbook: ONE query per screen of a trading UI, exactly as a
//! frontend would send it (docs/design.md §10). `README.md` prints every
//! query (a unit test keeps the two identical) and the ClickHouse
//! integration tests run every one of them against real Polymarket data.
//!
//! Placeholders (`{chain}`, `{market_id}`...) stand for request
//! parameters. **Every id placeholder is filled with plain hex, no `0x`**,
//! and token ids with a decimal string; the query does the rest.
//!
//! Identity columns are chain neutral `FixedString(32)` (docs/design.md
//! §13), so an id is 64 hex characters. A UI holding a 20 byte EVM address
//! passes its 40 characters unchanged: the PARAMETERIZED views
//! (`prediction_candles_*_v`, `prediction_trades_v`,
//! `prediction_holders_v`, `prediction_positions_v`,
//! `prediction_activity_v`) left pad it with 12 zero bytes themselves, and
//! the primary key range read survives the padding (it is a constant
//! expression, verified with `EXPLAIN indexes = 1`). Queries against
//! `prediction_markets_v` take ids that are 32 bytes on every chain
//! (`market_id`, `event_id`), so they just `unhex()` them.

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
WHERE chain = {chain} AND status = 'open'
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
WHERE chain = {chain}
  AND positionCaseInsensitiveUTF8(coalesce(title, ''), '{text}') > 0
ORDER BY volume_total DESC NULLS LAST
LIMIT 20",
};

/// Market page header: everything about one market.
pub const MARKET_HEADER: Recipe = Recipe {
    screen: "Market page header",
    sql: "\
SELECT *
FROM prediction_markets_v
WHERE chain = {chain} AND market_id = unhex('{market_id}')",
};

/// The markets of a multi outcome event, most likely first.
pub const EVENT_MARKETS: Recipe = Recipe {
    screen: "Multi outcome event",
    sql: "\
SELECT market_id, title, event_title, event_index,
       outcome_prices[1] AS yes_price, volume_total, status
FROM prediction_markets_v
WHERE chain = {chain} AND event_id = unhex('{event_id}')
ORDER BY yes_price DESC NULLS LAST, event_index",
};

/// Price chart of one outcome (1m / 1h / 1d: same query, other view).
pub const PRICE_CHART: Recipe = Recipe {
    screen: "Price chart",
    sql: "\
SELECT bucket, open, high, low, close, volume, shares, trades, traders
FROM prediction_candles_1h_v(chain = {chain}, registry = '{registry}',
                             outcome_token_id = toUInt256('{token}'))
WHERE bucket >= now() - INTERVAL 30 DAY
ORDER BY bucket",
};

/// Trades tape of a market, newest first.
pub const TRADES_TAPE: Recipe = Recipe {
    screen: "Trades tape",
    sql: "\
SELECT timestamp, outcome_index, outcome, side, price, shares, collateral,
       trader, tx_id
FROM prediction_trades_v(chain = {chain}, market_id = '{market_id}')
ORDER BY block_number DESC, tx_index DESC, ordinal DESC
LIMIT 50",
};

/// Top holders of a market, per outcome.
pub const HOLDERS: Recipe = Recipe {
    screen: "Holders",
    sql: "\
SELECT outcome_index, outcome, holder, shares, avg_entry_price,
       current_price, value
FROM prediction_holders_v(chain = {chain}, market_id = '{market_id}')
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
FROM prediction_positions_v(chain = {chain}, holder = '{holder}')
ORDER BY value DESC NULLS LAST, realized_pnl DESC NULLS LAST",
};

/// Trade history of a wallet, newest first.
pub const WALLET_TRADES: Recipe = Recipe {
    screen: "Wallet trade history",
    sql: "\
SELECT timestamp, title, outcome, action, role, price, shares, collateral,
       fee, tx_id
FROM prediction_activity_v(chain = {chain}, holder = '{holder}')
WHERE action IN ('buy', 'sell')
ORDER BY block_number DESC, tx_index DESC, ordinal DESC
LIMIT 50",
};

/// Leaderboard of a period.
pub const LEADERBOARD: Recipe = Recipe {
    screen: "Leaderboard",
    sql: "\
SELECT trader, volume, net_cash_flow, fees, trades, outcome_tokens_traded
FROM prediction_leaderboard_v(chain = {chain}, from_day = '{from_day}',
                              to_day = '{to_day}')
ORDER BY volume DESC
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
    /// The query with its placeholders filled.
    pub fn render(&self, parameters: &[(&str, &str)]) -> String {
        parameters.iter().fold(
            self.sql.to_owned(),
            |sql, (name, value)| {
                sql.replace(&format!("{{{name}}}"), value)
            },
        )
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
            assert!(recipe.sql.contains("{chain}"), "{}", recipe.screen);
        }

        let rendered =
            MARKET_HEADER.render(&[("chain", "137"), ("market_id", "ab")]);
        assert!(!rendered.contains('{'), "{rendered}");
    }
}
