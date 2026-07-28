use std::collections::HashMap;

use tick_math::FixedX18;

use crate::{
    error::LedgerError,
    types::{Fill, FIndexRecord, PayFee, SettlementResult, SubPeriod},
};

/// Per-market fill + FIndex history plus the running settlement checkpoint.
struct MarketLedger {
    /// Sorted ascending by `f_tag`. Only fills strictly after
    /// `checkpoint_f_tag` are pending: a fill landing exactly on the
    /// checkpoint's own tag is folded into `size_at_checkpoint` immediately
    /// by `record_fill` and never appears here (see its doc comment).
    fills: Vec<Fill>,
    /// Exact `FIndex` records, keyed by `f_tag`. No interpolation path
    /// exists anywhere in this crate: every sub-period boundary needs one
    /// of these present, or `settle_to` fails loudly.
    findex: HashMap<u32, FIndexRecord>,
    /// Position size as of `checkpoint_f_tag`, the size carried into the
    /// still-open period.
    size_at_checkpoint: FixedX18,
    checkpoint_f_tag: u32,
}

impl MarketLedger {
    fn new(initial_size: FixedX18, checkpoint_f_tag: u32) -> Self {
        Self {
            fills: Vec::new(),
            findex: HashMap::new(),
            size_at_checkpoint: initial_size,
            checkpoint_f_tag,
        }
    }
}

/// Off-chain replica of Boros lazy settlement, verified against
/// `pendle-finance/boros-core-public` (`contracts/lib/PaymentLib.sol`,
/// `contracts/core/market/settle/ProcessMergeUtils.sol`,
/// `contracts/core/market/core/MarketInfoAndState.sol`).
///
/// The contract settles lazily: each position stores the last `FIndex` it
/// was synced against. Walking a user's swept (filled) orders in `FTag`
/// order, for each group of fills sharing an `FTag`:
///   1. price the elapsed window using the size held *before* this group's
///      fills, against `(storedIndex, indexAtThisFTag)`
///   2. merge this group's fills into the position size
///   3. advance the stored index to `indexAtThisFTag`
/// `settle_to` mirrors that loop exactly, generalized so the final boundary
/// can be any target `f_tag` (not only one that has a fill on it), the
/// same shape as syncing a position with no new fills, just an elapsed
/// `FIndex` window.
///
/// Out of scope: the upfront fixed-leg cost
/// (`PaymentLib.calcUpfrontFixedCost`), paid at fill time out of the
/// trade's own `signedCost`, entirely separate from this floating/fee
/// settlement. That belongs with fill/order processing (`oms-core`), not
/// here.
#[derive(Default)]
pub struct SettlementLedger {
    markets: HashMap<u32, MarketLedger>,
}

impl SettlementLedger {
    pub fn new() -> Self {
        Self { markets: HashMap::new() }
    }

    /// Register a market with the position size it holds as of
    /// `as_of_f_tag` (usually the market's current `FTag` at the time you
    /// start tracking it). Must be called before `record_fill` /
    /// `record_findex` for that market.
    pub fn init_market(&mut self, market_id: u32, initial_size: FixedX18, as_of_f_tag: u32) {
        self.markets.insert(market_id, MarketLedger::new(initial_size, as_of_f_tag));
    }

    /// Record a fill, tagged with the market's `FTag` at the moment it was
    /// processed (`SweptF.fTag` on-chain, see `types::Fill`'s doc comment
    /// for why this must not be a timestamp).
    ///
    /// A fill landing exactly on the current checkpoint's own `f_tag` is
    /// folded into the position size immediately, with zero settlement
    /// math: this mirrors `PaymentLib.calcSettlement`'s `last == current`
    /// fast path: two `FIndex` reads for the same `f_tag` are identical, so
    /// there's no elapsed window to price. Anything later becomes a
    /// pending sub-period boundary for the next `settle_to`.
    pub fn record_fill(&mut self, market_id: u32, fill: Fill) -> Result<(), LedgerError> {
        let ledger = self.market_mut(market_id)?;

        if fill.f_tag < ledger.checkpoint_f_tag {
            return Err(LedgerError::FillBeforeCheckpoint(fill.f_tag));
        }
        if fill.f_tag == ledger.checkpoint_f_tag {
            ledger.size_at_checkpoint += fill.size_delta;
            return Ok(());
        }

        let pos = ledger.fills.partition_point(|f| f.f_tag <= fill.f_tag);
        ledger.fills.insert(pos, fill);
        Ok(())
    }

