# Token launchpads (docs/design.md §11)

Decode by event family from `logs`, never by address registry. `family` = which
decoder, never a brand. Everything below marked **LIVE** was checked against real
chain data on 2026-09-18/19 (public RPCs + Sourcify); **SRC** = read in
Sourcify-verified source; every `topic0` is recomputed from its canonical
signature by a unit test (`events.rs`).

## 1. Evidence (Phase 1)

Endpoints used: Robinhood Chain (chain id **4663**, confirmed by `eth_chainId`
= `0x1237`) `https://robinhood-rpc.publicnode.com`,
`https://rpc.mainnet.chain.robinhood.com`; BNB Chain
`https://bsc-rpc.publicnode.com` (+ `bsc-dataseed.bnbchain.org`, `bsc.drpc.org`);
Base `https://base-rpc.publicnode.com`, `https://mainnet.base.org`; Monad
`https://rpc.monad.xyz` (chain id 143). Verified source:
`https://sourcify.dev/server/v2/contract/<chainId>/<address>?fields=abi,sources`.

### 1.1 `pons_v2` - full curve family (SRC + LIVE)

Source: `PonsV2LaunchFactory` `0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e`
(Sourcify **exact match**, solc 0.8.35). The bundle contains
`PonsV2BondingCurve.sol`, `PonsV2MemeHook.sol`, `PonsV2LaunchLocker.sol`: the
per-token curve contracts are not verified one by one, their source is the
factory's. Hook `0xE5e702641Ea86F4ae6cC3cDaeD2B886f976Be044` (match), fee escrow
`0xd3afeb2a57f70ef218aa82451c51b2fb0416ac9e` (match: `Credited`, `Claimed`),
launch forwarder `0xe33e9e479df8802cb0866d5d05258bec4cf62948` (exact match).

| event (emitter) | decoded into |
|---|---|
| `TokenLaunched(address indexed token, address indexed curve, address indexed deployer, address pairToken, uint256 launchConfigId, uint256 graduationThreshold)` (factory) | `launchpad_tokens` |
| `CurveBuy(address indexed buyer, address indexed recipient, uint256 quoteIn, uint256 tokensOut, uint256 fee, uint256 tax)` (curve) | `launchpad_trades` |
| `CurveSell(address indexed seller, address indexed recipient, uint256 tokensIn, uint256 quoteOut, uint256 fee, uint256 tax)` (curve) | `launchpad_trades` |
| `FeesSwept(uint256 protocolAmount, uint256 buybackAmount, uint256 creatorAmount)` (curve) | `launchpad_creator_fees`, phase `curve` |
| `PoolGraduated(address indexed token, uint256 positionId, uint256 tokenAmount, uint256 pairTokenAmount)` (factory) + the Uniswap v4 `Initialize` of the same transaction | `launchpad_graduations` |
| `PoolRegistered(bytes32 indexed poolId, address memecoin, address quoteToken, address creator)` (hook) | pool id -> token for the fee rows |
| `PoolFeesSwept(bytes32 indexed poolId, uint256 protocolAmount, uint256 buybackAmount, uint256 creatorAmount, uint256 tokensLocked)` (hook) | `launchpad_creator_fees`, phase `dex` |

Semantics read in the source (`buy` / `sell`): `quoteIn` is the GROSS amount
spent (fee, creator tax and snipe tax included; the unspent part is refunded
with `CurveBuyRefunded`); the event's `fee` = base fee + snipe tax; `tokensOut`
is transferred `curve -> recipient` by the token BEFORE the event. On a sell
`quoteOut` is NET of fee and tax, and `tokensIn` moves `seller -> curve`.
The trader is `recipient` (buy) / `seller` (sell): `buyer` is the forwarder or a
router.

Live observations (24 transactions kept as fixtures, all REAL):

* token `0x95eb2d48...e37d`, curve `0x24991528...6ed3`: launched in block
  66,679,543 and graduated 12 blocks (1.2 s) later. 32 buys, no sell. The launch
  transaction exempts 16 addresses from the snipe tax; ONE transaction in the
  next block buys for 15 of them through contract `0x14b9a544...` (15 `CurveBuy`
  in one tx): a textbook bundle, and the fixture of the sniper view.
* the graduating buy emits `CurveBuyRefunded`, `CurveBuy`, curve `FeesSwept`
  (25.9e15 protocol / 147.9e15 creator wei, mirrored by escrow `Credited`),
  `CurveCompleted(recipient, 4.2e18 quote, 285.7M tokens)`, v4 `Initialize`
  (pool id `0x3875d4c5...e856`, currency0 = native, hook = the Pons hook),
  `PoolRegistered`, `PoolGraduated`, and a v4 `Swap` in the new pool. Sum of
  `quoteIn - fee - tax` over the 32 buys = 4,200,000,000,000,000,003 wei =
  the threshold (the first note said 33 buys; the decoder and the exact sum
  say 32, and ClickHouse reproduces the number digit for digit).
* a curve quoted in an ERC-20 exists (token `0x7e8a82cc...`, sells with both
  legs visible as ERC-20 transfers).
* **The research note was wrong on one point:** curve-phase creator fees ARE an
  event (`FeesSwept` on the curve + `Credited` on the escrow), not only a
  `getLaunchFeePolicy` read.
