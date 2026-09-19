# Prediction markets (`src/predictions`)

Display first (docs/design.md §10): the tables were designed backwards from
the screens of a trading UI. Every screen is ONE query against a view, with
no joins or arithmetic left to the client - see the [query cookbook](#query-cookbook).

Everything is decoded **by event family, never by address**. Nothing about
Polymarket (or any other venue) is configured anywhere: its forks on BNB
Chain and Base decode on day one, as the fixtures show.

## How on-chain prediction markets emit data (researched September 2026)

Every claim below was checked against real logs fetched with
`eth_getLogs` / `eth_getTransactionReceipt` from public RPC endpoints; the
transactions are kept verbatim in `fixtures_data.rs` (16 real transactions,
Polygon / Gnosis / Base / BNB Chain). Every `topic0` is asserted against
`keccak256(signature)` in `events.rs`.

### The Conditional Tokens Framework (family `ctf`)

One ERC-1155 contract per deployment holds every position of every market
(Polygon `0x4D97…6045`, Gnosis `0xCeAf…c0Ce`, Base `0xC9c9…6e18` and others,
BNB Chain `0x22DA…d244`, `0x9400…1d9f`, `0xAD1a…D774`...).

| Event | What it tells |
|---|---|
| `ConditionPreparation(conditionId, oracle, questionId, outcomeSlotCount)` | a market exists |
| `PositionSplit(stakeholder, collateralToken, parentCollectionId, conditionId, partition[], amount)` | `amount` collateral became `amount` shares of EVERY outcome |
| `PositionsMerge(...)` | the reverse |
| `ConditionResolution(conditionId, oracle, questionId, outcomeSlotCount, payoutNumerators[])` | the payout vector |
| `PayoutRedemption(redeemer, collateralToken, parentCollectionId, conditionId, indexSets[], payout)` | winnings paid |
| ERC-1155 `TransferSingle` / `TransferBatch` | every balance change, including the mints / burns of the above |

The ids are arithmetic (`ids.rs`):

```text
conditionId  = keccak256(oracle ++ questionId ++ uint256(outcomeSlotCount))
collectionId = alt_bn128 point of keccak256(conditionId ++ uint256(indexSet))   (x, parity of y in bit 254)
positionId   = uint256(keccak256(collateralToken ++ collectionId))              = the ERC-1155 token id
```

so **outcome index <-> ERC-1155 token id is computed off the commit path,
without RPC**, from every split / merge / redemption, and a
`ConditionPreparation` whose id does not hash is rejected. Verified: the ids
computed from 21 real `PositionSplit`s on four chains (Polygon, Gnosis,
Base, BNB Chain; USDC.e, wrapped collateral, WXDAI, USDC, USDT) equal the ids the
registry minted in the `TransferBatch` right before them. (`TokenRegistered`
of the exchange says the same, but it is a claim of its emitter - the
indexer does not use it.)

### Order book exchanges (families `ctf_exchange`, `ctf_exchange_v2`)

* V1, Polymarket CTF Exchange `0x4bFb…982E` and NegRisk CTF Exchange
  `0xC5d5…f80a` until early 2026, and every fork:
  `OrderFilled(orderHash, maker, taker, makerAssetId, takerAssetId, makerAmountFilled, takerAmountFilled, fee)`,
  `OrdersMatched(...)`. **Asset id 0 is the collateral**: `makerAssetId = 0`
  means the order BUYS `takerAssetId`. The fee is charged in what the order
  receives (shares for a buy, collateral for a sell).
* V2, live on Polygon now (`0xe111…996b`, NegRisk `0xe222…0f59`; also seen
  on Base): `OrderFilled(orderHash, maker, taker, side, tokenId, makerAmountFilled, takerAmountFilled, fee, builder, metadata)`,
  `side` 0 = BUY. Fees always in collateral. Traders pay in a wrapper token
  (pUSD `0xC011…2DFB`) that an adapter unwraps to USDC.e, which still backs
  the positions.

A match of one taker order against N maker orders emits **N + 2 events
about the same shares**: one `OrderFilled` per maker order, one
`OrderFilled` for the taker order (its `taker` is the exchange itself) and
one `OrdersMatched`. Summing every `OrderFilled` double counts (this is the
well known "Polymarket volume is doubled" problem).

There are three ways two orders match: `complementary` (a buy against a sell
of the same outcome: shares change hands), `mint` (two buys of the two
outcomes: the exchange splits fresh collateral into a full set) and `merge`
(two sells: a full set is merged back). Real example
(`V1_NEG_RISK_MATCH`, Polygon 78000012): one taker buying NO at 0.941 is
filled by one maker selling NO at 0.941 and five makers buying YES at 0.059.

### The canonical trade

**One `prediction_trades` row = one filled MAKER order, told from the
TAKER's point of view.** The taker's own `OrderFilled` and `OrdersMatched`
are never rows - the decoder only uses them to learn whose point of view to
take, the match type and the taker's fee (split over the fills pro rata,
the last one takes the remainder, so it adds up exactly).

