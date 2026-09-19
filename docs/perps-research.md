# Perpetual futures venues: who has the volume, and what an EVM log indexer can actually see

Research note for the owner. No code. Data retrieved 2026-09-19 00:27-00:40 UTC
(2026-09-18 evening, America time). Sources, endpoints and everything that could
not be verified are listed in the appendix. Every figure below comes from a fetched
source; nothing is estimated from memory.

## 1. Executive summary (plain language)

* DefiLlama tracks 209 perp venues; 101 had volume in the last 30 days. Total perp
  volume: **$666.7B in 30 days**, $140.8B in 7 days, $23.0B in 24 hours. Open
  interest: $13.4B.
* The market is extremely concentrated and it is **not on EVM chains**. Hyperliquid
  alone is 36.0% of all volume. The top 6 venues (Hyperliquid, Aster, Lighter, ApeX,
  edgeX, Variational) are 74.2%, and **none of the six emits its trades as EVM event
  logs**. They run on their own chains, on zk rollups with private state, or on
  off-chain matching engines.
* **The uncomfortable number: about 3.9% of perp volume ($26.0B of $666.7B in 30
  days) can be read from EVM logs on chains our data source (HyperSync) supports
  today.** Counting EVM chains HyperSync does not serve (RISE, Reya, Orderly's
  L2, Derive's L2, Cronos, Somnia...) the ceiling is about 5.2% ($34.7B). The other
  ~95% needs API adapters or is not publicly available at all.
* Inside that 3.9%, the single most valuable thing to build is the
  **Vertex-style `FillOrder` family used by Nado on Ink: $9.5B/30d, more than every
  GMX, Gains and Synthetix style venue combined**. It is one contract, one event,
  with a public GPL source repo.
* The "classic" on-chain perps the owner probably has in mind are now small:
  GMX (all versions and forks) $2.5B, Gains $1.25B, Synthetix on-chain perps $0.
  Newer on-chain venues are as large or larger: Avantis $2.8B (Base), Perpl $2.5B
  (Monad), SynFutures $2.4B (Base), Aark $2.1B (Arbitrum).
* Unlike spot DEXes, **perps have almost no "one ABI, hundreds of forks" effect**.
  The only real fork families are GMX v1 (64 forks listed, nearly all dead: $89M
  combined) and GMX v2 (9 forks, one alive: HertzFlow $86M). Everything else is one
  decoder per protocol. "Agnostic" for perps means a common output table, not a
  common input ABI.
* Recommended order: (1) Nado/Vertex `FillOrder`, (2) GMX v2 EventEmitter,
  (3) the Gains-shaped group (Gains, Avantis, Ostium), (4) Perpl, (5) SynFutures,
  then small ones. Roughly seven decoders cover ~90% of what is reachable.
* If the owner wants perp coverage that means something in market terms, the real
  project is **API adapters for Hyperliquid, Aster and Lighter (55.3% of volume
  between them)**. Hyperliquid is the easy one: public S3 archives of every fill.
* Volume caveat: for off-chain venues DefiLlama's number is whatever the venue's own
  API reports. It cannot be audited. Log-derived volume can.

## 2. Ranking: top 50 perp venues by 30-day volume

Source: `defillama.com/perps` page data (the JSON the page embeds), fetched
2026-09-19 00:28 UTC. Market share is against the page total of $666,669,261,893.
"Chain" is DefiLlama's label, not a verified fact (section 3 corrects several).
Parent protocols are shown as DefiLlama rolls them up; where only one child has
volume it is named in brackets.

| # | Venue | DefiLlama chain label | 24h | 7d | 30d | Share 30d | Cumulative | Open interest |
|---|---|---|---|---|---|---|---|---|
| 1 | Hyperliquid | Hyperliquid L1 | 7.18B | 44.73B | 240.17B | 36.03% | 36.03% | 7.29B |
| 2 | Aster | Off Chain | 3.36B | 16.02B | 73.17B | 10.98% | 47.00% | 1.39B |
| 3 | Lighter (zkLighter 45.94B + Robinhood Chain 9.41B) | zkLighter / Robinhood Chain | 1.64B | 13.03B | 55.35B | 8.30% | 55.30% | 823.6M |
| 4 | ApeX Protocol (Omni) | Ethereum | 467.1M | 5.42B | 43.10B | 6.47% | 61.77% | 139.5M |
| 5 | edgeX (V2) | edgeX L1 | 1.10B | 9.95B | 42.66B | 6.40% | 68.17% | 618.5M |
| 6 | Variational (Omni) | Arbitrum | 2.80B | 10.87B | 40.27B | 6.04% | 74.21% | 890.7M |
| 7 | GMTrade | Solana | 1.10B | 7.35B | 26.06B | 3.91% | 78.12% | 117.4M |
| 8 | Grvt | GRVT | 495.1M | 3.25B | 14.54B | 2.18% | 80.30% | 458.0M |
| 9 | Pacifica | Solana | 415.6M | 2.59B | 12.54B | 1.88% | 82.18% | 48.0M |
| 10 | StandX | StandX | 477.1M | 2.86B | 11.95B | 1.79% | 83.97% | 30.1M |
| 11 | Nado | Ink | 319.5M | 2.21B | 9.48B | 1.42% | 85.39% | 40.6M |
| 12 | Antarctic | Off Chain | 302.5M | 2.10B | 8.49B | 1.27% | 86.67% | 362.5M |
| 13 | Extended | Ethereum / Starknet | 262.5M | 1.87B | 7.81B | 1.17% | 87.84% | 96.1M |
| 14 | AZverse | Off Chain | 210.0M | 1.51B | 7.56B | 1.13% | 88.97% | n/a |
| 15 | Jupiter (Perpetual Exchange) | Solana | 380.1M | 1.50B | 7.39B | 1.11% | 90.08% | 37.0M |
| 16 | QFEX | Off Chain | 306.0M | 1.55B | 5.80B | 0.87% | 90.95% | 232.4M |
| 17 | Upscale | Off Chain | 192.8M | 1.14B | 5.10B | 0.76% | 91.72% | n/a |
| 18 | Ondo Finance (Ondo Perps) | Off Chain | 135.2M | 721.8M | 3.89B | 0.58% | 92.30% | 89.7M |
| 19 | RISEx | RISE | 117.1M | 639.7M | 3.08B | 0.46% | 92.76% | 26.7M |
| 20 | SoSoValue (SoDEX) | ValueChain | 111.1M | 1.26B | 3.00B | 0.45% | 93.21% | 124.3M |
| 21 | Reya | ReyaChain | 87.0M | 718.0M | 2.93B | 0.44% | 93.65% | 7.9M |
| 22 | Vest Markets | Base / Off Chain | 142.4M | 579.7M | 2.88B | 0.43% | 94.08% | 95.2M |
| 23 | Avantis | Base | 141.5M | 722.1M | 2.77B | 0.42% | 94.50% | n/a |
| 24 | SUN (SunPerp / SunX) | Tron | 66.8M | 482.0M | 2.64B | 0.40% | 94.89% | 5.2M |
| 25 | Perpl | Monad | 23.9M | 704.0M | 2.53B | 0.38% | 95.27% | 1.3M |
| 26 | SynFutures (V3 only; V1/V2 are 0) | Base / Blast | 107.9M | 529.0M | 2.41B | 0.36% | 95.63% | 1.1M |
| 27 | Rocky Exchange | Canton | 14.3M | 411.3M | 2.38B | 0.36% | 95.99% | n/a |
| 28 | GMX (V2 only; V1 is 0) | Arbitrum / Avalanche / Botanix / MegaETH | 27.6M | 245.4M | 2.34B | 0.35% | 96.34% | 24.3M |
| 29 | TxFlow | TxFlow | 69.5M | 439.1M | 2.25B | 0.34% | 96.68% | 12.9M |
| 30 | Arcus | Robinhood Chain | 76.3M | 503.4M | 2.16B | 0.32% | 97.01% | 8.3M |
| 31 | Aark Digital | Arbitrum | 46.9M | 392.9M | 2.09B | 0.31% | 97.32% | n/a |
| 32 | Injective Orderbook | Injective | 71.9M | 448.5M | 1.45B | 0.22% | 97.54% | 3.8M |
| 33 | Orderly | Orderly Network | 30.3M | 273.8M | 1.40B | 0.21% | 97.75% | 39.2M |
| 34 | Gains Network | Arbitrum / Polygon / Base / ApeChain / MegaETH | 68.9M | 357.7M | 1.25B | 0.19% | 97.93% | 22.5M |
| 35 | dYdX (V4 only; V3 is 0) | dYdX chain | 83.7M | 138.9M | 1.19B | 0.18% | 98.11% | 54.0M |
| 36 | Decibel | Aptos | 70.6M | 342.9M | 1.19B | 0.18% | 98.29% | 4.4M |
| 37 | Polymarket (Polymarket Perps) | Polygon | 134.9M | 724.9M | 1.10B | 0.16% | 98.46% | 58.0M |
| 38 | Katana Perps | Katana | 22.8M | 165.9M | 1.04B | 0.16% | 98.61% | 0.8M |
| 39 | Phoenix | Solana | 29.5M | 207.2M | 1.01B | 0.15% | 98.76% | 23.4M |
| 40 | Derive | Derive Chain | 29.2M | 189.7M | 815.1M | 0.12% | 98.89% | 69.0M |
| 41 | Primit | Avalanche | 28.2M | 198.1M | 632.1M | 0.09% | 98.98% | 65.2M |
| 42 | Hibachi | Arbitrum / Base / Arc | 32.4M | 121.3M | 594.4M | 0.09% | 99.07% | 1.9M |
| 43 | TRUE DEX | Off Chain | 4.8M | 83.3M | 554.6M | 0.08% | 99.15% | n/a |
| 44 | Astros Protocol | Sui | 15.7M | 105.7M | 537.6M | 0.08% | 99.23% | 13.8M |
| 45 | TurboFlow | Solana / BSC | 7.7M | 61.9M | 518.1M | 0.08% | 99.31% | n/a |
| 46 | Ostium | Arbitrum | 4.2M | 33.5M | 510.2M | 0.08% | 99.39% | 7.9M |
| 47 | Gate DEX | GateLayer | 10.2M | 117.6M | 432.1M | 0.06% | 99.45% | 7.3M |
| 48 | KiloEx | BSC / opBNB / Base (+3 dead chains) | 12.6M | 77.6M | 407.0M | 0.06% | 99.51% | 0.5M |
| 49 | Paradex | Paradex | 61.4M | 59.4M | 297.6M | 0.04% | 99.56% | 8.6M |
| 50 | BULK | Bulk | 31.1M | 232.9M | 292.8M | 0.04% | 99.60% | 2.6M |

Below rank 50, the EVM-relevant names and their 30d volume: LeverUp (Monad) $246.6M,
SYMMIO $202.1M, Carbon.inc (Base) $160.1M, Somnex (Somnia) $131.1M, Aevo $105.9M,
Synthetix (V4 only) $97.1M, Fulcrom (Cronos / ZKsync / Cronos zkEVM) $89.3M,
HertzFlow (BSC) $86.0M, foxify (Sonic) $38.7M, Moonlander (Cronos) $25.1M,
Drake (Monad) $14.8M, MUX $12.1M, SparkDEX perps (Flare) $11.5M, Holdstation $6.0M,
Lynx $1.8M, Pika $1.8M.

Things worth noticing in the ranking:

* **Names that are at zero volume on DefiLlama today**: GMX V1, Synthetix V3
  (Base/Arbitrum on-chain perps), dYdX V3, Drift, MYX Finance, Vertex Edge, Level,
  HMX-style venues, Kwenta-era front ends, IntentX, Perennial, BSX, RabbitX, Mango,
  and ~100 others. The lead's memory list (Drift, MYX, Vertex, Kwenta, Bluefin...)
  is mostly stale: Bluefin is $39.4M and on Sui.
* DefiLlama flags Lighter and Paradex as "zero fee perp" venues (their volume is
  less costly to generate). Four entries are flagged double-counted (Helix, THENA,
  Quickswap, XTrade); they are negligible ($3.1M combined).
* Ostium's 30d figure ($510M) is far above its current run rate (7d $33.5M, 24h
  $4.2M). A third-party repository of exploit analyses contains Ostium contract
  sources (see appendix); I did not investigate what happened. Treat its volume as
  falling.
