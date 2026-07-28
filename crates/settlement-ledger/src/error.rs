use thiserror::Error;
use tick_math::MathError;

#[derive(Debug, Error, PartialEq)]
pub enum LedgerError {
    #[error("market {0} not initialized, call init_market first")]
    UnknownMarket(u32),

    #[error("no FIndexRecord for f_tag={0}, settlement never interpolates; the exact record must be supplied (e.g. from the market's FIndexUpdated event log)")]
    MissingFIndexRecord(u32),

    #[error("FIndexRecord already exists for f_tag={0} with a different value, refusing to silently overwrite settled history")]
    ConflictingFIndexRecord(u32),

    #[error("settle_to(f_tag={upto}) is before the current checkpoint (f_tag={checkpoint}), settlement must be monotonic")]
    NonMonotonicSettlement { checkpoint: u32, upto: u32 },

    #[error("fill at f_tag={0} is before the current checkpoint, would rewrite already-settled history")]
    FillBeforeCheckpoint(u32),

    #[error("FIndexRecord at f_tag={0} is before the current checkpoint, would rewrite already-settled history")]
    FIndexRecordBeforeCheckpoint(u32),

    #[error("fee_index decreased between f_tag={last_f_tag} and f_tag={current_f_tag}, violates the protocol invariant that feeIndex is monotonically non-decreasing")]
    FeeIndexDecreased { last_f_tag: u32, current_f_tag: u32 },

    #[error(transparent)]
    Math(#[from] MathError),
}