Why this one: it is the finest grain the chain has (every price level of
the book that was hit is a print on the tape), it names BOTH traders (a
portfolio needs the maker's fills as much as the taker's), and it counts
every share once. It is proven lossless on the real 6 maker match: the six
rows add up to the taker order's own event to the last unit (1,346.42 shares
for 1,266.98122 USDC). For `mint` / `merge` fills a row carries both prints:
the taker's token at `collateral_amount / share_amount` and the maker's
token at `maker_collateral_amount / share_amount` - they always add up to 1.

**Volume = collateral that changed hands**: one leg for a complementary
fill, both legs of a mint / merge (the full set really was paid for by two
parties). Nothing is ever counted twice.

### Multi outcome events (family `neg_risk`)

Polymarket's NegRiskAdapter (`0xd91E…5296`, a second generation at
`0xacB0…8B94`) groups binary markets into an event:
`MarketPrepared(marketId, oracle, feeBips, data)` = the event,
`QuestionPrepared(marketId, questionId, index, data)` = one YES/NO market of
it (`questionId` = `marketId` + index, and the adapter is the CTF oracle of
the condition). `PositionsConverted` turns NO positions of some questions
into YES positions of the others. `event_id` / `event_title` / `event_index`
of `prediction_markets_v` come from here.

### Titles: what IS on chain

* UmaCtfAdapter `QuestionInitialized(questionID, requestTimestamp, creator, ancillaryData, rewardToken, reward, proposalBond)`:
  **`ancillaryData` really is human readable UTF-8**:
  `q: title: Kansas City Royals vs. Pittsburgh Pirates: O/U 10.5, description: ..., market_id: ... res_data: p1: 0, p2: 1, p3: 0.5. Where p1 corresponds to Under, p2 to Over ...`.
  Title, description and the outcome labels are parsed (`text.rs`), the raw
  payload is stored. The adapter is the oracle of the condition and
  `questionID` its `questionId`: that is the join.
* NegRisk `MarketPrepared.data` / `QuestionPrepared.data`: `title: ...` /
  `question: ...` text (the second generation adapter hex encodes it once
  more - handled).
* `QuestionReset` / `QuestionFlagged` of the UMA adapter = the proposal was
  disputed: `status = 'disputed'` until the resolution.

### AMM venues (family `fpmm`)