    /// Record an exact `FIndex` published at `f_tag`
    /// (`fTagToIndex[fTag]` on-chain, sourced from the market's
    /// `FIndexUpdated` event or the equivalent REST API field). Re-recording
    /// the same value at a known `f_tag` is a no-op; recording a
    /// *different* value at an already-known `f_tag` is a hard error:
    /// that would mean two data sources disagree about immutable history.
    pub fn record_findex(&mut self, market_id: u32, record: FIndexRecord) -> Result<(), LedgerError> {
        let ledger = self.market_mut(market_id)?;

        if record.f_tag < ledger.checkpoint_f_tag {
            return Err(LedgerError::FIndexRecordBeforeCheckpoint(record.f_tag));
        }
        if let Some(existing) = ledger.findex.get(&record.f_tag) {
            if *existing != record {
                return Err(LedgerError::ConflictingFIndexRecord(record.f_tag));
            }
            return Ok(());
        }
        ledger.findex.insert(record.f_tag, record);
        Ok(())
    }

    /// Compute the floating-payment and fee for `(checkpoint_f_tag, upto_f_tag]`
    /// and advance the checkpoint to `upto_f_tag`.
    ///
    /// Requires an exact `FIndexRecord` at every sub-period boundary
    /// (the checkpoint, every pending fill's `f_tag` strictly in between,
    /// and `upto_f_tag` itself), `MissingFIndexRecord` otherwise. There is
    /// no fallback approximation; that's the entire point of keying by
    /// `FTag` instead of timestamp.
    pub fn settle_to(&mut self, market_id: u32, upto_f_tag: u32) -> Result<SettlementResult, LedgerError> {
        let ledger = self.market_mut(market_id)?;

        if upto_f_tag < ledger.checkpoint_f_tag {
            return Err(LedgerError::NonMonotonicSettlement {
                checkpoint: ledger.checkpoint_f_tag,
                upto: upto_f_tag,
            });
        }

        let start_f_tag = ledger.checkpoint_f_tag;

        if upto_f_tag == start_f_tag {
            return Ok(SettlementResult {
                market_id,
                start_f_tag,
                end_f_tag: upto_f_tag,
                total: PayFee::ZERO,
                sub_periods: vec![],
            });
        }

        // sub-period boundaries: checkpoint, every distinct pending fill
        // f_tag strictly inside the window, then the target tag
        let mut boundaries: Vec<u32> = vec![start_f_tag];
        boundaries.extend(
            ledger.fills.iter()
                .map(|f| f.f_tag)
                .filter(|&t| t > start_f_tag && t < upto_f_tag),
        );
        boundaries.push(upto_f_tag);
        boundaries.dedup();

        // pull pending fills into an owned Vec up front, decouples the
        // fill-consuming iterator below from `ledger.fills`'s borrow, since
        // we need to mutate `ledger.fills` (via `retain`) after this loop
        let pending: Vec<Fill> = ledger.fills.iter()
            .filter(|f| f.f_tag > start_f_tag && f.f_tag <= upto_f_tag)
            .copied()
            .collect();
        let mut fill_iter = pending.into_iter().peekable();

        let mut sub_periods = Vec::with_capacity(boundaries.len().saturating_sub(1));
        let mut total = PayFee::ZERO;
        let mut running_size = ledger.size_at_checkpoint;

        for pair in boundaries.windows(2) {
            let (start, end) = (pair[0], pair[1]);

            // apply any fill landing exactly at `start` before pricing this
            // window, the checkpoint boundary's own fills were already
            // folded in by record_fill's fast path, never queued here
            while let Some(f) = fill_iter.peek() {
                if f.f_tag == start && start != start_f_tag {
                    running_size += fill_iter.next().unwrap().size_delta;
                } else {
                    break;
                }
            }

            let last = ledger.findex.get(&start).copied().ok_or(LedgerError::MissingFIndexRecord(start))?;
            let current = ledger.findex.get(&end).copied().ok_or(LedgerError::MissingFIndexRecord(end))?;
            let result = calc_settlement(running_size, &last, &current)?;

            total = total.add(result);
            sub_periods.push(SubPeriod { start_f_tag: start, end_f_tag: end, size_held: running_size, result });
        }

        // fold in any fill landing exactly at upto_f_tag, it didn't affect
        // the payment just computed, but does affect the size carried forward
        for f in fill_iter {
            running_size += f.size_delta;
        }

        ledger.fills.retain(|f| f.f_tag > upto_f_tag);
        ledger.findex.retain(|&tag, _| tag >= upto_f_tag);
        ledger.size_at_checkpoint = running_size;
        ledger.checkpoint_f_tag = upto_f_tag;

        Ok(SettlementResult { market_id, start_f_tag, end_f_tag: upto_f_tag, total, sub_periods })
    }

