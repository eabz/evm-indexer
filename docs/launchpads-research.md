# Token launchpads: who has the activity, and what an EVM log indexer can see

Research note for the owner. No code. Data retrieved 2026-09-19 00:50-01:10 UTC
(2026-09-18 evening, America time). Sources, endpoints and everything that could not
be verified are in the appendix. Every figure comes from a fetched source or from a
live chain query made during this session; nothing is estimated from memory. Where I
give an estimate of my own it is labelled as such.

## 1. Executive summary (plain language)

* DefiLlama tracks 285 launchpads; 119 earned fees in the last 30 days. Total:
  **$256.5M fees in 30 days** ($57.7M in 7 days, $8.0M in 24h), **$6.36B traded on
  bonding curves** (59 venues report volume). The previous 30 days were $82.0M fees
  and $3.21B volume - the category tripled in a month.
* **The market is not where we all remember it.** The #1 launchpad by fees is
  **Pons, on Robinhood Chain** (an EVM chain, Arbitrum Orbit, chain id 4663, served
  by HyperSync): $128.2M fees / $2.24B curve volume in 30 days. pump.fun (Solana) is
  #2 by fees ($44.7M) and still #1 by curve volume ($2.53B). #3 is Flap on BNB Chain
  ($33.8M / $0.70B). These three are 80.6% of all launchpad fees.
* **The reachable number, stated plainly: 73.2% of 30-day launchpad fees, 49.3% of
  bonding-curve volume and 45.2% of protocol revenue happen on EVM chains HyperSync
  serves.** Solana is 26.8% of fees, 50.7% of volume, 54.7% of revenue. EVM chains
  HyperSync does not serve (X Layer etc.) are 0.04%.
* Two honesty notes on that 73%: (1) Pons' fee figure includes fees from its tokens
  after they graduate to Uniswap v4, while pump.fun's excludes PumpSwap, which
  DefiLlama lists separately as a DEX ($97.5M fees, $18.2B volume in 30 days). Volume
  (49%) is the like-for-like number. (2) The Robinhood Chain boom is about ten weeks
  old and rode a **90-day gas waiver that ends around the end of September 2026**;
  Pons' 24h fees ($2.9M) are already below its 30-day average ($4.3M/day).
* On EVM the work is small. **Two decoders cover 86.5% of the EVM-reachable fees**:
  the Pons V2 curve family (verified source, 5 events, confirmed against live
  logs) and the Flap Portal family (verified source, same ABI on 4 chains). I
  counted 13,658 Pons launches and 157 graduations in the last 24 hours directly
  from the chain.
* **Cheap win:** most other EVM launchpads (o1, NOXA, Pons V1, LetsCash, Bags on
  Robinhood, Clanker, Zora, BaseStonk...) launch straight into a Uniswap V3/V4 pool.
  Our spot-DEX decoders already capture every trade; only a small "this token was
  launched by X, creator Y" event per venue is missing. Pons and Flap graduates also
  land in Uniswap V4 / PancakeSwap pools we already decode.
* Owner's named examples: **pump.fun** = Solana program, fully decodable, not EVM.
  **fomo** = a mobile social trading app (fomo.family), $32.2M fees in 30 days, no
  contracts of its own - a front end over other venues, attributable only by its fee
  wallet. **bags.fm** = a platform on top of Meteora's bonding-curve program on
  Solana (87% of its fees) plus its own contracts on Robinhood Chain (13%).
* The lead's memory list is mostly stale: Believe $196, flaunch $479, boop $0,
  Heaven $0, time.fun $0, Creator.bid $11K, Zora coins $16K, SunPump $88K, Moonshot
  $84K, Clanker $328K, four.meme $241K (30-day fees).
* **Solana:** Envio's Solana HyperSync is real and well shaped for this (filter by
  program + instruction discriminator, inner instructions, logs, token balance
  changes, same API token), but young (0.2.0, published 2026-08-12, a data-loss bug
  fixed in that release) and **history only reaches back to about slot 403M (roughly
  seven months, my estimate)**. Three Solana programs (pump.fun, Raydium LaunchLab,
  Meteora DBC) carry essentially all of Solana's launchpad volume. Recommendation:
  build EVM first (weeks, not months); add Solana as a second, program-filtered
  ingest pipeline writing into the same launchpad tables afterwards.

## 2. Ranking: top 40 launchpads by 30-day fees

Source: `api.llama.fi/overview/fees` (fees), same with `dataType=dailyRevenue`
(revenue), `api.llama.fi/overview/dexs` (volume), all filtered to
`category == "Launchpad"`, fetched 2026-09-19 00:50:17-00:50:20 UTC. The free API
served all three with HTTP 200 (no 402 this time), including a per-chain breakdown.
"Fees" = everything users paid (protocol + creators + buybacks); "Revenue" = the
protocol's cut. "Curve vol" = volume on the launchpad's own curve, where DefiLlama has
a volume adapter (n/a = none). Share is of the $256,494,615 category total.
"Prev 30d" = fees in the 30 days before.

| # | Venue | Chain(s), share of venue fees | 24h fees | 7d fees | 30d fees | 30d revenue | Share | Cumulative | 30d curve vol | Prev 30d fees |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | Pons V2 | Robinhood | 2.86M | 29.21M | 128.16M | 22.36M | 49.97% | 49.97% | 2.24B | 2.89M |
| 2 | pump.fun | Solana | 1.65M | 9.37M | 44.66M | 32.66M | 17.41% | 67.38% | 2.53B | 37.30M |
| 3 | Flap sh | BSC 98%, Robinhood 2% | 1.12M | 7.33M | 33.83M | 10.97M | 13.19% | 80.57% | 698.61M | 8.01M |
| 4 | StonkFun | Solana | 625.6K | 3.59M | 11.91M | 11.91M | 4.64% | 85.21% | 242.25M | 819.6K |
| 5 | Pons V1 | Robinhood | 79.5K | 841.6K | 7.25M | 1.54M | 2.83% | 88.04% | n/a | 16.52M |
| 6 | o1 Launchpad | Robinhood 50%, Base 50% | 28.1K | 398.9K | 4.59M | 2.46M | 1.79% | 89.83% | n/a | 947.4K |
| 7 | NOXA Fun | Robinhood ~100% | 263.6K | 511.2K | 3.82M | 0 | 1.49% | 91.32% | n/a | 4.19M |
| 8 | BONK.fun (LetsBonk) | Solana | 470.5K | 1.74M | 3.50M | 2.08M | 1.36% | 92.68% | n/a | 142.6K |
| 9 | Raydium LaunchLab | Solana | 355.7K | 1.61M | 3.04M | 784.8K | 1.19% | 93.87% | 148.58M | 27.8K |
| 10 | Binance Alpha | BSC 87%, Base 8%, Ethereum 5% | 67.5K | 401.5K | 2.12M | 2.12M | 0.82% | 94.69% | n/a | 1.29M |
| 11 | Bags | Solana 87%, Robinhood 13% | 3.5K | 21.6K | 1.75M | 870.1K | 0.68% | 95.37% | n/a | 358.5K |
| 12 | StonkBrokers | Robinhood 99%, Base 1% | 46.7K | 656.0K | 1.69M | 860.6K | 0.66% | 96.03% | 4.68M | 2.33M |
| 13 | LetsCash | Robinhood | 13.8K | 131.8K | 1.45M | 181.2K | 0.57% | 96.60% | 46.63M | 1.08M |
| 14 | Graphite Protocol | Solana | 187.0K | 701.2K | 1.39M | 1.39M | 0.54% | 97.14% | n/a | 55.5K |
| 15 | Pools (pools.trade) | Robinhood | 45.3K | 293.1K | 1.21M | 0 | 0.47% | 97.61% | n/a | 1.09M |
| 16 | Meteora Dynamic Bonding Curve | Solana | 73.5K | 315.4K | 980.5K | 179.7K | 0.38% | 97.99% | 300.20M | 779.0K |
| 17 | Ansem.io | Solana | 59 | 13.7K | 738.6K | 738.6K | 0.29% | 98.28% | n/a | 1.33M |
| 18 | PAIR | Robinhood | 1.0K | 18.2K | 585.7K | 177.5K | 0.23% | 98.51% | n/a | n/a |
| 19 | BaseStonk | Base ~100% | 3.9K | 45.5K | 448.9K | 322.6K | 0.18% | 98.68% | 29.43M | 69.6K |
| 20 | Sentry | Ink 65%, Robinhood 35% | 1.0K | 22.5K | 348.1K | 111.4K | 0.14% | 98.82% | n/a | 20.4K |
| 21 | Clanker | Base 90%, Robinhood 6%, Ethereum 4% | 5.5K | 30.7K | 327.8K | 54.6K | 0.13% | 98.95% | n/a | 116.9K |
| 22 | Pez Family | Robinhood | 179 | 5.7K | 316.1K | 302.7K | 0.12% | 99.07% | 438.4K | n/a |
| 23 | four.meme | BSC | 7.5K | 36.5K | 240.7K | 236.1K | 0.09% | 99.16% | 95.43M | 685.8K |
| 24 | Metaplex (Genesis) | Solana | 68 | 10.1K | 214.8K | 214.8K | 0.08% | 99.25% | n/a | 146.2K |
| 25 | Rapid Launch | Solana | 7.8K | 31.7K | 156.0K | 134.8K | 0.06% | 99.31% | n/a | 129.8K |
| 26 | Umia | Base | 2.7K | 13.6K | 154.2K | 154.2K | 0.06% | 99.37% | 1.86M | n/a |
| 27 | Nad.fun V1 | Monad | 559 | 3.8K | 142.4K | 59.8K | 0.06% | 99.43% | 6.49M | 26.7K |
| 28 | Coinbarrel | Robinhood | 1.4K | 1.9K | 136.0K | 50.0K | 0.05% | 99.48% | n/a | 29.8K |
| 29 | token.select | Robinhood | 4.5K | 12.5K | 124.3K | 31.0K | 0.05% | 99.53% | n/a | 16.1K |
| 30 | Squeeze | Solana 95%, Robinhood 5% | 298 | 10.9K | 110.2K | 107.1K | 0.04% | 99.57% | n/a | 79.9K |
| 31 | Cook Market | Robinhood | 589 | 97.5K | 105.3K | 10.4K | 0.04% | 99.61% | n/a | n/a |
| 32 | SunPump | Tron | 286 | 14.8K | 88.4K | 88.4K | 0.03% | 99.65% | n/a | 12.3K |
| 33 | Ignix | X Layer | 709 | 25.0K | 88.2K | 40.4K | 0.03% | 99.68% | 3.54M | n/a |
| 34 | Jupiter Studio | Solana | 9.9K | 15.3K | 85.8K | 77.3K | 0.03% | 99.71% | n/a | 28.1K |
| 35 | Moonshot Create | Solana | 22.2K | 29.5K | 84.5K | 83.1K | 0.03% | 99.75% | n/a | 82.2K |
| 36 | Rise.rich | Solana | 1.2K | 5.7K | 82.6K | 62.0K | 0.03% | 99.78% | 3.42M | 87.8K |
| 37 | Hookers | Robinhood | 307 | 12.8K | 82.3K | 20.1K | 0.03% | 99.81% | n/a | 2.1K |
| 38 | EasyA Kickstart | Solana | 866 | 8.2K | 77.6K | 31.0K | 0.03% | 99.84% | n/a | 78.6K |
| 39 | Alt Fun | Hyperliquid L1 | 161 | 689 | 61.0K | 40.7K | 0.02% | 99.86% | 8.20M | 8.9K |
| 40 | PinkSale | Solana 97%, Base 2% | 3.8K | 5.6K | 45.7K | 45.7K | 0.02% | 99.88% | n/a | 7.9K |

