use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiskError {
    #[error("margin calculation failed: {0}")]
    Margin(#[from] margin_sim::MarginError),
}

/// A single pre-trade limit breach. `check_pre_trade` can return more than
/// one: a hypothetical order can violate several limits simultaneously,
/// and the caller should see all of them, not just the first one found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RiskViolation {
    NetDv01Exceeded { would_be: f64, cap: f64 },
    GrossDv01Exceeded { would_be: f64, cap: f64 },
    NotionalExceeded { would_be: f64, cap: f64 },
    ProjectedHealthRatioTooLow { projected: f64, floor: f64 },
    OrderRateThrottled { count_in_window: u32, cap: u32 },
}
