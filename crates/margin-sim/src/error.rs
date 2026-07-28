use thiserror::Error;
use tick_math::MathError;

use crate::types::{MarketId, TokenId};

#[derive(Debug, Error)]
pub enum MarginError {
    #[error("unknown market {0}, no MarginConfig registered")]
    UnknownMarket(u32),

    /// `Err.MMTokenMismatch()`, `MarginManager.sol:30`. A cross account is
    /// scoped to one token (see `MarginConfig::token_id` doc comment), so
    /// touching a market whose collateral token doesn't match is rejected
    /// on-chain before it ever gets this far in real usage. Catching it
    /// here too means a caller building a `MarginAccount` by hand (tests,
    /// simulation, whatever) finds out immediately instead of getting a
    /// nonsensical netted number.
    #[error("market {market_id:?} settles in token {market_token:?}, account is cross-margined in token {account_token:?}")]
    TokenMismatch { market_id: MarketId, market_token: TokenId, account_token: TokenId },

    #[error(transparent)]
    Math(#[from] MathError),
}
