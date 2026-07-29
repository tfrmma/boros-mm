//! Error classification against the error taxonomy published in
//! `@pendle/sdk-boros`'s `dist/errors/ErrorCodes.d.ts` (downloaded from the
//! npm registry and read directly, currently checked against 1.6.3, up
//! from 1.5.0). The source GitHub repo, `pendle-finance/sdk-boros-public`,
//! returns 404 from this environment, this is verified against the
//! compiled package, not the repo.
//!
//! The taxonomy has 226 distinct codes across two families: API-level
//! validation (`ApiErrorCodes`) and decoded Solidity custom errors
//! (`ErrorCodes`). Cross-checked the on-chain half directly against
//! `contracts/lib/Errors.sol` in `pendle-finance/boros-core-public`: three
//! were in that source but missing here (`OTCInvalidAgent`,
//! `OTCMessageExpired`, `OTCRequestExecuted`), added as `Fatal` following
//! the same pattern as the `AuthModule`/`ConditionalModule` codes right
//! above them. The 1.6.x bump also added a `P2P_*` subsystem (8 new
//! codes) not present at 1.5.0, classified below by name.
//!
//! What's left unclassified is genuinely ambiguous from the name alone,
//! not just unreviewed:
//!
//! - `DATA_INCONSISTENCY`, `HTTP_EXCEPTION`, `FailedCall`, `InvalidLength`,
//!   too generic to tell whether a resend would help
//! - `OTC_SHEET_WRITE_FAILED`, `OTC_TRADE_UPDATE_FAILED`, could be "the OTC
//!   trade state is genuinely invalid" (fatal) or "we failed a write, try
//!   again" (retriable), the name alone doesn't say which
//!
//! Both land as `Unknown`, same as any code not in this taxonomy at all,
//! this client treats that as **not retriable by default**, blindly
//! retrying a rejected trading operation is the dangerous default,
//! surfacing an unrecognized code for a human or a higher layer to decide
//! is the safe one.

/// How a rejected request should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient infrastructure issue (RPC, DB, internal 5xx, an external
    /// data fetch). Safe to retry the identical request with backoff.
    Retriable,
    /// The request or account state itself was rejected. Retrying the same
    /// payload will fail the same way, the caller needs to change
    /// something (re-quote, refresh a session, fix input) before trying
    /// again, not just wait and resend.
    Fatal,
    /// Not in the classified set below. Treated as non-retriable by this
    /// client's default policy, but distinguished from `Fatal` so logging
    /// and monitoring can tell "we know this is a real rejection" apart
    /// from "we've never seen this code and don't actually know."
    Unknown,
}

/// Infra/transport-level codes, confident these are safe to retry.
const RETRIABLE_CODES: &[&str] = &[
    "BLOCKCHAIN_RPC_ERROR",
    "EXTERNAL_SERVICE_ERROR",
    "INTERNAL_SERVER_ERROR",
    "DATABASE_ERROR",
    // same shape as EXTERNAL_SERVICE_ERROR, just scoped to one data source
    "HISTORICAL_PRICE_FETCH_FAILED",
    // name says it outright: an optimistic-concurrency conflict on a P2P
    // offer row, the request itself wasn't wrong, resend it
    "P2P_CONFLICT_RETRY",
];

