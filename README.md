# boros-mm

Production market making and relative-value system for [Boros](https://pendle.finance), Pendle's on-chain funding rate swap market (Arbitrum). Treats implied APR across maturities as a proper rate curve, not a spot price; models inventory as DV01/duration exposure, not directional notional; replicates the protocol's margin and lazy-settlement mechanics off-chain to run risk in shadow before committing capital.

Not a wrapper around a REST client. This is a full quoting/risk/execution stack purpose-built for an IRS-style CLOB, with settlement, margin, and liquidation mechanics that don't map cleanly onto spot or perp market making.

---

## Why Boros

Boros lets you trade the cash flow of a perpetual's funding rate directly: a fixed-rate side (PT-style) and a floating/leveraged side (YU, formerly YT) settling against a TWAP oracle of realized funding. Structurally it's an interest rate swap market, multiple maturities quoting simultaneously on the same underlying, a curve-fitting problem rather than a single-price one.

The edge comes from balance sheets not being shared across venues. Perp funding liquidity is uneven, positioning diverges, margin engines differ, and the result is persistent, structural funding rate dislocation across Binance, Bybit, OKX, Hyperliquid. Classic cross-venue cash-and-carry captures this but is capital-intensive and path-dependent. Boros turns the divergence into a tradable rate instrument with its own order book.

---

## Protocol mechanics that drive the architecture

**Contract topology.** `Router` → `MarketHub` → `Market`, cascading. `MarketHub` holds deposits and enforces margin; each `Market` executes trades and reports payment and margin-check data. Accounts are keyed by `MarketAcc` (EVM address, subaccountId, collateral tokenId, marketId), up to 256 subaccounts per user. An account is either **isolated** (single market) or **cross-margin** (multiple markets sharing the same collateral token), which forces an explicit design decision early on: a classifier for which strategies run isolated (directional arb) versus cross (multi-maturity market making on the same underlying), since mixing the two contaminates the health ratio of otherwise-healthy positions.

**Order book.** Gas-optimized CLOB, 65,536 discrete tick levels per side, mapped to rates via `rate = 1.00005^(tick · tickStep) − 1`: fine granularity near zero, coarser at the extremes. The AMM side quotes continuous rates, but every limit order has to land on an integer tick, so converting a theoretical optimal rate to a tick introduces a systematic rounding delta that needs to be priced into EV rather than ignored. Matching is rate-time priority (best rate, FIFO within a tick). Margin and liquidation reference a **mark rate**, a 5-minute TWAP of recent trades, which can diverge materially from the live book or the AMM's implied rate in thin markets, the risk engine has to model the basis between where you're quoted and where you're liquidated, not assume they're the same number.

**Margin.** Position value = size × mark rate × time-to-maturity. Account value = cash + Σ position values across all markets sharing that account. Maintenance margin = |size| × max(|mark rate|, rate floor) × MM factor × max(TTM, min-time threshold). Health ratio = total value / total maintenance margin. Thresholds are progressive: above 1.0 normal, approaching 1.0 risky orders can be force-cancelled, at or below 1.0 liquidation-eligible, at or below 0.7 forced deleverage against the largest opposing-exposure counterparty. IM/MM factors are global per market but can be customized per account, whitelisted market makers get preferential factors and Close-Only exemptions. Negotiating MM-whitelist status with the Pendle team is an operational prerequisite, not an afterthought: it changes both capital efficiency and how exposed you are to being locked into close-only right when you'd want to size up.

**Settlement.** The fixed leg settles upfront: long pays PV of size × rate × TTM/year at trade time (`PaymentLib.calcUpfrontFixedCost`). The floating leg settles lazily via a per-market `FIndex` (`{FTag, floatingIndex, feeIndex}`) published at discrete oracle-update or force-cancel ("purge") events, not on a continuous timestamp. A position stores the last `FIndex` it synced against; settling walks the `FTag`s since then in order, pricing each sub-period at the size held *before* that period's fills (`payment = signedSize.mulFloor(ΔfloatingIndex)`, `fee = |signedSize|.mulUp(ΔfeeIndex)`, `PaymentLib.calcSettlement`). No interpolation is ever valid here: an `FIndex` is either the exact value recorded on-chain for a given `FTag`, or it's unknown. PnL accounting can't assume continuity; it has to replicate the event algorithm (fills grouped by `FTag`, then floating settlement against the exact recorded index, repeat) or the local position silently diverges from on-chain truth.

**Integration.** Two-tier signing: sensitive actions (deposit, withdraw, agent approval) sign with the root wallet direct to chain; trading actions (place, cancel, transfer) sign with a scoped agent wallet routed through Pendle's Send Txs Bot for gas and nonce management. The official SDK (`@pendle/sdk-boros` for execution, `@pendle/boros-offchain-math` for `FixedX18` and tick/rate conversion) is TypeScript only, no native Rust SDK. That gap is the central engineering trade-off of the project (see Architecture below).

---

## Risk model

Risk here is protocol-specific, not generic market-making risk:

| Risk | Description |
|---|---|
| **Settlement basis** | Delta between the fixed rate locked and the floating index actually reported by the venue's funding oracle. Modeled as mean-reverting, calibrated against realized funding history, not white noise. |
| **Settlement timing** | Floating payment for a sub-period is computed on position size *before* that period's fills, a gaming window where reducing position right before an `FIndex`-publishing event changes exposure for the period about to close. Has to be an explicit policy, not incidental to inventory update cadence. |
| **Margin/liquidation basis (TWAP)** | Mark rate for margin is a 5-min TWAP lagging the live book. Margin simulation needs to run against a locally-replicated mark rate, not internal mid, or proximity to liquidation gets systematically underestimated in fast moves. |
| **Tick-band purge** | Dynamic bands around mark rate widen with TTM; leaving the band gets an order rejected, or purged if resting. Hard constraint in the quoting engine, not a soft warning. |
| **Close-Only mode** | Aggregate OI approaching the market cap restricts new orders to reduce-only, barring whitelist exemption. Requires continuous OI-utilization monitoring per market. |
| **Cross-margin contagion** | A cross account sharing collateral across maturities can see an individually healthy position pressured by an adverse move elsewhere in the same account, a portfolio construction decision, not just an execution detail. |
| **Permissioned bot risk** | Liquidation Bot, Force-Cancel Bot, CLO Bot are Pendle-operated infrastructure, not deterministic contracts under your control. Justifies an independent kill switch, not just exception handling in the main bot. |
| **Key management** | Root wallet (cold, multisig) vs. agent wallet (hot, trading-scoped) is a protocol requirement, mapped onto standard execution key-separation patterns. |

---

## Architecture

The only official signing/calldata SDK is TypeScript; the decision engine is Rust. Reimplementing `FixedX18` and tick/rate math in Rust is viable; the math is closed-form, not opaque, but shipping it without cross-validation is exactly the kind of silent risk that has no place in production. So the decision engine is **100% Rust** (pricing, risk, quoting, the hot path), and the execution layer initially delegates calldata construction and signing to the official SDK through a thin Node sidecar, validated by a golden-vector harness in CI that diffs the Rust reimplementation against real TS SDK output across thousands of generated cases. Native Rust signing is the plan once that harness has run clean for a few weeks.

```
boros-mm/                          # Cargo workspace
├── crates/
│   ├── tick-math/                 # rate(tick) = 1.00005^(tick·tickStep) − 1, FixedX18, golden-vector tests vs @pendle/boros-offchain-math
│   ├── curve-engine/               # per-zone implied-APR curve (Fritsch-Carlson monotone cubic), butterfly relative-value signal, strategy tool, not protocol-required
│   ├── feed-ingest/                 # feedhandler-core-rs extension: Boros WS + REST fallback, normalized funding feeds (Binance/Bybit/OKX/Hyperliquid)
│   ├── margin-sim/                  # off-chain replica of Position/Total Value, IM, MM, Health Ratio, Liquidation Incentive, shadows on-chain state
│   ├── quoting-engine/              # Avellaneda-Stoikov/GLFT closed-form adapted to DV01 inventory + on-chain maker rate bounds, zero-maker-fee spread economics
│   ├── arb-engine/                  # cross-venue basis (Boros vs CEX funding) + curve-engine butterfly signal translated into a directional trade
│   ├── risk-engine/                 # pre-trade DV01/notional/health-ratio limits, shadow-vs-real divergence monitoring, independent kill switch
│   ├── settlement-ledger/           # lazy settlement replica (FTag-indexed FIndex history + floating/fee payment) for local PnL reconciliation
│   └── oms-core/                    # OrderId encoding, local order state machine, TimeInForce, upfront fixed-leg cost
├── services/
│   ├── mm-bot/                      # main binary: tokio event loop wiring quoting-engine + margin-sim + execution-adapter
│   ├── arb-bot/                     # cross-venue funding divergence scanner + executor
│   └── risk-monitor/                # independent watchdog process: separate failure domain, kill-switch authority
├── execution-adapter/
│   ├── sidecar-ts/                  # thin wrapper over @pendle/sdk-boros: calldata, agent signing, Send Txs Bot
│   └── rust-bridge/                 # tonic gRPC client to sidecar-ts, retry/backoff + ~130-code error taxonomy classification
└── tools/
    ├── backtester/                  # replay against historical Boros NDJSON exports (trades, OHLCV, settlements, book snapshots)
    └── golden-vector-gen/           # generates and diffs tick/rate/margin cases: Rust vs TS SDK
```

`risk-monitor` runs as a fully separate process by design. Liquidation, force-cancel, and Close-Only mode execute through permissioned Pendle infrastructure outside direct control, so the watchdog can't share a failure domain with the quoting engine. It has independent authority to flatten positions through the execution adapter if health ratio crosses a conservative threshold ahead of 1.0.

---

### Built and tested

**`crates/tick-math`**
`FixedX18` is an i128 fixed-point type with exact add/sub; `mul_fixed`/`div_fixed` fall back to f64 and are documented as unfit for money math. `mul_floor`/`mul_ceil` (signed) and `mul_div_up`/`mul_div_down` (unsigned, `u128` magnitude) are exact 256÷128 integer arithmetic, no f64, matching `PMath.sol`'s rounding surface (`mulFloor`/`mulCeil`/`mulUp`/`mulDown`) 1:1. It's a bit-serial binary long division rather than multi-limb Knuth Algorithm D, exact, but not the fastest possible; worth revisiting if this ever sits on a hot path. `mul3_div_floor_u32`/`mul3_div_up_u32` extend the same exact-division machinery to a triple product with a single division (up to 3×128-bit limbs), needed because `size × rate × time_to_maturity` as two chained two-operand divisions is a different, less accurate computation than one fused division even with both sides exact, `time_to_maturity` is bounded to `u32` because the contract types it that way (`PaymentLib.calcPositionValue(int256, int256, uint32)`). This is what closes `margin-sim`'s double-rounding gap, see below. Also here: `SECONDS_PER_YEAR`/`ONE_MUL_YEAR` (`365 days`, no leap adjustment; `1e18 * 365 days`), `tick_to_rate`/`rate_to_tick` with `Rounding::Floor`/`Ceil` and a precomputed `LN_TICK_BASE`, and `u128_wide_mul` (schoolbook 4-limb multiply). Golden-vector harness loads `vectors.json` from the TS SDK if present, falls back to embedded cases otherwise. 46 unit tests + 2 golden-vector tests, 0 warnings.

**`crates/margin-sim`**, ported from `MarginViewUtils.sol`/`LiquidationViewUtils.sol`
`MarginEngine` exposes `compute_account_state` and `margin_headroom_for_order`, with per-market `MarketState` carrying mark rate and time-to-maturity as raw seconds, matching the contract's `uint32 timeToMat` instead of a pre-converted FixedX18 year-fraction. `Position::value()` is now a single fused triple-product-then-one-division (`tick_math::mul3_div_floor_u32`), matching `PaymentLib.calcPositionValue` exactly instead of two chained `mul_fixed` calls, this closes a double-rounding gap that two independent multiplications can't avoid. `calc_pm`/`calc_mm`/`calc_im_from_pm` are exact ports of `_calcPM`/`_calcMM`/`_calcIM`, including the worst-case position/order netting logic (a resting order that only closes exposure now correctly reduces required margin instead of adding to it, down to exactly zero extra margin when it closes the position in full) and the real `rawDivUp` rounding rather than floor. `HealthThresholds` was removed outright rather than patched: there's no such protocol constant. `LiquidationViewUtils.sol` shows liquidation eligibility is exactly `health_ratio < 1.0` (`LIQUIDATION_HEALTH_RATIO`), no fixed "risky" cutoff anywhere in source, the liquidation incentive is computed dynamically from governance `LiqSettings { base, slope, feeRate }`, modeled as its own struct, fetched fresh, no invented default. 10 tests, 0 warnings, including two that specifically exercise the netting fix: a partially-closing order nets against the position instead of adding IM on top, and a fully-closing order adds exactly zero extra margin.

**`crates/settlement-ledger`**
`SettlementLedger` keeps a per-market fill and `FIndex` history, `FTag`-indexed, no timestamp anywhere in the settlement path. `settle_to` is a direct port of `PaymentLib.calcSettlement` plus `ProcessMergeUtils.__processSweptUntilStop`: sub-periods split at each pending fill's `FTag`, priced with the size held before that fill, using the exact `mul_floor`/`mul_div_up` from `tick-math`. It tracks both the floating payment and the protocol fee (`feeIndex`) as a pair (`PayFee`); an earlier draft only had the payment leg. Never interpolates, a missing `FIndexRecord` for a required `FTag` is a hard error (`MissingFIndexRecord`), not an approximation. The upfront fixed-leg cost (`calcUpfrontFixedCost`) is explicitly out of scope here, it belongs with fill/order processing in `oms-core`, and is tracked as a TODO rather than implemented. 15 tests, 0 warnings, including the exact `last == current FIndex` zero-payment fast path and a floor-vs-ceil negative-remainder case exercised through the full settlement path, not just the underlying math crate.

**`crates/oms-core`**
`OrderId` is an exact bit-packing of side, XOR-and-inverted-encoded tick, and order index, no `marketId`, no `expiry`, no `nonce`. `OrderStatus` models the 4-state on-chain status (`NotExist`/`Open`/`PendingSettle`/`Purged`) alongside this crate's own richer `LocalOrderStatus` (`Open`/`PartiallyFilled`/`Filled`/`Cancelled`/`ForcedCancelled`), since the contract doesn't retain "cancelled" as distinct from "never existed" once an order's gone. `OrderTracker` drives local order state off the 5 lifecycle events (`LimitOrderPlaced`/`Filled`/`PartiallyFilled`/`Cancelled`/`ForcedCancelled`), correctly handling `LimitOrderFilled(from, to)` as a contiguous range that sweeps the whole market, silently skipping ids that aren't ours. `calc_upfront_fixed_cost` is an exact port of `PaymentLib.calcUpfrontFixedCost`, the fixed-leg payment stream deliberately left out of `settlement-ledger`. Also here: `Trade`/`Fill`, `MarketAcc`, `TimeInForce`, and the `orderAndOtc` request/response shapes (`LongShort`, `CancelData`, `OtcTrade`). Deliberately not implemented: `Trade::from_size_and_rate`, which needs a signed truncate-toward-zero `mulDown` that `tick-math` doesn't have yet (only floor/ceil signed, up/down unsigned exist so far). This crate only ever receives already-computed `Trade`/`Fill` values and never predicts one, so it isn't blocked, just tracked. 21 tests, 0 warnings, including the priority-ordering property (higher tick sorts first for LONG, lower tick sorts first for SHORT) and the range-sweep-but-filter-to-ours case.

**`execution-adapter`**, the first piece of this project to cross into TypeScript and depend on a package outside its own source tree
Built against `@pendle/sdk-boros` on npm. `proto/execution.proto` is the shared contract, `PlaceOrder`/`CancelOrders`/`GetTxStatus` mirroring `Exchange.placeOrder`/`cancelOrders` and the SendTxsBot trace endpoints; `FixedX18` and `OrderId` cross the wire as decimal strings, never a native numeric type. `rust-bridge` provides `ExecutionClient` (retry/backoff, retrying only requests classified `Retriable`), `error_class` (classification against the ~130-code error taxonomy, confident where the name is unambiguous, everything else defaults to non-retriable `Unknown` instead of guessed), and `types` (proto ↔ `oms_core`/`tick_math` conversions), with 14 unit tests and 4 integration tests against an in-process mock gRPC server proving the retry logic retries on `Retriable`, stops immediately on `Fatal`/`Unknown`, and gives up cleanly at `max_attempts`. `sidecar-ts` wraps the `Exchange`/`Agent`/`BorosBackend.createSendTxsBotSdk` classes, signs and encodes nothing itself, typechecks and builds clean against the installed SDK, with 5 passing tests for the error-mapping logic, the most speculative part of the adapter, since the SDK exposes no typed error class for API/REST-level rejections, only for decoded contract reverts, so that path is best-effort by design. Key separation follows the SDK directly: `Agent.create`/`Agent.createFromPrivateKey` plus `Exchange.approveAgent` is the two-tier signing scheme, root wallet signs the one-time agent approval only, `sidecar-ts` only ever holds the agent key (env-loaded for local dev; production needs a proper secrets manager, not implemented here).

**`crates/curve-engine`**, the first Sprint 3 piece, and the one with no protocol source to check against, because there isn't one. Boros doesn't publish or require a term structure; each market's implied APR is independently discovered by its own orderbook and AMM (`whitepapers/AMM.pdf`). This is a strategy tool grounded in standard curve-construction literature (Fritsch & Carlson 1980; Hagan & West 2006) instead. `MonotoneCubicSpline` is Fritsch-Carlson monotone cubic Hermite interpolation, f64 throughout since this is a reference/signal curve rather than a settlement calculation, same precedent as `tick_to_rate`/`rate_to_tick`, and it never extrapolates past the observed maturity range. `Curve` fits a spline per `Zone` (Boros's own cross-margin grouping, same underlying, multiple maturities), with `rate_at()` for an interpolated reference rate and `detect_butterflies()` for relative-value signals. These are named signals, not arbitrage, on purpose: unlike a bond curve, where a negative butterfly is a riskless, replicable position, Boros markets at different maturities aren't fungible on-chain, there's no mechanism to convert exposure from one maturity to another directly. A detected butterfly is a candidate calendar spread, nothing downstream should treat it as more than that. 17 tests, 0 warnings, including no-overshoot checks on both monotone data and a local extremum (the actual failure mode a naive cubic spline has, and the reason this algorithm exists) and sign-correctness on both a cheap and a rich middle maturity.