Curve volume, top names (share of the $6,364,699,584 total; 87 adapters, 59 with
volume): pump.fun 39.75%, Pons V2 35.12%, Flap 10.98% (BSC $690.3M, Robinhood $8.2M,
X Layer $0.09M), Meteora DBC 4.72%, StonkFun 3.81%, LaunchLab 2.33%, four.meme 1.50%,
LetsCash 0.73%, BaseStonk 0.46%. Everything else is below 0.15%.

Per-chain totals for the category (30 days):

| Chain | Fees | Share | Revenue | Share | Curve volume | Share | On HyperSync |
|---|---|---|---|---|---|---|---|
| Robinhood Chain | $148.30M | 57.82% | $27.16M | 28.98% | $2,296.5M | 36.08% | yes (4663) |
| Solana | $68.65M | 26.76% | $51.31M | 54.74% | $3,224.4M | 50.66% | separate Solana product (section 4) |
| BNB Chain | $35.29M | 13.76% | $12.83M | 13.69% | $785.8M | 12.35% | yes |
| Base | $3.40M | 1.32% | $1.95M | 2.08% | $31.9M | 0.50% | yes |
| Ink, Monad, Ethereum, Hyperliquid, Tron, Arc | $0.71M | 0.28% | $0.39M | 0.41% | $21.9M | 0.34% | yes |
| X Layer, Eden, Stable, Shido, GateLayer... | $0.10M | 0.04% | $0.05M | 0.05% | $4.1M | 0.06% | no (Stable "on request") |
| TON, Aptos | $0.04M | 0.01% | $0.04M | 0.04% | $0.1M | 0.00% | no (non-EVM) |

Tokens launched (DefiLlama has no such metric; these are my own `eth_getLogs` counts
from public RPCs, 24.18-hour window ending 2026-09-19 ~01:00 UTC):

| Venue | Launch event counted | Launches / 24h | Graduations / 24h |
|---|---|---|---|
| Pons V2 (Robinhood) | `TokenLaunched` on factory `0x7eD5...EC7e` | 13,658 | 157 (`PoolGraduated`), 1.15% |
| Flap (Robinhood only) | `TokenCreated` on portal `0x2660...Eb09` | 1,863 | not counted |
| Pons V1 (Robinhood) | `TokenLaunched` on factory `0xA5aA...1feB` | 0 (legacy; still earns fees from old pools) | - |
| four.meme (BSC) | `TokenCreate` on `0x5c95...762b` | not obtained: 15 creates in a 400-block (~3 min) sample; the public BSC RPCs refused wider ranges | - |
| Flap (BSC), pump.fun, all Solana | - | not obtained from a fetched source | - |

Second sources: The Defiant (2026-09-01, citing DefiLlama and CoinGecko) reports Pons
fees $4.89M on Aug 31, $21.04M over 7 days, 63.9% of all launchpad fees that day,
pump.fun $1.72M (22.5%), Robinhood Chain DEX volume $1.49B/24h. CoinDesk (2026-09-03,
via search snippet) reports ~25,000 Pons launches and $544M volume on Sept 2 and
~646,000 tokens from ~167,000 creators since July. These are consistent with the API
(the 24h run rate has fallen since). Both ultimately lean on DefiLlama, so they
confirm the reading, not the measurement; my own log counts are the independent check.

Things worth noticing:

* **Concentration**: top 3 = 80.6% of fees, top 10 = 94.7%. Below rank 23 every
  venue earns less than $8K/day.
* **Speed of change**: Pons V2 started 2026-08-03 (V1 2026-07-13). Six weeks ago it
  did not exist. Pons V1 fell from $16.5M to $7.3M, LetsCash from $76.7M to $46.6M
  volume, four.meme from $213.5M to $95.4M. Re-pull before building anything.
* **Fee/volume mismatch**: Pons V2's fees are 5.7% of its curve volume because the fee
  figure also counts Uniswap v4 fees on graduated tokens and creator "taxes"; the
  volume figure is curve-only. pump.fun is the opposite: curve-only fees, with
  PumpSwap reported as a DEX. Adding PumpSwap and the pump.fun mobile app, the pump
  group earned $148.9M in 30 days, more than Pons V1+V2 ($135.4M).
* Binance Alpha (#10) is a fee collector on Binance Wallet's swap route, not a
  launchpad. Graphite Protocol is LetsBonk's joint-venture partner (its figure is a
  share of LetsBonk fees, a double count in spirit). Ansem.io and Metaplex are
  one-off sale platforms.
* DefiLlama's volume coverage is partial (no volume adapter for BONK.fun, Bags, o1,
  NOXA, Pons V1...). For the "direct to DEX" venues there is no curve volume by
  construction - their trading is inside Uniswap's numbers.

### 2.1 Front ends, trading apps and bots (no event family of their own)

These route into other venues' contracts. They have users and fees, often more than
the launchpads, but nothing to decode except "who took a fee in this transaction".
Their volume **overlaps** the venue volume above; never add the two.

| App | DefiLlama category | 30d fees | 30d volume | Chains (share of its volume) | How DefiLlama attributes it |
|---|---|---|---|---|---|
| GMGN | Telegram Bot | $51.08M | $6.37B | Robinhood 65%, BSC 28%, Solana 5%, Base 2% | EVM: transfers to fee collector `0xb8159ba378904F803639D274cEc79F788931c9C8`; Solana: nine fee wallets (`fees/gmgnai.ts`) |
| Axiom | Trading App | $46.51M | $2.38B | Solana 98%, Robinhood 2%, BSC | Solana router program `FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9` + ~22 fee wallets; BSC trade contracts `0x0570...b02d`, `0x9689...8341` (`fees/axiom.ts`) |
| **fomo Wallet** | Trading App | $32.23M | n/a in the DEX list | reported as Solana only | Solana fee wallet `R4rNJHaffSUotNmqSKNEfDcJE8A7zJUkaoM5Jkd7cYX` (USDC inflows) + gas sponsor `AgmLJBMD...zN51`; EVM trades go through Relay and are reported by fomo itself in a Dune table (`dune.tryfomo.fomo_relay_fees`) |
| pump.fun Mobile App | Interface | $6.70M | $352.7M | Solana | fee wallet |
| Terminal | Telegram Bot | $5.11M | - | Solana, Ethereum, BSC, Base | fee wallets |
| Maestro | Telegram Bot | $1.94M | - | BSC, Ethereum, Solana, Base | fee wallets |
| Bankr | Interface | $1.63M | - | Base, Robinhood | not inspected |
| Trojan | Telegram Bot | $1.08M | - | Solana | fee wallets |
| fomo Perps | Interface | $0.82M | - | Hyperliquid (builder code) | - |
| Photon $550K, moonshot.money $286K, BONKbot $108K, Banana Gun $9.9K, BullX $920, Unibot $0 | | | | | |