/// Business-logic / validation rejections, confident retrying the exact
/// same request is pointless. Grouped by the subsystem each code belongs to
/// (all source-verified against `boros-core-public` for the on-chain names,
/// or unambiguous from the API-level name).
const FATAL_CODES: &[&str] = &[
    // API-level validation
    "REQUEST_VALIDATION_ERROR", "VALIDATION_ERROR", "INVALID_INPUT",
    "INVALID_TIMESTAMP", "INVALID_AGENT", "INVALID_SIGNATURE",
    "INVALID_ADDRESS", "INVALID_CHAIN_ID", "INVALID_ORDER_REQUEST",
    "INVALID_INTERVAL", "BULK_ORDERS_EMPTY", "TIME_RANGE_TOO_LONG",
    "ESTIMATED_DURATION_TOO_LONG", "MISSING_EXT_ROUTER",
    "MISSING_EXT_CALLDATA", "EIP7702_INCOMPATIBLE",
    "InvalidAMMAcc", "InvalidAMMId", "InvalidFeeRates", "InvalidMaturity",
    "InvalidNumTicks", "InvalidObservationWindow", "InvalidTokenId",
    // not-found (the referenced entity genuinely doesn't exist)
    "MARKET_NOT_FOUND", "ASSET_NOT_FOUND", "ORDER_NOT_FOUND",
    "POSITION_NOT_FOUND", "USER_NOT_FOUND", "ORDER_BOOKS_NOT_FOUND",
    "AMM_NOT_FOUND", "UNDERLYING_APR_NOT_FOUND", "LEADERBOARD_NOT_FOUND",
    "MERKLE_NOT_FOUND", "MOON_PHASE_NOT_FOUND",
    // account/margin rejections
    "INSUFFICIENT_BALANCE", "INSUFFICIENT_MARGIN",
    "INITIAL_MARGIN_EXCEEDS_BALANCE", "LEVERAGE_EXCEEDS_MAX",
    "MMHealthCritical", "MMHealthNonRisky", "MMInsufficientIM",
    "MMInsufficientMinCash", "MMInvalidCritHR", "MMIsolatedMarketDenied",
    "MMMarketAlreadyEntered", "MMMarketExitDenied", "MMMarketLimitExceeded",
    "MMMarketMismatch", "MMMarketNotEntered", "MMSimulationOnly",
    "MMTokenMismatch", "MMTransferDenied",
    // order/market rejections
    "MARKET_EXPIRED", "MARKET_PAUSED", "SLIPPAGE_EXCEEDED",
    "ORDER_ALREADY_FILLED", "ORDER_CANCELLED", "POSITION_CLOSED",
    "VALUE_TOO_SMALL", "ORDER_VALUE_TOO_LOW", "ORDER_WRONG_SIDE",
    "ORDER_SIZE_EXCEEDS_POSITION", "MarketCLO", "MarketDuplicateOTC",
    "MarketMatured", "MarketMaxOrdersExceeded", "MarketOICapExceeded",
    "MarketOrderALOFilled", "MarketOrderCancelled",
    "MarketOrderFOKNotFilled", "MarketOrderFilled",
    "MarketOrderNotFound", "MarketOrderRateOutOfBound", "MarketPaused",
    "MarketSelfSwap", "MarketZeroSize", "MarketLastTradedRateTooFar",
    "MarketInvalidDeleverage", "MarketInvalidFIndexOracle",
    "MarketInvalidLiquidation", "MarketLiqNotReduceSize",
    "CLOInvalidThreshold", "CLOMarketInvalidStatus", "CLOThresholdNotMet",
    "LiquidationAMMNotAllowed", "SimulationOnly", "ProfitMismatch",
    "ZeroArbitrageSize",
    // MarketHub-level (MH = MarketHub, MarketHubSetAndView.sol)
    "MHInvalidLiquidator", "MHMarketExists", "MHMarketNotByFactory",
    "MHMarketNotExists", "MHTokenExists", "MHTokenLimitExceeded",
    "MHTokenNotExists", "MHWithdrawNotReady",
    // AMM
    "AMMCutOffReached", "AMMInsufficientCashIn", "AMMInsufficientCashOut",
    "AMMInsufficientLiquidity", "AMMInsufficientLpOut",
    "AMMInsufficientSizeOut", "AMMInvalidParams", "AMMInvalidRateRange",
    "AMMNegativeCash", "AMMNotFound", "AMMSignMismatch",
    "AMMTotalSupplyCapExceeded", "AMMWithdrawOnly",
    "AMM_CUT_OFF_REACHED", "AMM_WITHDRAW_ONLY",
    // token-level
    "ERC20InsufficientAllowance", "ERC20InsufficientBalance",
    "ERC20InvalidApprover", "ERC20InvalidReceiver",
    "ERC20InvalidSender", "ERC20InvalidSpender",
    "TOKEN_NOT_SUPPORTED", "BOROS20NotEnoughBalance",
    // trade validation
    "TradeALOAMMNotAllowed", "TradeAMMAlreadySet",
    "TradeMarketIdMismatch", "TradeOnlyAMMAccount",
    "TradeOnlyForIsolated", "TradeOnlyMainAccount",
    "TradeUndesiredRate", "TradeUndesiredSide",
    // math, retrying identical numbers reproduces the identical overflow
    "Overflow", "MathOutOfBounds", "MathInvalidExponent", "DivFailed",
    "DivWadFailed", "MulWadFailed", "SDivWadFailed", "SMulWadFailed",
    // auth, needs a session/credential fix, not a blind resend
    "UNAUTHORIZED", "FORBIDDEN", "Unauthorized", "AGENT_EXPIRED",
    "INVALID_API_KEY", "DISABLED_AGENT", "AuthAgentExpired",
    "AuthExpiryInPast", "AuthIntentExecuted", "AuthIntentExpired",
    "AuthInvalidConnectionId", "AuthInvalidMessage", "AuthInvalidNonce",
    "AuthSelectorNotAllowed",
    // order cancellation
    "OrderCancellerDuplicateMarketId", "OrderCancellerDuplicateOrderId",
    "OrderCancellerInvalidOrder", "OrderCancellerNotRisky",
    // FIndex (funding index) oracle timing, wrong time to call, not a
    // transient failure
    "FIndexInvalidTime", "FIndexNotDueForUpdate", "FIndexUpdatedAtMaturity",
    // conditional orders subsystem
    "ConditionalActionExecuted", "ConditionalInvalidAgent",
    "ConditionalInvalidParams", "ConditionalInvalidValidator",
    "ConditionalMessageExpired", "ConditionalOrderExpired",
    "ConditionalOrderNotReduceOnly",
    // deleverager bot subsystem
    "DeleveragerAMMNotAllowed", "DeleveragerDuplicateMarketId",
    "DeleveragerHealthNonRisky", "DeleveragerIncomplete",
    "DeleveragerLoserHealthier", "DeleveragerLoserInBadDebt",
    "DeleveragerWinnerInBadDebt",
    // pauser (circuit breaker) subsystem
    "PauserNotRisky", "PauserTokenMismatch",
    // cross-chain portal subsystem
    "PortalInvalidMessenger", "PortalMessengerNotSet",
    // withdrawal rate-limiting subsystem
    "WithdrawalPoliceAlreadyRestricted", "WithdrawalPoliceInvalidCooldown",
    "WithdrawalPoliceInvalidThreshold", "WithdrawalPoliceUnsatCondition",
    // risk zone / governance config subsystem
    "ZoneGlobalCooldownAlreadyIncreased", "ZoneInvalidGlobalCooldown",
    "ZoneInvalidLiqSettings", "ZoneInvalidRateDeviationConfig",
    "ZoneMarketInvalidStatus",
    // deposit box (cross-chain deposit intents) subsystem
    "DEPOSIT_BOX_ID_IN_USE", "DEPOSIT_BOX_INTENT_EXISTS",
    "DEPOSIT_BOX_INTENT_INVALID_STATUS", "DEPOSIT_BOX_INTENT_NOT_FOUND",
    "DEPOSIT_LESS_THAN_TREASURY", "MINIMUM_DEPOSIT_NOT_MET",
    "InsufficientDepositAmount",
    // OTC trade subsystem (excluding SHEET_WRITE_FAILED/TRADE_UPDATE_FAILED,
    // see module doc for why those two stay Unknown)
    "OTC_TRADE_NOT_FOUND", "OTC_TRADE_INVALID_STATUS", "OTC_TRADE_EXPIRED",
    "OTC_TRADE_DUPLICATE", "OTC_TRADE_INVALID_REQUEST",
    "OTC_TRADE_UNAUTHORIZED", "OTC_USER_NOT_ELIGIBLE",
    // OTCModule, same fatal pattern as the AuthModule/ConditionalModule
    // codes above (invalid agent, expired message, already-executed intent)
    "OTCInvalidAgent", "OTCMessageExpired", "OTCRequestExecuted",
    "InsufficientProfit",
    // referrals / VIP / whitelist eligibility
    "REFERRAL_CODE_EXISTS", "REFERRAL_CODE_NOT_FOUND",
    "USER_ALREADY_HAS_REFERRAL_CODE", "USER_ALREADY_JOINED_REFERRAL",
    "CANNOT_JOIN_OWN_REFERRAL", "ELIGIBILITY_REQUIREMENT_NOT_MET",
    "USER_NOT_VIP", "USER_NOT_WHITELISTED",
    // fee/gas validation, "identical resend" won't clear this, the caller
    // needs to raise the fee, that's a different request
    "MAX_FEE_TOO_LOW",
    // P2P offer subsystem, added in SDK 1.6.x: not-found/expired/invalid
    // patterns match the same-shaped codes above, P2P_CONFLICT_RETRY is
    // the one code in this group that's actually retriable, see above
    "P2P_NOT_FOUND", "P2P_ROW_DELETED", "P2P_INVALID_STATE",
    "P2P_OFFER_EXPIRED", "P2P_SIZE_OUT_OF_BOUNDS",
    "P2P_INSUFFICIENT_MARGIN", "P2P_INVALID_SIGNATURE",
];

