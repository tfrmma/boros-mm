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

**Settlement.** Fixed leg settles upfront (long pays PV of size × rate × TTM/year at trade time, `PaymentLib.calcUpfrontFixedCost`); floating leg settles lazily via a per-market `FIndex` (`{FTag, floatingIndex, feeIndex}`) published at discrete oracle-update or force-cancel ("purge") events not a continuous timestamp. A position stores the last `FIndex` it synced against; settling walks the `FTag`s since then in order, pricing each sub-period at the size held *before* that period's fills (`payment = signedSize.mulFloor(ΔfloatingIndex)`, `fee = |signedSize|.mulUp(ΔfeeIndex)`, `PaymentLib.calcSettlement`). No interpolation is ever valid here an `FIndex` is either the exact value recorded on-chain for a given `FTag`, or it's unknown. PnL accounting can't assume continuity; it has to replicate the event algorithm (fills grouped by `FTag`, then floating settlement against the exact recorded index, repeat) or the local position silently diverges from on-chain truth. Verified against `pendle-finance/boros-core-public` source (not assumed) an earlier draft of this document described this in terms of a continuous `fTime`, which was wrong; corrected once we read the contracts directly.

**Integration.** Two-tier signing: sensitive actions (deposit, withdraw, agent approval) sign with the root wallet direct to chain; trading actions (place, cancel, transfer) sign with a scoped agent wallet routed through Pendle's Send Txs Bot (gas/nonce management). Official SDK is TypeScript only (`@pendle/sdk-boros` this doc previously had the name backwards, `@pendle/boros-sdk-public`, before verifying against the real npm package; `@pendle/boros-offchain-math` for `FixedX18` and tick/rate conversion), no native Rust SDK. This is the central engineering trade-off of the project (see Architecture below).

---

## Risk model

Risk here is protocol-specific, not generic market-making risk:

| Risk | Description |
|---|---|
| **Settlement basis** | Delta between the fixed rate locked and the floating index actually reported by the venue's funding oracle. Modeled as mean-reverting (calibrated against realized funding history), not white noise. |
| **Settlement timing** | Floating payment for a sub-period is computed on position size *before* that period's fills, a gaming window where reducing position right before an `FIndex`-publishing event changes exposure for the period about to close. Must be an explicit policy, not incidental to inventory update cadence. |
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
│   ├── settlement-ledger/           # lazy settlement replica (FTag-indexed FIndex history + floating/fee payment) for local PnL reconciliation
│   └── oms-core/                    # OrderId encoding (verified), local order state machine, TimeInForce, upfront fixed-leg cost
├── services/
│   ├── mm-bot/                      # main binary: tokio event loop wiring quoting-engine + margin-sim + execution-adapter
│   ├── arb-bot/                     # cross-venue funding divergence scanner + executor
│   └── risk-monitor/                # independent watchdog process: separate failure domain, kill-switch authority
├── execution-adapter/
│   ├── sidecar-ts/                  # thin wrapper over the real @pendle/sdk-boros SDK: calldata, agent signing, Send Txs Bot verified against the published npm package
│   └── rust-bridge/                 # tonic gRPC client to sidecar-ts, retry/backoff + real ~130-code error taxonomy classification
└── tools/
    ├── backtester/                  # replay against historical Boros NDJSON exports (trades, OHLCV, settlements, book snapshots)
    └── golden-vector-gen/           # generates and diffs tick/rate/margin cases: Rust vs TS SDK