**`crates/quoting-engine`**, Avellaneda-Stoikov (2008) / Guéant-Lehalle-Fernandez-Tapia (2012) closed form, adapted to DV01 inventory and rate space
`reservation_rate`/`optimal_spread` implement the classic A-S skew and spread plus a modest `carry_adjustment` term, weaker than the `-qf` funding/inventory coupling in *"Funding-Aware Optimal Market Making for Perpetual DEXs"* (arXiv:2605.06405): Boros pays its fixed leg upfront rather than as a running cost like perpetual funding, so the paper's coupling doesn't transfer cleanly, and the crate documents that limitation in-code. `carry_weight = 0.0` disables it entirely. One finding worth flagging: the two terms of `optimal_spread` don't move the same direction in `γ`, the liquidity term can shrink as risk-aversion increases for short horizons or low vol, which is the common case for frequent requoting, so "higher `γ` always widens the spread" is false in general. Two tests cover both regimes (inventory-dominated widens with `γ`, liquidity-dominated narrows with it), the non-monotonic case is easy to miss if you only test the inventory-dominated regime. `MakerRateBounds`/`clamp_rate` is an exact port of the maker rate bound formula (`Upper/Lower Bound = MarkRate × (1 ± (loConst + loSlope × TTM))`, `Mechanics/OrderBook.md#restrictions`), a quote outside these bounds is a guaranteed on-chain revert (`Limit Rate Out of Bounds`), not a soft suggestion. Spread economics assume zero maker fee (`Mechanics/Fees.md`: maker orders incur no fees when placed), so the spread doesn't carry a fee-coverage floor. No calibration defaults anywhere, `gamma`/`sigma`/`kappa`/`horizon_secs`/`carry_weight` are all required and validated at construction, these are trading parameters a desk calibrates, not something the crate should guess. 21 tests, 0 warnings.

