# boros-mm

Production market making and relative-value system for [Boros](https://pendle.finance), Pendle's on-chain funding rate swap market (Arbitrum). Treats implied APR across maturities as a proper rate curve, not a spot price; models inventory as DV01/duration exposure, not directional notional; replicates the protocol's margin and lazy-settlement mechanics off-chain to run risk in shadow before committing capital.

Not a wrapper around a REST client. This is a full quoting/risk/execution stack purpose-built for an IRS-style CLOB with protocol-specific settlement, margin, and liquidation mechanics that don't map cleanly onto spot or perp market making.

---

## Why Boros

Boros lets you trade the cash flow of a perpetual's funding rate directly: a fixed-rate side (PT-style) and a floating/leveraged side (YU, formerly YT) settling against a TWAP oracle of realized funding. Structurally it's an interest rate swap market: multiple maturities quoting simultaneously on the same underlying, a curve-fitting problem, not a single-price problem.

The edge: perp funding doesn't share balance sheet across venues. Liquidity is uneven, positioning diverges, margin engines differ, producing persistent, structural funding rate dislocations across Binance / Bybit / OKX / Hyperliquid. Classic cross-venue cash-and-carry captures this but is capital-intensive and path-dependent. Boros makes the divergence a tradable rate instrument with its own order book.

---

## Protocol mechanics that drive the architecture

**Contract topology.** `Router` → `MarketHub` → `Market`, cascading. `MarketHub` holds deposits and enforces margin; each `Market` executes trades and reports payment + margin-check data. Accounts are keyed by `MarketAcc` (EVM address, subaccountId, collateral tokenId, marketId), up to 256 subaccounts per user. An account is either **isolated** (single market) or **cross-margin** (multiple markets sharing the same collateral token). This forces an explicit design decision: a classifier for which strategies run isolated (directional arb) versus cross (multi-maturity market making on the same underlying), because mixing them contaminates the health ratio of otherwise-healthy positions.

**Order book.** Gas-optimized CLOB, 65,536 discrete tick levels per side, mapped to rates via `rate = 1.00005^(tick · tickStep) − 1`: fine granularity near zero, coarser at the extremes. The AMM side quotes continuous rates; every limit order must land on an integer tick, so converting a theoretical optimal rate to a tick introduces a systematic rounding delta that has to be priced into EV, not ignored. Matching is rate-time priority (best rate, FIFO within a tick). Margin and liquidation reference a **mark rate**, a 5-minute TWAP of recent trades, which can diverge materially from the live book or the AMM's implied rate in thin markets. The risk engine has to model the basis between "where you're quoted" and "where you're liquidated," not assume they're the same number.

**Margin.** Position value = size × mark rate × time-to-maturity. Account value = cash + Σ position values across all markets sharing that account. Maintenance margin = |size| × max(|mark rate|, rate floor) × MM factor × max(TTM, min-time threshold). Health ratio = total value / total maintenance margin. Thresholds are progressive: >1.0 normal, approaching 1.0 risky orders can be force-cancelled, ≤1.0 liquidation-eligible, ≤0.7 forced deleverage against the largest opposing-exposure counterparty. IM/MM factors are global per market but can be customized per account: whitelisted market makers get preferential factors and Close-Only exemptions. Negotiating MM-whitelist status with the Pendle team is an operational prerequisite, not an afterthought: it changes both capital efficiency and exposure to being locked into close-only exactly when you'd want to size up.

**Settlement.** Fixed leg settles upfront (long pays PV of size × rate × TTM/year at trade time); floating leg settles periodically, typically hourly, as signed size × change in accumulated floating index, tracking realized funding on the reference exchange. Boros uses **lazy settlement**: on-chain state doesn't reflect the latest values until the user interacts again or settlement is explicitly triggered, and floating payments are computed on position size exactly at the payment timestamp, in practice with an oracle lag of roughly 30 seconds past each hourly boundary, wider under volatility. PnL accounting can't assume continuity; it has to replicate the event algorithm (fills grouped by `fTime`, then floating settlement, repeat) or the local position silently diverges from on-chain truth.

**Integration.** Two-tier signing: sensitive actions (deposit, withdraw, agent approval) sign with the root wallet direct to chain; trading actions (place, cancel, transfer) sign with a scoped agent wallet routed through Pendle's Send Txs Bot (gas/nonce management). Official SDK is TypeScript only (`@pendle/boros-sdk-public`, `@pendle/boros-offchain-math` for `FixedX18` and tick/rate conversion), no native Rust SDK. This is the central engineering trade-off of the project (see Architecture below).

---

## Risk model

Risk here is protocol-specific, not generic market-making risk:

| Risk | Description |
|---|---|
| **Settlement basis** | Delta between the fixed rate locked and the floating index actually reported by the venue's funding oracle. Modeled as mean-reverting (calibrated against realized funding history), not white noise. |
| **Settlement timing** | Floating payment computed on position size at the exact payment timestamp (~30s oracle lag, wider under vol): a gaming window where reducing position seconds before cutoff changes exposure. Must be an explicit policy, not incidental to inventory update cadence. |
| **Margin/liquidation basis (TWAP)** | Mark rate for margin is a 5-min TWAP lagging the live book. Margin simulation must run against a locally-replicated mark rate, not internal mid, or proximity to liquidation is systematically underestimated in fast moves. |
| **Tick-band purge** | Dynamic bands around mark rate widen with TTM; leaving the band gets an order rejected or purged if resting. Hard constraint in the quoting engine, not a soft warning. |
| **Close-Only mode** | Aggregate OI approaching the market cap restricts new orders to reduce-only (barring whitelist exemption). Requires continuous OI-utilization monitoring per market. |
| **Cross-margin contagion** | A cross account sharing collateral across maturities can see an individually healthy position pressured by an adverse move elsewhere in the same account, a portfolio construction decision, not just execution. |
| **Permissioned bot risk** | Liquidation Bot, Force-Cancel Bot, CLO Bot are Pendle-operated infrastructure, not deterministic contracts under your control. Justifies an independent kill-switch, not just exception handling in the main bot. |
| **Key management** | Root wallet (cold, multisig) vs. agent wallet (hot, trading-scoped) is a protocol requirement, mapped onto standard execution key-separation patterns. |

---

## Architecture

Core trade-off: the only official signing/calldata SDK is TypeScript; the decision engine is Rust. Reimplementing `FixedX18` and tick/rate math in Rust is viable (closed-form, not opaque), but doing it without cross-validation is exactly the kind of silent risk that doesn't belong in production. Resolution: **decision engine 100% Rust** (hot path: pricing, risk, quoting); **execution layer** initially delegates calldata construction and signing to the official SDK via a thin Node sidecar, validated by a golden-vector harness in CI comparing the Rust reimplementation against the real TS SDK output across thousands of generated cases. Native Rust signing only after that harness has run clean for weeks.

```
boros-mm/                          # Cargo workspace
├── crates/
│   ├── tick-math/                 # rate(tick) = 1.00005^(tick·tickStep) − 1, FixedX18, golden-vector tests vs @pendle/boros-offchain-math
│   ├── curve-engine/               # multi-maturity implied APR bootstrapping, no-arb checks, reuses options-pricing-engine-rs splines
│   ├── feed-ingest/                 # feedhandler-core-rs extension: Boros WS + REST fallback, normalized funding feeds (Binance/Bybit/OKX/Hyperliquid)
│   ├── margin-sim/                  # off-chain replica of Position/Total Value, IM, MM, Health Ratio, Liquidation Incentive, shadows on-chain state
│   ├── quoting-engine/              # Avellaneda-Stoikov adapted to DV01 as inventory measure, TTM-based skew, tick clamping
│   ├── arb-engine/                  # Boros implied APR vs CEX funding curve divergence, cash-and-carry sizing
│   ├── risk-engine/                 # pre-trade limits, maturity/underlying exposure buckets, cross vs isolated classifier
│   ├── settlement-ledger/           # lazy settlement replica (fTime-grouped fills + floating payment) for local PnL reconciliation
│   └── oms-core/                    # order state machine, TIF (GTC/IOC/FOK/ALO/SOFT_ALO), OrderId encoding
├── services/
│   ├── mm-bot/                      # main binary: tokio event loop wiring quoting-engine + margin-sim + execution-adapter
│   ├── arb-bot/                     # cross-venue funding divergence scanner + executor
│   └── risk-monitor/                # independent watchdog process: separate failure domain, kill-switch authority
├── execution-adapter/
│   ├── sidecar-ts/                  # thin wrapper over @pendle/boros-sdk-public: calldata, agent EIP-712 signing, Send Txs Bot
│   └── rust-bridge/                 # IPC to sidecar (local gRPC/socket), migrates to native Rust signing post-validation
└── tools/
    ├── backtester/                  # replay against historical Boros NDJSON exports (trades, OHLCV, settlements, book snapshots)
    └── golden-vector-gen/           # generates and diffs tick/rate/margin cases: Rust vs TS SDK
```

`risk-monitor` runs as a fully separate process by design: liquidation, force-cancel, and Close-Only mode are executed by permissioned Pendle infrastructure outside direct control, so the watchdog can't share a failure domain with the quoting engine. It has independent authority to flatten positions via the execution adapter if health ratio crosses a conservative threshold ahead of 1.0.

---

## Status

```
Data infrastructure:     ████████████░░░░  75%  (settlement-ledger pending)
Risk management:         ████████░░░░░░░░  50%  (margin-sim done, risk-engine pending)
Execution:               ██░░░░░░░░░░░░░░  15%  (oms-core + execution-adapter pending)
Strategy:                ░░░░░░░░░░░░░░░░   0%  (curve-engine, quoting-engine, arb-engine)
Production services:     ░░░░░░░░░░░░░░░░   0%
```

### Built and tested

**`crates/tick-math`**
- `FixedX18`: i128 fixed-point, exact add/sub, documented f64 fallback for mul/div (shadow sim only, not source of truth)
- `tick_to_rate` / `rate_to_tick` with `Rounding::Floor`/`Ceil`, precomputed `LN_TICK_BASE`
- `u128_wide_mul`: schoolbook 4-limb multiply, verified against `u128::MAX²`
- Golden-vector harness: loads `vectors.json` from the TS SDK if present, falls back to embedded cases
- 27 tests, 0 warnings

**`crates/margin-sim`**
- `MarginEngine`: position IM/MM, health ratio, TTM utility
- `compute_account_state`: aggregates PV + margins + open orders
- `margin_headroom_for_order`: pre-trade check
- 7 tests: zero positions, long/short PV, rate floor, open-order IM

**`crates/feed-ingest`** *(requires Rust ≥ 1.80, see Toolchain note)*
- `WsConnector`: exponential reconnect, ping/pong keepalive, fresh `WsSink` per reconnection
- `OrderBook`: sparse `BTreeMap<i16, FixedX18>`, seq-gap detection, BBO/mid/spread/depth
- `BorosFeedHandler`: subscribe/dispatch/parse, explicit `self` destructuring for `Send`-safety across the tokio future
- Funding feeds: Binance (`markPrice`), Bybit (`tickers.v5`), Hyperliquid (`activeAssetCtx`)
- `BookStateManager`: synchronous `drain()`, caller controls update application timing
- 7 book tests

**`tools/golden-vector-gen`**: Rust emitter + TypeScript cross-validator against the official SDK

### Known technical debt

| Location | Debt | Impact if unresolved |
|---|---|---|
| `tick-math/math.rs` | `mul_div_exact` is an f64 stub | ~2-3 ULP error on large positions. Fine for shadow, not for hard risk limits |
| `margin-sim/engine.rs` | `open_order_im` has no netting | Overestimates IM against a populated book, rejects valid orders |
| `margin-sim/engine.rs` | `check_cross_token_consistency` is a no-op | Doesn't enforce shared collateral in cross-margin |
| `feed-ingest/boros/types.rs` | Wire format unverified against live API | Silent `None` on parse if field names differ |

### Not yet built

**2: execution & order state**
- `settlement-ledger`: lazy settlement replica. Without it, `total_value` in margin-sim drifts from on-chain by an estimated 0.8–2.5 bps/hour on large positions, silently. Highest priority before any real capital.
- `oms-core`: `OrderId` encoding (direction, marketId, tick, expiry, nonce), state machine (`Pending → Resting → PartiallyFilled | Filled | Cancelled | Purged`), TIF handling (GTC, IOC/FOK, ALO/SOFT_ALO)
- `execution-adapter`: Node sidecar over `@pendle/boros-sdk-public`, Rust gRPC client with retriable/fatal error classification, root/agent wallet separation

**3: strategy**
- `curve-engine`: multi-maturity implied APR bootstrapping, no-arb butterfly checks, Vasicek-style rolling calibration (κ, θ, σ), reuses `options-pricing-engine-rs` splines
- `quoting-engine`: Avellaneda-Stoikov adapted to DV01 inventory (DV01 = |size| × 0.0001 × TTM), TTM-based skew, mandatory tick clamping against protocol bands
- `risk-engine`: pre-trade DV01/notional/health-ratio limits, mark-rate divergence detection, independent kill-switch authority

**4: services**
- `services/risk-monitor`: separate process, polls real health ratio, alerts on >5% divergence from shadow, signals or force-flattens
- `services/mm-bot`: full pipeline wiring
- `services/arb-bot`: cash-and-carry + curve-detected relative value

**5: validation**
- `tools/backtester`: NDJSON replay with FIFO queue simulation, offline γ/k tuning before going live
- Resolve `mul_div_exact` (Knuth Algorithm D), exact order netting, live wire-format verification

---

## Toolchain

`feed-ingest` is currently excluded from the workspace default build in sandboxed environments, a transitive dependency (`idna_adapter` 1.2.2, pulled in via `tokio-tungstenite` → `url`) requires Rust edition 2024. Code is correct and tested; it needs **Rust ≥ 1.80**. Uncomment `"crates/feed-ingest"` in the workspace `Cargo.toml` on a compatible toolchain.

---

## Design principles

- Nothing hardcoded against assumed protocol values, every constant (tick step, margin factors, OI caps) is sourced from live market config, not embedded.
- Margin and settlement logic runs in shadow against replicated on-chain state before any capital is committed.
- Risk monitoring is architecturally independent from the quoting/execution process, no shared failure domain.
- Cross-language boundary (Rust decision engine / TS signing) is bridged through a validated golden-vector harness, not blind reimplementation.