* In an 8.3 h window (300,000 blocks) the signature was emitted 4,305 times by
  the real factory and 3 times by two unverified contracts
  (`0xb45beb38...`, `0x56fc1db9...`): forks or forgeries, which is what the
  trust rule (section 3) is for.

### 1.2 `flap_portal` - full curve family (SRC + LIVE)

Source: `Portal` implementation `0xAb8Ec926b6e113c2212aF152b086eA62d1FDced9` on
BNB Chain (Sourcify exact match, solc 0.8.26). Portals: BNB Chain
`0xe2cE6ab80874Fa9Fa2aAE65D277Dd6B8e65C9De0`, Robinhood Chain
`0x26605f322f7fF986f381bB9A6e3f5DAb0bEaEb09`. The proxies do not use the
EIP-1967 implementation slot (it reads zero), so "same ABI" was confirmed the
only way that counts: **every topic0 the two portals emitted in a live sample
(17 distinct on BNB Chain, 15 on Robinhood) is in the verified BNB ABI**, and
the Robinhood create / buy / sell fixtures decode with the same code. Monad:
NOT confirmed (no `TokenBought` from any address in the last 3,000 blocks of
the public RPC; the portal address in the research note is truncated).

No indexed parameters anywhere. Decoded: `TokenCreated(uint256 ts, address
creator, uint256 nonce, address token, string name, string symbol, string
meta)`, `TokenBought` / `TokenSold(uint256 ts, address token, address
buyer|seller, uint256 amount, uint256 eth, uint256 fee, uint256 postPrice)`,
`LaunchedToDEX(address token, address pool, uint256 amount, uint256 eth)`,
`TokenQuoteSet(address token, address quoteToken)`,
`FlapTokenProgressChanged(address token, uint256 newProgress)` (wad, 1e18 =
graduated), `TaxV2OnBondingCurvePaid(address indexed token, uint256 amount)` /
`TaxOnBondingCurvePaid` (the token's own tax, `launchpad_creator_fees`).

Live observations: on a buy `eth` is the GROSS quote paid (the ERC-20 quoted
fixture moves exactly `eth` to the portal and the portal forwards `fee` = 1 %);
the token moves `portal -> buyer` in exactly `amount`. The graduation fixture
is a buy through router `0xa02a8481...` (`buyer` = the router, the user is
`tx_from`) that ends with `FlapTokenProgressChanged = 1e18` and
`LaunchedToDEX` into PancakeSwap V2 pair `0x91b8bdf2...` (200M tokens +
15.998 BNB; LP minted to `0x...dEaD`). The pair is created at LAUNCH
(`PairCreated` in the create transaction).

### 1.3 Launch attribution only (one event each; trading is in `dex_swaps`)

8.3 h window on Robinhood Chain, address-less `eth_getLogs` by topic0:

| family | signature | status | live activity |
|---|---|---|---|
| `pons_v1` (Pons V1, NOXA) | `TokenLaunched(address indexed token, address indexed deployer, address indexed dexFactory, address pairToken, address pool, uint256 dexId, uint256 launchConfigId, uint256 positionId, uint256 restrictionsEndBlock, uint256 initialBuyAmount)` | SRC (Pons V1 factory `0xA5aA...1feB`, and live emitter `0xe67bef2a...` is a Sourcify match) + LIVE | 6 launches from 2 factories, none from the Pons V1 factory itself |
| `letscash` | `TokenLaunched(address indexed token, address indexed creator, bytes32 indexed poolId, uint256 configId, uint256 firstBuyIn, uint256 firstBuyOut, address hook, address feeRecipient)` | LIVE only (proxy `0x5bd1Fbe7...4661`, implementation unverified) | 4 |
| `bags` | `TokenCreated(address indexed token, address indexed curve, address indexed creator, address feeShare, address partner, bytes32 poolId, string name, string symbol, string metadataURI)` | SRC + LIVE (`0xe8Cc4431...Cb37`) | 3 |
| `clanker_v4` | `TokenCreated(address msgSender, address indexed tokenAddress, address indexed tokenAdmin, string tokenImage, string tokenName, string tokenSymbol, string tokenMetadata, string tokenContext, int24 startingTick, address poolHook, bytes32 poolId, address pairedToken, address locker, address mevModule, uint256 extensionsSupply, address[] extensions)` | SRC (Base `0xE85A59c6...83a9`, Robinhood `0xd3f2cc17...9a94`) + LIVE | 35 in 5 h on Base, 6 on Robinhood |

**Bags inspected (the open point of the research):** the per-token `curve`
(`0xce8650a0...` in the fixture) is a beacon proxy that holds the whole supply
and DOES emit its own trade event (topic0 `0x6d9c6fad...36b9`, 3 topics, 10 data
words; 3 trades in 2.5 h on the sampled token) next to a Uniswap v4 `Swap`. Its
implementation is not verified under the curve address, the venue is 0.12 % of
EVM-reachable fees, so Bags stays **attribution only**; its curve trades are a
known gap.

Not built (dead or unverifiable), with the 30-day fees of the research note:
**o1** ($4.59M; source-verified `Launched`, but 0 events in 8.3 h on Robinhood
Chain), **Clanker v3.1** (`TokenCreated` verified on Base
`0x2A787b23...7382`, carries no pool, 0 events in the last 22 h),
**NOXA** own factory (`0xD9eC...FCcB`: 0 events; its signature is covered by
`pons_v1`), **Pools** ($1.21M), **PAIR** ($0.59M), **BaseStonk** ($0.45M),
**Coinbarrel** ($0.14M, 0 events): adapter-level signatures only.

### 1.4 four.meme and Virtuals

* **four.meme: verified empirically, NOT built.** `TokenPurchase(address,
  address,uint256,uint256,uint256,uint256,uint256,uint256)` on
  `0x5c952063...762b` is live; in a real purchase the 4th word equals the
  token's `Transfer` to the buyer and word5 + word6 (`cost` + `fee`) equals the
  USDT `Transfer` of the buyer to the manager, so `(token, account, price,
  amount, cost, fee, offers, funds)` is right. The implementation is not on
  Sourcify and the event does not name its quote token. 0.13 % of
  EVM-reachable fees: dropped from this round, listed as the first family to
  add (table driven, a few hours).
* **Virtuals: dropped.** Nothing beyond the DefiLlama adapter could be verified.

## 2. Data model (Phase 2)

Four base tables, four MV-fed side tables, six aggregates, two operator
tables. Migrations `0030` (tables), `0031` (aggregates), `0032` (views),
`0033` (deduplication windows). Every storage rule of docs/design.md
§1-§2 applies: binary columns, `ReplacingMergeTree(_version, is_deleted)`,
`epoch`, month partitions for the event streams, `chain` partitions for
lookups, no `DELETE` anywhere, `epoch` LAST in every aggregate sorting
key, and no `non_replicated_deduplication_window` in a `CREATE`. `0090`
did that for the modules that existed when it was written and an applied
migration never changes, so `0033` is this module's own copy: every base
table AND every materialized-view target, or the side tables and the
aggregates would count a retried insert twice.

### 2.1 Chain neutral from day one

The tables follow docs/design.md §13, exactly like `dex_*` and
`prediction_*`:

| | launchpad_* |
|---|---|
| identity columns (token, emitter, curve, creator, trader, caller, recipient, quote_token, tx_from, tx_to) | `FixedString(32)`: on EVM the 20 address bytes left-padded with 12 zero bytes, on Solana the 32 raw pubkey bytes. Rust side: `Address` through `crate::utils::format::SerId32` - nothing here hand rolls the padding, and reading a row whose padding is not zero fails loudly instead of truncating a pubkey |
| pool ids | `FixedString(32)`, natively 32 bytes for Uniswap V4, left-padded for a pool contract; `pool_kind` says which. Rust side: `B256` with `SerB256`, because a pool id is NOT an address even on EVM |
| transaction id | `tx_id String`, the RAW bytes (32 on EVM, 64 for a Solana signature, which a `FixedString(32)` could not hold). Never a sorting-key column. Rust side: `Bytes` through `SerTxId`, built with `utils::format::tx_id()`, read back with `tx_hash_of()` |
| position of a row | `(chain, block_number, tx_index, ordinal)`. `ordinal` IS the log index on EVM; there is no `log_index` column |
| amounts | `UInt256` exact in the base tables, `Float64` in every aggregate (the 256-bit rule) |

**Printing an id** is the one thing the bytes cannot say by themselves, so
use THE expression of migration `0006`, where `chains_v` gives the family:
`base58Encode(substring(id, 1, 32))` for `svm`, `concat('0x',
lower(hex(substring(id, 13))))` for `evm`. The `substring()` is not
decoration: `toString(id)` and `CAST(id AS String)` TRIM TRAILING ZERO
BYTES (checked on 25.12.1.322:
`length(toString(toFixedString(unhex('0102030000'), 5)))` is 3), so
anything that routes an id through them shortens a pubkey.
`substring(id, 1, 32)` and `concat(id, '')` keep every byte, which is
also why `hex(substring(id, 13))` is safe. Note that on 25.12.1.322
`base58Encode(id)` on a `FixedString` did NOT trim - the conversion the
header of migration `0006` warns about is the `toString` / `CAST` one.
Use the `substring()` form regardless: it is right on every build.

**The 20 vs 32 byte seam.** The only EVM-only table these views touch is
`erc20_transfers` (`launchpad_token_holders_v`), whose `token_address` /
`from` / `to` are `FixedString(20)`. They are PADDED up to 32 bytes there,
never the other way round: `substring(id, 13, 20)` on the 32-byte side
would map every Solana pubkey onto some EVM address, while padding simply
finds no row. Same rule as `dex_token_info_v`. Joins into
`dex_pools` / `dex_pool_current_v` are direct - both sides are already
32-byte ids.

### 2.2 Tables

| table | one row = | ORDER BY | partition |
|---|---|---|---|
| `launchpad_tokens` | a launch EVENT | `(chain, token, emitter, block_number, tx_index, ordinal)` | `chain` |
| `launchpad_trades` | a curve buy / sell | `(chain, block_number, tx_index, ordinal)` | month |
| `launchpad_graduations` | a curve moving into a DEX pool | `(chain, block_number, tx_index, ordinal)` | month |
| `launchpad_creator_fees` | one component of a fee sweep, or a token tax | `(chain, block_number, tx_index, ordinal, component)` | month |
| `launchpad_trades_by_token` | trades tape + candle source | `(chain, token, block_number, tx_index, ordinal)` | `chain` |
| `launchpad_trades_by_trader` | sniper / portfolio screens | `(chain, trader, block_number, tx_index, ordinal)` | `chain` |
| `launchpad_launches_by_time` | the new-launch feed | `(chain, timestamp, block_number, tx_index, ordinal)` | `chain` |
| `launchpad_launches_by_creator` | the creator page | `(chain, creator, timestamp, block_number, tx_index, ordinal)` | `chain` |
| `launchpad_trusted_emitters`, `launchpad_frontends` | operator data, never block scoped, never purged | `(chain, emitter)` / `(chain, address, kind)` | - |

Why these sort keys: the three event streams are written in block order and
read by range, so `(chain, block_number, tx_index, ordinal)` is both the
insert order and the purge predicate, and a re-inserted block replaces
itself key for key. `launchpad_tokens` is a REGISTRY, read by token id
(`token page`, and the curve -> token join every fee row needs), so it is
keyed and partitioned like `dex_pools`. Each side table exists because a
screen asks the question the other way round, and every one of them is fed
by an MV that passes `_version` / `is_deleted` / `epoch` through, so a
tombstone on the base table kills the side rows for free.

Aggregates (`LAUNCHPADS_DERIVED`, all `AggregatingMergeTree`, all
`PARTITION BY toYYYYMM(bucket)`, `epoch` last in the key):
`launchpad_candles_1m` and `_1h` per `(chain, token, emitter)`,
`launchpad_venue_trades_1d` and `launchpad_graduations_1d` per
`(chain, family, emitter)`, `launchpad_launches_1d` per
`(chain, family, emitter, creator)` (so the same table serves the venue
screen and the creator screen), `launchpad_creator_fees_1d` per
`(chain, family, emitter, recipient, kind, phase)`.

**`emitter` is in every aggregate key on purpose.** It is the only thing a
reader can still use after aggregation to keep a forger's rows out. A
rebuild is month-chunked (`derived::rebuild_statements`) because one INSERT
spanning more than 100 monthly partitions is refused by ClickHouse.

### 2.3 No background resolver

Nothing in this module needs RPC. The one thing the events do not carry -
which token a `pons_v2` curve belongs to, since `CurveBuy` is emitted by
the curve and names no token - is answered by the corroborating ERC-20
`Transfer` in the same transaction (§3), and independently by the
`TokenLaunched` row. So there is no `worker.rs` / `resolve.rs` here, and
therefore nothing on the commit path and no `eth_call` to confirm through
`tokens::call_confirmed`.

## 3. What is trusted, and what that proves

Registry-free decoding means **any contract can emit any of these events**.
In an 8.3 h window on Robinhood Chain the real Pons V2 factory emitted
`TokenLaunched` 4,305 times and two unverified contracts emitted it 3
times. Two independent defences:

### 3.1 Corroboration of the amounts (`decode.rs`, at decode time)

A curve event is a claim of its emitter; an ERC-20 `Transfer` is a claim of
the TOKEN. A leg is **verified** when, in the same transaction, some token
contract reports a movement of EXACTLY the leg amount to the emitter (in
leg) or from the emitter (out leg), emitted by a contract other than the
emitter and not already consumed by another leg. Candidates naming more
than one token leave the leg unverified - ambiguity is never resolved by
guessing - and one transfer verifies one leg, so a contract cannot emit a
thousand trades over a single real movement. A per-token curve (`pons_v2`)
moves the assets BEFORE it emits, so only earlier log indices count; the
Flap portal is a singleton that settles some legs after its event, so there
any position counts. This is the same rule as `src/dex/corroborate.rs`.

* **What it proves:** that asset moved, in that amount, to or from the
  emitter, in that transaction.
* **What it does NOT prove:** that the movement was a trade at a market
  price. Wash trading through a real curve stays possible, exactly as it is
  on a real DEX pool.
* **Native-coin legs can never be proven from logs.** There is no log for a
  value transfer. All 32 buys of the Pons V2 token that graduated in the
  fixtures have a VERIFIED token leg and an UNVERIFIED quote leg.
  `attach_transactions` adds `transactions.value`, which is a strictly
  weaker statement: it proves the sender sent that much native coin in the
  transaction, not that the curve received it, and not how it was split.
  The row therefore also carries `sole_unverified_quote`, 1 only when this
  is the single trade of the transaction with an unverified quote leg. The
  fixtures contain the counter-example that makes the flag necessary: ONE
  transaction buys for 15 different recipients through a bundler, with one
  `value` for all 15. So: `sole_unverified_quote = 1 AND tx_value >=
  quote_amount` is an upper bound on what the buyer paid, and nothing more.
  A quote leg is never valued at 0 when it is unverified - it is NULL.

Result on the fixtures: 43 of 43 curve trades have a verified token leg;
the quote leg is verified for every ERC-20-quoted trade (Pons V2 ERC-20
curve, Flap on BNB Chain) and for none of the native-quoted ones.

### 3.2 Trusted emitters (`launchpad_trusted_emitters`, at read time)

A real launchpad is a SINGLETON with verified source; a forgery costs one
transaction. The table is operator data - migrations seed nothing, like
`quote_tokens` and `dex_trusted_emitters` - and holds the singletons: a
factory, a portal, a graduation hook, a fee escrow. Per-token curves are
not listed one by one: `launchpad_trusted_curves_v` is the listed
singletons UNION every `launchpad_tokens.curve` a listed emitter
announced, which is exactly the set a forger cannot enter.

**Picking a token is NOT a trust decision.** The first cut of these views
filtered the feeds and left the token page, the chart, the tape, the
snipers and the holders unfiltered, on the theory that the caller had
already chosen the token. That was wrong, and it was the worst place to be
wrong: the caller chooses the token, an attacker chooses the rows. Anyone
can emit a `CurveBuy` naming a REAL token - with a real movement behind
it, so the corroboration passes - and move that token's `trades`, `buys`,
`unique_traders`, volume, first / last price, `raised_raw` and therefore
`curve_progress`; one forged buy of at least the graduation threshold
makes a real token read "about to graduate". A forger who predicts the
token address (V2 / V3 addresses are predictable) can also emit a
`TokenLaunched` for it EARLIER than the real venue, win the `argMin` and
make the real launch read `trusted = 0`. So every token-scoped `*_v` view
now restricts ALL of its sources - launches, trades, graduations, candles
- to `launchpad_trusted_curves_v`, and each one has an `*_all_v` twin that
keeps the unfiltered picture for exploration. A token with no trusted
launch yields no rows at all: missing numbers, never wrong ones.
`integration_tests::a_forged_curve_moves_no_token_page_number` asserts
every one of those screens is byte for byte identical before and after the
forged rows exist, including the earlier-launch case, and that every
`_all_v` twin does show them.

**Picking a CREATOR is not a trust decision either**, and there the victim
is a wallet that did nothing at all. A launch names its creator in the
event, so a forger can emit a `TokenLaunched` naming any address as
`creator`: unfiltered, that launch lands on the stranger's page, never
graduates, and so raises `launches`, raises `died` and drags
`graduation_rate` down - manufacturing precisely the serial-rugger signal
the creator page exists to report. The other three sources are open the
same way: a forged `CurveBuy` on one of those tokens moves `trades`,
`volume_quote_raw` and `last_trade_time` (and through it `died`), a forged
graduation flips `graduated`, and a forged fee sweep naming the wallet as
`recipient` inflates `realised_creator_fees_raw`. So
`launchpad_creator_tokens_v` and `launchpad_creator_v` restrict all four
sources to `launchpad_trusted_curves_v`, and
`launchpad_creator_tokens_all_v` / `launchpad_creator_all_v` are the
exploration twins (the `_all_v` header also carries `trusted_launches`, how
many of the counted launches came from a trusted curve).
`integration_tests::a_forged_launch_moves_no_creator_page_number` asserts
the two creator screens are byte identical before and after the forged
rows and that both twins move.

**Why trusted-by-default-off is the right default.** The alternative -
counting everything and hoping the corroboration filters it - fails against
the cheapest attack there is: deploy a token, deploy a fake curve, move
real tokens between two wallets you own through it, and every leg is
verified. Corroboration bounds the *amounts*; only the emitter list bounds
*who may be a venue at all*. The cost of the default is that a freshly
launched real venue is invisible until an operator adds one row, which is
what `launchpad_new_launches_all_v` and `launchpad_venues_1d_all_v` are
for: they show everything, with a `trusted` column, and are the tool for
deciding what to add.

### 3.3 The verified addresses found in Phase 1

Ready to run. Nothing below is in a migration.

```sql
INSERT INTO launchpad_trusted_emitters (chain, emitter, family, label) VALUES
  (4663, unhex('0000000000000000000000007ed598bcef8bd9edd8c97a195c6d13f40801ec7e'), 'pons_v2', 'PonsV2LaunchFactory'),
  (4663, unhex('000000000000000000000000e5e702641ea86f4ae6cc3cdaed2b886f976be044'), 'pons_v2', 'PonsV2MemeHook'),
  (4663, unhex('000000000000000000000000d3afeb2a57f70ef218aa82451c51b2fb0416ac9e'), 'pons_v2', 'PonsV2 fee escrow'),
  (56,   unhex('000000000000000000000000e2ce6ab80874fa9fa2aae65d277dd6b8e65c9de0'), 'flap_portal', 'Flap Portal (BNB Chain)'),
  (4663, unhex('00000000000000000000000026605f322f7ff986f381bb9a6e3f5dab0beaeb09'), 'flap_portal', 'Flap Portal (Robinhood Chain)'),
  (4663, unhex('000000000000000000000000e8cc4431adf8b5a847c113ef0c6af9043219cb37'), 'bags', 'Bags factory'),
  (8453, unhex('000000000000000000000000e85a59c628f7d27878aceb4bf3b35733630083a9'), 'clanker_v4', 'Clanker v4 factory (Base)'),
  (4663, unhex('000000000000000000000000d3f2cc1731b7fd17f28798835c2e02f0a1839a94'), 'clanker_v4', 'Clanker v4 factory (Robinhood Chain)');
```

Two more were seen live but are NOT Sourcify-verified at the address that
emits, so add them only after your own check:

```sql
INSERT INTO launchpad_trusted_emitters (chain, emitter, family, label) VALUES
  (4663, unhex('0000000000000000000000005bd1fbe78a78fe8236fa00cf48fbeba74ae34661'), 'letscash', 'LetsCash proxy, implementation unverified'),
  (4663, unhex('000000000000000000000000f4fc0cd27fc8ecf17e55ee4c3f7201897df3eb75'), 'pons_v1', 'Pons V1 style factory seen live');
```

Front ends seen in the fixtures. They are NEVER venues; adding a row only
splits an existing venue's volume:

```sql
INSERT INTO launchpad_frontends (chain, address, name, kind) VALUES
  (4663, unhex('000000000000000000000000e33e9e479df8802cb0866d5d05258bec4cf62948'), 'Pons launch forwarder', 'router'),
  (4663, unhex('00000000000000000000000014b9a544e8c179fc2040d3089dcc73baf25aa8f9'), 'bundler seen in block 66679544', 'router'),
  (56,   unhex('000000000000000000000000a02a848143d20bc2c14821efc36d0345351a8ccd'), 'Flap router', 'router');
```

### 3.4 The text is hostile too

`name`, `symbol` and `metadata_uri` are bytes chosen by whoever emitted
the log - there is no registry - and they come back out of every feed onto
a screen. `decode::sanitize` removes, at decode time:

* the `Cc` control characters, including newline and tab (a name is one
  line: a newline forges a second row in a log line or a CSV export),
* every Unicode `Cf` FORMAT character. `char::is_control()` is `Cc` only,
  so on its own it lets through the bidi overrides and isolates
  (`U+202A..U+202E`, `U+2066..U+2069`) that make a name render as text it
  does not contain - the "Trojan Source" class - the zero width
  characters, `U+FEFF`, and the tag block `U+E0020..U+E007F`, which is a
  whole second invisible string,
* the `U+2028` / `U+2029` separators,

collapses the whitespace runs that leaves behind, and caps the result at
128 characters. A hidden character inside a word is dropped WITHOUT a
space, because that is exactly where it was put to hide a join.

It does NOT escape HTML, and it must not: the strings are stored as text,
so **the UI escapes them** for whatever it renders into. A `<script>` in a
symbol is data here and has to stay data there.

## 4. The query cookbook

One query per screen. Every query below is also a `Recipe` in
`cookbook.rs` (a unit test asserts the two texts are identical) and is run
by the ClickHouse integration tests with hand-computed assertions.

Every `{name:Type}` is a ClickHouse **bound parameter**, sent beside the
statement (`param_name=` over HTTP, `.param(..)` with the `clickhouse`
crate) and NEVER pasted into its text - which is why no placeholder is
inside quotes. Ids are plain hex without `0x`: 64 characters for a 32 byte
id, or the 40 of an EVM address, which the parameterized views left pad
themselves (a constant expression, so the primary key range read
survives). Anything else can only fail to match, with one exception worth
knowing: an EMPTY string pads to the 32 zero bytes, which here is the real
bucket holding the trades whose token leg stayed unverified. `tx_id` comes
back as the raw transaction bytes - `hex(tx_id)` to print it.

Every screen below reads a trust-filtered view; the `*_all_v` twins
(§3.2) are the exploration tool, and only the launch feed ships one as a
recipe, labelled as such. **`name` and `symbol` are hostile text**: the
decoder has stripped the control, bidi, zero width and tag characters
(§3.4), but they are stored as text and the UI must escape them for
whatever it renders into.

### New launch feed

```sql
SELECT launch_time, token, family, emitter, creator, name, symbol,
       quote_token, initial_price_raw, first_minute_trades,
       first_minute_buys, first_minute_volume_raw, first_minute_traders,
       graduation_threshold_raw
FROM launchpad_new_launches_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50
```

Newest first over one `chain` partition of `launchpad_launches_by_time`,
with the first minute of the curve joined from `launchpad_candles_1m_v` by
`(token, curve, launch minute)` - a key read of the aggregate, never a scan
of the trades. `initial_price_raw` is the OPEN of the launch minute.

### New launch feed (untrusted included)

```sql
SELECT launch_time, token, family, emitter, trusted, name, symbol,
       first_minute_trades, first_minute_volume_raw
FROM launchpad_new_launches_all_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50
```

The same feed without the trust filter: this is how an operator decides
what to put in `launchpad_trusted_emitters`.

### Token page header

```sql
SELECT *
FROM launchpad_token_v(chain = {chain:UInt64}, token = {token:String})
```

One row: the launch, whether its emitter is trusted, the traded volume and
prices, `curve_progress` in 0..1, and the graduation with its `pool_id`.
Progress comes from the venue when it reports one (`flap_portal`), else
from the quote RAISED against the graduation threshold. For `pons_v2`,
`quoteIn` is gross, so what counts towards the threshold is
`quoteIn - fee - tax` on buys minus the net `quoteOut` of sells - summed
over the 32 real buys of the token that graduated this gives
4,200,000,000,000,000,003 wei against a threshold of 4.2e18, i.e. 1.0000
(clamped to 1). Asserted against a real ClickHouse.

### Price chart

```sql
SELECT bucket, open_raw, high_raw, low_raw, close_raw, volume_quote_raw,
       volume_token_raw, trades, unique_traders, curve_progress
FROM launchpad_candles_1m_v(chain = {chain:UInt64}, token = {token:String})
ORDER BY bucket
```

Merged `argMin`/`argMax` states over `(block_number, tx_index, ordinal)`,
with the validity rule applied before the merge. Prices are quote units per
token unit in RAW units; the same query against `launchpad_candles_1h_v`
gives the hourly chart. Candles are per `(token, emitter)`, so a forged
curve can never be added into a real token's candle.

### Trades tape

```sql
SELECT timestamp, side, trader, caller, token_amount_raw, quote_amount_raw,
       price_raw, fee_amount_raw, token_verified, quote_verified,
       tx_id
FROM launchpad_token_trades_v(chain = {chain:UInt64}, token = {token:String},
                              from_block = {from_block:UInt64})
LIMIT 50
```

`trader` is the event's beneficiary, `caller` the router / forwarder it
came through. `price_raw` is NULL, never 0, when a leg is zero.

### Top holders

```sql
SELECT account, balance_raw, share_of_initial_supply, received, sent
FROM launchpad_token_holders_v(chain = {chain:UInt64}, token = {token:String},
                               as_of_block = {as_of_block:UInt64})
LIMIT 50
```

Net balances from the ERC-20 transfers the TOKEN itself emitted, so this is
the one holder list that does not depend on any launchpad event being
honest. It stays cheap because a launchpad token has no history before its
launch block: the scalar subquery on `launchpad_tokens` prunes
`erc20_transfers` by its own primary key. `as_of_block` = the graduation
block gives the top-holder concentration AT graduation.

### Graduation feed

```sql
SELECT graduation_time, token, family, pool_id, pool_kind, pool_status,
       pool_protocol, token_amount_raw, quote_amount_raw, graduation_tx
FROM launchpad_graduations_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 50
```

`pool_id` is the join key into the DEX module: the view already carries
`pool_status` / `pool_trusted` / `pool_protocol` from
`dex_pool_current_v`, so the same page keeps charting the token from
`dex_candles_1m_v` after the curve is gone. Proven on a real graduated
token: Pons V2 `0x95eb2d48...` graduated into Uniswap V4 pool
`0x3875d4c5...e856`, which is the pool id the V4 `Initialize` of the same
transaction created; Flap `0x4dfef57d...` graduated into PancakeSwap V2
pair `0x91b8bdf2...`, whose left-padded address is its `dex_pools.pool_id`.

### Creator page header

```sql
SELECT launches, graduated, died, graduation_rate, first_launch,
       last_launch, volume_quote_raw, realised_creator_fees_raw
FROM launchpad_creator_v(chain = {chain:UInt64}, creator = {creator:String},
                         as_of = {now:UInt32}, dead_after = {dead_after:UInt32})
```

The serial-rugger signal in one row. `died` = never graduated and no trade
for `dead_after` seconds. `realised_creator_fees_raw` counts only fee rows
whose recipient the fee ESCROW named (`kind = 'creator'`), so it is money
that provably moved, not a fee policy read over RPC.

All four sources are restricted to `launchpad_trusted_curves_v` (§3.2):
without that, anyone could name this wallet as the `creator` of a launch
that never graduates, or as the `recipient` of a fee sweep, and both
numbers are exactly the ones a reader judges the wallet by. The
exploration twin is `launchpad_creator_all_v`, which counts every emitter
and adds `trusted_launches`.

### Creator launches

```sql
SELECT launch_time, token, symbol, graduated, died, trades,
       volume_quote_raw, last_trade_time, pool_id
FROM launchpad_creator_tokens_v(chain = {chain:UInt64}, creator = {creator:String},
                                as_of = {now:UInt32}, dead_after = {dead_after:UInt32})
LIMIT 200
```

One row per launch of the wallet, newest first, launches / graduations /
trades all taken from trusted curves. `launchpad_creator_tokens_all_v` is
the unfiltered twin and carries `trusted` per row.

### Sniper view

```sql
SELECT trader, blocks_after_launch, buys, token_amount_raw,
       quote_amount_raw, share_of_initial_supply, funder, bundle_size,
       is_creator
FROM launchpad_snipers_v(chain = {chain:UInt64}, token = {token:String},
                         blocks = {blocks:UInt64})
LIMIT 100
```

Buys in the launch block and the first `blocks` blocks after it.
`bundle_size` is the number of DISTINCT recipients served by one
transaction - on the real fixture the bundler in block 66,679,544 serves 15
of the 16 addresses the launch transaction had exempted from the snipe tax,
and they all come out with `bundle_size = 15` and the same `funder`.
`share_of_initial_supply` divides by the supply the token minted in its own
launch transaction.

### Venue stats

```sql
SELECT family, bucket, launches, graduations, graduation_rate,
       trades, volume_quote_raw, volume_quote_verified_raw, fees_raw,
       unique_traders, unique_creators
FROM launchpad_venues_1d_v(chain = {chain:UInt64})
LIMIT 100
```

One row per VENUE per day, from the three daily aggregates, counting only
emitters in `launchpad_trusted_curves_v`. It rolls up by `family` and not
by emitter because a `pons_v2` trade is emitted by the token's own curve,
so a per-emitter row would be a per-TOKEN row; the distinct counts are
`uniqMerge`d from the aggregate states, never summed.
`launchpad_venues_1d_all_v` keeps the per-emitter breakdown with a
`trusted` column. `graduation_rate` here is same-day graduations over
same-day launches, a THROUGHPUT ratio: a token launched on Monday graduates
on Tuesday, so the cohort answer per token is
`launchpad_creator_tokens_v` / `launchpad_token_v`, not this column.

### Front end attribution

```sql
SELECT family, emitter, frontend, trades, volume_quote_raw,
       volume_quote_verified_raw, unique_traders
FROM launchpad_frontend_volume_v(chain = {chain:UInt64}, since = {since:UInt32})
LIMIT 100
```

Splits a venue's volume by the front end that routed it (`caller`, else
`tx_to`, else `direct`). The rows partition the venue's volume: they add up
to it and are never added to it.