**"fomo" resolved.** It is *fomo* by FOMO Labs (`fomo.family`, iOS/Android): a
self-custodial social trading app - a feed of what friends and top traders buy, one
tap to copy, Apple Pay funding, one USDC cash balance. Its own site lists six chains:
Solana, Base, BNB Chain, Monad, Ethereum and Robinhood Chain. It has **no launch or
trading contracts of its own**: Solana swaps go through existing DEX programs with a
USDC fee to its wallet, and non-Solana trades are filled cross-chain by Relay solvers
(DefiLlama's adapter says so explicitly and books all of it under Solana). It is not
a launchpad. For an EVM log indexer fomo is close to invisible: the EVM leg is a
Relay solver transaction, and the link back to a fomo user lives on Solana / off
chain. A separate, unrelated entry "copyfomo" ($45K) is a Telegram bot. Second source
for size: a search result quoting DefiLlama says fomo earned $1.76M on 2026-09-06,
above pump.fun's $1.1M that day.

Attribution recipe for front ends on EVM, if the owner wants it later: a small
registry table `(chain, address, app, kind: fee_wallet | router)`; tag a trade when
the same transaction contains a native or ERC-20 transfer to a registered fee wallet,
or when `tx.to` is a registered router. Native-ETH fee payments inside a router call
are internal transfers, which this project does not store (no traces) - so router
address matching is the reliable half.

## 3. Where the data lives: classification with evidence

Categories: **A** = EVM, launches + curve trades + graduation are event logs.
**B** = EVM, launch is an event but trading happens in a normal DEX pool from block
one (our spot decoders already capture the trades; only launch attribution is
missing). **C** = non-EVM. **D** = unclear. "HS" = chain on the HyperSync
supported-networks page fetched 2026-09-19 00:51 UTC.

Evidence types: **SRC** = event list read from verified source on Sourcify;
**LIVE** = topic0 computed from the signature and matched against real logs pulled
from a public RPC in this session; **ADAPTER** = only seen in DefiLlama's adapter;
**IDL** = public Anchor IDL. Robinhood Chain's Blockscout sits behind a Cloudflare
bot check that I did not bypass, so explorer links are given but the evidence is
Sourcify + RPC.

| # | Venue | Cat | Where the data is | Evidence |
|---|---|---|---|---|
| 1 | **Pons V2** | **A** | Robinhood Chain (HS yes). Factory `PonsV2LaunchFactory` `0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e` emits `TokenLaunched`, `PoolGraduated`. **One curve contract per token** emits `CurveBuy` / `CurveSell` (77 distinct curves traded in a 100-second sample). On graduation the factory creates a Uniswap v4 pool (PoolManager `0x8366a39CC670B4001A1121B8F6A443A643e40951`, hook `PonsV2MemeHook` `0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044`) and locks the position. | SRC exact match ([Sourcify](https://sourcify.dev/server/v2/contract/4663/0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e)); LIVE: launch tx [`0xdb2f70e1...8bda7e`](https://robinhoodchain.blockscout.com/tx/0xdb2f70e1d4e583562df38dd5d57cfd15df227757c4e3d1af571288563a8bda7e) (block 66669547), buy tx [`0xac3ca1ed...6ff51b`](https://robinhoodchain.blockscout.com/tx/0xac3ca1edc496e10b6299bd35dbb45fe66c0578ac680b3aa9e53491828e6ff51b), sell tx `0xe6aa1676...f4195d`, graduation tx [`0xb256fd4e...d2fe78`](https://robinhoodchain.blockscout.com/tx/0xb256fd4ea2d3a2e9e3357355544b65b78e4fe154f13ed499e28c8a322ed2fe78) (shows `PoolGraduated` + v4 `Initialize`, `ModifyLiquidity`, `Swap`); [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/ponsdotfamily-v2/index.ts) |
| 2 | pump.fun | C | Solana program `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`. Anchor events `CreateEvent`, `TradeEvent`, `CompleteEvent`, `CompletePumpAmmMigrationEvent`, `CollectCreatorFeeEvent`... emitted through the `event_authority` self-CPI (so they are inner instructions, not just log text). Graduates to PumpSwap (`pump_amm` program, IDL in the same repo). | IDL: [pump-public-docs/idl/pump.json](https://github.com/pump-fun/pump-public-docs/blob/main/idl/pump.json) |
| 3 | **Flap** | **A** | One `Portal` proxy per chain: BSC `0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0`, Robinhood `0x26605f322f7fF986f381bB9A6e3f5DAb0bEaEb09` (HS yes both), X Layer `0xb30D...8678` (HS no), Monad `0x30e8...8b23`. All launches, trades, progress and graduation (`LaunchedToDEX`) come from that single address. | SRC exact match for the BSC implementation `0xAb8Ec926b6e113c2212aF152b086eA62d1FDced9` ([Sourcify](https://sourcify.dev/server/v2/contract/56/0xAb8Ec926b6e113c2212aF152b086eA62d1FDced9)); topic0 of `TokenBought`/`TokenSold` equal the constants in the [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/flap.ts); LIVE on Robinhood: create tx `0x10718c23...bbd32a`; [addresses doc](https://docs.flap.sh/flap/developers/deployed-contract-addresses) |
| 4 | StonkFun | C | A *platform* configured on Raydium LaunchLab (program `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj`), graduating to Raydium CPMM. Identified by its platform fee wallet `AvVCE7Ue...ffVPz`. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/stonkfun.ts) (reads `raydium_launchpad_evt_tradeevent`) |
| 5 | **Pons V1** | **B** | Robinhood. Factory `PonsLaunchFactory` `0xA5aAb3F0c6EeadF30Ef1D3Eb997108E976351feB` (older `0x0c37...77a4`) emits `TokenDeployed` + `TokenLaunched(... address pool, uint256 dexId ...)`: the token goes straight into a Uniswap-V3-style pool with single-sided liquidity; trades are plain V3 `Swap`s. No new launches in the last 24h; fees still accrue from old pools. | SRC exact match ([Sourcify](https://sourcify.dev/server/v2/contract/4663/0xA5aAb3F0c6EeadF30Ef1D3Eb997108E976351feB)); [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/ponsdotfamily/index.ts) |
| 6 | **o1 Launchpad** | **B** | Robinhood + Base (+Monad). Several "suites" of factory + Uniswap v4 hook + escrow. Factory `ERC20LaunchpadFactory` e.g. `0x8b40fc20c405d47d725c9723d056a1c6f62bbccf` emits `Launched(token, poolId, creator, quote, supply, tickSpacing)`; trading is v4 `Swap` on the PoolManager; the hook adds `Trade(poolId, executor, referrer, feeCurrency, totalFee, comment)`. | SRC exact match (factory); hook events ADAPTER ([events.ts](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/o1-launchpad/events.ts), which cites o1's own repo `o1exchange/o1-launch`). No launch on that factory in my 2.8h sample. |
| 7 | **NOXA Fun** | **B** | Factories on Robinhood `0xD9eC2db5f3D1b236843925949fe5bd8a3836FCcB`, Monad, MegaETH, Merlin (HS yes), Intuition, Stable. **Same `TokenLaunched` signature as Pons V1** (V3 single-sided LP, "no LP migration"). Trades = V3 `Swap`. | ADAPTER ([noxa-fun](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/noxa-fun/index.ts)); factory not on Sourcify; 0 logs in 2.8h sample |
| 8 | BONK.fun / LetsBonk | C | Platform on Raydium LaunchLab; fee wallets `56XVRVAs...`, `9sHpTfmV...`. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/letsbonk/index.ts) |
| 9 | Raydium LaunchLab | C | Program `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj`; events `PoolCreateEvent`, `TradeEvent`, `CreateVestingEvent`, `ClaimVestedEvent`; `migrate_to_amm` / `migrate_to_cpswap` instructions; has `event_authority`. | IDL: [raydium-idl](https://github.com/raydium-io/raydium-idl/blob/master/raydium_launchpad/raydium_launchpad.json) |
| 10 | Binance Alpha | not a launchpad | `FeeCollected(address recipient, address indexed token, uint256 amount)` on a swap-fee contract per chain. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/binance-alpha.ts) |
| 11 | **Bags** | C (Solana) + **A or B** (Robinhood; open point) | Solana: platform over Meteora DBC (tx signer `BAGSB9TpGrZxQbEsrEznv5jXXdwyP6AXerN8aVRiAmcv`) plus its own fee-share program. Robinhood: `BagsFactory` behind proxy `0xe8Cc4431adF8b5A847C113EF0c6af9043219Cb37` emits `TokenCreated(token, curve, creator, feeShare, partner, poolId, name, symbol, metadataURI)`; v4 hook `BagsV4Hook` `0x2380aBf72C17aABAb76480244759AC7E2932EEcC` emits `PoolRegistered(poolId, bondingCurve, feeShare, ...)`, `HookFeeTaken`, `FeesSwept`. A per-token `curve` contract exists; its trade events were not inspected. | SRC exact match (factory impl `0x7dfa...Ef1C`, hook); LIVE: create tx `0xd52b53cf...42a767` (2 creates in 2.8h); [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/launch-on-bags/index.ts) |
| 12 | StonkBrokers | B/D | Robinhood. A mix of tokenised-stock AMM vaults, lockers (V3/V4) and launch fees. Bespoke; not a clean family. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/stonkbrokers/index.ts) |
| 13 | **LetsCash** | **B** | Robinhood. Factory proxy `0x5bd1Fbe78a78fe8236fa00CF48fbEBA74ae34661` emits `TokenLaunched(token, creator, poolId, configId, firstBuyIn, firstBuyOut, hook, feeRecipient)`; trading in Uniswap v4; hooks emit `FeeAccrued(poolId, amount)`. | LIVE: launch tx `0x83a10739...d3c611` (topic0 `0x17091df6...` matches); implementation not on Sourcify; [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/letscash.ts) |
| 14 | Graphite Protocol | C | Revenue share of LetsBonk. Nothing to index. | adapter |
| 15 | Pools (pools.trade) | B | Robinhood. `UERC20Factory`; every token gets a hookless 0.25% Uniswap v4 ETH pool. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/pools-trade.ts) (Allium SQL) |
| 16 | Meteora DBC | C | Program `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN`; events `EvtInitializePool`, `EvtSwap`, `EvtSwap2`, `EvtCurveComplete`, `EvtClaimCreatorTradingFee`...; migrates to Meteora DAMM v1/v2. Hosts Bags, and (per DefiLlama's SQL) other partner platforms by `config`. | IDL: [dynamic-bonding-curve-sdk](https://github.com/MeteoraAg/dynamic-bonding-curve-sdk) |
| 18 | PAIR | B | Robinhood; `PairPoolCreated(projectToken, quoteToken, poolId, positionId, ...)` then v4 trading; `FeesAllocated`. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/pairdotfund.ts) |
| 19 | BaseStonk | B | Base + Robinhood; a series of Uniswap v4 hooks (v2..v6) on the canonical PoolManager `0x4985...2b2b`. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/basestonk.ts) |
| 20 | Sentry | B | Ink + Robinhood; fee-router over Uniswap V2/V3/V4 + launches into 1% V3 pools. DefiLlama reads a Goldsky subgraph. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/sentry/index.ts) |
| 21 | **Clanker** | **B** | Base, Robinhood, Ethereum, Arbitrum, Unichain, Monad. Deploys token + single-sided Uniswap pool (V3 in early versions, v4 + hook now); `TokenCreated` on the factory (signature differs per version v0..v4; not inspected here - DefiLlama attributes by fee wallet only). | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/clanker.ts); signatures UNVERIFIED |
| 22 | **Pez Family** | **A** | Robinhood. **Byte-identical event set to Pons V2** (`TokenLaunched`, `CurveBuy`, `CurveSell`, `PoolGraduated`, `PoolFeesSwept`), own factory `0xed355f423a5158347beb562c250f6095efcdb25b`, own hook. | ADAPTER ([pez-family-v2](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/pez-family-v2/index.ts)) |
| 23 | **four.meme** | **A** | BSC. `TokenManager2` proxy `0x5c952063c7fc8610FFDB798152D69F0B9550762b` (V1 `0xEC45...fBbC`). One address emits `TokenCreate`, `TokenPurchase`, `TokenSale`, `LiquidityAdded` (graduation to PancakeSwap). | LIVE: topic0s match real logs - create tx `0xaa407a08...572460`, buy `0x270b6ec4...73aa4bf`, sell `0x6ec35584...5f65f28`. Implementation not on Sourcify, so parameter *names* are unverified, types are confirmed by hash. |
| 27 | **Nad.fun** | **A** | Monad (HS yes). Bonding curve `0xA7283d07812a02AFB7C09B60f8896bCEA3F90aCE`: `CurveCreate`, `CurveBuy(sender, token, amountIn, amountOut)`, `CurveSell`, `CurveGraduate(token, pool)`. Note: same event *names* as Pons, different signatures. | ADAPTER ([nad-fun](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/nad-fun.ts)) |
| 28-31 | Coinbarrel, token.select, Cook Market, Hookers | B / A | Robinhood. Coinbarrel: V3 then v4 launcher, own `TokenLaunched` variant. token.select: own factory, migrates to a 1% V3 pool. **Cook Market: Pons-V2-style curve with extended signatures** (adds `poolId`, `token`, `progressBps`). Hookers: v4 hook with `SwapFeesAccrued`. | adapters |
| 32 | SunPump | C-ish | Tron. HyperSync lists Tron, but this project has never run against it (address format, no verified event check). DefiLlama reads fee wallets. $88K - park. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/fees/sunpump.ts) |
| 33 | Ignix | A | X Layer (HS **no**). `Trade(token, trader, isBuy, ...)`. | adapter |
| 39 | Alt Fun | A | Hyperliquid (HyperEVM, HS yes). `Buy`/`Sell(token, user, usdc, tokens)`. | adapter |
| - | Virtuals Protocol (DefiLlama files it under "AI Agents": $671K fees, $5.63M curve volume; Robinhood 82% of volume, Base 12%, Arc 6%) | A | Bonding factory events `PreLaunched` / `Launched` (current generation), `PairCreated` (legacy Base); volume from VIRTUAL-token flows through the bonding pairs. Signatures not inspected. | [adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/virtual-protocol.ts); UNVERIFIED |
| - | Zora Coins ($16K), flaunch ($479), Creator.bid ($11K) | B | Base; Uniswap v4 pools with hooks. Too small to matter today. | DefiLlama rows |
| - | Believe ($196), boop ($0), Heaven ($0), Jupiter Studio ($86K), Moonshot Create ($85K), time.fun ($0) | C | Solana, negligible. | DefiLlama rows |

Totals by category (30 days, Launchpad category only):

| Bucket | Fees | Share | Curve volume | Share |
|---|---|---|---|---|
| A on HyperSync chains (Pons V2 family, Flap, four.meme, Nad.fun, Alt Fun, small ones) | ~$163M | ~63.6% | ~$3.05B | ~47.9% |
| B on HyperSync chains (Pons V1, NOXA, o1, LetsCash, Pools, PAIR, BaseStonk, Sentry, Clanker, StonkBrokers, Coinbarrel, token.select, Hookers, Bags-Robinhood pending the open point...) | ~$22.5M | ~8.8% | ~$0.08B reported (true trading is inside Uniswap's numbers) | ~1.3% |
| Binance Alpha (not a launchpad) | $2.1M | 0.8% | - | - |
| **On HyperSync EVM chains, total** | **$187.7M** | **73.18%** | **$3.136B** | **49.27%** |
| EVM chains not on HyperSync | $0.10M | 0.04% | $4.1M | 0.06% |
| Non-EVM (Solana 99.9% of it) | $68.7M | 26.78% | $3.225B | 50.66% |

The A/B split is my classification of the named venues; the HyperSync / non-EVM
totals are exact sums of DefiLlama's per-chain breakdown.

## 4. Solana feasibility

### 4.1 What Envio's Solana HyperSync is (from the crates, docs and repo)

Sources: crates `hypersync-client-solana`, `hypersync-solana-net-types`,
`hypersync-solana-schema` 0.2.0 (tarballs downloaded from crates.io and read),
`docs.envio.dev/docs/HyperSync/solana` and `/solana-query`, repo
`github.com/enviodev/hypersync-client-solana`, all 2026-09-19 00:55-00:57 UTC.

* **Endpoint**: `https://solana.hypersync.xyz` (`POST /query` JSON, `POST /query/arrow`
  Arrow IPC, `GET /height`, SSE head stream, a partial JSON-RPC facade). `GET /height`
  answered 448,251,517 without a token while I was testing. Everything else needs the
  Bearer token - **the same Envio API token** (docs point to
  `envio.dev/app/api-tokens`, the same page as EVM).
* **Tables**: `blocks` (slot, blockhash, parent_slot, parent_blockhash, block_time,
  block_height), `transactions` (signatures, fee_payer, success, err, fee, compute
  units, account keys, version...), **`instruction_calls`** (one row per program
  invocation, outer *and* inner/CPI, with `instruction_address` path, invoked program,
  account list, raw data, and pre-split discriminator columns `d1/d2/d4/d8` and
  account positions `a0..a9`), `logs` (program_id, kind = invoke / success / failed /
  consumed / log / data / other, message), **`account_activity`** (per transaction and
  account: SOL pre/post balance, and for token accounts mint, owner, decimals,
  pre/post token balance), `rewards`.
* **Filters, server-side**: by invoked program, by 1/2/4/8-byte discriminator (8 =
  Anchor), by account at positions 0-9, inner-only or outer-only, successful
  transactions only; transactions by fee payer or signature; logs by program and
  kind; account activity by account, owner, mint, token program. Field selection per
  table like EVM. This is exactly what a launchpad decoder needs: "all inner
  instructions of program `6EF8...` whose data starts with the Anchor event
  discriminator".
* **Reorgs**: responses carry a `rollback_guard` (head window first/last slot and
  block hashes) "mirroring the EVM" one; the client is expected to compare stored
  block hashes and re-sync on mismatch. There is no commitment-level parameter in the
  query type; how far behind the tip the server runs and whether it serves
  processed / confirmed data is not stated in what I read.
* **History depth - the main limitation**: docs say history "starts at the earliest
  slot we have indexed rather than at genesis - mainnet is around slot 403,000,000 as
  of September 2026", extended backwards "prioritized by demand". At ~0.4 s per slot
  (my arithmetic, not Envio's) 45M slots is about 7 months, i.e. roughly February
  2026. pump.fun's 2024-2025 history is not available from this source today.
* **Data-source seams**: the docs mention ranges ingested from SQD (Subsquid), from
  RPC and from Firehose, with different completeness (invoke/success log lines are
  missing on SQD/RPC ranges; per-instruction error and compute units are null on some
  ranges; failed transactions keep no instruction rows under the server's "trim"
  policy; vote transactions are dropped and `transaction_index` is a re-numbered
  dense rank, not the on-chain index).
* **Maturity**: no "beta" or "production" label anywhere I read. Facts: first crates
  2026-05-18, 0.1.0 on 2026-07-26, four release candidates in a week, 0.2.0 on
  2026-08-12; 0.2.0's changelog fixes a bug where streaming "silently dropped the
  tail of any chunk the server truncated... losing up to 99% of rows on dense
  ranges"; rc.4 renamed tables and columns (breaking); the README still says
  `= "0.1"` and promises builders "in the next releases"; repo has 0 stars, ~3,600
  total downloads; Node bindings exist, Python does not; Envio's HyperIndex has a
  Solana mode built on it. License MPL-2.0. **Read: usable, actively developed, API
  declared "locked" a month ago, not yet battle-tested.** Pricing and rate limits
  are not published (usage is metered in "credits"); ask Envio.
* A bundled decoder exists in the client (`decode/anchor_idl.rs`, `borsh_runtime.rs`,
  a Metaplex token-metadata decoder): Anchor IDL in, decoded instruction out. The
  three launchpad IDLs below are public, so decoding is not a research problem.

### 4.2 What Solana would unlock

| Program | Id | Hosts | 30d fees | 30d curve volume |
|---|---|---|---|---|
| pump.fun | `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` | pump.fun (+ mobile app) | $44.66M | $2,529.9M |
| Raydium LaunchLab | `LanMV9sAd7wArD4vJFi2qDdfnVhFxYSUg6eADduJ3uj` | StonkFun, BONK.fun/LetsBonk (+Graphite share), Raydium's own | $11.91M + $3.50M + $1.39M + $3.04M | $242.2M + $148.6M (overlap between the StonkFun and LaunchLab rows not stated by DefiLlama) |
| Meteora DBC | `dbcij3LWUppWqq96dh6gJWwBifmcGfLSB5D4DuSMaqN` | Bags (Solana part $1.51M), Meteora's own row $0.98M, other partner configs (Jupiter Studio, Believe... not verified) | ~$2.5M+ | $300.2M |

Those three programs account for $3,221M of Solana's $3,224M launchpad curve volume
(the rest is Rise.rich $3.4M). All three expose trades as Anchor events through the
`event_authority` self-CPI, i.e. as inner `instruction_calls` rows - decodable with
fixed Borsh layouts, no log-text parsing. pump.fun's `TradeEvent` is richer than any
EVM launchpad event: mint, user, sol/token amounts, is_buy, **virtual and real
reserves after the trade** (price and curve progress for free), protocol fee, creator
and creator fee, cashback, timestamp. `CreateEvent` carries name, symbol, uri, mint,
bonding curve, user, creator, supply and curve parameters. Graduation is
`CompleteEvent` + `CompletePumpAmmMigrationEvent` (pool address, amounts, fee).
Post-graduation trading (PumpSwap $18.2B/30d, Raydium CPMM, Meteora DAMM) would need
Solana DEX decoders too - that is the bigger half of the work and is a different
project from "launchpads".

### 4.3 What it would mean for this project

This project is EVM-shaped on purpose. Concrete frictions:

| Topic | EVM today | Solana | Consequence |
|---|---|---|---|
| Account ids | `FixedString(20)` | 32-byte pubkey | core tables cannot be reused; shared analytics tables need a 32-byte id (left-pad EVM addresses, as `dex_pools.pool_id` already does) |
| Tx id | 32-byte hash | 64-byte signature | `FixedString(64)` or `String` in shared tables |
| Position key | `(chain, block_number, log_index)` | `(slot, transaction_index, instruction_address[])` - a path, not an integer | flatten the path to an ordinal at ingest, or key on `(slot, tx_index, ordinal)`; `transaction_index` is Envio's dense rank, not the chain's |
| Progress unit | block number, every block exists | slot, some slots empty | checkpoints and gap detection must tolerate missing slots |
| Reorgs | parent-hash chain + tombstones/epochs | commitment levels; forks resolve within ~32 slots; `rollback_guard` gives hashes | the `purge_range` primitive can be reused (it is range based), but reorg *detection* needs a Solana variant |
| Token amounts | `UInt256` | u64 | fits |
| Token metadata | ERC-20 `name/symbol/decimals` via RPC | Metaplex metadata account / Token-2022 extensions; decimals come with `account_activity` | a second metadata resolver |
| "Logs" | structured topics + data | free text; structured events are inner instructions | decoders work on `instruction_calls`, not `logs` |
| Volume | ~200-300 logs/s on a busy EVM chain | thousands of tx/s | **never ingest the full chain**; request only the three programs (+ token balance rows for their mints) |
| Native transfers | tx value only (no traces) | `account_activity` gives SOL deltas per account | sniper/funding analytics are actually *easier* on Solana |

Options the lead asked me to weigh:

* **(i) second ingest pipeline, shared analytics tables** - a `src/source/` sibling
  that speaks Solana HyperSync, requests only the launchpad programs, decodes into
  the same `launchpad_*` tables (section 6d), with its own small base tables
  (`sol_blocks` for timestamps/hashes, `sol_instruction_calls` filtered to the
  programs so decoders can be re-run). Reuses ClickHouse writer, migrations,
  checkpoints, derived-table/bucket-repair machinery, metrics.
* **(ii) sister project** - cleaner separation, duplicates all of the above plumbing.
* **(iii) not worth it.**

**My recommendation: (i), second, and scoped.** Not (iii): half of all bonding-curve
volume and the best-known brand (pump.fun) are on Solana, and the owner named three
Solana-centric examples. Not (ii): the expensive, already-built parts of this project
(exactly-once writes, purge/epoch, aggregates, migrations) are chain-neutral, and the
launchpad tables are designed below to be chain-neutral too. But do not make Solana a
"full chain" like the EVM ones - no `sol_transactions` for the whole network, no
generic Solana explorer tables. Preconditions before starting: confirm with Envio
(a) history depth plans, (b) credit pricing for a program-filtered stream, (c) which
commitment level the server serves. If any answer is bad, the fallback source for
history is Dune (decoded `pump_solana.*`, `raydium_solana.raydium_launchpad_*`,
`meteora_solana.dynamic_bonding_curve_*` tables exist - DefiLlama's adapters use
them) for a one-off backfill, with HyperSync for the live tail.

Effort (my estimate, one engineer who knows this codebase): Solana source + slot
checkpointing + rollback handling **M/L (2-3 weeks)**; pump.fun decoder **S**;
LaunchLab and DBC decoders **S each** (platform attribution by config/platform
account adds a little); token metadata resolver **M**; PumpSwap / Raydium CPMM /
Meteora DAMM swap decoders for post-graduation performance **M each**. Total to
"launchpad feeds for Solana, no post-graduation candles": about 4-5 weeks; with
post-graduation trading, 8+ weeks. Compare section 6: the EVM side is about 2 weeks.

Alternative Solana data sources, briefly (pricing not checked for any of them):
Geyser / Yellowstone gRPC (validator plugin stream, offered by Triton, Helius,
QuickNode and others: lowest latency, live only, no history, you filter and decode
everything yourself); Helius / Triton RPC with enhanced history APIs (request-priced,
fine for backfilling specific programs, slow for bulk); Triton "Old Faithful" (full
ledger archive, heavy); SQD (Subsquid) Solana datasets (program-filtered historical
instructions - notably one of Envio's own ingest sources); Dune / Flipside (decoded
SQL tables, excellent for backfill and cross-checks, not a streaming source, API
credit priced); Google BigQuery public Solana dataset (existence and freshness not
verified in this session).

## 5. EVM event families (categories A and B)

Signature status: **SRC** = read in Sourcify-verified source; **LIVE** = hash matched
against real logs this session; **ADAPTER** = DefiLlama adapter only (UNVERIFIED).

| Family | Members (30d fees) | Chains (HS) | 30d fees | Of all launchpads | Of EVM-reachable ($187.7M) | 30d curve vol | Of all vol |
|---|---|---|---|---|---|---|---|
| **Pons V2 curve** | Pons V2 128.16M, Pez Family 0.32M; extended-signature variants: Cook Market 0.11M, Wonk Fun 6K (Arc), Solonpad and Surged (Arc, dust) | Robinhood, Arc (yes) | $128.6M | 50.1% | 68.5% | $2,236M | 35.1% |
| **Flap Portal** | Flap | BSC, Robinhood, Monad (yes); X Layer (no) | $33.8M | 13.2% | 18.0% | $698.5M on HS chains | 11.0% |
| NOXA-style "launch into V3" (B) | Pons V1 7.25M, NOXA 3.82M (+Coinbarrel 0.14M, ARK Launch on Arc: own variants) | Robinhood, Monad, MegaETH, Merlin, Arc (yes) | $11.2M | 4.4% | 6.0% | in Uniswap V3 volume | - |
| Uniswap-v4-hook launchers (B) | o1 4.59M, StonkBrokers 1.69M, LetsCash 1.45M, Pools 1.21M, PAIR 0.59M, BaseStonk 0.45M, Sentry 0.35M (V3), Clanker 0.33M, Bags-Robinhood 0.23M, token.select 0.12M, Hookers 0.08M, Zora 0.02M | Robinhood, Base, Ink, Ethereum (yes) | $11.1M | 4.3% | 5.9% | in Uniswap v4 volume | - |
| four.meme TokenManager | four.meme | BSC (yes) | $0.24M | 0.09% | 0.13% | $95.4M | 1.5% |
| Nad.fun | Nad.fun V1 | Monad (yes) | $0.14M | 0.06% | 0.08% | $6.5M | 0.1% |
| Virtuals bonding (outside the Launchpad category) | Virtuals | Robinhood, Base, Arc (yes) | $0.67M | - | - | $5.6M | - |
| Long tail, one ABI each | Alt Fun, Frontier, Ignix (X Layer), Arrowpad, RobinFun, bot.fun, Bubble Curve, Arena Launch, basedbid, hookos, priority.trade... | many | <$0.4M combined | <0.2% | | <$15M | |

Unlike perps, there **is** a fork effect here, but it is local and recent: the
Robinhood Chain boom produced copies of Pons (Pez is byte-identical; Cook, Wonk,
Solonpad, Surged are the same design with added fields, so different topic0s), and
Pons V1 itself shares its launch event with NOXA. Outside that cluster almost every
venue invented its own event names. "pump.fun clone on EVM" is a product description,
not an ABI: Flap, four.meme, Nad.fun, Alt Fun, Frontier and Ignix all do the same
thing with six different signatures. DefiLlama's `forkedFrom` metadata is nearly empty
for this category (4 of 285 entries).

### 5.1 Pons V2 curve family - category A

* Addresses (Robinhood Chain): factory `0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e`
  (deployed block 26,841,846; DefiLlama start 2026-08-03), v4 hook
  `0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044`, Uniswap v4 PoolManager
  `0x8366a39CC670B4001A1121B8F6A443A643e40951`, PositionManager
  `0x58daec3116aae6D93017bAAea7749052E8a04fA7`. Pez factory
  `0xed355f423a5158347beb562c250f6095efcdb25b`. One curve contract per token.
* Events (**SRC + LIVE**, compiler 0.8.35):
  * `TokenLaunched(address indexed token, address indexed curve, address indexed deployer, address pairToken, uint256 launchConfigId, uint256 graduationThreshold)` - topic0 `0x8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607`
  * `CurveBuy(address indexed buyer, address indexed recipient, uint256 quoteIn, uint256 tokensOut, uint256 fee, uint256 tax)` - `0xec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455` (emitted by the curve; parameter names from the Pez adapter, types confirmed by hash)
  * `CurveSell(address indexed seller, address indexed recipient, uint256 tokensIn, uint256 quoteOut, uint256 fee, uint256 tax)` - `0x8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df`
  * `PoolGraduated(address indexed token, uint256 positionId, uint256 tokenAmount, uint256 pairTokenAmount)` - `0x0a44ef75df69c534f43cd6c1aa3ef8983065fe5fe79ef9e79f6494e6f258c259`
  * Hook: `PoolRegistered(bytes32 indexed poolId, address memecoin, address quoteToken, address creator)`, `PoolFeesSwept(bytes32 indexed poolId, uint256 protocolAmount, uint256 buybackAmount, uint256 creatorAmount, uint256 tokensLocked)` (`0x2f3c4357...ea9b30`), `HookFeeCollected(bytes32 indexed poolId, address currency, uint256 feeAmount, uint256 taxAmount)`.
  * Factory extras: `GraduationTokensPermanentlyLocked(token, amount)`, `CreatorFeeRecipientUpdated(token, previous, new)`, `BuybackEnabledUpdated`, `LaunchSwept`, `LaunchGraduationRescued`, `PairTokenEconomicsUpdated(pairToken, phantomQuote, graduationThreshold, decimals)`, `SnipeTaxSecondsUpdated`, `SnipeTaxStartBpsUpdated`, `MaxCreatorTaxUpdated`.
  * Cook Market / Wonk variants (**ADAPTER**): `CurveBuy(..., uint256 tax, bytes32 indexed poolId, address token, uint16 progressBps)` and a `TokenLaunched` that also carries `poolId`, band token ids, `name`, `symbol`. Different topic0s - a second signature generation in the same decoder.
* Normalised fields: launch = token, curve, creator (`deployer`), quote token,
  graduation threshold, config id, launch tx; name/symbol/supply come from the ERC-20
  itself (fixed 1B supply per press coverage - confirm via `totalSupply`). Trade =
  curve -> token (join on `TokenLaunched.curve`), trader = **`recipient`**, side,
  token amount, quote amount, price = quote/tokens, fee, creator tax. Progress =
  running sum of net quote in the curve vs `graduationThreshold` (the sample token
  graduated with 4.2 ETH paired against 204.08M tokens). Graduation = token, v4
  position id, amounts; pool id from the v4 `Initialize` in the same transaction
  (currency0 = native ETH `0x0`, currency1 = token, hook = the Pons hook); LP is
  locked (position NFT minted to a locker contract `0x2674...4952` in the sample).
  Creator fees = `PoolFeesSwept.creatorAmount` per pool after graduation; on the
  curve the creator share is a policy read (`getLaunchFeePolicy(token)`), not an
  event - DefiLlama reads it by RPC.
* Traps seen in the sample transactions: (1) launch-with-initial-buy goes through a
  forwarder (`tx.to = 0xe33e9e47...`), so `CurveBuy.buyer` is the forwarder and the
  real buyer is `recipient`; the `deployer` topic was the real creator in my sample
  but a whitelisted-launcher path exists (`WhitelistedLauncherUpdated`), so creator
  may be a front end for some launches. (2) Quote is native ETH: there is no ERC-20
  transfer for the quote leg, the event is the only price source. (3) `tax` is a
  per-token creator tax and an anti-snipe tax that decays within seconds of launch -
  effective price differs from curve price. (4) A graduation happens *inside* a user's
  buy (router `0x7eea5f60...`), followed by a v4 `Swap` in the same tx. (5) Curve
  events have no token in them - an unknown curve address (partial sync) needs a
  `token()` RPC read, same pattern as `dex_pools` `source = 'rpc'`. (6) 0.1-second
  blocks and ~7 curve logs per second at the moment; trivial for ClickHouse, but
  "same block as launch" is too narrow a sniper definition on this chain.
* Decode by topic0 with no address filter and every byte-identical fork works on day
  one, which is this project's principle. Effort: **S**.

### 5.2 Flap Portal - category A

* One `Portal` (TransparentUpgradeableProxy) per chain, addresses in section 3.
  BSC since 2024-06-27, X Layer 2025-08-18, Monad 2025-10-30, Robinhood 2026-07-08.
* Events (**SRC**, implementation `0xAb8Ec926...ced9` on BSC, compiler 0.8.26;
  Robinhood implementation `0xF9209bDB...bd99` is a partial match on Sourcify; **no
  indexed parameters anywhere**):
  * `TokenCreated(uint256 ts, address creator, uint256 nonce, address token, string name, string symbol, string meta)` - `0x504e7f360b2e5fe33cbaaae4c593bc55305328341bf79009e43e0e3b7f699603`
  * `TokenBought(uint256 ts, address token, address buyer, uint256 amount, uint256 eth, uint256 fee, uint256 postPrice)` - `0xa800a2038683844fac66747f771bfdfae862eb28b16bcfa387afa9fbacce8ff7`
  * `TokenSold(uint256 ts, address token, address seller, uint256 amount, uint256 eth, uint256 fee, uint256 postPrice)` - `0x03a4693e592f5e75dc7c136acb39b146d2b4966c0e509c34f362dee02b3b861a`
  * `LaunchedToDEX(address token, address pool, uint256 amount, uint256 eth)` - `0x6e4f47630b8745b8cacbd44f42a8a33e7eea7cc08ef22fc7630f4f385784ff7d`
  * `FlapTokenProgressChanged(address token, uint256 newProgress)`, `FlapTokenCirculatingSupplyChanged`, `TokenQuoteSet(address token, address quoteToken)` (quote is not always the native coin: "eth" means the quote token's units), `TokenCurveSet` / `TokenCurveSetV2(token, r, h, k)` (curve parameters), `TokenDexSupplyThreshSet`, `TokenDexPreferenceSet(token, dexId, lpFeeProfile)`, `TokenMigratorSet`, `FlapTokenTaxSet` / `FlapTokenAsymmetricTaxSet(token, buyTax, sellTax)`, `TaxOnBondingCurvePaid`, `FlapTokenCLPoolCreated(token, poolId, sqrtPriceX96)`, `TokenPoolInfoUpdated`, `VanityTokenCreated(token, creator, beneficiary)`, `TokenVersionSet`, `FlapTokenStaged`, `TokenRedeemed`.
* Normalised fields: the most complete of any EVM family - name, symbol and metadata
  string in the launch event, price after each trade (`postPrice`), progress as its
  own event, curve parameters, graduation target DEX and pool. Creator = `creator`
  (check `VanityTokenCreated.beneficiary` for launches through a front end).
* Traps: tax tokens are first class here (buy/sell tax, dividends, "tax processor"
  dispatches) - post-graduation DEX amounts are fee-on-transfer; several quote tokens
  per chain; seven events per launch (the create tx in my sample emitted 7 config
  logs) so the launch row should be assembled per transaction; graduation can go to
  PancakeSwap V2/V3-style or a CL pool depending on `dexId` - join to `dex_pools` by
  pool address. The upgradeable proxy means signatures can change; the verified
  implementation is the reference.
* Effort: **S** (one address per chain, flat events).

### 5.3 "Launch straight into a DEX pool" - category B (cheap win)

Trades, candles, liquidity and volume for these tokens are **already produced by the
existing `uniswap_v3` / `uniswap_v4` decoders**. What is missing is one row per
launch saying "token T, pool P, creator C, launched via venue V", so the UI can show
a new-launch feed and group DEX activity by launchpad. Join key: v4 `poolId`
(`bytes32`, equals `dex_pools.pool_id`) or V3 pool address.

| Venue | Launch event (status) | Join key in event |
|---|---|---|
| Pons V1, NOXA | `TokenLaunched(address indexed token, address indexed deployer, address indexed dexFactory, address pairToken, address pool, uint256 dexId, uint256 launchConfigId, uint256 positionId, uint256 restrictionsEndBlock, uint256 initialBuyAmount)` - topic0 `0xdb51ea9ad51ab453a65a4cb7e60c3cb378c9501bb002609f8f97778fb6c4235a` (SRC for Pons V1; NOXA ADAPTER, same text) | `pool` address |
| o1 | `Launched(address indexed token, bytes32 indexed poolId, address indexed creator, address quote, uint256 supply, int24 tickSpacing)` - `0x207384e8...0eaf9c` (SRC); `LaunchBuyExecuted(...)` for the dev buy (ADAPTER) | `poolId` |
| LetsCash | `TokenLaunched(address indexed token, address indexed creator, bytes32 indexed poolId, uint256 configId, uint256 firstBuyIn, uint256 firstBuyOut, address hook, address feeRecipient)` - `0x17091df6...608897` (LIVE) | `poolId` |
| Bags (Robinhood) | `TokenCreated(address indexed token, address indexed curve, address indexed creator, address feeShare, address partner, bytes32 poolId, string name, string symbol, string metadataURI)` - `0x643b3b60...e4cced` (SRC + LIVE) | `poolId` (+ a `curve`, see open points) |
| PAIR | `PairPoolCreated(address indexed projectToken, address indexed quoteToken, bytes32 indexed poolId, ...)` (ADAPTER) | `poolId` |
| Coinbarrel | `TokenLaunched(address indexed token, address indexed creator, address pool, uint256 positionId, bool isToken0, uint256 restrictionEndBlock, uint256 devBuyAmount)` (ADAPTER) | `pool` |
| Clanker v0..v4, Zora, flaunch, Pools, BaseStonk, Hookers, token.select, Sentry | not inspected / UNVERIFIED | pool or poolId |

Traps: v4 hooks take fees outside the pool's `fee` field (hook fee events such as
`HookFeeTaken`, `FeeAccrued`, `SwapFeesAccrued`, `PoolFeesSwept`), so DEX-derived
"fees" understate what traders paid; single-sided initial liquidity means the first
swaps move price violently (candle outliers); "restrictions" (max wallet / max tx for
the first N blocks) are enforced in the token, not visible as events; the dev buy is
inside the launch tx and usually attributed to the factory as `sender`.
Effort: **S** for the first (a generic "launch attribution" decoder with a signature
table), then a few hours per extra venue.

### 5.4 four.meme, Nad.fun and the tail

* **four.meme** (BSC, **LIVE**): `TokenCreate(address,address,uint256,string,string,uint256,uint256,uint256)` `0x396d5e90...0cad20` (believed `creator, token, requestId, name, symbol, totalSupply, launchTime, launchFee`), `TokenPurchase(address,address,uint256,uint256,uint256,uint256,uint256,uint256)` `0x7db52723...e62942` and `TokenSale(...)` `0x0a5575b3...1bae19` (believed `token, account, price, amount, cost, fee, offers, funds`), `LiquidityAdded(address,uint256,address,uint256)` `0xc18aa711...3c44b0` (believed `base, offers, quote, funds`). Names are from memory and UNVERIFIED; the type lists are confirmed by hash against live logs. Two more per-trade events on the same contract (topic0 `0x48063b12...`, `0x741ffc46...`, 32 bytes of data) were not identified. No indexed parameters. V1 manager `0xEC4549ca...fBbC` uses different, older signatures (`etheramount` fields per DefiLlama's SQL). Volume has halved month on month; historically it was the largest BSC launchpad, so it has backfill value. **S**.
* **Nad.fun** (Monad, ADAPTER): `CurveCreate(address indexed creator, address indexed token, address indexed pool, string name, string symbol, string tokenURI, uint256 virtualMon, uint256 virtualToken, uint256 targetTokenAmount)`, `CurveBuy(address indexed sender, address indexed token, uint256 amountIn, uint256 amountOut)`, `CurveSell(...)`, `CurveGraduate(address indexed token, address indexed pool)`; a newer generation uses `Buy(token, buyer, quoteIn, tokenOut)` / `Sell` / `Graduate(token, pair)`. **S**, $6.5M volume.
* **Virtuals**: not inspected beyond the adapter's description. Worth a look only
  because 82% of its curve volume is now on Robinhood Chain. **S/M**, UNVERIFIED.
* Everything else: below $10M/month combined. Skip until one of them grows.

### 5.5 Rug / sniper analytics: what is computable from events + ERC-20 transfers

| Metric | Computable? | From |
|---|---|---|
| Creator history (tokens launched, how many graduated, how many died in N minutes) | yes | launch + graduation events |
| Dev buy at launch, dev current holdings, dev sold everything | yes | curve trades where `recipient == creator` + `erc20_transfers_by_account` |
| Buys in the first N seconds / blocks, share of supply they took | yes | curve trades ordered by `(block, log_index)`; Pons' anti-snipe tax parameters are events |
| Top-holder concentration at graduation and now | yes | balance fold over `erc20_transfers` (curve/pool/locker addresses excluded via the launch rows) |
| Post-graduation performance, volume, liquidity | yes, already | existing DEX candles joined on pool id |
| LP locked or burned | yes | graduation tx: position NFT recipient / `GraduationTokensPermanentlyLocked` / Flap `LaunchedToDEX` |
| Creator fee income | mostly | `PoolFeesSwept`, `FeesSwept`, `FeesSplit`, Flap tax events; Pons curve-phase creator share needs a policy read |
| Bundled wallets (many buyers funded by one wallet) | partly | funding via plain ETH transfers is in `transactions` (top-level value); funding through a disperser contract is an internal transfer and **invisible without traces** |
| Wash trading between related wallets | heuristics only | same limitation |
| Token image, description, socials | no | off chain (IPFS / venue API); Flap puts a `meta` string and Bags a `metadataURI` in the launch event, Pons does not |
| Honeypot / transfer restrictions | no (needs simulation) | token bytecode |

## 6. Recommendation

### (a) How much is reachable

**73.2% of 30-day launchpad fees ($187.7M of $256.5M), 49.3% of bonding-curve volume
($3.14B of $6.36B) and 45.2% of protocol revenue are on EVM chains HyperSync serves
today.** The right way to say it to a user: *about half of launchpad trading, and the
fastest-growing half, is within reach of this indexer with two small decoders; the
other half is three Solana programs.* This is the opposite of the perps finding
(3.9%). Two cautions: the EVM share rests on one chain's ten-week-old boom with a gas
subsidy that ends this month (in the previous 30 days the whole category was $82.0M
and the EVM share of fees about 49% - my estimate, applying each venue's current
chain split to its previous-period fees - and before July 2026 Robinhood Chain
launchpads did not exist); and if
post-graduation trading is counted, Solana is larger (PumpSwap alone is $18.2B/30d;
Robinhood Chain's whole DEX volume was reported as $17.75B/30d on 2026-09-01).

### (b) EVM shortlist, in build order

| Order | What | 30d fees covered | Cumulative share of EVM-reachable | Reuse | Effort | Why here |
|---|---|---|---|---|---|---|
| 1 | Pons V2 curve family (5 core events + hook fee events; second signature set for Cook/Wonk) | $128.6M | 68.5% | every byte-identical fork on any chain | S | Half the category. Verified source, confirmed live. Graduates into v4 pools we already decode. |
| 2 | Flap Portal | $33.8M | 86.5% | same ABI on BSC, Robinhood, Monad (and X Layer if ever served) | S | Second largest, two years of BSC history, richest events (progress, postPrice, metadata). |
| 3 | Generic "launch attribution" decoder for direct-to-DEX venues: Pons V1/NOXA, o1, LetsCash, Bags-RH first | ~$17M of the $22.3M in B | ~95% | one table, a signature list; trading already decoded | S, then hours per venue | Cheapest win per line of code; turns existing DEX data into launchpad data. |
| 4 | four.meme | $0.24M (vol $95M) | ~96% | BSC history back to 2024 | S | Small now, historically large; cheap. |
| 5 | Clanker (all versions), Virtuals, Nad.fun | ~$1.1M | ~97% | Base/Farcaster ecosystem; Virtuals on 3 chains | S/M each | Brand names the owner's users will expect, low volume today. Needs signature work first. |
| - | Front-end attribution registry (GMGN, Axiom, fomo, Terminal, Maestro...) | n/a (overlaps) | - | all chains | S | Optional; answers "which app do traders use", not "what happened". |

Steps 1-3 are roughly two weeks of work including tables, aggregates and tests, and
cover ~95% of what is reachable. Because raw logs are stored, all of it can be applied
retroactively to anything already synced.

What a trader/analyst UI gets:

* **New-launch feed** per chain and venue, with name/symbol, creator, dev buy, quote
  token, and creator track record next to it.
* **Curve progress** (percent to graduation, live price, buys vs sells, unique
  buyers) and a "close to graduating" list.
* **Graduation feed** with destination pool, liquidity, LP lock status - each row
  links straight into the existing DEX candles for post-graduation performance.
* **Creator pages / serial ruggers**: launches, graduation rate, average lifetime,
  fees earned, dev sell timing.
* **Sniper view**: first-seconds buyers, share of supply taken, how fast they sold,
  repeat snipers across launches; holder concentration at graduation.
* **Venue league table** computed from our own logs (launches, graduation rate,
  volume, fees, unique traders per day) - auditable, unlike DefiLlama's mix of Dune,
  Allium, subgraph and self-reported sources.

### (c) Solana position

Data shape: program-filtered inner instructions carrying Anchor events, plus token
balance deltas; slots not blocks; signatures not hashes. Unlocks pump.fun, Raydium
LaunchLab (StonkFun, LetsBonk) and Meteora DBC (Bags) = 26.8% of fees, 50.7% of
curve volume, 54.7% of revenue, and the owner's three named examples. Envio's Solana
HyperSync fits technically and uses the same token, but has ~7 months of history and
one month of API stability. **Recommendation: option (i), after the EVM work,
program-filtered only, with Dune as the backfill fallback; ask Envio the three
questions in 4.3 before committing.** Estimate 4-5 weeks for launch/curve/graduation
feeds, 8+ weeks if post-graduation Solana DEX trading is included.

### (d) Venue-agnostic table sketch

Conventions follow the design document: raw integer amounts (`UInt256`), decimals
applied in views, unknown = `NULL`, `ReplacingMergeTree(_version, is_deleted)` +
`epoch`, positional sorting keys, `PARTITION BY toYYYYMM(timestamp)`, `protocol` =
who, `family` = which decoder. To stay chain-neutral, identities are
**`FixedString(32)`** (EVM address left-padded, exactly like `dex_pools.pool_id`;
a Solana pubkey fits natively), `tx_id` is `String` (32-byte hash or 64-byte
signature), and the position key is `(chain, block_number, tx_index, ordinal)` where
`block_number` = slot on Solana and `ordinal` = `log_index` on EVM or the flattened
instruction path on Solana.

**`launchpad_tokens`** - one row per launch; key `(chain, block_number, tx_index, ordinal)`; side table by `(chain, token)`
`family`, `protocol`, `emitter`, `token`, `curve` (Nullable: per-token curve contract / bonding-curve account), `creator`, `tx_from`, `quote_token` (zero = native), `name`, `symbol`, `metadata_uri` (Nullable), `total_supply` (Nullable), `graduation_threshold` (Nullable), `curve_params` (String/JSON, Nullable), `initial_buy_quote` (Nullable), `dex_pool_id` (Nullable - set at launch for category B), `launch_kind` (`curve` | `direct_pool`), `source` (`log` | `instruction`).

**`launchpad_trades`** - one row per curve trade (category A only; category B trades stay in `dex_swaps`)
`family`, `protocol`, `emitter`, `token`, `trader` (the beneficiary, not the router), `payer` (Nullable: event `buyer/sender` when different), `tx_from`, `side`, `token_amount`, `quote_amount`, `quote_token`, `fee` (protocol + platform), `creator_fee` (Nullable), `tax` (Nullable), `price_after` (Nullable), `reserve_quote_after` / `reserve_token_after` (Nullable), `progress_bps` (Nullable).

**`launchpad_graduations`**
`family`, `protocol`, `token`, `curve`, `dex_protocol`, `dex_pool_id` (join to `dex_pools`), `token_amount`, `quote_amount`, `lp_recipient` (Nullable), `lp_locked` (Nullable UInt8), `migration_fee` (Nullable), `trigger_tx_from`.

**`launchpad_creator_fees`** - fee accruals and claims
`family`, `protocol`, `token` (Nullable when only a pool id is known), `dex_pool_id` (Nullable), `recipient`, `currency`, `amount`, `kind` (`creator` | `protocol` | `buyback` | `partner` | `holders`), `phase` (`curve` | `dex`).

Aggregates (as `DerivedTable`s): per-token curve candles 1m/1h; per-venue daily
launches, graduations, volume, fees, unique traders/creators; per-creator lifetime
stats. Views: `launchpad_tokens_v` (latest state: progress, graduated?, pool, holders
from the ERC-20 fold), `launchpad_first_buyers_v`.

Column-fill matrix:

| Column | Pons V2 family | Flap | four.meme | Nad.fun | Category B (direct pool) | pump.fun | Raydium LaunchLab | Meteora DBC |
|---|---|---|---|---|---|---|---|---|
| token, creator | yes | yes | yes | yes | yes | yes | yes (creator; mint in `base_mint_param`) | yes |
| name / symbol | no (ERC-20 read) | yes + `meta` | yes | yes + tokenURI | Bags yes; others ERC-20 read | yes + uri | yes (mint params) | via metadata account |
| quote token | yes | via `TokenQuoteSet` | via tx context (UNVERIFIED) | native MON | yes | yes (`quote_mint`) | via pool config | via config |
| curve params / threshold | threshold | r, h, k + supply threshold | not in event | virtual reserves + target | n/a | virtual reserves + supply | curve_param | via config account |
| trade: trader | `recipient` | `buyer` / `seller` | `account` | `sender` (may be router) | in `dex_swaps` (router problem applies) | `user` | not in event - from instruction accounts | not in event - from instruction accounts |
| trade: amounts, side | yes | yes | yes | yes | `dex_swaps` | yes | yes | yes |
| fee / creator fee / tax | fee + tax | fee; taxes as separate events | fee | no | hook fee events per venue | fee, creator_fee, cashback, buyback | protocol, platform, creator, share fee | in `swap_result` |
| price after trade | derive | `postPrice` | `price` | derive | sqrtPriceX96 | from reserves | from reserves | from `swap_result` |
| reserves / progress | running sum vs threshold (Cook/Wonk: `progressBps`) | `FlapTokenProgressChanged` | `offers` / `funds` fields | derive | n/a | reserves in every trade | reserves in every trade | `quote_reserve_amount` + `migration_threshold` in `EvtSwap2` |
| graduation: pool | v4 `Initialize` in same tx | `pool` in event | PancakeSwap pair via tx | `pool` in event | n/a (born in pool) | `pool` in migration event | migrate instruction accounts | migrate instruction accounts |
| LP lock info | locker + permanently-locked event | not inspected | not inspected | LpManager events | locker / position id in event | burned by program (not verified) | not inspected | lock instructions exist |
| creator fee claims | `PoolFeesSwept` | tax / dividend events | not inspected | `Distributed` | `FeesSwept`, `FeesSplit`, `FeeAccrued`... | `CollectCreatorFeeEvent` | `claim_creator_fee` instruction | `EvtClaimCreatorTradingFee` |

General traps to write down before any code:

1. **Trader is not `tx.from` and often not the event's `buyer`**: forwarders,
   routers, bots and front ends sit in between. Prefer the beneficiary field; keep
   `tx_from` and `payer` as separate columns.
2. **Creator is not `tx.from`** for launches made through front ends or whitelisted
   launchers (Flap `VanityTokenCreated.beneficiary`, Pons whitelisted launchers, Bags
   `feeShare`/`partner`, Clanker deploying on behalf of Farcaster users).
3. Native-coin quote legs have no ERC-20 transfer; the curve event is the only price.
4. Tax / fee-on-transfer tokens are normal in this category (Flap, Pons `tax`, v4
   hook fees): received amounts differ from event amounts, DEX fee fields understate.
5. One curve contract per token (Pons, Bags) means decode by topic0 without an address
   filter and resolve unknown curves by RPC in the background, never inline.
6. Launches are spammy by design (13,658 a day on one venue, ~1% graduate): the feed
   needs server-side filters (min buyers, min quote raised), and token metadata
   resolution must not be triggered for every launch.
7. Venues rise and die within weeks. Keep decoders table-driven (`family`, topic0,
   signature generation) and re-pull the ranking before each build.
8. Volumes in this category are inflated by bots and self-trading; log-derived
   numbers are auditable but not "organic".

## 7. Appendix

### Endpoints and retrieval times (UTC)

| When | What | Result |
|---|---|---|
| 2026-09-19 00:50:17-20 | `https://api.llama.fi/overview/fees?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` (and `&dataType=dailyRevenue`) | 200. 166 entries with `category == "Launchpad"`, 119 with 30d fees > 0. `breakdown30d` gives per-chain values (free). |
| same | `https://api.llama.fi/overview/dexs?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` | 200 (not paywalled, unlike `/overview/derivatives`). 87 Launchpad entries, 59 with volume. |
| same | `https://api.llama.fi/protocols` | 200, 8.9 MB. 285 Launchpad entries; `forkedFromIds` populated for only 4. |
| 00:51:05 | `https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks` | 200. Listed: Robinhood (4663), BSC, Base, Monad, MegaETH, Ink, Arc (5042), Merlin, Hyperliquid (999), Tron, Ethereum, Arbitrum, Unichain, Plasma, Sonic... "Access on request": Stable, XDC, Zircuit. **Not listed**: X Layer, GateLayer, Eden, Shido, Intuition. Solana is a separate product/page. |
| ~00:51 | `github.com/DefiLlama/dimension-adapters` at commit `4c38bc204cce1efe84a4f6ef730528b476c7c0ff` (2026-09-18) | Adapters read for addresses, event ABIs and methodology (paths linked in section 3). Side note: DefiLlama's Pons and Flap adapters query a ClickHouse table `evm_indexer.logs`. |
| 00:53 | `robinhoodchain.blockscout.com` (API and UI) | Cloudflare bot challenge from curl and from the browser pane; not bypassed. Explorer links in this note were therefore not opened. |
| 00:54-00:58 | `https://sourcify.dev/server/v2/contract/{chainId}/{address}?fields=abi,compilation,proxyResolution` | Verified ABIs: Pons V2 factory and hook, Pons V1 factory, o1 factory, Bags factory impl and hook (chain 4663); Flap Portal impl (chain 56). Not on Sourcify: NOXA factory, o1 hook, LetsCash impl, four.meme impl and V1. |
| 00:54-01:01 | Public RPCs `https://rpc.mainnet.chain.robinhood.com` (rate-limited, 429s), `https://robinhood-rpc.publicnode.com`, `https://bsc-rpc.publicnode.com` (from `chainid.network/chains.json`) | `eth_getLogs` samples, receipts of three Pons transactions, 24.18h launch/graduation counts (blocks 65,808,534-66,672,534; ~0.1 s blocks). topic0s computed locally with a self-written keccak-256 (self-tested against the ERC-20 `Transfer` hash). BSC public RPCs refused ranges above a few hundred blocks; one alternative RPC returned empty results and was discarded. |
| 00:55:37 | `https://crates.io/api/v1/crates/{hypersync-client-solana,hypersync-solana-schema,hypersync-solana-net-types}` and `/0.2.0/download` | Metadata + tarballs, unpacked in the session scratch directory (not in the repo). |
| 00:56 | `docs.envio.dev/docs/HyperSync/solana`, `/solana-query`, `/solana-client`, `/api-tokens`, `docs/HyperIndex/solana/evm-vs-solana`; `raw.githubusercontent.com/enviodev/hypersync-client-solana/main/CHANGELOG.md`; `https://solana.hypersync.xyz/height` | Section 4.1. |
| 00:57 | IDLs: `pump-fun/pump-public-docs/idl/pump.json`, `raydium-io/raydium-idl/.../raydium_launchpad.json`, `MeteoraAg/dynamic-bonding-curve-sdk/.../idl.json` | Program ids, event lists, `event_authority` presence. |
| ~01:05 | Web search + fetch: The Defiant 2026-09-01, CryptoTimes 2026-09-01 (gas waiver), search snippets of CoinDesk 2026-09-03 and fomo's own sites (`fomo.family`, `tryfomo.org`) | Second-source context. Read through a summarising fetch tool, not verbatim (it mis-dated The Defiant piece as 2024; the content is 2026). |

### Could not verify / open points

* **Tokens launched** for pump.fun, LaunchLab, DBC, Flap-BSC and four.meme (24h/7d/30d):
  no fetched source. DefiLlama has no such metric; Dune dashboards were not opened.
  Only Pons V2/V1 and Flap-Robinhood were counted from chain. 7d/30d launch counts
  were not computed for any venue.
* **Robinhood Chain gas waiver** ("90 days from 2026-07-01") comes from one press
  article; not confirmed with Robinhood's own documentation.
* **Overlaps in DefiLlama's Solana rows**: whether LaunchLab's row contains StonkFun
  and BONK.fun activity, and whether Meteora DBC's row contains Bags', is not stated.
  Graphite is a share of LetsBonk. I did not de-duplicate; Solana's category total
  may be somewhat overstated, which would push the EVM share *up*.
* **Pons fee comparability**: Pons fees include post-graduation v4 fees and creator
  taxes; pump.fun's exclude PumpSwap. No like-for-like fee split was computed.
* **Pons curve-phase creator fee split** is a contract read (`getLaunchFeePolicy`),
  not an event. `CurveBuy`/`CurveSell` parameter names come from adapters (types
  confirmed by hash); the curve contract's own verified source was not fetched.
* **Bags on Robinhood**: `TokenCreated` has a `curve` and the hook registers a
  `bondingCurve` - so it may be category A with its own curve trade events. The curve
  contract was not inspected. Settled by reading one curve's ABI on Sourcify.
* **four.meme**: parameter names unverified; two unidentified per-trade topic0s;
  V1 signatures not checked; implementation not on Sourcify (BscScan not reachable).
* **NOXA, LetsCash implementation, o1 hook, Pez, Cook, Wonk, Nad.fun, Clanker (all
  versions), Virtuals, Zora, PAIR, Coinbarrel**: signatures are ADAPTER-level or not
  inspected. Whether NOXA or Pons V1 is the origin of the shared `TokenLaunched`
  signature is unknown.
* **Flap on Robinhood**: implementation is a partial (not exact) Sourcify match; I
  assumed the BSC ABI applies (topic0s observed live on Robinhood were consistent with
  it: `TokenQuoteSet`, `TokenVersionSet`, `FlapTokenTaxSet` hashes matched).
* **"Hyperliquid L1" launchpads** (Alt Fun, LiquidLaunch): assumed HyperEVM from their
  Solidity events; not checked. 0.03% of fees either way.
* **Tron / SunPump**: HyperSync lists Tron; nothing about it was tested.
* **fomo**: chain list is from its own marketing pages; the claim that it has no
  contracts is inferred from DefiLlama's adapter (fee wallet + Relay), not from an
  audit of its transactions. Its EVM-side fee figures are self-reported to Dune.
* **Solana HyperSync**: pricing, rate limits, served commitment level, lag behind tip
  and backfill schedule are not documented where I looked. History start (~slot 403M)
  converted to "about seven months" with my own 0.4 s/slot assumption. I did not run
  a query (no token used in this research).
* **BigQuery public Solana dataset**, SQD, Old Faithful, Helius/Triton pricing: not
  checked in this session; listed as options only.
* **Front-end volume vs venue volume**: GMGN's $4.15B on Robinhood Chain exceeds all
  Robinhood launchpad curve volume ($2.30B) because it includes post-graduation DEX
  trading; the split was not computed.
* No explorer page was opened for any transaction hash quoted (bot protection); all
  hashes come from RPC responses in this session.
