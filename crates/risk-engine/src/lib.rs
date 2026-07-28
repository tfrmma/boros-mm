//! Pre-trade limits, runtime shadow-vs-real divergence detection, and an
//! independent kill-switch authority. Built on `margin-sim`'s
//! source-verified `MarginEngine`, doesn't re-derive margin math.
//!
//! Risk monitoring runs with no shared failure domain with the
//! quoting/execution process. This crate provides the logic; process
//! separation itself is enforced by `services/risk-monitor`.

pub mod dv01;
pub mod error;
pub mod kill_switch;
pub mod monitor;
pub mod pre_trade;
pub mod types;

pub use dv01::position_dv01;
pub use error::{RiskError, RiskViolation};
pub use kill_switch::{KillSwitch, KillSwitchState};
pub use monitor::{check_health_ratio_divergence, check_mark_rate_divergence};
pub use pre_trade::check_pre_trade;
pub use types::{DivergenceConfig, PreTradeLimits, RiskAlert};
