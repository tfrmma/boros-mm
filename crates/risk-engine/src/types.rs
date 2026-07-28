/// Pre-trade limits for one account/market. No defaults, these are risk
/// policy the desk sets, not something this crate should guess (same
/// discipline as `quoting-engine::AvellanedaStoikovParams`).
#[derive(Debug, Clone, Copy)]
pub struct PreTradeLimits {
    /// Cap on `|net_dv01|` after the hypothetical order.
    pub max_net_dv01: f64,
    /// Cap on `sum(|dv01|)` across all positions after the hypothetical
    /// order, distinct from net: a fully-hedged book (net≈0) can still
    /// carry unbounded gross exposure to basis/curve risk between legs.
    pub max_gross_dv01: f64,
    /// Cap on total notional (sum of `|size|`) after the hypothetical order.
    pub max_notional: f64,
    /// Minimum acceptable `total_value/total_mm` after the hypothetical
    /// order, checked via `margin_sim::MarginEngine`, the same shadow
    /// margin math used everywhere else in this workspace.
    pub min_projected_health_ratio: f64,
    /// Max orders placeable within `throttle_window_secs`.
    pub max_orders_per_window: u32,
    pub throttle_window_secs: u32,
}

/// A runtime divergence alert: the shadow (this workspace's own
/// calculation) has drifted from the real on-chain/API value by more than
/// the configured tolerance. Doesn't say what to do about it; that's the
/// caller's policy (e.g. `services/risk-monitor`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RiskAlert {
    HealthRatioDivergence { shadow: f64, real: f64, relative_diff: f64 },
    MarkRateDivergence { shadow: f64, real: f64, abs_diff: f64 },
}

/// Tolerance configuration for `monitor::check_health_ratio_divergence` /
/// `check_mark_rate_divergence`. No defaults, same reasoning as
/// `PreTradeLimits`.
#[derive(Debug, Clone, Copy)]
pub struct DivergenceConfig {
    /// e.g. `0.05` for a 5% relative divergence trigger.
    pub max_health_ratio_relative_diff: f64,
    /// Absolute rate difference (FixedX18-scale-equivalent f64, e.g.
    /// `0.001` for 10bps).
    pub max_mark_rate_abs_diff: f64,
}