* Several entries are not really perp DEXes: Upscale is a prop-trading simulator
  (section 3), QFEX is a centralised exchange that uses a chain only for
  withdrawals.

## 3. Where the execution data lives

Categories:

* **A1** - matching and settlement logic run in EVM contracts; each trade, position
  change and liquidation is an event log. Indexable now.
* **A2** - matching is off-chain, **but the operator posts every fill on-chain as an
  event log**. For an indexer this is as good as A1 (often better: one contract,
  one event). This category is not in the lead's brief; the data forced it.
* **B** - EVM used for custody / proofs only. Trades are not visible as logs.
* **C** - own chain or non-EVM. API adapter needed.
* **D** - unclear.

"HS" = is the chain on the HyperSync supported-networks list (fetched 2026-09-19
00:29 UTC). A useful secondary signal used throughout: **how DefiLlama itself
computes the venue's volume** (its open-source adapter). If DefiLlama reads event
logs, the logs exist and the ABI is in the adapter; if it calls the venue's API, that
is usually because there is nothing on chain to read.

### 3.1 Top 50

| # | Venue | Cat | What is on-chain / where data lives | Evidence |
|---|---|---|---|---|
| 1 | Hyperliquid | C | HyperCore (own L1, native order book, not EVM). HyperEVM (chain id 999, HS: yes) is a separate execution environment; perp fills do **not** appear as HyperEVM logs. Public archives: S3 `s3://hl-mainnet-node-data/node_fills_by_block` (every fill), `explorer_blocks`, `replica_cmds`, `misc_events_by_block` (funding etc.); `s3://hyperliquid-archive` (L2 book snapshots, asset contexts, ~monthly, "no guarantee"). Requester pays, LZ4. Plus REST `api.hyperliquid.xyz/info` and WebSocket. | [Historical data docs](https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data); DefiLlama uses its own HL indexer + `api.hyperliquid.xyz/info` ([helper](https://github.com/DefiLlama/dimension-adapters/blob/master/helpers/hyperliquid.ts)) |
| 2 | Aster | C (with a small A1 side product) | Order-book perps now run on **Aster Chain**, its own L1 (PoSA, "Clearinghouse", per-market order books) with **account privacy**: docs say trader activity is publicly visible only "when Account Privacy is off", and that "certain operations remain offchain". Binance-style REST/WS API (`/fapi/v3/...`). The "1001x" mode is the old ApolloX on-chain oracle-priced product on BNB Chain (A1, ApolloX event family, see 4.8); DefiLlama does not split its volume out. | [docs export](https://docs.asterdex.com/llms-full.txt); [API docs repo](https://github.com/asterdex/api-docs); DefiLlama lists chain "Off Chain" |
| 3 | Lighter | B | zk rollup. Ethereum contract `0x3B4D794a66304F130a4Db8F2551B0070dfCf5ca7` shows deposits, withdrawals, batch commits, state roots; account **deltas** are published as blobs (L2BEAT reproduced state from them) - not fills. Robinhood Chain deployment (`0x94bab9693ba2f6358507effcbd372b0660afff9d`): `Deposit`, `WithdrawPending`, `BatchCommit`, `BatchVerification`, `BatchesExecuted`, `StateRootUpdate` only. API: `mainnet.zklighter.elliot.ai/api/v1` (`recentTrades` public; `trades` needs auth; per-account CSV export 12 months). Market-wide parquet trade dumps since genesis (2025-01-17) exist via `historicalTrade` but access requires an in-app transfer of 100 LIT. Third-party archives: Tardis, 0xArchive. | [Lighter historical data](https://apidocs.lighter.xyz/docs/historical-data); [L2BEAT Lighter](https://l2beat.com/scaling/projects/lighter); [L2BEAT Lighter Robinhood](https://l2beat.com/layer2s/projects/lighter-robinhood) |
| 4 | ApeX (Omni) | B | Off-chain order book, zk-proof settlement; on-chain side is custody on Ethereum/Arbitrum/Base/BNB/Mantle. DefiLlama computes volume from `omni.apex.exchange/api/v3/klines` and `/ticker`. Public REST v3 market data. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/apex-omni/index.ts) |
| 5 | edgeX | C/B | DefiLlama labels it "edgeX L1"; volume from `edgex-prod-v2.edgex.exchange/api/v2/public/quote/getKline`. No per-trade EVM logs found. Public REST (`/api/v2/public/...`). | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/edgeX-v2/index.ts) |
| 6 | Variational | B | RFQ and pricing off-chain; each counterparty pair clears into an escrow "settlement pool" on Arbitrum. Verified factory `0x0F820B9afC270d658a9fD7D16B1Bdc45b70f074C` emits `PoolCreated(...)`; no per-trade event is documented. Only API is `GET /metadata/stats`; no market-wide trade history. **Open point**: whether the SettlementPool implementation `0x8db6c8B7a085C3839e93EB8DaD45b93FB1ef5836` emits anything per trade - reading its event list on the explorer settles it. | [mainnet contracts](https://docs.variational.io/technical-documentation/mainnet-contracts); [API](https://docs.variational.io/technical-documentation/api); [factory on Blockscout](https://arbitrum.blockscout.com/address/0x0F820B9afC270d658a9fD7D16B1Bdc45b70f074C) |
| 7 | GMTrade | C | GMX's design ported to Solana programs (GitHub org `gmsol-labs`). Non-EVM. Data: Subsquid GraphQL `gmx-solana-sqd.squids.live/gmx-solana-base:prod/api/graphql`, market info API `market-info-mainnet-prod.gmtrade.xyz`. | [DefiLlama gmx-sol adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/gmx-sol.ts) |
| 8 | Grvt | B/C | Own zk chain (GRVT), off-chain book. Public market data API `market-data.grvt.io`. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/grvt-perps/index.ts) |
| 9 | Pacifica | C | Solana custody, off-chain book. REST `api.pacifica.fi/api/v1` (`/info`, `/kline`, prices). | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/pacifica/index.ts) |
| 10 | StandX | D (leaning C) | Marketing says "fully onchain order book" but no chain id, explorer, RPC or contract address for a "StandX chain" was found; BNB Chain and Solana look like deposit rails. REST `perps.standx.com/api` incl. market-wide `query_recent_trades`; WS. Settled by: a published explorer/RPC. | [API docs](https://docs.standx.com/standx-api/perps-http) |
| 11 | **Nado** | **A2** | Ink (chain id 57073, HS: **yes**). Off-chain sequencer; batches submitted through `Endpoint.submitTransactionsChecked`; **`OffchainExchange` emits one `FillOrder` log per fill** (25 seen in one batch tx `0xd86b6273...cf1de5`, block 56279573). Also `Liquidation` (Clearinghouse) and `FundingPayment` (PerpEngine). Vertex-style architecture; source public, GPL-2.0-or-later. Archive API also exists (`archive.prod.nado.xyz/v1`). | [contracts](https://docs.nado.xyz/more/contracts); [source](https://github.com/nadohq/nado-contracts/blob/main/core/contracts/interfaces/IOffchainExchange.sol); [OffchainExchange on explorer](https://explorer.inkonchain.com/address/0x8373C3Aa04153aBc0cfD28901c3c971a946994ab) |
| 12 | Antarctic | B | Litepaper: orders batched and matched off-chain; zk rollup posts state roots and proofs. Deposit contracts on Arbitrum/Ethereum/BNB (addresses not found). Public API is a roadmap item. | [docs export](https://docs.antarctic.exchange/llms-full.txt) |
| 13 | Extended | C | Starknet (Cairo, non-EVM); earlier StarkEx on Ethereum. REST `api.starknet.extended.exchange/api/v1` (markets, stats). | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/extended/index.ts) |
| 14 | AZverse | D (leaning B) | "Execute on AXIS. Settle where assets live." No architecture, contract or API docs. DefiLlama reads a private stats endpoint. | [site](https://azverse.xyz/en); [DefiLlama helper](https://github.com/DefiLlama/dimension-adapters/blob/master/helpers/azverse.ts) |
| 15 | Jupiter Perps | C | Solana program, oracle-priced pool (JLP). Volume via `perp-api.jup.ag`. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/jupiter-perpetual/index.ts) |
| 16 | QFEX | C (centralised) | Matching engine on AWS; custodians hold funds; chain is only a withdrawal rail. Market-wide trades over WebSocket. | [architecture](https://docs.qfex.com/qfex/architecture) |
| 17 | Upscale | not a venue | Prop-firm evaluation: "all trading is conducted strictly in a simulated environment". Nothing to index. | [site](https://upscale.trade) |
| 18 | Ondo Perps | C (centralised custody) | Matching engine in an SGX enclave; an omnibus account on Ethereum holds collateral; docs: "There is no pooled smart contract governing user funds". API `api.ondoperps.xyz/v1`: market-wide paginated trades (REST) + WS. | [architecture](https://docs.ondoperps.xyz/architecture.md) |
| 19 | RISEx | A1 (ABI unknown) | RISE L2 (chain id 4153, HS: **no**). Docs: orders are transactions to `OrdersManager` `0xE03C1D5081eb2d0E6bFd62A949C5b12eFa44F2cD` / `PerpsManager` `0x53f10fAcFC8965750494E6965F5d6dA39B41d852`; both emit heavy log traffic but are **unverified** on the explorer, so event names are unknown. DefiLlama uses the API (`api.rise.trade`), not logs. | [deployments](https://docs.risechain.com/docs/risex/contracts/deployments) |
| 20 | SoDEX (SoSoValue) | C | ValueChain: matching on dedicated order-book app-chains "not on the EVM layer". REST recent trades (max 500), WS. | [how ValueChain works](https://sodex.com/documentation/about-valuechain/how-valuechain-works) |
| 21 | Reya | A1 (moving to A2) | Reya Network (Arbitrum Orbit, chain id 1729, public RPC `rpc.reya.network`, `eth_getLogs` capped at 2,000 blocks, HS: **no**). DefiLlama computes volume from logs: `PassivePerpMatchOrder` on `0x27e5cb712334e101b3c232eb0be198baaa595f5f`. **Reya's docs announce a cut-over on 2026-09-28** to an order-book contract (`perpOB`) with an off-chain matcher where "each fill is validated and settled on-chain"; new mainnet event names are not yet documented. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/reya-dex.ts); [Reya developer docs](https://docs.reya.xyz/developers/mainnet/readme.md) |
| 22 | Vest | B | Matching and pricing off-chain, zk-verified; Base holds balances and commitments. Docs: orders "can easily be stored on-chain in the future". REST `serverprod.vest.exchange/v2`. | [architecture](https://docs.vest.exchange/overview/vest-architecture/overview) |
| 23 | **Avantis** | **A1** | Base (HS: yes). Gains-shaped: `Trading`, `TradingStorage` `0x8a311D7048c35985aa31C131B9A13e03a5f7422d`, `TradingCallbacks` `0x0C16ff40065Cc3Ab4bc55B60E447504AFB9C7970` emit `MarketExecuted` / `LimitExecuted`. Note: `docs.avantisfi.com` now redirects to `docs.veranta.xyz` (rebrand in progress, not confirmed by me). | [official trades indexer + ABIs](https://github.com/Avantis-Labs/avantis-trades-indexer) |
| 24 | SunPerp / SunX | B/C | Tron (HS lists Tron, but) volume comes from `api.sunperp.com`; off-chain book. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/sunperp/index.ts) |
| 25 | **Perpl** | **A1** | Monad (chain id 143, HS: yes). On-chain order book in one contract `0x34B6552d57a35a1D042CcAe1951BD1C370112a6F` (start block 54773010, unverified on MonadScan, but full ABI is in the MIT-licensed SDK). DefiLlama reads logs. | [SDK ABI](https://github.com/PerplFoundation/dex-sdk/tree/main/crates/sdk/abi/dex); [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/perpl/index.ts) |
| 26 | **SynFutures V3** | **A1** | Base (HS: yes; Blast sunset 2025-04-11). On-chain AMM + order book ("Oyster AMM"); one `Instrument` contract per market, enumerated from Gate `0x208B443983D8BcC8578e9D86Db23FbA547071270`; each emits `Trade`. DefiLlama reads logs. Fork: Monday Trade (0 volume). | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/synfutures-v3/index.ts) |
| 27 | Rocky | C | Canton (non-EVM, privacy by design), off-chain matching, settlement batched every 5 s. No public per-trade ledger. | [site](https://rocky.exchange) |
| 28 | **GMX V2** | **A1** | Arbitrum, Avalanche, MegaETH (HS: yes), Botanix (HS: no). All events go through one `EventEmitter` per chain. | [EventEmitter.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/event/EventEmitter.sol) |
| 29 | TxFlow | C | Own DAG-based L1; API "coming soon"; DefiLlama reads a Dune query. | [docs](https://docs.txflow.com) |
| 30 | Arcus | B | Robinhood Chain (Arbitrum Orbit, chain id 4663, HS: yes) carries only a Checkpoint Manager and a Bridge Vault. Matching is an off-chain engine + permissioned app-chain. **Good public API**: `GET api.arcus.xyz/v1/trades` market-wide, no auth, time-paged. | [architecture](https://docs.arcus.xyz/concepts/exchange-architecture); [trades API](https://docs.arcus.xyz/api-reference/public/get-recent-public-trades) |
| 31 | **Aark** | **A1** | Arbitrum (HS: yes). `FuturesManager` `0x0b848a8A5eC8950E67d19E7a21A6Be29F44F685e` emits `MoonOrderOpenedV2` / `MoonOrderClosedV2`. DefiLlama reads logs. The DefiLlama figure only counts this "Moon" (1000x) product; Aark's order-book "Perpetual Mode" is, per its docs, "Powered by Orderly Network" and never touches Arbitrum. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/aark/index.ts) |
| 32 | Injective | C | Cosmos chain native order book module (HS lists an Injective EVM, but the exchange module is not EVM logs). Public indexer API. | DefiLlama label |
| 33 | Orderly | A2 (chain not on HS) | Off-chain engine; every fill is uploaded to the Orderly L2 (OP-stack, chain id 291, public RPC `rpc.orderly.network`, HS: **no**) where the Ledger `0x6F7a338F2aA472838dEFD3283eB360d4Dff5D203` emits `ProcessValidatedFutures` per account-side fill. See 3.2 and 4.9. | [ILedgerEvent.sol](https://github.com/OrderlyNetwork/contract-evm/blob/main/src/interface/ILedgerEvent.sol); [addresses](https://orderly.network/docs/build-on-omnichain/addresses) |
| 34 | **Gains** | **A1** | Arbitrum, Base, Polygon, MegaETH (HS: yes), ApeChain (HS: no). One diamond per chain. | [official SDK ABI](https://www.npmjs.com/package/@gainsnetwork/sdk) |
| 35 | dYdX V4 | C | Cosmos app-chain. Public indexer REST/WS `indexer.dydx.trade/v4` (market-wide trades per market). | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/dydx-v4/index.ts) |
| 36 | Decibel | C | Aptos (Move). Aptos GraphQL + `api.mainnet.aptoslabs.com/decibel/api/v1`. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/decibel/index.ts) |
| 37 | Polymarket Perps | B | Launched 2026-09-03. Docs: "Trading itself does not produce per-trade onchain transactions." Polygon shows deposits, withdrawals and state-root commitments only. API `api.perpetuals.polymarket.com/v1/info/...` (rolling 24h snapshots per DefiLlama's adapter). | [architecture](https://docs.polymarket.com/perps/learn-about-trading/architecture) |
| 38 | **Katana Perps** | **A2** | Katana (chain id 747474, HS: yes). Runs on IDEX technology (Katana acquired IDEX). Off-chain matching, but the Exchange contract (`0x62230CeA619F734cc215bB8074bbF07bE4Eb633e` since 2026-04-27, before that `0x835Ba5b1B202773A94Daaa07168b26B22584637a`) emits `TradeExecuted` per fill. DefiLlama reads logs. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/katana-perps.ts) |
| 39 | Phoenix | C | Solana. `perp-api.phoenix.trade/v1`. | DefiLlama adapter |
| 40 | Derive | A2 (chain not on HS) | Off-chain book; each match settles on Derive Chain (OP-stack, chain id 957, public RPC `rpc.derive.xyz`, HS: **no**). TradeModule `0xB8D20c2B7a1Ad2EE33Bc50eF10876eD3035b5e7b` emits `OrderMatched(address base, uint taker, uint maker, bool takerIsBid, int amtQuote, uint amtBase)`; perps and options share the event. API `public/get_trade_history` returns `tx_hash`. | [ITradeModule.sol](https://github.com/derivexyz/v2-matching/blob/master/src/interfaces/ITradeModule.sol); [API](https://docs.derive.xyz/reference/public-get_trade_history) |
| 41 | **Primit** | **A2** | Avalanche C-Chain (HS: yes). Fills matched off-chain (own engine or routed to Orderly) and then **recorded** on-chain by `TradeRecorder` `0xC005A9bb11f162329f3EfCCc35F69F9Bb635EeC6` as `TradeRecorded`. It is a self-published log, not settlement: it proves nothing, but it is indexable. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/primit-perps/index.ts) |
| 42 | Hibachi | B | Off-chain book with ZK proofs (Succinct SP1), encrypted data on Celestia; custody on Arbitrum/Base/Arc. Market-wide public trades via API (max 100 per call). Volume from `data-api.hibachi.xyz`. | [DefiLlama adapter](https://github.com/DefiLlama/dimension-adapters/blob/master/dexs/hibachi/index.ts) |
| 43 | TRUE DEX | C | DefiLlama: off-chain matching, stats API. | DefiLlama adapter |
| 44 | Astros | C | Sui. | DefiLlama label |
| 45 | TurboFlow | D | BSC + Solana; adapter not log-based. Not investigated further (0.08%). | |
| 46 | **Ostium** | **A1** | Arbitrum (HS: yes). Gains-shaped, public source. | [IOstiumTradingCallbacks.sol](https://github.com/0xOstium/smart-contracts-public/blob/main/src/interfaces/IOstiumTradingCallbacks.sol) |
| 47 | Gate DEX | D (leaning B) | GateLayer is an EVM chain (chain id 10088, HS: no). Marketing says executed orders are "recorded on-chain"; no contract address or event spec found. Volume from `api.gateperps.com`. Settled by: a ledger contract on GateScan showing per-fill logs. | [docs](https://gateweb3.gitbook.io/docs/perps/en) |
| 48 | **KiloEx** | **A1** | BSC, opBNB, Base (HS: yes; Manta/Taiko/B2 marked dead by DefiLlama in 2026). `PerpTrade` contract emits `IncreasePositionV3` / `DecreasePositionV3` / `PositionLiquidated` (Pika-style lineage). DefiLlama uses KiloEx's API, not logs. | [official SDK ABI](https://github.com/KiloExPerp/kiloex-python-sdk/blob/main/abi/PerpTrade.abi) |
| 49 | Paradex | C | Starknet app-chain (Cairo). Public REST/WS. | DefiLlama label |
| 50 | BULK | C | Own chain / Solana custody; `mainnet-api1.bulk.trade`. | DefiLlama adapter |

### 3.2 Hybrids the lead asked about

* **Hyperliquid HyperEVM vs HyperCore.** HyperSync serves HyperEVM (999). It
  contains ERC-20 transfers, HyperEVM DEX swaps, and bridge movements between
  HyperCore and HyperEVM. It does not contain perp fills. Anything perp-related
  must come from the S3 archives or the info API.
* **Aster.** Two products under one name: the order book (now on Aster Chain, with
  privacy mode) and the legacy on-chain "1001x" product on BNB Chain. Only the
  second is EVM logs, and DefiLlama does not report it separately, so its size is
  unknown (appendix: could not verify).
* **Orderly and its front ends.** Orderly is one shared off-chain order book;
  custody vaults sit on many EVM chains, and the engine uploads trades in batches
  (`futuresTradeUpload`) to the Orderly L2 (OP-stack, chain id 291), where the Ledger
  emits **one `ProcessValidatedFutures` log per account-side fill**, plus
  `LiquidationResult(V2)`, `AdlResult(V2)`, `SettlementResult`. `accountId` is a hash
  of broker + address and `AccountRegister` maps it to a `brokerId`, so fills can be
  attributed to front ends from logs alone. So Orderly is A2 - but on a chain
  HyperSync does not serve (it has a public RPC). DefiLlama itself uses the API
  (`api-evm.orderly.org/md/volume/daily_stats`, and per-builder
  `api.orderly.org/md/volume/builder/daily_stats?broker_id=...`). Front ends listed
  in DefiLlama's `factory/orderly.ts`: WOOFi Pro, Raydium Perps, What Exchange,
  Honeypot, VEAX, OKLong, Coin360, Toro, Velto, Baumz, BabyDoge, Clober, SalsaDex,
  PerpTools, Nexus, MemeMax, Primit, RWAPerp, Halfmoon; standalone adapters exist for
  Kodiak perps, ADEN and QuickSwap perps (QuickSwap is one of DefiLlama's
  double-count flags for this reason); Aark's order-book mode also routes through
  Orderly. Treat "Orderly" as ONE venue, $1.40B. **Primit appears in that Orderly
  broker list and also as its own entry ($632M) - part of Primit's volume is
  therefore probably inside Orderly's number too** (not flagged by DefiLlama).
* **Synthetix.** On-chain Perps V2 (Optimism) and V3 (Base/Arbitrum) are at **zero**
  volume. "Synthetix V4" ($97M) is on Ethereum mainnet and DefiLlama reads it from
  `papi.synthetix.io/v1/info` - an off-chain order book (category B). Kwenta and the
  other Synthetix front ends have no current volume. The Synthetix event family is
  therefore historical-only; I did not deep-dive it.
* **GMX forks.** DefiLlama lists 64 forks of GMX V1 and 9 of GMX V2. Live volume:
  V1 family = Fulcrom $89.3M (Cronos, ZKsync, Cronos zkEVM) and dust; GMX's own V1 is
  0. V2 family = GMX $2.34B + HertzFlow $86M (BSC, EventEmitter
  `0xf6030850365F79E7a8CAB31850A063199fd0CC10`). GMTrade ($26B) is the GMX design on
  Solana - same concepts, not EVM.
* **Gains forks.** DefiLlama lists 4 (KRAV, Gambit Trade, MyMetaTrader, Artura), all
  at zero. But Avantis and Ostium are Gains-*shaped* (same storage/callbacks split,
  same oracle-callback flow, similar event names with different signatures), and
  Holdstation and Lynx are in the same style.
* **ApolloX / APX shape.** DefiLlama lists Moonlander (Cronos) as a fork of "APX".
  LeverUp (Monad) emits events with ApolloX names (`OpenMarketTrade`,
  `CloseTradeSuccessful`, `ExecuteCloseSuccessful`) - same shape, extended structs.
  Aster's 1001x mode is the original.
* **SYMMIO.** Intent-based bilateral perps, fully on-chain events on a diamond per
  chain (Base, BSC, Arbitrum, Mantle, Mode, Polygon, Sonic, Berachain, Coti). Front
  ends per SYMMIO's deployments page: Thena (BSC), Based, Befi, BMX, Pear, Vibe and
  **Carbon** (MultiAccount on Base `0x39EcC772f6073242d6FD1646d81FA2D87fe95314`).
  Carbon.inc is the renamed IntentX: DefiLlama's adapter reads a field called
  `intentXFEOIAnalytics` from the Carbon app and `factory/symmio.ts` lists
  `carbon-perps` on Base. So Carbon.inc's $160M is SYMMIO-family volume and is
  indexed through the SYMMIO diamond; whether it overlaps the $202M SYMMIO entry is
  not stated by DefiLlama ([SYMMIO deployments](https://docs.symm.io/api-endpoints-and-deployments/symmio-perps-deployments.md)).
* **Polymarket Perps, Derive, Aevo.** DefiLlama reads all three from the venue API.
  Polymarket Perps (launched 2026-09-03) is category B by its own docs. Derive
  settles every match on its own OP-stack chain with an `OrderMatched` log (A2, not
  on HyperSync). Aevo's docs say matched orders "get posted on Aevo's smart
  contracts" on its OP-stack L2, but no contract source, address or event name was
  found (B/D; settled by decoding one settlement tx on `explorer.aevo.xyz`).

### 3.3 Totals by category (30 days)

| Bucket | Venues | 30d volume | Share of all perp volume |
|---|---|---|---|
| A1 on HyperSync chains | Avantis, Perpl, SynFutures, GMX V2, Aark, Gains, Ostium, KiloEx, LeverUp, SYMMIO, HertzFlow | $14.85B | 2.23% |
| A2 on HyperSync chains | Nado, Katana Perps, Primit | $11.15B | 1.67% |
| **Reachable with HyperSync today (A1 + A2)** | | **$26.00B** | **3.90%** |
| A1 / A2 on EVM chains HyperSync does not serve | RISEx, Reya, Orderly (L2), Derive (L2, incl. options), Somnex, Fulcrom (Cronos part), Moonlander | $8.46B | 1.27% |
| Small A1 tail (Carbon.inc via SYMMIO, MUX, SparkDEX, Holdstation, Lynx, Pika) | | $0.19B | 0.03% |
| **Ceiling for "EVM logs at all"** | | **$34.66B** | **5.20%** |
| Everything else (B, C, D) | | $632.0B | 94.8% |

Caveats on the 3.90%: GMX and Gains figures include their Botanix / ApeChain
volume, which HyperSync does not serve (DefiLlama's page does not give the
per-chain split, and the per-chain API is paywalled), so the true figure is slightly
lower. Aster's on-chain 1001x product is not counted because its size is unknown,
which pushes the other way. Primit is probably partly double-counted with Orderly.
Somnex and Moonlander were placed in A1 from DefiLlama's fork/chain metadata, not
from inspected contracts. Reya changes architecture on 2026-09-28.

## 4. Event families (category A)

Signature status legend: **SRC** = read in the protocol's own public source repo or
official SDK ABI; **ADAPTER** = only seen in DefiLlama's adapter (UNVERIFIED against
verified contract source); **UNVERIFIED** = name known, full signature not seen.
None has been keccak-checked yet.

Family volumes and shares (30d; "of A" = of the $34.66B ceiling):

| Family | Members with volume | Chains (HS) | 30d | Of all perps | Of A |
|---|---|---|---|---|---|
| Vertex-style `FillOrder` | Nado | Ink (yes) | $9.48B | 1.42% | 27.4% |
| Gains-shaped | Avantis 2.77B, Gains 1.25B, Ostium 0.51B (+Holdstation, Lynx dust) | Base, Arbitrum, Polygon, MegaETH (yes); ApeChain (no) | $4.53B | 0.68% | 13.1% |
| RISEx | RISEx | RISE (no) | $3.08B | 0.46% | 8.9% |
| Reya | Reya | Reya Network (no) | $2.93B | 0.44% | 8.5% |
| Perpl on-chain CLOB | Perpl | Monad (yes) | $2.53B | 0.38% | 7.3% |
| GMX V2 EventEmitter | GMX 2.34B, HertzFlow 0.09B | Arbitrum, Avalanche, MegaETH, BSC (yes); Botanix (no) | $2.43B | 0.36% | 7.0% |
| SynFutures V3 | SynFutures | Base (yes) | $2.41B | 0.36% | 7.0% |
| Aark | Aark | Arbitrum (yes) | $2.09B | 0.31% | 6.0% |
| IDEX-style `TradeExecuted` | Katana Perps | Katana (yes) | $1.04B | 0.16% | 3.0% |
| Primit recorder | Primit | Avalanche (yes) | $0.63B | 0.09% | 1.8% |
| Pika / KiloEx | KiloEx (+Pika dust) | BSC, opBNB, Base (yes) | $0.41B | 0.06% | 1.2% |
| Orderly Ledger | Orderly (one book, ~20 front ends) | Orderly L2 (no) | $1.40B | 0.21% | 4.0% |
| Derive `OrderMatched` | Derive (perps + options) | Derive Chain (no) | $0.82B | 0.12% | 2.4% |
| SYMMIO | SYMMIO 0.20B + Carbon.inc 0.16B (overlap unknown) | 9 EVM chains (most yes) | $0.20-0.36B | 0.03-0.05% | 0.6-1.0% |
| ApolloX-shaped | LeverUp 0.25B, Moonlander 0.03B (+ Aster 1001x, unknown) | Monad, BSC (yes); Cronos (no) | $0.27B+ | 0.04% | 0.8% |
| GMX V1 Vault | Fulcrom 0.09B, MUX (aggregator), dust | Cronos (no), ZKsync (yes), many | $0.09B | 0.01% | 0.3% |
| Synthetix Perps V2 / V3 | none | Optimism, Base, Arbitrum | $0 | 0% | 0% |

### 4.1 Vertex-style `FillOrder` (Nado) - A2

* Contracts on Ink: Endpoint `0x05ec92D78ED421f3D3Ada77FFdE167106565974E`,
  Clearinghouse `0xD218103918C19D0A10cf35300E4CfAfbD444c5fE`, OffchainExchange
  `0x8373C3Aa04153aBc0cfD28901c3c971a946994ab`, PerpEngine
  `0xF8599D58d1137fC56EcDd9C16ee139C8BDf96da1`, SpotEngine
  `0xFcD94770B95fd9Cc67143132BB172EB17A0907fE`.
* Events (**SRC**, [IOffchainExchange.sol](https://github.com/nadohq/nado-contracts/blob/main/core/contracts/interfaces/IOffchainExchange.sol), [IClearinghouseEventEmitter.sol](https://github.com/nadohq/nado-contracts/blob/main/core/contracts/interfaces/clearinghouse/IClearinghouseEventEmitter.sol), [IPerpEngine.sol](https://github.com/nadohq/nado-contracts/blob/main/core/contracts/interfaces/engine/IPerpEngine.sol)):
  * `FillOrder(uint32 indexed productId, bytes32 indexed digest, bytes32 indexed subaccount, int128 priceX18, int128 amount, uint64 expiration, uint64 nonce, uint128 appendix, bool isolated, bool isTaker, int128 feeAmount, int128 baseDelta, int128 quoteDelta)` - topic0 observed on the explorer: `0xb563bd3722620e7af6c3dae109897ca2f45fbbc5975fb6553bb2d53b77e54bf3`.
  * `Liquidation(bytes32 indexed liquidatorSubaccount, bytes32 indexed liquidateeSubaccount, uint32 productId, bool isEncodedSpread, int128 amount, int128 amountQuote)`
  * `FundingPayment(uint32 productId, uint128 dt, int128 openInterest, int128 payment)`
  * `ModifyCollateral(int128 amount, bytes32 indexed subaccount, uint32 productId)`
* Fields: trader = first 20 bytes of `subaccount` (bytes32 = address + 12-byte
  name); market = `productId` (perp vs spot products share the id space - the same
  event covers Nado spot); side = sign of `baseDelta`; size = `|baseDelta|` (1e18),
  notional = `|quoteDelta|` (1e18); price `priceX18`; fee `feeAmount`; maker/taker
  flag. Two logs per match (maker and taker) - count taker only for volume.
* Funding and OI: **yes, both from events** (`FundingPayment` carries cumulative
  open interest and the payment per product).
* Not available: realised PnL (must be derived from the running position), leverage,
  liquidation price. Increase vs decrease must be derived from the running position.
* Traps: `tx.from` is always the sequencer; spot and perp products mixed; a product
  id to symbol map must come from the API or `AddOrUpdateProduct` + engine calls;
  because the sequencer is trusted to post fills, a delayed or halted sequencer means
  delayed logs. This is the same ABI lineage as Vertex (now zero volume) and its
  DefiLlama-listed fork Blitz, so history on Arbitrum/Blast/Mantle/Sei could be
  back-filled with the same decoder - signature equality with Vertex is UNVERIFIED.
* Effort: **S/M**.

### 4.2 GMX V2 EventEmitter

* One `EventEmitter` per chain: Arbitrum `0xC8ee91A54287DB53897056e12D9819156D3822Fb`,
  Avalanche `0xDb17B211c34240B014ab6d61d4A31FA0C0e20c26`, MegaETH
  `0xAf2E131d483cedE068e21a9228aD91E623a989C2` (from GMX's SDK config), Botanix
  `0xAf2E131d483cedE068e21a9228aD91E623a989C2`; HertzFlow on BSC
  `0xf6030850365F79E7a8CAB31850A063199fd0CC10`.
* Events (**SRC**, [EventEmitter.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/event/EventEmitter.sol)):
  * `EventLog(address msgSender, string eventName, string indexed eventNameHash, EventUtils.EventLogData eventData)`
  * `EventLog1(address msgSender, string eventName, string indexed eventNameHash, bytes32 indexed topic1, EventUtils.EventLogData eventData)` - topic0 per DefiLlama's HertzFlow adapter `0x137a44067c8961cd7e1d876f4754a5a3a75989b4552f1843fc69c3b372def160`
  * `EventLog2(... bytes32 indexed topic1, bytes32 indexed topic2, EventUtils.EventLogData eventData)`
  * `EventLogData` is seven key/value bags (address, uint, int, bool, bytes32, bytes,
    string), each with `items` and `arrayItems`. The logical event is the
    `eventName` string; `topics[1]` is `keccak(eventName)` so it can be filtered.
* Logical events we need ([PositionEventUtils.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/position/PositionEventUtils.sol), [MarketEventUtils.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/market/MarketEventUtils.sol), [OrderEventUtils.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/order/OrderEventUtils.sol)):
  `PositionIncrease`, `PositionDecrease`, `PositionFeesCollected`,
  `PositionFeesInfo`, `InsolventClose`, `OrderCreated` / `OrderExecuted` /
  `OrderCancelled` / `OrderFrozen`, `OpenInterestUpdated`,
  `OpenInterestInTokensUpdated`, `Funding`, `Borrowing`,
  `FundingFeeAmountPerSizeUpdated`, `CumulativeBorrowingFactorUpdated`,
  `MarketPoolValueInfo`, plus `MarketCreated` from the factory.
* Fields: `account`, `market` (the market token address; index/long/short tokens come
  from `MarketCreated`), `collateralToken`, `isLong`, `sizeDeltaUsd` and `sizeInUsd`
  (**1e30**), `sizeDeltaInTokens`, `collateralAmount` / `collateralDeltaAmount`
  (collateral token decimals), `executionPrice` and `indexTokenPrice.min/max`
  (1e30 divided by token decimals - i.e. per-token-unit prices, a classic mistake),
  `priceImpactUsd`, `basePnlUsd` (realised PnL on decrease), `orderType`,
  `orderKey`, `positionKey`. Fees (position, borrowing, funding, UI, referral) come
  from `PositionFeesCollected` joined by `orderKey`.
* Liquidation flag: `PositionDecrease` with `orderType == 7` (`Liquidation` in
  [Order.sol](https://github.com/gmx-io/gmx-synthetics/blob/main/contracts/order/Order.sol): MarketSwap 0, LimitSwap 1, MarketIncrease 2,
  LimitIncrease 3, MarketDecrease 4, LimitDecrease 5, StopLossDecrease 6,
  Liquidation 7, StopIncrease 8).
* Funding and OI: **both derivable from events alone** (`OpenInterestUpdated`
  carries delta and next value per market/collateral/side; funding and borrowing
  factor updates are emitted whenever market state is touched).
* Traps: keepers execute, so `tx.from` is never the trader - use `account`; two-step
  flow (`OrderCreated` tx, then `OrderExecuted` + `PositionIncrease` in a keeper tx);
  key names inside the bags have changed across releases (e.g.
  `pendingPriceImpactUsd` in current `PositionIncrease`, the repo has `v2.2`/`v2.3`
  branches) so decode **by key name, tolerate missing keys, never by index**; the
  same emitter also carries swaps, deposits, withdrawals, GLV and config events (high
  log volume - filter on `topics[1]`); `string indexed` means topics[1] is a hash;
  oracle-priced, so there are no native candles; ADL shows up as decrease orders.
  The generic ABI decode of nested dynamic tuples is the main cost.
* Effort: **M**.

### 4.3 Gains-shaped (Gains, Avantis, Ostium)

Shared shape: trader calls `Trading`; an oracle callback executes in
`TradingCallbacks`; open trades are identified by `(trader, pairIndex, index)` or
`(user, index)`; pairs are numeric indexes into a `PairsStorage` that must be read to
get symbols. Signatures differ per protocol and per version, so this is one decoder
*pattern* with three signature sets.

**Gains (gTrade)** - one diamond per chain: Arbitrum
`0xFF162c694eAA571f685030649814282eA457f169`, Base
`0x6cD5aC19a07518A8092eEFfDA4f1174C72704eeb`, Polygon
`0x209A9A01980377916851af2cA075C2b170452018`, MegaETH
`0x2D5B1ba6E2093a5b927Fe5bF8C049B107de31eaF`, ApeChain
`0x2BE5D7058AdBa14Bc38E4A83E94A81f7491b0163` (**SRC**: `@gainsnetwork/sdk` 1.8.10
`addresses.json` and `GNSMultiCollatDiamond` ABI, 177 events). Key events, canonical
types:

* `MarketExecuted((address,uint32),address,uint32,(address,uint32,uint16,uint24,bool,bool,uint8,uint8,uint120,uint64,uint64,uint64,bool,uint160,uint24),bool,uint256,uint256,uint256,(uint256,int256,int256,int256,int256,uint64),int256,uint256,uint256)`
  = `(orderId, user indexed, index indexed, Trade t, bool open, oraclePrice, marketPrice, liqPrice, PriceImpact priceImpact, int256 percentProfit, amountSentToTrader, collateralPriceUsd)`
* `LimitExecuted((address,uint32),address,uint32,uint32,(Trade),address,uint8,uint256,uint256,uint256,(PriceImpact),int256,uint256,uint256,bool)`
  - `orderType` tells TP / SL / **LIQ** / limit open.
* `PositionSizeIncreaseExecuted(...)`, `PositionSizeDecreaseExecuted(...)`,
  `LeverageUpdateExecuted(...)` - partial size changes (full signatures in the SDK
  ABI; long nested tuples).
* `TradeStored`, `TradeClosed(address indexed user, uint32 indexed index, bool isPnlPositive)`, `TradeCollateralUpdated`.
* OI: `PairOiAfterV10Updated(uint8 indexed collateralIndex, uint16 indexed pairIndex, uint256 oiDeltaCollateral, uint256 oiDeltaToken, bool open, bool long, (uint128,uint128) newOiCollateral, (uint128,uint128) newOiToken)`, `BorrowingPairOiBeforeV10Updated`, `BorrowingGroupOiUpdated`.
* Funding / borrowing: `PendingAccFundingFeesStored(uint8 indexed collateralIndex, uint16 indexed pairIndex, (int128 accFundingFeeLongP, int128 accFundingFeeShortP, int56 lastFundingRatePerSecondP, uint32 lastFundingUpdateTs, uint168))`, `BorrowingPairAccFeesUpdated`, `TradingFeesRealized`, `HoldingFeesChargedOnTrade`.
* Fields: `Trade` = user, index, pairIndex, leverage (1e3), long, collateralIndex,
  tradeType, collateralAmount (collateral decimals), openPrice (1e10), tp, sl,
  positionSizeToken. Size USD = collateral x leverage x `collateralPriceUsd` (1e8).
  Realised PnL = `percentProfit` (1e10) x collateral; payout = `amountSentToTrader`.
* Traps: **signatures changed at almost every release** (the ABI itself has
  `...BeforeV10...` / `...AfterV10...` events); old history on Polygon/Arbitrum
  predates the diamond (v6 separate contracts). Multi-collateral: amounts are in DAI,
  WETH, USDC or GNS depending on `collateralIndex`. Forex / stocks / commodities
  pairs - there is no index token address, only a pair index. Counter-trades and
  partial closes. Building full history needs several signature generations.

**Avantis** (Base) - **SRC**: [TradingCallbacks_v1_5 ABI in Avantis' own indexer](https://github.com/Avantis-Labs/avantis-trades-indexer/blob/main/abis/TradingCallbacks_v1_5.ts):

* `MarketExecuted(uint256 orderId, (address trader, uint256 pairIndex, uint256 index, uint256 initialPosToken, uint256 positionSizeUSDC, uint256 openPrice, bool buy, uint256 leverage, uint256 tp, uint256 sl, uint256 timestamp) t, bool open, uint256 price, uint256 positionSizeUSDC, int256 percentProfit, uint256 usdcSentToTrader, bool isPnl)`
* `LimitExecuted(uint256 orderId, uint256 limitIndex, (Trade) t, uint8 orderType, uint256 price, uint256 positionSizeUSDC, int256 percentProfit, uint256 usdcSentToTrader, bool isPnl)`
* USDC-only (6 decimals), which makes it simpler than Gains. Avantis' open-source
  Ponder indexer is a ready reference for handler logic (`Trading_v2`,
  `TradingStorage_v1_5`, `Referral`). Note the version suffixes: older signature sets
  exist. Funding/OI events were not inspected (UNVERIFIED).

**Ostium** (Arbitrum) - **SRC**: [IOstiumTradingCallbacks.sol](https://github.com/0xOstium/smart-contracts-public/blob/main/src/interfaces/IOstiumTradingCallbacks.sol):

* `MarketOpenExecuted(uint256 indexed orderId, IOstiumTradingStorage.Trade t, uint256 priceImpactP, uint256 tradeNotional)`
* `MarketCloseExecuted(uint256 indexed orderId, uint256 indexed tradeId, uint256 price, uint256 priceImpactP, int256 percentProfit, uint256 usdcSentToTrader)` and `MarketCloseExecutedV2(..., uint256 percentageClosed)`
* `LimitOpenExecuted(uint256 indexed orderId, uint256 limitIndex, Trade t, uint256 priceImpactP, uint256 tradeNotional)`, `LimitCloseExecuted(uint256 indexed orderId, uint256 indexed tradeId, LimitOrder orderType, uint256 price, uint256 priceImpactP, int256 percentProfit, uint256 usdcSentToTrader)` (`orderType` distinguishes TP / SL / LIQ)
* `FeesCharged(uint256 indexed orderId, uint256 indexed tradeId, address indexed trader, uint256 rolloverFees, int256 fundingFees)` and `FeesChargedV2(... int256 rolloverFees, int256 fundingFees)`; `VaultLiqFeeCharged`, `DevFeeCharged`, `VaultOpeningFeeCharged`, `OracleFeeCharged`, `BuilderFeeCharged`.
* `Trade` = `(uint256 collateral /*1e6*/, uint192 openPrice /*1e18*/, uint192 tp, uint192 sl, address trader, uint32 leverage /*1e2*/, uint16 pairIndex, uint8 index, bool buy, bool isDayTrade)`.
* Traps: close events carry only `tradeId` - trader and pair must be joined from the
  open event; V1 and V2 events coexist; RWA pairs have market hours.

Effort for the group: **M** for Gains (current generation only), **S** each for
Avantis and Ostium once the pattern exists; **L** if full Gains history is wanted.

### 4.4 Perpl (Monad on-chain order book)

* One contract `0x34B6552d57a35a1D042CcAe1951BD1C370112a6F`; **SRC**: [Exchange.json in the MIT SDK](https://github.com/PerplFoundation/dex-sdk/blob/main/crates/sdk/abi/dex/Exchange.json) (204 events; the on-chain contract is not explorer-verified).
* Key events: `TakerOrderFilled` / `TakerOrderFilledV2(uint256 entryPricePNS, uint256 collatPricePNS, uint256 pnlPricePNS, uint256 lotLNS, uint256 feeCNS, int256 amountCNS, uint256 balanceCNS, uint256 builderId, uint256 builderFeeCNS)`, `MakerOrderFilled` / `V2(uint256 perpId, uint256 accountId, uint256 orderId, uint256 pricePNS, uint256 lotLNS, uint256 feeCNS, ...)`, `PositionOpened` / `V2`, `PositionIncreased` / `V2`, `PositionDecreased`, `PositionClosed(perpId, accountId, positionType, pricePNS, int256 deltaPnlCNS, int256 fundingCNS)`, `PositionInverted`, `PositionLiquidated(perpId, posAccountId, positionType, markPricePNS, liqPricePNS, liqLotLNS, posLotLNS, deltaPnlCNS, fundingCNS, ...)`, `PositionDeleveraged` / `V2`, `FundingEventCompleted(perpId, fundingEventBlock, specifiedRatePct100k, actualRatePct100k, fundingPricePNS, fundingPaymentPNS, fundingSumPNS, allowOverwrite)`, `AccountCreated(address account, uint256 id)`.
* Fields: almost everything - real traded prices (so **this venue has genuine
  candles**), size in lots, fees, realised PnL, funding paid, liquidation and ADL as
  separate events, funding rate as an event. OI must be summed from position events.
* Traps: **no indexed parameters at all** (filter by address + topic0 only); traders
  are numeric `accountId`s - an `AccountCreated` map is mandatory; `TakerOrderFilled`
  does not carry perp or account - it must be correlated with the neighbouring
  position / maker events in the same transaction by log order; custom fixed-point
  units (PNS price, LNS lot, CNS collateral = 1e6) with per-market scaling that must
  be read from the contract; two event renames already (2026-06 and block
  95,662,781 on 2026-08-13), each changing topic0. The protocol is young and will
  keep changing.
* Effort: **M**.

### 4.5 SynFutures V3

* Gate (Base) `0x208B443983D8BcC8578e9D86Db23FbA547071270`; `getAllInstruments()`
  enumerates per-market `Instrument` contracts (65 on 2026-05-20 per DefiLlama's
  note). This is the closest analogue to a spot DEX factory/pool layout, so it fits
  the existing pool-discovery machinery.
* Event (**ADAPTER**, UNVERIFIED): `Trade(uint32 indexed expiry, address indexed trader, int256 size, uint256 amount, int256 takenSize, uint256 takenValue, uint256 entryNotional, uint16 feeRatio, uint160 sqrtPX96, uint256 mark)`.
* Fields: trader, market = emitting Instrument (+ `expiry`; perpetual is a reserved
  expiry value), side = sign of `size`, notional `entryNotional` (1e18), price from
  `sqrtPX96` (real AMM price - candles possible) and `mark`. Liquidations, funding,
  `Adjust`/`Settle`/`Sweep` events exist in the protocol but were not inspected
  (UNVERIFIED). New-instrument event on Gate not inspected (UNVERIFIED).
* Effort: **S/M**.

### 4.6 Aark (Arbitrum)

* `FuturesManager` `0x0b848a8A5eC8950E67d19E7a21A6Be29F44F685e`. Events (**ADAPTER**, UNVERIFIED):
  `MoonOrderOpenedV2(address user, uint32 moonIndex, uint32 marketId, uint32 timestamp, uint64 entryPrice, int64 qty, uint16 leverage, int64 lastAccFundingFactor, uint64 takeProfit, uint48 initMargin, uint48 openFee, uint16 executionFee)` and
  `MoonOrderClosedV2(address user, uint256 moonIndex, uint32 marketId, uint64 indexPrice, int48 pnl, uint48 closeFee, int48 fundingFee, uint48 userPayback, uint256 timestamp)`.
  Price 1e8, qty 1e10 (from the adapter).
* DefiLlama had to hard-code a bad-price exclusion (a 1000PEPE glitch on 2026-04-24),
  which says something about data quality. Aark's older non-"Moon" perp product and
  liquidation events were not inspected. Contract source not located.
* Effort: **S**, but low confidence until the contract source is read.

### 4.7 A2 one-event venues: Katana Perps and Primit

* Katana Perps (**ADAPTER**, UNVERIFIED): `TradeExecuted(address buyWallet, address sellWallet, string baseAssetSymbol, string quoteAssetSymbol, uint64 baseQuantity, uint64 quoteQuantity, uint8 makerSide, int64 makerFeeQuantity, uint64 takerFeeQuantity)`; quantities in 8-decimal "pips". This is the IDEX exchange event (Katana acquired IDEX; typechain types ship in [katana-perps-sdk-js](https://github.com/katanaperps/katana-perps-sdk-js)). Liquidation / deleverage / funding events not inspected. A market-wide REST `GET /v1/trades` also exists (`api-perps.katana.network`).
* Primit (**ADAPTER**, UNVERIFIED): `TradeRecorded(bytes32 indexed tradeId, address indexed taker, address indexed maker, string symbol, uint8 side, uint256 price, uint256 amount, int256 takerFee, int256 makerFee, bool isClose, uint64 filledAt)`; 1e18 price and amount; exclude `taker == maker`.
* Primit's adapter notes that position state is off-chain; the recorder is
  operator-written, so completeness cannot be checked on-chain.
* Both give trader, counterparty, symbol string, side, price, size, fees. Neither
  gives PnL, funding or OI. Effort: **S** each.

### 4.8 Small families

* **KiloEx / Pika-style** (**SRC**: official SDK ABI): `IncreasePositionV3(uint256 indexed positionId, address indexed user, uint256 indexed productId, bool isLong, uint256 price, uint256 oraclePrice, uint256 margin, uint256 leverage, uint256 fee, int256 funding, uint256 orderType, uint256 borrowing, uint256 reqMargin, uint256 reqLeverage, bytes extraInfo, uint256 sid)`, `DecreasePositionV3(uint256 indexed positionId, address indexed user, uint256 indexed productId, uint256 price, uint256 entryPrice, uint256 margin, uint256 leverage, uint256 fee, int256 pnl, int256 fundingPayment, bool wasLiquidated, uint256 orderType, uint256 borrowingFee, uint256 remainMargin, bytes extraInfo, uint256 sid)`, `PositionLiquidated(...)`. Very complete rows (PnL, funding, liquidation flag in one event). "V3" in the event name implies older signatures for history. `PerpTrade` addresses per chain not collected (the SDK config lists market/order-book addresses, e.g. BSC market `0x298e94D5494E7c461a05903DcF41910e0125D019`; discover the emitter by topic0). The PositionRouter also emits `Create/Execute/Cancel...PositionV3` order-lifecycle events. **S**.
* **SYMMIO** (**SRC**: [IPartiesEvents.sol](https://github.com/SYMM-IO/protocol-core/blob/main/contracts/interfaces/IPartiesEvents.sol)): `SendQuote(address partyA, uint256 quoteId, address[] partyBsWhiteList, uint256 symbolId, PositionType positionType, OrderType orderType, uint256 price, uint256 marketPrice, uint256 quantity, uint256 cva, uint256 lf, uint256 partyAmm, uint256 partyBmm, uint256 tradingFee, uint256 deadline)`, `OpenPosition(uint256 quoteId, address partyA, address partyB, uint256 filledAmount, uint256 openedPrice)`, `FillCloseRequest(uint256 quoteId, address partyA, address partyB, uint256 filledAmount, uint256 closedPrice, QuoteStatus quoteStatus, uint256 closeId)` (an older form without `closeId` also exists; DefiLlama's adapter types the 6th field as `uint8 orderType` - same topic0 either way since enums are uint8, but flag it), `ForceClosePosition`, `EmergencyClosePosition`, liquidation events. Fill events carry only `quoteId`: side and symbol must be joined from `SendQuote`. Diamonds per chain are listed in DefiLlama's adapter. **M** for the join logic, low volume.
* **ApolloX-shaped** (LeverUp diamond on Monad `0xea1b8E4aB7f14F7dCA68c5B214303B13078FC5ec`; **ADAPTER**): `OpenMarketTrade(address indexed user, bytes32 indexed tradeHash, (OpenTrade) ot)`, `CloseTradeSuccessfulV2`, `ExecuteCloseSuccessfulV2(..., uint8 executionType, ...)`, and a newer `OpenPosition` / `PositionIncreased` / `ClosePosition` / `PositionDecreased` / `ExecuteDecreaseOrderSuccessful` set keyed by `positionHash` ([LeverUp event docs](https://developer-docs.leverup.xyz/onchain/events.md): "Every event is emitted by the Diamond"; two-phase keeper execution; liquidation appears to be an `executionType`/`kind` value, not its own event). Aster 1001x on BSC is the original; signatures there not checked. **S/M**.
* **GMX V1 Vault** (**SRC**: [Vault.sol](https://github.com/gmx-io/gmx-contracts/blob/master/contracts/core/Vault.sol)): `IncreasePosition(bytes32 key, address account, address collateralToken, address indexToken, uint256 collateralDelta, uint256 sizeDelta, bool isLong, uint256 price, uint256 fee)`, `DecreasePosition(` same `)`, `LiquidatePosition(bytes32 key, address account, address collateralToken, address indexToken, bool isLong, uint256 size, uint256 collateral, uint256 reserveAmount, int256 realisedPnl, uint256 markPrice)`, `UpdatePosition(bytes32 key, uint256 size, uint256 collateral, uint256 averagePrice, uint256 entryFundingRate, uint256 reserveAmount, int256 realisedPnl, uint256 markPrice)`, `ClosePosition(...)`, `UpdateFundingRate(address token, uint256 fundingRate)`. No indexed fields; all USD values 1e30; position key = `keccak(account, collateralToken, indexToken, isLong)`; some forks added a `markPrice` to `UpdatePosition` later (GMX did, mid-life) so two topic0s exist. This is the only true "one ABI, many forks" perp family (64 forks) and the cheapest to write (**S**), but live volume is $0.09B and mostly on Cronos (no HyperSync). Worth it only for historical completeness (2021-2023 GMX/Arbitrum history is large).
* **Synthetix Perps V2/V3**: zero live volume. Skip unless history is wanted.

### 4.9 Category A on chains HyperSync does not serve

RISEx ($3.08B, RISE), Reya ($2.93B, Reya Network), Orderly ($1.40B, Orderly L2) and
Derive ($0.82B, Derive Chain) together are $8.2B - larger than the Gains-shaped
group plus GMX. All four chains have public RPCs; they need either an RPC-based
source or Envio adding the chain.

Orderly Ledger events (**SRC**, [ILedgerEvent.sol](https://github.com/OrderlyNetwork/contract-evm/blob/main/src/interface/ILedgerEvent.sol)):
`ProcessValidatedFutures(bytes32 indexed accountId, bytes32 indexed symbolHash, bytes32 feeAssetHash, int128 tradeQty, int128 notional, uint128 executedPrice, int128 fee, int128 sumUnitaryFundings, uint64 tradeId, uint64 matchId, uint64 timestamp, bool side)`
(an older overload has `uint128 fee` - two topic0s), `LiquidationResult` / `LiquidationResultV2`, `AdlResult` / `AdlResultV2`. This is a very good dataset:
real order-book prices, `matchId` pairs maker and taker rows, `sumUnitaryFundings`
gives the funding index at each fill, liquidations and ADL are explicit, and broker
attribution covers ~20 front ends with one decoder. `symbolHash` and `accountId`
are hashes and need a dictionary (API or registration events). If the owner ever adds
a non-HyperSync source, **Orderly is the first family to build on it**.

Derive (**SRC**, [ITradeModule.sol](https://github.com/derivexyz/v2-matching/blob/master/src/interfaces/ITradeModule.sol)): `OrderMatched(address base, uint taker, uint maker, bool takerIsBid, int amtQuote, uint amtBase)` and `FeeCharged(uint acc, uint recipient, uint takerFee)`; taker/maker are numeric subaccount ids; perps and options share the event (`base` = asset contract).
 RISEx additionally has no public ABI (contracts unverified; ask the
team or look in their SDK). Reya's `PassivePerpMatchOrder(uint128 indexed marketId, uint128 indexed accountId, int256 orderBase, (uint256 protocolFeeCredit, uint256 exchangeFeeCredit, uint256 takerFeeDebit, int256[] makerPayments, uint256 referrerFeeCredit) matchOrderFees, uint256 executedOrderPrice, uint128 referrerAccountId, uint256 blockTimestamp)` is **ADAPTER**-level (an older form without the referrer fields also exists), and Reya's announced 2026-09-28 cut-over will replace it. Park RISEx and Reya.

## 5. Recommendation

### (a) How much is reachable

**3.9% of 30-day perp volume ($26.0B of $666.7B) is readable from EVM logs on
HyperSync chains; 5.2% ($34.7B) from EVM logs on any chain.** Put differently:
Hyperliquid does in about three days what every log-indexable venue combined does
in a month. An EVM-log perp module is a legitimate product (complete, auditable,
trader-level data for ~20 venues), but it is a niche dataset, not "the perp market".

### (b) Shortlist, in suggested build order

Score = volume x reuse x (1 / cost). All on HyperSync chains.

| Order | Family | 30d volume | Cumulative share of the reachable $26.0B | Reuse | Effort | Why this position |
|---|---|---|---|---|---|---|
| 1 | Vertex-style `FillOrder` (Nado, Ink) | $9.48B | 36.5% | Vertex/Blitz history (unverified) | S/M | Biggest by far, one contract, public source, funding and OI come as events. Also exercises the "A2" pattern. |
| 2 | GMX V2 EventEmitter | $2.43B | 45.8% | GMX on 3 chains + HertzFlow + 8 dormant forks | M | Best-known venue, richest data (PnL, fees, OI, funding), a real fork family. The key/value decoder is the cost. |
| 3 | Gains-shaped: Avantis, then Gains, then Ostium | $4.53B | 63.2% | 3 live protocols, 4 chains | M (+S, +S) | Start with Avantis (largest, USDC-only, official reference indexer). Gains current-generation only. Ostium last (volume falling). |
| 4 | Perpl (Monad) | $2.53B | 72.9% | none | M | Only venue with a genuine on-chain order book and real prices; ABI still moving - build late to avoid churn. |
| 5 | SynFutures V3 (Base) | $2.41B | 82.2% | factory/instrument layout reuses pool discovery | S/M | Needs source verification first. |
| 6 | Aark (Arbitrum) | $2.09B | 90.3% | none | S | Cheap, but source unseen and data-quality warning. |
| 7 | Katana Perps + Primit | $1.67B | 96.7% | none | S + S | One event each. |
| 8 | KiloEx, LeverUp/ApolloX-shaped, SYMMIO, HertzFlow (free with #2) | $0.94B | 100% | small | S, S/M, M | Only if completeness matters. |
| - | GMX V1 Vault | ~$0.09B live | - | 64 forks, large 2021-23 history | S | Historical value only. |
| - | Orderly Ledger (needs a non-HyperSync source) | $1.40B | - | ~20 front ends, one decoder | S/M | Best data quality of any family; blocked only by chain coverage. Ask Envio about chain 291. |

Steps 1-3 give 63% of the reachable volume with three decoder patterns; steps 1-6
give 90%. Volumes this small also move fast (Ostium fell ~90% inside the window;
Perpl's 24h is a tenth of its 7d average), so re-pull the ranking before starting
each family.

### (c) The venues that matter, and what an API adapter would look like

B/C dominates: Hyperliquid 36.0%, Aster 11.0%, Lighter 8.3%, ApeX 6.5%, edgeX 6.4%,
Variational 6.0%, GMTrade 3.9%.

1. **Hyperliquid (36.0%)** - the only one of the three with a complete, public,
   historical, market-wide dataset. Source: `s3://hl-mainnet-node-data/node_fills_by_block`
   (every fill with both users, price, size, side, fee, closed PnL, liquidation
   info), older `node_fills` / `node_trades` for earlier periods,
   `misc_events_by_block` for funding, `s3://hyperliquid-archive/asset_ctxs` for OI /
   funding / mark snapshots. LZ4 files, **requester pays** AWS transfer. Live tail:
   WebSocket `trades` or run a non-validating node. Caveats: docs state no guarantee
   of timeliness or completeness for the archive bucket; format changed over time
   (three fill datasets); HIP-3 builder-deployed perp dexes add markets under
   separate namespaces. No licence text was found on the data page - **terms not
   reviewed**. Historical depth: fills back to the early node datasets (exact first
   date not verified). Effort: M. This single adapter is worth 9x the entire EVM-log
   module in volume terms.
2. **Aster (11.0%)** - Binance-compatible REST/WS (`/fapi/v1|v3/...`: `aggTrades`,
   `klines`, `fundingRate`, `openInterest`). Market-wide trades are anonymous (no
   trader address), and Aster Chain's privacy mode means trader-level data will be
   partial even from the chain explorer. Historical depth of `aggTrades` not
   verified; no bulk archive found. Terms not reviewed. You would get candles,
   volume, funding, OI - not positions or liquidations per trader.
3. **Lighter (8.3%)** - REST `recentTrades` (public, shallow), WS trade stream for
   live capture; **full history is gated**: market-wide parquet dumps since
   2025-01-17 require a 100 LIT in-app payment; otherwise Tardis / 0xArchive
   (commercial). The on-chain blobs give account deltas, not fills - a research
   project, not an adapter. Terms not reviewed.

Honourable mentions with clean public trade APIs: Arcus (`/v1/trades`, no auth, time
paged), dYdX V4 indexer, GMTrade (public Subsquid GraphQL), Nado's archive API
(redundant with logs). No usable market-wide data: Variational (stats endpoint
only), Antarctic, AZverse, Rocky (Canton privacy).

API-sourced rows differ in kind from log rows: no block/log coordinates, no reorg
semantics, often no trader. They should land in the same normalised tables with a
`source` discriminator and a synthetic ordering key, and must never be mixed into
the tombstone/epoch reorg machinery.

### (d) Venue-agnostic table sketch

Same conventions as `dex_*`: raw integer amounts plus a decimals-adjusted view;
unknown stays `NULL`, never 0; `protocol` = who, `family` = which decoder.

**`perp_markets`** - key `(chain, family, emitter, market_id)`
`market_id` (String: address, numeric pair/product id, or symbol), `protocol`,
`index_token` (Nullable - forex/stock pairs have none), `symbol`, `collateral_tokens`,
`size_decimals`, `price_decimals`, `created_block`.

**`perp_trades`** - key `(chain, block_number, log_index)`; one row per position
change or fill
`family`, `protocol`, `emitter`, `market_id`, `trader`, `counterparty` (Nullable),
`position_key` (Nullable), `order_key` (Nullable), `side` (long/short),
`action` (open / increase / decrease / close / liquidation / adl),
`is_taker` (Nullable), `size_usd`, `size_base`, `price`, `collateral_token`,
`collateral_delta`, `fee_usd`, `funding_fee`, `borrow_fee`, `realised_pnl`,
`is_liquidation`, `executor` (tx.from - the keeper/sequencer), `source` (log / api).

**`perp_liquidations`** - a view over `perp_trades WHERE is_liquidation`, plus
columns only liquidation events have (`liquidator`, `mark_price`,
`remaining_collateral`, `liq_fee`). A separate table only if a family emits
liquidations that are not also trades (Nado, Perpl, KiloEx `PositionLiquidated`).

**`perp_funding`** - key `(chain, block_number, log_index)`
`market_id`, `funding_rate` or cumulative factor (say which in `kind`),
`borrow_rate`, `oi_long`, `oi_short`, `oi_unit` (usd / token / collateral).

**`perp_oi_1h` / `perp_volume_1d` / `perp_trader_stats_1d`** aggregates, same MV
style as the DEX module. **Candles only for venues with real traded prices.**

**Position snapshots**: do not store them as events. Offer a
`perp_positions_current` replacing table folded from `perp_trades` per
`position_key`; GMX V2, GMX V1 (`UpdatePosition`), Gains (`TradeStored`) and KiloEx
give the post-trade state in the event, the others need a running sum. Defer.

What each family can fill:

| Column | Nado | GMX V2 | Gains | Avantis | Ostium | Perpl | SynFutures | Aark | Katana / Primit | KiloEx | SYMMIO | GMX V1 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| trader | yes (from subaccount) | yes | yes | yes | open only; close needs join | via accountId map | yes | yes | yes (both sides) | yes | yes (partyA) | yes |
| counterparty | no (two rows per match) | pool | pool | pool | pool | maker rows | AMM / makers | pool | yes | pool | yes (partyB) | pool |
| market | productId | market token addr | pairIndex | pairIndex | pairIndex | perpId | instrument addr + expiry | marketId | symbol string | productId | symbolId via join | indexToken addr |
| side | sign of baseDelta | yes | yes | yes | yes (open) / join | positionType | sign of size | sign of qty | yes | open only; close needs join | via join | yes |
| increase / decrease | derive | yes | yes | `open` flag | event name | event name | derive | event name | Primit `isClose`; Katana no | event name | event name | event name |
| size USD | quoteDelta 1e18 | yes 1e30 | derive (collateral x lev x price) | yes 1e6 | tradeNotional | lots x price | entryNotional 1e18 | price x qty | price x qty | margin x leverage | qty x price | yes 1e30 |
| execution price | yes | yes | yes | yes | yes | yes (real) | yes (real) | yes | yes (real) | yes | yes | yes |
| collateral | separate event | yes | yes | yes | yes | yes | not inspected | initMargin | no | margin | cva/lf/mm on quote | yes |
| fee | yes | via `PositionFeesCollected` join | separate fee events | not inspected | separate events | yes | feeRatio | yes | yes | yes | tradingFee on quote | yes |
| realised PnL | no | yes (`basePnlUsd`) | percentProfit | percentProfit | percentProfit | yes | not inspected | yes | no | yes | no (derive) | liquidation / `UpdatePosition` |
| liquidation flag | `Liquidation` event | orderType 7 | `LimitExecuted.orderType` | `LimitExecuted.orderType` | `LimitCloseExecuted.orderType` | own event | not inspected | not inspected | not inspected | `wasLiquidated` | own events | own event |
| funding / borrow paid | no (per-product only) | yes | yes (holding fee events) | not inspected | `FeesCharged` | yes | not inspected | yes (close) | no | yes | own event | fee only |
| funding rate series | yes | yes | yes | not inspected | not inspected | yes | not inspected | factor on open | no | not inspected | not inspected | yes |
| open interest from events | yes (in `FundingPayment`) | yes | yes | not inspected | derive | derive | derive | derive | no | derive | derive | derive |
| native candles meaningful | yes (order book) | no (oracle) | no | no | no | yes | yes | no | yes | no | partly (RFQ) | no |

General traps to write into the design before any code:

1. `tx.from` is a keeper or sequencer almost everywhere - never use it as trader.
2. Oracle-priced venues (GMX, Gains-shaped, Aark, KiloEx, ApolloX-shaped) have no
   price discovery; their "candles" would just be the oracle.
3. Market identity is heterogeneous (address / numeric index / string) and often
   needs an RPC read or a config event to get a symbol - reuse the token-metadata
   worker pattern.
4. Decimal conventions differ per family and sometimes per field (1e30, 1e18, 1e10,
   1e8, 1e6, custom PNS/LNS/CNS).
5. Perp ABIs churn far more than spot ABIs: Perpl renamed events twice in three
   months; Gains, KiloEx, Ostium, Avantis, LeverUp all carry `V2`/`V3`/version
   suffixes. Budget for a signature registry keyed by `(family, topic0)` with several
   generations per family, and re-verify before each build.
6. A2 venues are operator-reported: logs are complete only as long as the operator
   keeps posting them. Label them so.

## 6. Appendix

### Endpoints and retrieval times (UTC)

| When | What | Result |
|---|---|---|
| 2026-09-19 00:27:08 | `https://api.llama.fi/overview/derivatives?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` | **HTTP 402** "Upgrade to the paid API plan". Same for `/overview/derivatives/arbitrum` and `/summary/derivatives/gmx`. The free API no longer serves perp volume. |
| 2026-09-19 00:27:09 | `https://api.llama.fi/overview/open-interest?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true` | 200, 128 protocols. Its `total24h` = $15.53B differs from the perps page's open interest of $13.41B - **two DefiLlama sources disagree**; per-venue OI in section 2 is from the perps page. |
| 2026-09-19 00:27:11 | `https://api.llama.fi/protocols` | 200, 8.9 MB. Used for category, chains, parent, `forkedFromIds` (note: `forkedFrom` is null for derivatives; the populated field is `forkedFromIds`). |
| 2026-09-19 ~00:28 | `https://defillama.com/perps` | curl gets a Cloudflare 403; loaded in a real browser and read the embedded `__NEXT_DATA__` JSON (`props.pageProps.protocols`, 209 entries; totals `total24h` 23,012,782,346, `total7d` 140,772,896,205, `total30d` 666,669,261,893.13, `openInterest` 13,409,200,303). Sum of protocol rows equals the page total. All volume figures in this note come from here. |
| 2026-09-19 00:29:32 | `https://docs.envio.dev/docs/HyperSync/hypersync-supported-networks` | 200. Relevant: Arbitrum, Avalanche, Base, BSC, opBNB, Manta, Polygon, Blast, Ink, Katana, MegaETH, Monad, Robinhood, Sonic, Mantle, Mode, Berachain, Flare, ZKsync, Optimism, Ethereum, Hyperliquid (999), Tron, Arc, Injective*, Sei*. **Not listed**: Botanix, ApeChain mainnet, Cronos, Cronos zkEVM, RISE, Reya, Somnia, Taiko, B2, Derive, Aevo, Orderly. |
| 2026-09-19 ~00:31 | `github.com/DefiLlama/dimension-adapters` at commit `4c38bc204cce1efe84a4f6ef730528b476c7c0ff` (2026-09-18) | Read `dexs/`, `open-interest/`, `helpers/`, `factory/` to see, per venue, whether DefiLlama derives volume from logs or from an API. |
| 2026-09-19 00:30-00:40 | GitHub raw sources: gmx-io/gmx-synthetics (main), gmx-io/gmx-contracts, gmx-io/gmx-interface (SDK contract config), nadohq/nado-contracts, 0xOstium/smart-contracts-public, Avantis-Labs/avantis-trades-indexer, SYMM-IO/protocol-core, KiloExPerp/kiloex-python-sdk, PerplFoundation/dex-sdk; npm `@gainsnetwork/sdk@1.8.10` via unpkg | Event signatures in section 4. |
| 2026-09-19 00:30-00:45 | Venue docs (linked inline in section 3) | Classification evidence. Several were read through a summarising fetch tool, not verbatim. |

### Could not verify / open points

* **Per-chain volume splits** (GMX on Botanix, Gains on ApeChain, KiloEx per chain,
  Fulcrom per chain): the per-chain endpoint is paywalled and the page JSON has no
  split. The 3.9% headline slightly overstates because of this.
* **Aster 1001x on-chain volume**: not reported separately anywhere I could reach.
  Its BNB Chain contract and event signatures were not checked.
* **No event signature has been keccak-verified.** ADAPTER-level signatures
  (SynFutures `Trade`, Aark, Katana `TradeExecuted`, Primit `TradeRecorded`, Reya,
  LeverUp) were not seen in verified contract source. Perpl's and RISEx's contracts
  are not verified on their explorers at all (Perpl's ABI comes from the official
  SDK; RISEx's is unknown). Gains, Avantis and KiloEx signatures come from official
  SDK/indexer ABIs, not from explorer-verified source.
* **Variational**: whether settlement-pool contracts emit per-trade events (6% of all
  perp volume hangs on this; current evidence says no).
* **StandX, AZverse**: architecture undocumented (D).
* **Carbon.inc / SYMMIO overlap**: Carbon is confirmed as a SYMMIO front end
  (SYMMIO deployments page, DefiLlama `factory/symmio.ts`); whether DefiLlama's SYMMIO
  figure already contains Carbon's volume is not stated.
* **Primit / Orderly overlap**: Primit is in DefiLlama's Orderly broker list and is
  also a standalone entry; not flagged as double-counted.
* **Somnex, Moonlander, Fulcrom**: classified A1 from DefiLlama metadata (fork-of /
  chain), contracts not inspected. Moonlander's DefiLlama adapter uses an API.
* **Reya after 2026-09-28**: new order-book contract and event names not yet
  published for mainnet.
* **Gate DEX, Aevo**: claims of on-chain recording with no contract or event spec.
* Block explorers behind bot protection (Arbiscan, BscScan) could not be opened;
  contract addresses are as cited by docs, SDKs and DefiLlama adapters.
* **Avantis -> Veranta**: the docs domain redirects; rebrand not confirmed.
* **Ostium**: cause of the volume collapse not investigated (a third-party exploit
  analysis repo, `DarkNavySecurity/web3-exploit-analysis`, contains Ostium contract
  sources).
* **Nado = Vertex ABI**: architecture matches; byte-for-byte signature equality with
  Vertex's `FillOrder` not checked, and explicit attribution not found.
* **Derive and Orderly L2 logs**: event definitions read in source; no live
  transaction was decoded to confirm one log per fill. Neither chain is on HyperSync,
  so it does not change the 3.9% headline.
* **API terms/licences**: not reviewed for any venue. Lighter's full history is
  behind a 100 LIT payment. Hyperliquid S3 is requester-pays.
* **Volume quality**: for every B/C venue DefiLlama's number is the venue's own API
  output. Rankings among off-chain venues (especially zero-fee and points-farming
  venues) should be read with that in mind.
* Historical depth of Hyperliquid's fill archives (first available date) not checked.