Gnosis FixedProductMarketMaker (Omen and the AI agent markets on Gnosis,
Limitless' older markets on Base): `FPMMBuy` / `FPMMSell(trader, amount, feeAmount, outcomeIndex, outcomeTokens)`.
The pool is the maker; the token id comes from the ERC-1155 transfer the
pool makes right before the event. Decoded as trades (`match_type = 'amm'`).

### Venue coverage

On-chain EVM prediction market volume, DefiLlama "Prediction Market"
category, 30 days to 2026-09-18 (Kalshi, Polymarket US and other off-chain
venues excluded: they have no logs):

| Venue | Chain | 30d volume | Family | Evidence (real tx in the fixtures) |
|---|---|---|---|---|
| Polymarket International | Polygon | $2,278M | `ctf` + `ctf_exchange` (history) + `ctf_exchange_v2` (now) + `neg_risk` + `uma` | `V1_NEG_RISK_MATCH`, `V2_MINT_MATCH`, 10 more |
| Opinion | BNB Chain | $332M | `ctf` + `ctf_exchange` fork | same events seen from 3 exchanges / 4 registries in 100 blocks, `BSC_V1_MATCH` |
| Predict.fun | BNB Chain (Blast) | $283M | `ctf` + `ctf_exchange` fork (`0x8BC0…B689`, USDT 18 decimals) | scan of BNB Chain head |
| Limitless | Base | $13M | `ctf` + `ctf_exchange` fork, `fpmm` for old markets | `BASE_V1_MATCH`, `FPMM_SELL` |
| Polymarket Combos | Polygon | $21M | not decoded (parlay contracts, own events) | - |
| PredictStreet | ADI chain | $56M | unknown chain, not researched | - |
| SX Bet | SX Rollup | $55M | own order book events | - |
| Overtime / Thales, Azuro | several | $8M / $4M | own sports book events (no CTF) | - |
| PancakeSwap Prediction, Myriad, Truemarkets, others | | < $10M each | own events | - |
| Omen / Seer / agent markets | Gnosis | tiny | `ctf` + `fpmm` | `FPMM_BUY` |

**~95% of the on-chain EVM volume (~$2.9bn of ~$3.05bn) is the CTF family
and is covered.** The rest is listed under known gaps.

## Tables and views

Storage follows docs/design.md §1-§2: binary `FixedString` / `UInt256`,
`ReplacingMergeTree(_version, is_deleted)` + `epoch`, no DELETE ever,
aggregates keyed by `epoch` with the shared `epoch_floor_v` validity rule.

| Table | Rows | Sort key | Why |
|---|---|---|---|
| `prediction_markets` | one per `ConditionPreparation` | (chain, market_id, registry, block, log) | identity first: FINAL = one row per market |
| `prediction_resolutions` | one per `ConditionResolution` | same | payout vector, winning outcome |
| `prediction_questions` | titles, events, disputes | (chain, question_id, emitter, kind, ...) | joins a market by (question_id, emitter = oracle) |
| `prediction_outcome_tokens` (+ `_by_market`) | outcome index <-> token id, computed | (chain, registry, token) / (chain, market, ...) | NOT block scoped: arithmetic no reorg can change |
| `prediction_trades` | THE canonical trade | (chain, block, log), month partitions | base table |
| `prediction_position_events` | split / merge / redeem / convert | (chain, block, log) | open interest, funding flows |
| `prediction_transfers` | one per ERC-1155 id moved, with the REASON of both legs | (chain, block, log, batch index) | exact balances |
| `prediction_trades_by_token` (MV) | tape | (chain, registry, token, block, log) | a market's tape = the tail of two ranges |
| `prediction_ledger_by_holder` / `_by_token` (MV) | everything that changed a balance or has a price, per account | (chain, holder, registry, token, ...) / (chain, registry, token, holder, ...) | a wallet = one range, the holders of an outcome = one range |
| `prediction_candles_1m/1h/1d` | OHLC of the probability per outcome token, volume, trades, unique traders (`uniqState`) | (chain, registry, token, bucket, epoch) | a chart = one range |
| `prediction_market_flows_1d` | split / merged / redeemed collateral per market | (chain, registry, market, collateral, bucket, epoch) | open interest |
| `prediction_trader_trades_1d`, `prediction_trader_flows_1d` | per trader and day | (chain, bucket, trader, ..., epoch) | leaderboard = one range per period |
| `prediction_venues` | exchange -> collateral token, by the background resolver | (chain, exchange) | decimals of leaderboard amounts |
| `prediction_venue_labels` | USER populated brand names | (chain, address) | `venue` column, excludes contracts from the leaderboard |
| `prediction_market_metadata` | EXTERNAL enricher (never the indexer) | (chain, market_id) | off-chain titles, slugs, categories, images, end dates |
| `prediction_market_list` | the market list as a table, recomputed by ClickHouse every minute (refreshable materialized view, atomic swap, no DELETE) | (chain, market_id, registry) | list / search / header without any aggregation at query time |

Views: `prediction_markets_v` (list, search, header), parameterized
`prediction_candles_1m/1h/1d_v`, `prediction_trades_v`, `prediction_holders_v`,
`prediction_positions_v`, `prediction_activity_v`, `prediction_leaderboard_v`,
and `prediction_markets_live_v` (the definition of the list, always exact,
reads every market - not for consumers).

Why parameterized views (`SELECT ... FROM view(chain = 137, holder = ...)`):
the token id <-> market mapping is resolved at QUERY time (that is what makes
indexing from the middle of the chain heal itself), and a parameter is the
only way to push "which wallet / market" into every subquery of a view, so
each one is a primary key range read instead of a join over millions of
tokens.

### Positions and PnL

* **Balances are exact**: `sum(share_delta)` as `Int256` over every ERC-1155
  transfer leg of `(registry, token, holder)` - mints and burns of splits,
  merges and redemptions are transfers too.
* The decoder tags every transfer leg with the reason the two balances
  changed, from the sibling events of the same transaction: `trade` (to /
  from an exchange that filled orders in that transaction), `split`,
  `merge`, `redeem` (also through an adapter that did it on the user's
  behalf), else `transfer`.
* Money columns use the average cost method over everything that has a
  price: buys (fee included), splits (a full set costs 1, each of its n
  outcomes 1/n - the same convention Polymarket uses), sells, merges and
  redemptions (at the payout). `realized_pnl = proceeds - avg_entry_price *
  shares disposed`, `unrealized_pnl = (mark - avg_entry_price) * shares`,
  mark = payout once resolved, else last price.
* **Transfers that are not trades** (wallet to wallet, exchange escrow,
  NegRisk conversions) change `balance` but never the cost basis; they are
  reported as `unpriced_shares`. A wallet that only ever received a token
  has `avg_entry_price = NULL` and NULL PnL - unknown, not zero.

### Current price, volume, open interest, status

`outcome_prices[i]` = last print of outcome i (`argMax` by
`(block_number, log_index)` through the daily candles); 24h volume from the
hourly candles; `open_interest` = split - merged - redeemed collateral of
the registry's own events (it only knows the indexed history: started mid
chain it can be negative); `status` = `resolved` (a `ConditionResolution`
exists, with `payouts`, `winning_outcome`), `disputed` (UMA reset / flag),
else `open`. For AMM markets the price is the last trade too (pool state is
a known gap).