### Latency on the fixtures

Measured by `integration_tests::the_cookbook_runs_on_real_data` on
ClickHouse 25.12 (a throwaway single-node server, 9 launches, 43 curve
trades, 2 graduations, 9 fee rows, 153 ERC-20 transfers), warm cache,
while the machine was busy. These are the cost of the QUERY SHAPE, not of
a real data volume: every one is a primary-key range read or a small
aggregate merge, and none of them scans a base table.

| screen | rows | ms |
|---|---|---|
| New launch feed | 3 | 19 |
| New launch feed (untrusted included) | 7 | 13 |
| Token page header | 1 | 24 |
| Price chart | 1 | 8 |
| Trades tape | 32 | 7 |
| Top holders | 35 | 10 |
| Graduation feed | 1 | 36 |
| Creator page header | 1 | 17 |
| Creator launches | 1 | 12 |
| Sniper view | 26 | 11 |
| Venue stats | 2 | 31 |
| Front end attribution | 4 | 13 |

The three slowest are the ones that join something: the graduation feed
joins `dex_pool_current_v` (which rebuilds the pool's history from
`dex_pools` on every read), the venue screen joins three daily aggregates,
and the launch feed joins the 1-minute candles.

## 6. Tests

* `cargo test --lib launchpads` - 32 unit tests: every `topic0` against
  `keccak256(signature)`, the canonical-signature rules, the decoder's
  bounds checks against malformed data, `set_version` / `set_epoch` /
  `attach_transactions` over all 32 real transactions, the schema rules of
  docs/design.md §1-§2 over the migrations (engine, epoch, partitioning -
  including WHICH base table may escape month partitioning and why - no
  DELETE, no dedup window in a CREATE), the chain-neutral identity rules
  (including that no table keeps a `transaction_hash` column), hostile
  text losing its control / bidi / zero width / tag characters, every
  cookbook placeholder being a typed BOUND parameter that is not inside
  quotes, every `rebuild_sql` against the `SELECT` of its materialized
  view, month chunking, "no `sum()` over a 256-bit column", and the README
  printing every cookbook query verbatim.
