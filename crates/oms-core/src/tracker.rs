//! Local order state, reconstructed from the 5 real lifecycle events
//! (`IMarket.sol:224-232`, `pendle-finance/boros-core-public`):
//! ```solidity
//! event LimitOrderPlaced(MarketAcc maker, OrderId[] orderIds, uint256[] sizes);
//! event LimitOrderCancelled(OrderId[] orderIds);
//! event LimitOrderForcedCancelled(OrderId[] orderIds);
//! event LimitOrderPartiallyFilled(OrderId orderId, uint256 filledSize);
//! event LimitOrderFilled(OrderId from, OrderId to);
//! ```
//! `LimitOrderFilled` is a **range** of order ids (contiguous `order_index`
//! at the same side+tick, all fully filled in one batch, a gas-efficient
//! encoding, not a list) that sweeps the *entire market*, not just this
//! account's orders. This tracker only records orders it was told about via
//! `on_placed`; any id in a swept range it doesn't recognize belongs to a
//! different account and is silently skipped, that's expected, not an
//! error.

use std::collections::HashMap;

use tick_math::FixedX18;

use crate::{
    error::OmsError,
    order_id::{OrderId, Side},
    types::{LocalOrderStatus, Trade},
};

#[derive(Debug, Clone)]
pub struct LocalOrder {
    pub id: OrderId,
    pub side: Side,
    pub tick_index: i16,
    pub original_size: FixedX18,
    pub filled_size: FixedX18,
    pub status: LocalOrderStatus,
}

impl LocalOrder {
    fn new(id: OrderId, side: Side, tick_index: i16, size: FixedX18) -> Self {
        Self { id, side, tick_index, original_size: size, filled_size: FixedX18::ZERO, status: LocalOrderStatus::Open }
    }

    pub fn remaining_size(&self) -> FixedX18 {
        self.original_size - self.filled_size
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status, LocalOrderStatus::Filled | LocalOrderStatus::Cancelled | LocalOrderStatus::ForcedCancelled)
    }
}

/// A resting order fills at its own tick's rate, `tick_to_rate` needs
/// `tick_step` which the events themselves don't carry, only the market
/// config does, so it's fixed for the tracker's lifetime instead of
/// threaded through every call. One tracker per market, same assumption
/// `OrderId` itself already makes (it doesn't encode market_id either).
fn build_trade(tick_step: u8, side: Side, tick_index: i16, filled_delta: FixedX18) -> Result<Trade, OmsError> {
    let rate = tick_math::tick_to_rate(tick_index, tick_step)?;
    let signed_size = match side {
        Side::Long => filled_delta,
        Side::Short => -filled_delta,
    };
    Trade::from_size_and_rate(signed_size, rate)
}

/// Tracks this account's own resting orders against the market-wide event
/// stream. Does not talk to a WS/API itself, the caller feeds it decoded
/// events (from `feed-ingest`, or directly from on-chain logs), this crate
/// owns the state machine, not the transport.
pub struct OrderTracker {
    orders: HashMap<OrderId, LocalOrder>,
    tick_step: u8,
}

impl OrderTracker {
    pub fn new(tick_step: u8) -> Self {
        Self { orders: HashMap::new(), tick_step }
    }

    /// `LimitOrderPlaced(maker, orderIds, sizes)`, only call this for
    /// orders this account placed (the event itself carries `maker`; the
    /// caller is responsible for filtering to "is this us" before calling).
    pub fn on_placed(&mut self, ids: &[OrderId], sizes: &[FixedX18]) -> Result<(), OmsError> {
        if ids.len() != sizes.len() {
            return Err(OmsError::MismatchedPlacedLengths { ids: ids.len(), sizes: sizes.len() });
        }
        for (&id, &size) in ids.iter().zip(sizes) {
            let (side, tick, _) = id.unpack();
            self.orders.insert(id, LocalOrder::new(id, side, tick, size));
        }
        Ok(())
    }