## Query cookbook

Placeholders in `{braces}` are request parameters. Ids go in as hex without
`0x`. The Rust constants are in `cookbook.rs`; a unit test keeps this
section identical to them and the ClickHouse integration test runs every
query against the real fixture data and asserts hand computed numbers.
Latencies: ClickHouse 25.12 on a laptop, fixture sized data - they are
the fixed cost of a query (parsing nested views), not a benchmark.

### Market list

Market list: open markets by 24h volume.

```sql
SELECT market_id, registry, venue, title, event_title, category, tags,
       outcomes, outcome_prices, volume_24h, volume_total, open_interest,
       traders, end_date, status
FROM prediction_markets_v
WHERE chain = {chain} AND status = 'open'
ORDER BY volume_24h DESC NULLS LAST, volume_total DESC NULLS LAST
LIMIT 50
```

Why it is cheap: A plain scan of `prediction_market_list` (one narrow row per market, ordered by `(chain, market_id, registry)`): the filter and the sort read three columns, the 50 winners are read in full. Nothing is aggregated at query time - ClickHouse recomputes the table once a minute.

Measured on the fixture data: **3.62 ms** (median of 9).

### Market search

Market search by title.

```sql
SELECT market_id, registry, venue, title, event_title, outcomes,
       outcome_prices, volume_total, status
FROM prediction_markets_v
WHERE chain = {chain}
  AND positionCaseInsensitiveUTF8(coalesce(title, ''), '{text}') > 0
ORDER BY volume_total DESC NULLS LAST
LIMIT 20
```

Why it is cheap: Same table, substring match over the `title` column only.

Measured on the fixture data: **3.59 ms** (median of 9).

### Market page header

Market page header: everything about one market.

```sql
SELECT *
FROM prediction_markets_v
WHERE chain = {chain} AND market_id = unhex('{market_id}')
```

Why it is cheap: Primary key lookup `(chain, market_id)` in `prediction_market_list`.

Measured on the fixture data: **7.44 ms** (median of 9).

### Multi outcome event

The markets of a multi outcome event, most likely first.

```sql
SELECT market_id, title, event_title, event_index,
       outcome_prices[1] AS yes_price, volume_total, status
FROM prediction_markets_v
WHERE chain = {chain} AND event_id = unhex('{event_id}')
ORDER BY yes_price DESC NULLS LAST, event_index
```