* `TEST_DATABASE_URL=... cargo test launchpads::integration -- --ignored` -
  6 tests against a real ClickHouse, each in its own `_test` database
  created by the real migration runner, green in parallel over repeated
  runs:
  1. every row round-trips (including `1e27` as an exact integer) and each
     MV-fed side table carries exactly its parent's rows;
  2. every cookbook query runs, with the hand-computed numbers of §4
     (raised = threshold + 3 wei, progress = 1, the bundle of 15, the
     creator's 147,906,635,318,930,699 wei, the pool the DEX module
     resolves, the front-end split adding up to the venue total);
  3. a purge of a range where the canonical chain has FOUR FEWER trades,
     followed by an epoch bump and a month-chunked rebuild, gives byte for
     byte the same base tables, side tables and views as a clean index of
     the canonical chain (Float64 sums compared at Float32 precision: the
     rebuild adds in one group, the view added incrementally, and floating
     point addition is not associative);
  4. a forged launch plus a forged 1e30 trade - with a real token movement
     behind it, so the corroboration PASSES - moves no trusted number, and
     shows up in the `*_all_v` views;
  5. amounts of `2^256-1` round-trip exactly and the aggregates do not
     wrap.

## 5. What a trader UI still cannot get from this chain data

* **USD prices.** Every number here is in raw quote units. Valuation needs
  the `quote_tokens` table and the DEX module's native-price views, which
  is where it belongs - a launchpad quote is usually the chain's native
  coin, and that price comes from `dex_native_price_1h_v`.