    /// `LimitOrderFilled(from, to)`, a contiguous range of order indices at
    /// the same side+tick, all fully filled. Expands the range and updates
    /// any order in it that this tracker recognizes as its own, returning
    /// one `Trade` per one of our own orders that got hit (a range can
    /// sweep several of our resting orders at once, at different order
    /// indices but the same tick, each is its own economic fill).
    pub fn on_filled_range(&mut self, from: OrderId, to: OrderId) -> Result<Vec<Trade>, OmsError> {
        let (side_from, tick_from, idx_from) = from.unpack();
        let (side_to, tick_to, idx_to) = to.unpack();

        if side_from != side_to || tick_from != tick_to {
            return Err(OmsError::InvalidFillRange { from, to });
        }
        if idx_to < idx_from {
            return Err(OmsError::InvertedFillRange(idx_to, idx_from));
        }

        let mut trades = Vec::new();
        for idx in idx_from..=idx_to {
            let id = OrderId::from_parts(side_from, tick_from, idx)?;
            if let Some(order) = self.orders.get_mut(&id) {
                let remaining = order.remaining_size();
                order.filled_size = order.original_size;
                order.status = LocalOrderStatus::Filled;
                if !remaining.is_zero() {
                    trades.push(build_trade(self.tick_step, order.side, order.tick_index, remaining)?);
                }
            }
        }
        Ok(trades)
    }

    /// `LimitOrderPartiallyFilled(orderId, filledSize)`, an incremental
    /// fill amount, not a running total, accumulates onto whatever this
    /// order had already filled. Returns the `Trade` for this specific
    /// fill, or `None` if the order isn't ours (same silent-skip behavior
    /// as before, just now also handing back the economic record when it
    /// is ours).
    pub fn on_partially_filled(&mut self, id: OrderId, filled_size_delta: FixedX18) -> Result<Option<Trade>, OmsError> {
        let Some(order) = self.orders.get_mut(&id) else {
            return Ok(None);
        };
        order.filled_size += filled_size_delta;
        order.status = if order.remaining_size().is_zero() {
            LocalOrderStatus::Filled
        } else {
            LocalOrderStatus::PartiallyFilled
        };
        let trade = build_trade(self.tick_step, order.side, order.tick_index, filled_size_delta)?;
        Ok(Some(trade))
    }

    /// `LimitOrderCancelled(orderIds)`, user-initiated removal.
    pub fn on_cancelled(&mut self, ids: &[OrderId]) {
        for id in ids {
            if let Some(order) = self.orders.get_mut(id) {
                order.status = LocalOrderStatus::Cancelled;
            }
        }
    }

    /// `LimitOrderForcedCancelled(orderIds)`, the protocol's out-of-band
    /// purge bot (`_bookPurgeOob`), not user-initiated.
    pub fn on_forced_cancelled(&mut self, ids: &[OrderId]) {
        for id in ids {
            if let Some(order) = self.orders.get_mut(id) {
                order.status = LocalOrderStatus::ForcedCancelled;
            }
        }
    }

    pub fn get(&self, id: OrderId) -> Option<&LocalOrder> {
        self.orders.get(&id)
    }

    pub fn open_orders(&self) -> impl Iterator<Item = &LocalOrder> {
        self.orders.values().filter(|o| !o.is_terminal())
    }