Why it is cheap: Same table, filtered by `event_id` (a scan of one FixedString column of the chain's markets).

Measured on the fixture data: **2.94 ms** (median of 9).

### Price chart

Price chart of one outcome (1m / 1h / 1d: same query, other view).

```sql
SELECT bucket, open, high, low, close, volume, shares, trades, traders
FROM prediction_candles_1h_v(chain = {chain}, registry = unhex('{registry}'),
                             outcome_token_id = toUInt256('{token}'))
WHERE bucket >= now() - INTERVAL 30 DAY
ORDER BY bucket
```

Why it is cheap: `prediction_candles_1h` is ordered by `(chain, registry, outcome_token_id, bucket, epoch)`: the chart of one outcome is ONE contiguous range, already aggregated per bucket. The UI takes `registry` and the token ids from the market row.

Measured on the fixture data: **11.05 ms** (median of 9).

### Trades tape

Trades tape of a market, newest first.

```sql
SELECT timestamp, outcome_index, outcome, side, price, shares, collateral,
       trader, transaction_hash
FROM prediction_trades_v(chain = {chain}, market_id = unhex('{market_id}'))
ORDER BY block_number DESC, log_index DESC
LIMIT 50
```

Why it is cheap: `prediction_trades_by_token` is ordered by `(chain, registry, outcome_token_id, block_number, log_index)`: the tape of a market is the tail of two ranges (one per outcome). The market -> token ids lookup is a primary key read of `prediction_outcome_tokens_by_market`.

Measured on the fixture data: **11.31 ms** (median of 9).

### Holders

Top holders of a market, per outcome.

```sql
SELECT outcome_index, outcome, holder, shares, avg_entry_price,
       current_price, value
FROM prediction_holders_v(chain = {chain}, market_id = unhex('{market_id}'))
ORDER BY outcome_index, shares DESC
LIMIT 100
```

Why it is cheap: `prediction_ledger_by_token` is ordered by `(chain, registry, outcome_token_id, holder, ...)`: the ledger of one outcome is one range, grouped by holder in order.

Measured on the fixture data: **14.84 ms** (median of 9).

### Portfolio

Portfolio of a wallet: open positions and what they are worth, realized profit of the closed ones, winnings waiting to be redeemed.

```sql
SELECT market_id, title, outcome, status, shares, avg_entry_price,
       current_price, value, unrealized_pnl, realized_pnl, redeemable,
       unpriced_shares
FROM prediction_positions_v(chain = {chain}, holder = unhex('{holder}'))
ORDER BY value DESC NULLS LAST, realized_pnl DESC NULLS LAST
```

Why it is cheap: `prediction_ledger_by_holder` is ordered by `(chain, holder, registry, outcome_token_id, ...)`: everything a wallet ever did is ONE range. Its tokens are mapped to markets by primary key (`prediction_outcome_tokens`), the markets read by primary key from `prediction_market_list`.

Measured on the fixture data: **63.83 ms** (median of 9).

### Wallet trade history

Trade history of a wallet, newest first.

```sql
SELECT timestamp, title, outcome, action, role, price, shares, collateral,
       fee, transaction_hash
FROM prediction_activity_v(chain = {chain}, holder = unhex('{holder}'))
WHERE action IN ('buy', 'sell')
ORDER BY block_number DESC, log_index DESC
LIMIT 50
```

Why it is cheap: The same range of `prediction_ledger_by_holder`, priced legs only.

Measured on the fixture data: **20.43 ms** (median of 9).

### Leaderboard

Leaderboard of a period.

```sql
SELECT trader, volume, net_cash_flow, fees, trades, outcome_tokens_traded
FROM prediction_leaderboard_v(chain = {chain}, from_day = '{from_day}',
                              to_day = '{to_day}')
ORDER BY volume DESC
LIMIT 100
```

Why it is cheap: `prediction_trader_trades_1d` / `prediction_trader_flows_1d` are ordered by `(chain, bucket, trader, ...)`: a period is one range of pre-aggregated rows per trader and day.

Measured on the fixture data: **37.86 ms** (median of 9).


## What a trading UI still can NOT get from the chain

| Missing | Where it lives |
|---|---|
| Order book depth, best bid / ask, open orders | Polymarket CLOB API (`clob.polymarket.com`, REST + websocket). Orders are signed off chain and only touch the chain when matched. The on-chain "price" is the last trade. |
| Titles of markets whose question text is not on chain, slugs, categories, tags, images, end dates, featured / trending flags | Polymarket Gamma API (`gamma-api.polymarket.com/markets`, keyed by `conditionId`). An external enricher writes them into `prediction_market_metadata` keyed by `market_id` = `conditionId`; `prediction_markets_v` picks them up without any change to the UI query. Every such column is NULL until then. |
| Which human owns a proxy wallet, usernames, profile pictures | Polymarket's own database (Gamma / data API) |
| Resolution disputes in detail (proposer, disputer, bonds, votes) | UMA Optimistic Oracle events (`ProposePrice`, `DisputePrice`, `Settle`) - on chain but a different protocol, not decoded here |
| Liquidity rewards, builder attribution of V2 orders | reward programs off chain; `builder` / `metadata` hashes are in the V2 event but meaningless without the off-chain registry |

## Pipeline integration

```rust
let mut rows = predictions::decode(chain, &batch.logs);   // pure, never panics
rows.attach_transactions(|hash| ...);                     // tx_from / tx_to
rows.retain_transfers(|registry| registries.contains(registry)); // optional, see RegistrySet
rows.set_epoch(epoch);
rows.set_version(version);
// insert in predictions::INSERT_ORDER (the outcome token map first)
venue_worker.discover(&rows.venue_candidates());          // never blocks
token_worker.discover(rows.token_addresses());            // collateral decimals
```

`purge_range` tombstones `predictions::BASE_TABLES` (the side tables follow
through their materialized views) and rebuilds `PREDICTIONS_DERIVED`.
`decode` keeps every ERC-1155 transfer it sees (always correct); a
`RegistrySet` seeded with `KNOWN_REGISTRIES_SQL` drops unrelated NFT traffic.

`VenueWorker` mirrors `dex::PoolWorker` (`spawn` / `discover` / `stats` /
`shutdown`, bounded queue, drop-on-full, DB driven backfill with
`MISSING_VENUES_SQL`, negative caching, circuit breaker) and asks the ONE
thing events can not tell: `getCollateral()` / `collateralToken()` of an
exchange / pool.

Labels (optional, user populated):

```sql
INSERT INTO prediction_venue_labels (chain, address, venue) VALUES
  (137, unhex('4D97DCd97eC945f40cF65F87097ACe5EA0476045'), 'polymarket'),  -- the registry names its markets
  (137, unhex('e111180000d2663c0091e4f400237545b87b996b'), 'polymarket'),  -- exchanges are not traders
  (137, unhex('C5d563A36AE78145C45a50134d48A1215220f80a'), 'polymarket');
```

## Known gaps

* Venues with their own events: Polymarket Combos, SX Bet, Azuro, Overtime,
  PancakeSwap Prediction, Myriad, Truemarkets, PredictStreet (~5% of the
  on-chain volume together). Not on EVM logs at all: Kalshi, Polymarket US.
* Nested (deep) CTF positions (`parentCollectionId != 0`) and combined index
  sets (`0b110`) are stored as events but have no outcome token mapping.
  No venue with volume uses them.
* A condition split against several collateral tokens shows the positions
  of the first collateral only.
* NegRisk conversions are recorded (`kind = 'convert'`) but unpriced: the
  converted shares appear as `unpriced_shares`, open interest ignores them.
* Redemptions / splits through the V2 collateral adapters that emit no
  event naming the user are attributed to the adapter in the leaderboard's
  funding flows (positions are still exact and correctly tagged).
* FPMM: trades only. Liquidity events and the pool state price are not
  decoded; pool -> condition comes from the token map, not from the factory.
* The leaderboard's period number is a cash flow, not a mark to market PnL
  (exact average cost PnL is per wallet, in `prediction_positions_v`).
* `prediction_market_list` is up to a minute old (the chart, tape, holders
  and portfolio queries read live tables; prices in the portfolio come from
  the list).
* UMA dispute details and `QuestionResolved` are not decoded (the CTF's
  `ConditionResolution` is the truth for payouts).
