//! The query cookbook: ONE query per screen of a launchpad UI, exactly as
//! a frontend would send it (docs/design.md §11). `README.md` §4 prints
//! every query (a unit test keeps the two identical) and the ClickHouse
//! integration tests run every one of them against the real fixtures with
//! hand-computed assertions.
//!
//! Placeholders (`{chain}`, `{token}`...) stand for request parameters.
//! Ids are 32 bytes and are passed as hex without `0x` through `unhex()`;
//! an EVM address is therefore 24 zeros followed by the 40 address
//! characters. `{now}` / `{since}` are unix seconds. `tx_id` comes back as
//! the raw transaction bytes - `hex(tx_id)` to print it.

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
FROM launchpad_new_launches_v(chain = {chain}, since = {since})
LIMIT 50",
};

/// The same feed without the trust filter, for deciding what to trust.
pub const NEW_LAUNCHES_ALL: Recipe = Recipe {
    screen: "New launch feed (untrusted included)",
    sql: "\
SELECT launch_time, token, family, emitter, trusted, name, symbol,
       first_minute_trades, first_minute_volume_raw
FROM launchpad_new_launches_all_v(chain = {chain}, since = {since})
LIMIT 50",
};

/// Token page header: curve progress, price, volume, graduation.
pub const TOKEN_PAGE: Recipe = Recipe {
    screen: "Token page header",
    sql: "\
SELECT *
FROM launchpad_token_v(chain = {chain}, token = unhex('{token}'))",
};

/// Price chart of one token (1m; the 1h view takes the same parameters).
pub const PRICE_CHART: Recipe = Recipe {
    screen: "Price chart",
    sql: "\
SELECT bucket, open_raw, high_raw, low_raw, close_raw, volume_quote_raw,
       volume_token_raw, trades, unique_traders, curve_progress
FROM launchpad_candles_1m_v
WHERE chain = {chain} AND token = unhex('{token}')
ORDER BY bucket",
};

/// Trades tape of one token, newest first.
pub const TRADES_TAPE: Recipe = Recipe {
    screen: "Trades tape",
    sql: "\
SELECT timestamp, side, trader, caller, token_amount_raw, quote_amount_raw,
       price_raw, fee_amount_raw, token_verified, quote_verified,
       tx_id
FROM launchpad_token_trades_v(chain = {chain}, token = unhex('{token}'),
                              from_block = {from_block})
LIMIT 50",
};

/// Top holders of one token (`as_of_block` = the graduation block for the
/// concentration at graduation).
pub const HOLDERS: Recipe = Recipe {
    screen: "Top holders",
    sql: "\
SELECT account, balance_raw, share_of_initial_supply, received, sent
FROM launchpad_token_holders_v(chain = {chain}, token = unhex('{token}'),
                               as_of_block = {as_of_block})
LIMIT 50",
};

/// Graduation feed, with the destination pool the DEX module knows.
pub const GRADUATIONS: Recipe = Recipe {
    screen: "Graduation feed",
    sql: "\
SELECT graduation_time, token, family, pool_id, pool_kind, pool_status,
       pool_protocol, token_amount_raw, quote_amount_raw, graduation_tx
FROM launchpad_graduations_v(chain = {chain}, since = {since})
LIMIT 50",
};

/// Creator page header: the serial-rugger signal.
pub const CREATOR_PAGE: Recipe = Recipe {
    screen: "Creator page header",
    sql: "\
SELECT launches, graduated, died, graduation_rate, first_launch,
       last_launch, volume_quote_raw, realised_creator_fees_raw
FROM launchpad_creator_v(chain = {chain}, creator = unhex('{creator}'),
                         as_of = {now}, dead_after = {dead_after})",
};

/// Every launch of one creator.
pub const CREATOR_TOKENS: Recipe = Recipe {
    screen: "Creator launches",
    sql: "\
SELECT launch_time, token, symbol, graduated, died, trades,
       volume_quote_raw, last_trade_time, pool_id
FROM launchpad_creator_tokens_v(chain = {chain}, creator = unhex('{creator}'),
                                as_of = {now}, dead_after = {dead_after})
LIMIT 200",
};

/// Sniper view: who bought in the launch block and just after it.
pub const SNIPERS: Recipe = Recipe {
    screen: "Sniper view",
    sql: "\
SELECT trader, blocks_after_launch, buys, token_amount_raw,
       quote_amount_raw, share_of_initial_supply, funder, bundle_size,
       is_creator
FROM launchpad_snipers_v(chain = {chain}, token = unhex('{token}'),
                         blocks = {blocks})
LIMIT 100",
};

/// Venue leaderboard for a day.
pub const VENUES: Recipe = Recipe {
    screen: "Venue stats",
    sql: "\
SELECT family, bucket, launches, graduations, graduation_rate,
       trades, volume_quote_raw, volume_quote_verified_raw, fees_raw,
       unique_traders, unique_creators
FROM launchpad_venues_1d_v(chain = {chain})
LIMIT 100",
};

/// A venue's volume split by front end - never added to venue volume.
pub const FRONTENDS: Recipe = Recipe {
    screen: "Front end attribution",
    sql: "\
SELECT family, emitter, frontend, trades, volume_quote_raw,
       volume_quote_verified_raw, unique_traders
FROM launchpad_frontend_volume_v(chain = {chain}, since = {since})
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
            TOKEN_PAGE.render(&[("chain", "4663"), ("token", "ab")]);
        assert!(!rendered.contains('{'), "{rendered}");
    }
}