/// Classify a raw error code from the Boros API/backend. `code` should be
/// the exact string as returned (e.g. `"INSUFFICIENT_MARGIN"` or
/// `"MMHealthCritical"`), see module docs for how it's extracted from the
/// gRPC `Status` this client receives from `sidecar-ts`.
pub fn classify(code: &str) -> ErrorClass {
    if RETRIABLE_CODES.contains(&code) {
        ErrorClass::Retriable
    } else if FATAL_CODES.contains(&code) {
        ErrorClass::Fatal
    } else {
        ErrorClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infra_errors_are_retriable() {
        assert_eq!(classify("BLOCKCHAIN_RPC_ERROR"), ErrorClass::Retriable);
        assert_eq!(classify("INTERNAL_SERVER_ERROR"), ErrorClass::Retriable);
        assert_eq!(classify("HISTORICAL_PRICE_FETCH_FAILED"), ErrorClass::Retriable);
    }

    #[test]
    fn margin_rejection_is_fatal_not_retriable() {
        assert_eq!(classify("INSUFFICIENT_MARGIN"), ErrorClass::Fatal);
        assert_eq!(classify("MMHealthCritical"), ErrorClass::Fatal);
    }

    #[test]
    fn order_state_rejection_is_fatal() {
        assert_eq!(classify("ORDER_ALREADY_FILLED"), ErrorClass::Fatal);
        assert_eq!(classify("MarketOrderRateOutOfBound"), ErrorClass::Fatal);
    }

    #[test]
    fn subsystems_added_2026_07_18_are_fatal_not_unknown_anymore() {
        // these used to fall to Unknown before the full taxonomy pass,
        // spot-checking one from each newly-covered subsystem
        for code in [
            "ZoneGlobalCooldownAlreadyIncreased", "DeleveragerHealthNonRisky",
            "ConditionalOrderExpired", "PortalMessengerNotSet",
            "AMMInsufficientLiquidity", "MHMarketNotExists",
            "FIndexNotDueForUpdate", "WithdrawalPoliceUnsatCondition",
            "MAX_FEE_TOO_LOW",
        ] {
            assert_eq!(classify(code), ErrorClass::Fatal, "expected {code} to be Fatal");
        }
    }

    #[test]
    fn genuinely_ambiguous_codes_stay_unknown_not_guessed() {
        // real codes, left unclassified on purpose, see module doc for
        // why each one is too generic to call either way
        for code in [
            "DATA_INCONSISTENCY", "HTTP_EXCEPTION", "FailedCall",
            "InvalidLength", "OTC_SHEET_WRITE_FAILED", "OTC_TRADE_UPDATE_FAILED",
        ] {
            assert_eq!(classify(code), ErrorClass::Unknown, "expected {code} to stay Unknown");
        }
    }

    #[test]
    fn unrecognized_code_defaults_to_unknown_not_retriable() {
        assert_eq!(classify("some_future_code_that_does_not_exist_yet"), ErrorClass::Unknown);
    }

    #[test]
    fn retriable_and_fatal_sets_are_disjoint() {
        let overlap: Vec<_> = RETRIABLE_CODES.iter().filter(|c| FATAL_CODES.contains(c)).collect();
        assert!(overlap.is_empty(), "a code can't be both retriable and fatal: {overlap:?}");
    }
}