**`crates/risk-engine`**, pre-trade limits, runtime divergence monitoring, kill switch, built on `margin-sim`'s `MarginEngine` rather than re-deriving margin math
`check_pre_trade` checks DV01 (net and gross, via a shared `position_dv01` matching the `DV01 = |size| × ttm_years × 0.0001` formula this workspace has used conceptually since before this crate existed), notional, order-rate throttle, and projected health-ratio-after-order (delegated straight to `MarginEngine::compute_account_state`), returning every violation found rather than just the first. `monitor` runs shadow-vs-real divergence checks for health ratio and mark rate, with both/neither-infinite handled explicitly so a flat account on both sides doesn't register as a divergence. `KillSwitch` latches on trip and requires an explicit, reasoned `reset()`, it never silently self-heals, it can't enforce process isolation on its own, that's `services/risk-monitor`'s job, this crate just provides the logic both processes share. No defaults for any limit or tolerance: risk policy isn't this crate's to guess. 18 tests, 0 warnings.

**`crates/arb-engine`**, two relative-value signals, neither a riskless arbitrage despite the name (kept for continuity with the roadmap's own naming), each flagged with exactly what risk remains in its own doc comment
`detect_cross_venue_signal` compares Boros implied APR against a comparable CEX perp's expected funding. Basis risk is real: expected CEX funding isn't locked in the way Boros's fixed leg is, and the two floating indices aren't identical by construction. `to_calendar_spread_trade` wraps a `curve_engine::ButterflySignal` into a directional trade (opposite sides at mid vs. wings), deliberately not DV01-sized, since a true risk-minimized butterfly needs position-sizing context this crate doesn't have; it gives direction, not size. 7 tests, 0 warnings.

**`crates/feed-ingest`** *(requires Rust ≥ 1.80, see Toolchain note)*
`WsConnector` handles exponential reconnect and ping/pong keepalive with a fresh `WsSink` per reconnection. `OrderBook` is a sparse `BTreeMap<i16, FixedX18>` with seq-gap detection and BBO/mid/spread/depth. `BorosFeedHandler` covers subscribe/dispatch/parse, with explicit `self` destructuring to keep the tokio future `Send`-safe. Funding feeds cover Binance (`markPrice`), Bybit (`tickers.v5`), and Hyperliquid (`activeAssetCtx`). `BookStateManager` exposes a synchronous `drain()` so the caller controls update application timing. 5 book tests.

**`tools/golden-vector-gen`**: Rust emitter plus a TypeScript cross-validator against the official SDK.

**`services/risk-monitor`**, separate process by design, own failure domain from the quoting/execution path
Polls margin state independently and compares it against the shadow computation from `margin-sim`/`risk-engine`, flagging divergence past a conservative threshold rather than trusting either side blindly. Holds independent kill-switch authority to flatten positions through the execution adapter. 5 tests, 0 warnings.

**`services/mm-bot`**, main binary, multi-market from the start rather than a single-market MVP
Wires `quoting-engine` + `margin-sim` + `execution-adapter` into a tokio event loop: one `MarketRuntime` per configured market, sharing a `Zone` for curve fitting where markets are cross-margined together. Tests cover the pure decision logic, `reference_rate_for`, `rate_moved_enough`, `build_zone`, `build_margin_account`, `desired_quote`, including a check that the fitted curve interpolates correctly at an exact input point. 8 tests, 0 warnings.

**`services/arb-bot`**, cross-venue funding divergence scanner and executor
Same wiring pattern as `mm-bot`, decision logic from `arb-engine::cross_venue`/`calendar_spread` instead of `quoting-engine`. Two known scope cuts, both intentional and documented in-code rather than silent: the three legs of a calendar spread aren't placed atomically, so a failed second leg after a filled first leg leaves a partial position logged but not auto-unwound; and a cross-venue signal's CEX hedge leg is never placed by this bot, `arb_engine::CrossVenueSignal` only ever gives the Boros side. 33 tests, 0 warnings, covering config parsing, signal cooldowns, margin account construction, and the pure half of REST reconciliation split out specifically to be testable without a live call.

### Known technical debt

| Location | Debt | Impact if unresolved |
|---|---|---|
| `margin-sim/engine.rs` | `check_cross_token_consistency` is a no-op | Doesn't enforce shared collateral in cross-margin, needs `token_id` added to `MarginConfig` |
| `feed-ingest/boros/types.rs` | Wire format unverified against live API, and doesn't exist yet in this repo | Silent `None` on parse if field names differ, once written |
| `execution-adapter/sidecar-ts/errorMapping.ts` | API/REST-level error code extraction (`err.code`/`err.response.data.code`) is best-effort: the SDK exposes no typed error class for this family, only for decoded contract reverts | Wrong classification only surfaces once real traffic hits an API-level rejection; fix against an observed real error shape, not by guessing harder |
| `execution-adapter/rust-bridge/error_class.rs` | Only ~90 of 130+ error codes are classified with confidence (infra = retriable, clear business rejections = fatal); the rest (`Zone*`, `Conditional*`, `Deleverager*`, `Portal*`) default to `Unknown` (non-retriable) | Safe by design, never retries blindly, but some genuinely-fatal or genuinely-retriable codes get treated as unknown until someone classifies them with real context |
| `oms-core/types.rs` | `Trade::from_size_and_rate` not implemented; needs the signed, truncate-toward-zero `mulDown` variant, not yet in `tick-math` (which only has floor/ceil signed, up/down unsigned) | Not currently blocking, this crate only ever receives already-computed `Trade`s from events/API, but blocks any future cost-prediction use case such as quoting-engine estimating order cost pre-submission |
| `services/arb-bot` | Calendar spread legs placed sequentially, not atomically; no automatic unwind on any signal; cross-venue CEX hedge leg never placed by this bot | Exposure risk if a leg fails mid-sequence or a signal reverses before someone manually closes the other side, currently requires an operator watching |

### Not yet built

**5: validation**
- `tools/backtester`: NDJSON replay with FIFO queue simulation, offline γ/k tuning before going live
- Add `token_id` to `MarginConfig` and close `check_cross_token_consistency`; live wire-format verification for `boros/types.rs`
- No service in this repo has run against the live Boros backend yet. REST, WS, and execution-adapter are all verified against fixtures and mocks, not real traffic. This is the actual gap left, not missing code.

---

## Toolchain

`feed-ingest` is excluded from the workspace default build on toolchains below 1.80: a transitive dependency (`idna_adapter` 1.2.2, via `tokio-tungstenite` → `url`) requires edition 2024. Uncomment `"crates/feed-ingest"` in the workspace `Cargo.toml` once you're on **Rust ≥ 1.80**.

`execution-adapter/rust-bridge` hit the same class of problem through `tonic-build`'s dependency tree (`indexmap`, `tempfile`, `getrandom`). Fixed instead of worked around: `cargo update -p <pkg> --precise <older-version>` pinned each one to its last edition-2021-compatible release, so `rust-bridge` builds and tests clean on Rust 1.75. The pins live in `Cargo.lock`. An unqualified `cargo update` will re-resolve past them and bring the edition2024 error back, repin or move to Rust ≥ 1.80.

---

## Design principles

- Nothing hardcoded against assumed protocol values. Every constant (tick step, margin factors, OI caps) is sourced from live market config, not embedded.
- Margin and settlement logic runs in shadow against replicated on-chain state before any capital is committed.
- Risk monitoring is architecturally independent from the quoting/execution process, no shared failure domain.
- The Rust/TypeScript boundary is bridged through a validated golden-vector harness, not a blind reimplementation.
- Protocol mechanics are checked against `pendle-finance/boros-core-public` source and its published audits directly, not inferred from docs or assumed from analogous protocols.