* **Decimals**, until the token worker has stored them. Views expose
  `*_raw` always and the scaled column NULL, never a guess.
* **Native-coin amounts, provably.** §3.1.
* **Who is behind a wallet.** Bundles are visible (`bundle_size`, `funder`),
  but "the same person" is not: a funder that is a contract, a CEX or a
  relayer proves nothing.
* **Off-chain metadata.** `metadata_uri` is an IPFS CID; the image, the
  description and the social links are behind it. No fetching is done and
  none should be on the commit path.
* **Curve maths.** The bonding curve's exact shape (virtual reserves, the
  price function) is in the contract, not in the events. Everything here is
  realised prices, never a quote for a size not yet traded.
* **Bags curve trades.** Bags' per-token curve emits its own trade event
  (`topic0 0x6d9c6fad...36b9`) from an unverified implementation; the venue
  is 0.12 % of EVM-reachable fees, so it is attribution only. Known gap.
* **four.meme.** Semantics verified empirically (§1.4), not built: 0.13 %
  of EVM-reachable fees. First family to add, a few hours of work.
* **Dead / unverifiable venues**, with their 30-day fees from the research:
  o1 ($4.59M, source-verified but 0 events in 8.3 h), Clanker v3.1,
  Pools ($1.21M), PAIR ($0.59M), BaseStonk ($0.45M), Coinbarrel ($0.14M),
  Virtuals (nothing verifiable beyond a DefiLlama adapter).