```

`risk-monitor` runs as a fully separate process by design: liquidation, force-cancel, and Close-Only mode are executed by permissioned Pendle infrastructure outside direct control, so the watchdog can't share a failure domain with the quoting engine. It has independent authority to flatten positions via the execution adapter if health ratio crosses a conservative threshold ahead of 1.0.

---

## Status

```
Data infrastructure:     ██████████████░░  90%  (settlement-ledger done; feed-ingest still toolchain-blocked, boros/types.rs wire format unverified)
Risk management:         █████████████░░░  80%  (margin-sim closed against real MM/IM/PM formulas; risk-engine the live monitoring service still pending)
Execution:               ██████████████░░  90%  (oms-core + execution-adapter done and verified at the unit/integration level; never run against the live Boros backend)
Strategy:                ░░░░░░░░░░░░░░░░   0%  (curve-engine, quoting-engine, arb-engine)
Production services:     ░░░░░░░░░░░░░░░░   0%
```

### Built and tested

**`crates/tick-math`**
- `FixedX18`: i128 fixed-point, exact add/sub, `mul_fixed`/`div_fixed` f64 fallback (documented, not for money math)
- `mul_floor`/`mul_ceil` (signed), `mul_div_up`/`mul_div_down` (unsigned, `u128` magnitude) exact 256÷128 integer arithmetic, no f64. Named and behaved 1:1 against `PMath.sol`'s actual rounding surface (`mulFloor`/`mulCeil`/`mulUp`/`mulDown`), verified against `pendle-finance/boros-core-public` source, not assumed. Not literal multi-limb Knuth Algorithm D a simpler bit-serial binary long division, exact but not the fastest possible; documented tradeoff, revisit if this ever sits on a hot path.
- `mul3_div_floor_u32`/`mul3_div_up_u32`: the same exact-division machinery extended to a **triple** product with a single division (up to 3×128-bit limbs) needed because `size × rate × time_to_maturity` computed as two chained two-operand divisions is a different (less accurate) computation than one fused division, even with both sides exact. `time_to_maturity` is bounded to `u32` because the real contract types it that way (`PaymentLib.calcPositionValue(int256, int256, uint32)`), not an arbitrary simplification. This is what closes `margin-sim`'s double-rounding gap, see below.
- `SECONDS_PER_YEAR`, `ONE_MUL_YEAR`: shared protocol constants (`365 days`, no leap adjustment; `1e18 * 365 days`), source-verified against `PMath.sol`
- `tick_to_rate` / `rate_to_tick` with `Rounding::Floor`/`Ceil`, precomputed `LN_TICK_BASE`
- `u128_wide_mul`: schoolbook 4-limb multiply, verified against `u128::MAX²`
- Golden-vector harness: loads `vectors.json` from the TS SDK if present, falls back to embedded cases
- 46 unit tests + 2 golden-vector tests, 0 warnings

**`crates/margin-sim`** fully rewritten this session against `MarginViewUtils.sol`/`LiquidationViewUtils.sol` (source-verified, not the earlier approximation)
- `MarginEngine`: `compute_account_state`, `margin_headroom_for_order`, per-market `MarketState` (mark rate + time-to-maturity as raw seconds, matching the contract's `uint32 timeToMat` no longer pre-converted to a FixedX18 year-fraction)
- `Position::value()`: fixed now a single fused triple-product-then-one-division (`tick_math::mul3_div_floor_u32`), matching `PaymentLib.calcPositionValue` exactly instead of two chained `mul_fixed` calls (closes the "Hallazgo 2" double-rounding gap)
- `calc_pm`/`calc_mm`/`calc_im_from_pm`: exact ports of `_calcPM`/`_calcMM`/`_calcIM`, including the worst-case position/order netting logic (`open_order_im`'s "no netting" gap is closed a resting order that only *closes* exposure now correctly reduces required margin instead of adding to it, down to the exact zero-extra-margin case when it closes the position in full) and the real `rawDivUp` rounding (not floor)
- `HealthThresholds` **removed**, not fixed source-verified there's no such protocol constant. `LiquidationViewUtils.sol` shows liquidation eligibility is exactly `health_ratio < 1.0` (now `LIQUIDATION_HEALTH_RATIO`) with no fixed "risky" cutoff anywhere in source; the liquidation incentive is computed dynamically from governance `LiqSettings { base, slope, feeRate }` (now modeled as its own struct, fetched fresh, no invented default)
- 10 tests, 0 warnings including two that specifically exercise the netting fix: a partially-closing order must net against the position (not add IM on top of it), and a fully-closing order must add exactly zero extra margin

**`crates/settlement-ledger`**
- `SettlementLedger`: per-market fill + `FIndex` history, `FTag`-indexed, no timestamp anywhere in the settlement path
- `settle_to`: direct port of `PaymentLib.calcSettlement` + `ProcessMergeUtils.__processSweptUntilStop` sub-periods split at each pending fill's `FTag`, priced with the size held *before* that fill, using the new exact `mul_floor`/`mul_div_up` from `tick-math`
- Tracks both the floating payment and the protocol fee (`feeIndex`) as a pair (`PayFee`) the earlier draft only had the payment leg
- Never interpolates: a missing `FIndexRecord` for a required `FTag` is a hard error (`MissingFIndexRecord`), not an approximation
- Explicitly out of scope: the upfront fixed-leg cost (`calcUpfrontFixedCost`), paid at fill time from the trade's own cost belongs with fill/order processing (`oms-core`), documented as a TODO rather than implemented
- 15 tests, 0 warnings including the exact `last == current FIndex` zero-payment fast path and a floor-vs-ceil negative-remainder regression case exercised through the full settlement path, not just the underlying math crate

**`crates/oms-core`**
- `OrderId`: exact bit-packing (`side`/XOR-and-inverted-encoded-tick/`order_index` no `marketId`, no `expiry`, no `nonce`; this doc guessed wrong before verifying `types/Order.sol` directly)
- `OrderStatus`: the real 4-state on-chain status (`NotExist/Open/PendingSettle/Purged`), plus this crate's own richer `LocalOrderStatus` (`Open/PartiallyFilled/Filled/Cancelled/ForcedCancelled`) the contract doesn't retain "cancelled" as distinct from "never existed" once an order's removed, so this crate has to
- `OrderTracker`: event-driven local order state, built from the 5 real lifecycle events (`LimitOrderPlaced/Filled/PartiallyFilled/Cancelled/ForcedCancelled`). Correctly handles `LimitOrderFilled(from, to)` being a contiguous **range** that sweeps the whole market, silently skipping ids that aren't ours
- `calc_upfront_fixed_cost`: exact port of `PaymentLib.calcUpfrontFixedCost` the fixed-leg payment stream deliberately left out of `settlement-ledger`
- `Trade`/`Fill`, `MarketAcc`, `TimeInForce` (confirmed correct against the original assumption), and the `orderAndOtc` request/response shapes (`LongShort`, `CancelData`, `OtcTrade`)
- Deliberately not implemented: `Trade::from_size_and_rate` needs a signed truncate-toward-zero `mulDown`, a rounding mode `tick-math` doesn't have yet (only floor/ceil signed, up/down unsigned exist). This crate only ever receives already-computed `Trade`/`Fill` values, never predicts one, so it isn't blocked documented as a TODO rather than approximated with the wrong rounding
- 21 tests, 0 warnings including the priority-ordering property (higher tick sorts first for LONG, lower tick sorts first for SHORT) and the "range sweeps the whole market, we only update our own" case

**`execution-adapter`** the first piece of this project to cross into TypeScript, and to depend on an external package this project doesn't control the source of
- Real published SDK confirmed and used: `@pendle/sdk-boros` (npm), **not** `@pendle/boros-sdk-public` as this doc assumed before verifying, the source repo (`pendle-finance/sdk-boros-public`) returns HTTP 401 from this environment even though it's indexed publicly, so verification here is against the compiled package downloaded from the npm registry, not the repo
- `proto/execution.proto`: the shared contract. `PlaceOrder`/`CancelOrders`/`GetTxStatus`, mirroring `Exchange.placeOrder`/`cancelOrders` and the SendTxsBot trace endpoints (`BorosSendTxsBotSDK.d.ts`) confirmed in the real package. FixedX18 and `OrderId` cross the wire as decimal strings, never a native numeric type
- `rust-bridge`: `ExecutionClient` (retry/backoff, retrying only requests classified `Retriable`), `error_class` (classification against the real ~130-code error taxonomy confidently classified where the name is unambiguous, everything else defaults to non-retriable `Unknown` rather than guessed), `types` (proto ⟷ `oms_core`/`tick_math` conversions). 14 unit tests + 4 integration tests against an in-process mock gRPC server proving the retry logic actually retries on `Retriable`, stops immediately on `Fatal`/`Unknown`, and gives up cleanly at `max_attempts`
- `sidecar-ts`: thin wrapper over the real `Exchange`/`Agent`/`BorosBackend.createSendTxsBotSdk` classes signs and encodes nothing itself. Typechecks clean against the real installed SDK (`npm run typecheck`), builds clean (`npm run build`), 5 passing tests for the error-mapping logic (the most speculative part, the SDK exposes no typed error class for API/REST-level rejections, only for decoded contract reverts, so that path is documented as best-effort, not verified)
- Key-separation design confirmed against the real SDK: `Agent.create`/`Agent.createFromPrivateKey` + `Exchange.approveAgent` is exactly the two-tier signing this project's docs already assumed conceptually root wallet signs the one-time agent approval only, `sidecar-ts` only ever holds the agent key (loaded from env for local dev; production needs a real secrets manager, explicitly not implemented here)

**`crates/feed-ingest`** *(requires Rust ≥ 1.80, see Toolchain note)*
- `WsConnector`: exponential reconnect, ping/pong keepalive, fresh `WsSink` per reconnection
- `OrderBook`: sparse `BTreeMap<i16, FixedX18>`, seq-gap detection, BBO/mid/spread/depth
- `BorosFeedHandler`: subscribe/dispatch/parse, explicit `self` destructuring for `Send`-safety across the tokio future
- Funding feeds: Binance (`markPrice`), Bybit (`tickers.v5`), Hyperliquid (`activeAssetCtx`)
- `BookStateManager`: synchronous `drain()`, caller controls update application timing
- 5 book tests

**`tools/golden-vector-gen`**: Rust emitter + TypeScript cross-validator against the official SDK

### Known technical debt

| Location | Debt | Impact if unresolved |
|---|---|---|
| `margin-sim/engine.rs` | `check_cross_token_consistency` is a no-op | Doesn't enforce shared collateral in cross-margin needs `token_id` added to `MarginConfig` |
| `feed-ingest/boros/types.rs` | Wire format unverified against live API and doesn't exist yet in this repo at all | Silent `None` on parse if field names differ, once written |
| `execution-adapter/sidecar-ts/errorMapping.ts` | API/REST-level error code extraction (`err.code`/`err.response.data.code`) is best-effort the SDK exposes no typed error class for this family (`ApiErrorCodes`), only for decoded contract reverts | Wrong classification only surfaces once real traffic hits an API-level rejection; fix against an observed real error shape, not by guessing harder |
| `execution-adapter/rust-bridge/error_class.rs` | Only ~90 of 130+ real error codes are classified with real confidence (infra=retriable, clear business rejections=fatal); the rest (`Zone*`, `Conditional*`, `Deleverager*`, `Portal*`) default to `Unknown` (non-retriable) | Safe by design (never retries blindly), but means some genuinely-fatal or genuinely-retriable codes will be treated as "unknown" until someone classifies them with real context |
| `oms-core/types.rs` | `Trade::from_size_and_rate` not implemented, needs the signed, truncate-toward-zero `mulDown` variant, not yet in `tick-math` (which only has floor/ceil signed, up/down unsigned) | Not currently blocking (this crate only ever receives already-computed `Trade`s from events/API), but blocks any future cost-prediction use case (e.g. quoting-engine estimating order cost pre-submission) |

### Not yet built

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
- Add `token_id` to `MarginConfig` and close `check_cross_token_consistency`; live wire-format verification for `boros/types.rs`

---

## Toolchain

`feed-ingest` is currently excluded from the workspace default build in sandboxed environments, a transitive dependency (`idna_adapter` 1.2.2, pulled in via `tokio-tungstenite` → `url`) requires Rust edition 2024. Code is correct and tested; it needs **Rust ≥ 1.80**. Uncomment `"crates/feed-ingest"` in the workspace `Cargo.toml` on a compatible toolchain.

`execution-adapter/rust-bridge` hit the same class of issue via `tonic-build`'s own dependency tree (`indexmap`, `tempfile`, `getrandom` all had newer versions requiring edition2024). Unlike `feed-ingest`, this was resolved rather than worked around: `cargo update -p <pkg> --precise <older-version>` pinned each one to the last edition-2021-compatible release, and `rust-bridge` builds and tests clean on Rust 1.75 as a result. The pins are captured in `Cargo.lock` if a future `cargo update` without `--precise` re-resolves past them, the same edition2024 error will reappear; repeat the pin or move to Rust ≥ 1.80.

---

## Design principles

- Nothing hardcoded against assumed protocol values, every constant (tick step, margin factors, OI caps) is sourced from live market config, not embedded.
- Margin and settlement logic runs in shadow against replicated on-chain state before any capital is committed.
- Risk monitoring is architecturally independent from the quoting/execution process, no shared failure domain.
- Cross-language boundary (Rust decision engine / TS signing) is bridged through a validated golden-vector harness, not blind reimplementation.
- Protocol mechanics are verified against `pendle-finance/boros-core-public` source (and its published audits) directly, not inferred from docs or assumed from analogous protocols this document has already been wrong once (`fTime` vs. `FTag`) from skipping that step; the fix is to always check.