    /// Drop terminal orders older than the caller's own retention policy.
    /// This tracker keeps terminal orders around until the caller calls
    /// `forget()` on them (useful for a local audit trail), it never prunes
    /// on its own, since "how long to keep history" isn't this crate's call.
    pub fn forget(&mut self, id: OrderId) {
        self.orders.remove(&id);
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn f(v: f64) -> FixedX18 { FixedX18::from_f64(v) }

    #[test]
    fn placed_then_fully_filled_via_range() {
        let mut t = OrderTracker::new(1);
        let ids: Vec<OrderId> = (0..3).map(|i| OrderId::from_parts(Side::Long, 100, i).unwrap()).collect();
        let sizes = vec![f(10.0), f(20.0), f(30.0)];
        t.on_placed(&ids, &sizes).unwrap();

        let trades = t.on_filled_range(ids[0], ids[2]).unwrap();

        for (id, size) in ids.iter().zip(&sizes) {
            let o = t.get(*id).unwrap();
            assert_eq!(o.status, LocalOrderStatus::Filled);
            assert_eq!(o.filled_size, *size);
            assert_eq!(o.remaining_size(), FixedX18::ZERO);
        }
        // one Trade per order in the range, all Long so all positive size
        assert_eq!(trades.len(), 3);
        for trade in &trades {
            assert!(trade.signed_size.is_positive());
        }
    }

    #[test]
    fn filled_range_ignores_orders_we_dont_own() {
        // range covers order_index 0..=2 but we only placed index 1,
        // 0 and 2 belong to someone else and must be silently skipped
        let mut t = OrderTracker::new(1);
        let ours = OrderId::from_parts(Side::Long, 100, 1).unwrap();
        t.on_placed(&[ours], &[f(20.0)]).unwrap();

        let from = OrderId::from_parts(Side::Long, 100, 0).unwrap();
        let to = OrderId::from_parts(Side::Long, 100, 2).unwrap();
        let trades = t.on_filled_range(from, to).unwrap();

        assert_eq!(t.get(ours).unwrap().status, LocalOrderStatus::Filled);
        // only our one order produces a Trade, not the two we don't recognize
        assert_eq!(trades.len(), 1);
    }

    #[test]
    fn filled_range_rejects_mismatched_side_or_tick() {
        let mut t = OrderTracker::new(1);
        let from = OrderId::from_parts(Side::Long, 100, 0).unwrap();
        let to = OrderId::from_parts(Side::Short, 100, 5).unwrap();
        let err = t.on_filled_range(from, to).unwrap_err();
        assert_eq!(err, OmsError::InvalidFillRange { from, to });
    }

    #[test]
    fn partial_fill_accumulates_and_transitions_to_filled_when_complete() {
        let mut t = OrderTracker::new(1);
        let id = OrderId::from_parts(Side::Short, -50, 0).unwrap();
        t.on_placed(&[id], &[f(100.0)]).unwrap();

        let trade1 = t.on_partially_filled(id, f(30.0)).unwrap().expect("we own this order");
        let o = t.get(id).unwrap();
        assert_eq!(o.status, LocalOrderStatus::PartiallyFilled);
        assert_eq!(o.filled_size, f(30.0));
        assert_eq!(o.remaining_size(), f(70.0));
        // Short, so the Trade's signed_size should be negative
        assert!(trade1.signed_size.is_negative());

        t.on_partially_filled(id, f(70.0)).unwrap();
        let o = t.get(id).unwrap();
        assert_eq!(o.status, LocalOrderStatus::Filled);
        assert_eq!(o.remaining_size(), FixedX18::ZERO);
    }

    #[test]
    fn partial_fill_on_unknown_order_returns_none_not_an_error() {
        let mut t = OrderTracker::new(1);
        let id = OrderId::from_parts(Side::Long, 0, 0).unwrap();
        assert_eq!(t.on_partially_filled(id, f(5.0)).unwrap(), None);
    }

    #[test]
    fn trade_rate_sign_follows_tick_sign_not_side() {
        // tick and side are independent, a Short resting order can sit at a
        // positive tick just as easily as a negative one, the Trade's rate
        // sign comes from the tick alone, the size sign comes from side
        let mut t = OrderTracker::new(1);
        let id = OrderId::from_parts(Side::Short, 50, 0).unwrap(); // positive tick
        t.on_placed(&[id], &[f(10.0)]).unwrap();

        let trade = t.on_partially_filled(id, f(10.0)).unwrap().unwrap();
        assert!(trade.signed_size.is_negative(), "Short must give negative signed_size");
        assert!(trade.signed_cost.is_negative(), "positive rate * negative size = negative cost");
    }

    #[test]
    fn cancel_and_forced_cancel_are_distinct_terminal_states() {
        let mut t = OrderTracker::new(1);
        let a = OrderId::from_parts(Side::Long, 0, 0).unwrap();
        let b = OrderId::from_parts(Side::Long, 0, 1).unwrap();
        t.on_placed(&[a, b], &[f(1.0), f(1.0)]).unwrap();

        t.on_cancelled(&[a]);
        t.on_forced_cancelled(&[b]);

        assert_eq!(t.get(a).unwrap().status, LocalOrderStatus::Cancelled);
        assert_eq!(t.get(b).unwrap().status, LocalOrderStatus::ForcedCancelled);
    }

    #[test]
    fn open_orders_excludes_terminal() {
        let mut t = OrderTracker::new(1);
        let a = OrderId::from_parts(Side::Long, 0, 0).unwrap();
        let b = OrderId::from_parts(Side::Long, 0, 1).unwrap();
        t.on_placed(&[a, b], &[f(1.0), f(1.0)]).unwrap();
        t.on_cancelled(&[a]);

        let open: Vec<_> = t.open_orders().map(|o| o.id).collect();
        assert_eq!(open, vec![b]);
    }

    #[test]
    fn mismatched_placed_lengths_rejected() {
        let mut t = OrderTracker::new(1);
        let ids = vec![OrderId::from_parts(Side::Long, 0, 0).unwrap()];
        let err = t.on_placed(&ids, &[]).unwrap_err();
        assert_eq!(err, OmsError::MismatchedPlacedLengths { ids: 1, sizes: 0 });
    }

    #[test]
    fn forget_removes_from_tracker() {
        let mut t = OrderTracker::new(1);
        let id = OrderId::from_parts(Side::Long, 0, 0).unwrap();
        t.on_placed(&[id], &[f(1.0)]).unwrap();
        t.forget(id);
        assert!(t.get(id).is_none());
    }
}