    pub fn position_size(&self, market_id: u32) -> Option<FixedX18> {
        self.markets.get(&market_id).map(|l| l.size_at_checkpoint)
    }

    pub fn checkpoint(&self, market_id: u32) -> Option<u32> {
        self.markets.get(&market_id).map(|l| l.checkpoint_f_tag)
    }

    fn market_mut(&mut self, market_id: u32) -> Result<&mut MarketLedger, LedgerError> {
        self.markets.get_mut(&market_id).ok_or(LedgerError::UnknownMarket(market_id))
    }
}

/// Direct port of `PaymentLib.calcSettlement`
/// (`contracts/lib/PaymentLib.sol:15-22`):
/// ```solidity
/// function calcSettlement(int256 signedSize, FIndex last, FIndex current) internal pure returns (PayFee res) {
///     if (last == current) return PLib.ZERO;
///     res = PLib.from(
///         signedSize.mulFloor(current.floatingIndex() - last.floatingIndex()),
///         signedSize.abs().mulUp(current.feeIndex() - last.feeIndex())
///     );
/// }
/// ```
fn calc_settlement(signed_size: FixedX18, last: &FIndexRecord, current: &FIndexRecord) -> Result<PayFee, LedgerError> {
    if last.floating_index == current.floating_index && last.fee_index == current.fee_index {
        return Ok(PayFee::ZERO);
    }

    let delta_floating = current.floating_index - last.floating_index;
    let payment = signed_size.mul_floor(delta_floating)?;

    let delta_fee = current.fee_index - last.fee_index;
    if delta_fee.is_negative() {
        return Err(LedgerError::FeeIndexDecreased { last_f_tag: last.f_tag, current_f_tag: current.f_tag });
    }

    let abs_size_u = signed_size.inner().unsigned_abs();
    let delta_fee_u = delta_fee.inner() as u128; // safe: checked non-negative above
    let fee_raw = tick_math::mul_div_up(abs_size_u, delta_fee_u, FixedX18::SCALE as u128)?;
    let fee = FixedX18::raw(i128::try_from(fee_raw).map_err(|_| tick_math::MathError::Overflow)?);

    Ok(PayFee { payment, fee })
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: f64) -> FixedX18 { FixedX18::from_f64(v) }

    fn idx(f_tag: u32, floating: f64, fee: f64) -> FIndexRecord {
        FIndexRecord { f_tag, f_time: f_tag as u64 * 3600, floating_index: f(floating), fee_index: f(fee) }
    }

    #[test]
    fn no_fills_flat_period_payment_and_fee() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.000)).unwrap();
        l.record_findex(1, idx(3, 1.001, 0.0001)).unwrap();

        let res = l.settle_to(1, 3).unwrap();
        // payment: 1000 * (1.001 - 1.000) = 1.0
        assert!((res.total.payment.to_f64() - 1.0).abs() < 1e-9, "payment={}", res.total.payment.to_f64());
        // fee: |1000| * 0.0001 = 0.1
        assert!((res.total.fee.to_f64() - 0.1).abs() < 1e-9, "fee={}", res.total.fee.to_f64());
        assert_eq!(res.sub_periods.len(), 1);
        assert_eq!(l.checkpoint(1), Some(3));
        assert_eq!(l.position_size(1), Some(f(1000.0)));
    }

    #[test]
    fn fill_at_checkpoint_tag_applies_immediately_with_no_settlement() {
        // a fill sharing the checkpoint's own f_tag mirrors calcSettlement's
        // last==current fast path: no sub-period, no payment, just merged size
        let mut l = SettlementLedger::new();
        l.init_market(1, f(500.0), 5);
        l.record_fill(1, Fill { f_tag: 5, size_delta: f(200.0) }).unwrap();
        assert_eq!(l.position_size(1), Some(f(700.0)));
        assert_eq!(l.checkpoint(1), Some(5)); // unchanged, no settlement happened
    }

    #[test]
    fn partial_close_mid_window_splits_into_subperiods() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap();
        l.record_findex(1, idx(2, 1.0015, 0.0)).unwrap(); // fill boundary
        l.record_findex(1, idx(3, 1.002, 0.0)).unwrap();
        l.record_fill(1, Fill { f_tag: 2, size_delta: f(-500.0) }).unwrap();

        let res = l.settle_to(1, 3).unwrap();
        assert_eq!(res.sub_periods.len(), 2);

        let sp0 = res.sub_periods[0];
        assert_eq!(sp0.size_held, f(1000.0));
        assert!((sp0.result.payment.to_f64() - 1.5).abs() < 1e-6, "sp0 payment {}", sp0.result.payment.to_f64());

        let sp1 = res.sub_periods[1];
        assert_eq!(sp1.size_held, f(500.0));
        assert!((sp1.result.payment.to_f64() - 0.25).abs() < 1e-6, "sp1 payment {}", sp1.result.payment.to_f64());

        assert!((res.total.payment.to_f64() - 1.75).abs() < 1e-6, "total {}", res.total.payment.to_f64());
        assert_eq!(l.position_size(1), Some(f(500.0)));
    }

    #[test]
    fn position_flip_prices_each_sign_correctly() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(100.0), 1);
        l.record_findex(1, idx(1, 1.00, 0.0)).unwrap();
        l.record_findex(1, idx(2, 1.05, 0.0)).unwrap();
        l.record_findex(1, idx(3, 1.10, 0.0)).unwrap();
        l.record_fill(1, Fill { f_tag: 2, size_delta: f(-150.0) }).unwrap();

        let res = l.settle_to(1, 3).unwrap();
        // long 100 for the first half: 100*0.05=5, short 50 for the second: -50*0.05=-2.5
        assert!((res.total.payment.to_f64() - 2.5).abs() < 1e-6, "got {}", res.total.payment.to_f64());
        assert_eq!(l.position_size(1), Some(f(-50.0)));
    }

    #[test]
    fn last_equals_current_findex_is_zero_payment_fast_path() {
        // mirrors PaymentLib.calcSettlement's `if (last == current) return PLib.ZERO;`
        // exactly, a purge event that doesn't move the index still inserts
        // a checkpoint, and settling across it must be a true no-op
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0005)).unwrap();
        l.record_findex(1, idx(2, 1.000, 0.0005)).unwrap(); // purge: same index

        let res = l.settle_to(1, 2).unwrap();
        assert_eq!(res.total, PayFee::ZERO);
    }

    #[test]
    fn sequential_settlements_reuse_persisted_findex() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap();
        l.record_findex(1, idx(2, 1.001, 0.0)).unwrap();
        let first = l.settle_to(1, 2).unwrap();
        assert!((first.total.payment.to_f64() - 1.0).abs() < 1e-9);

        // no need to re-supply f_tag=2's record, it's still there from before
        l.record_findex(1, idx(3, 1.0025, 0.0)).unwrap();
        let second = l.settle_to(1, 3).unwrap();
        assert!((second.total.payment.to_f64() - 1.5).abs() < 1e-9, "got {}", second.total.payment.to_f64());
    }

    #[test]
    fn missing_findex_record_errors_instead_of_interpolating() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap();
        // no record at f_tag=2 at all
        let err = l.settle_to(1, 2).unwrap_err();
        assert_eq!(err, LedgerError::MissingFIndexRecord(2));
    }

    #[test]
    fn conflicting_findex_record_rejected() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap();
        let err = l.record_findex(1, idx(1, 1.001, 0.0)).unwrap_err(); // same f_tag, different value
        assert_eq!(err, LedgerError::ConflictingFIndexRecord(1));
    }

    #[test]
    fn identical_findex_record_resubmission_is_idempotent() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap();
        l.record_findex(1, idx(1, 1.000, 0.0)).unwrap(); // same value again, fine
    }

    #[test]
    fn non_monotonic_settlement_rejected() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 10);
        let err = l.settle_to(1, 5).unwrap_err();
        assert_eq!(err, LedgerError::NonMonotonicSettlement { checkpoint: 10, upto: 5 });
    }

    #[test]
    fn fill_before_checkpoint_rejected() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 10);
        let err = l.record_fill(1, Fill { f_tag: 5, size_delta: f(1.0) }).unwrap_err();
        assert_eq!(err, LedgerError::FillBeforeCheckpoint(5));
    }

    #[test]
    fn unknown_market_rejected() {
        let mut l = SettlementLedger::new();
        let err = l.record_fill(99, Fill { f_tag: 0, size_delta: f(1.0) }).unwrap_err();
        assert_eq!(err, LedgerError::UnknownMarket(99));
    }

    #[test]
    fn zero_length_settlement_is_noop() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 5);
        let res = l.settle_to(1, 5).unwrap();
        assert_eq!(res.total, PayFee::ZERO);
        assert!(res.sub_periods.is_empty());
    }

    #[test]
    fn fee_index_decrease_is_rejected_not_silently_absorbed() {
        let mut l = SettlementLedger::new();
        l.init_market(1, f(1000.0), 1);
        l.record_findex(1, idx(1, 1.000, 0.001)).unwrap();
        l.record_findex(1, idx(2, 1.001, 0.0005)).unwrap(); // fee_index went DOWN, invalid
        let err = l.settle_to(1, 2).unwrap_err();
        assert_eq!(err, LedgerError::FeeIndexDecreased { last_f_tag: 1, current_f_tag: 2 });
    }

    #[test]
    fn floor_rounding_matches_negative_edge_case() {
        // regression guard: raw(-1) size against a raw(1) index delta must
        // floor to -1, not truncate to 0, same case verified in tick-math,
        // exercised here through the full settlement path
        let mut l = SettlementLedger::new();
        l.init_market(1, FixedX18::raw(-1), 1);
        l.record_findex(1, FIndexRecord { f_tag: 1, f_time: 0, floating_index: FixedX18::ZERO, fee_index: FixedX18::ZERO }).unwrap();
        l.record_findex(1, FIndexRecord { f_tag: 2, f_time: 3600, floating_index: FixedX18::raw(1), fee_index: FixedX18::ZERO }).unwrap();

        let res = l.settle_to(1, 2).unwrap();
        assert_eq!(res.total.payment, FixedX18::raw(-1));
    }
}
