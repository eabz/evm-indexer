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
  66,679,543 and graduated 12 blocks (1.2 s) later. 33 buys, no sell. The launch
  transaction exempts 16 addresses from the snipe tax; ONE transaction in the
  next block buys for 15 of them through contract `0x14b9a544...` (15 `CurveBuy`
  in one tx): a textbook bundle, and the fixture of the sniper view.
* the graduating buy emits `CurveBuyRefunded`, `CurveBuy`, curve `FeesSwept`
  (25.9e15 protocol / 147.9e15 creator wei, mirrored by escrow `Credited`),
  `CurveCompleted(recipient, 4.2e18 quote, 285.7M tokens)`, v4 `Initialize`
  (pool id `0x3875d4c5...e856`, currency0 = native, hook = the Pons hook),
  `PoolRegistered`, `PoolGraduated`, and a v4 `Swap` in the new pool. Sum of
  `quoteIn - fee - tax` over the 33 buys = 4.2e18 + 3 wei = the threshold.
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
