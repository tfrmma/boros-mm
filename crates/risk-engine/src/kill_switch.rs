//! Independent kill-switch authority. Risk monitoring runs in a separate
//! process from quoting and execution, with no shared failure domain. This
//! struct only tracks trip/reset state; process isolation is enforced by
//! `services/risk-monitor`, which owns an instance of this and can halt
//! `execution-adapter` independently.

/// Once tripped, stays tripped until reset by calling `reset()`, which
/// takes a reason so there's an audit trail of who decided it was safe to
/// resume.
#[derive(Debug, Clone, PartialEq)]
pub enum KillSwitchState {
    Armed,
    Tripped { reason: String },
}

#[derive(Debug, Clone)]
pub struct KillSwitch {
    state: KillSwitchState,
}

impl Default for KillSwitch {
    fn default() -> Self {
        Self { state: KillSwitchState::Armed }
    }
}

impl KillSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trip(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        tracing::error!(reason = %reason, "kill switch tripped");
        self.state = KillSwitchState::Tripped { reason };
    }

    pub fn is_tripped(&self) -> bool {
        matches!(self.state, KillSwitchState::Tripped { .. })
    }

    pub fn state(&self) -> &KillSwitchState {
        &self.state
    }

    /// Explicit, reasoned reset. Tripping again afterward (even for the
    /// same underlying issue) is always allowed, this never latches into
    /// a state that can't be re-tripped.
    pub fn reset(&mut self, reset_reason: impl Into<String>) {
        let reset_reason = reset_reason.into();
        tracing::warn!(reset_reason = %reset_reason, previous_state = ?self.state, "kill switch reset");
        self.state = KillSwitchState::Armed;
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_armed() {
        let ks = KillSwitch::new();
        assert!(!ks.is_tripped());
    }

    #[test]
    fn trip_sets_tripped_with_reason() {
        let mut ks = KillSwitch::new();
        ks.trip("mark rate divergence exceeded 10%");
        assert!(ks.is_tripped());
        assert!(matches!(ks.state(), KillSwitchState::Tripped { reason } if reason.contains("divergence")));
    }

    #[test]
    fn reset_returns_to_armed() {
        let mut ks = KillSwitch::new();
        ks.trip("test");
        ks.reset("manually verified safe");
        assert!(!ks.is_tripped());
    }

    #[test]
    fn can_retrip_after_reset() {
        let mut ks = KillSwitch::new();
        ks.trip("first issue");
        ks.reset("resolved");
        ks.trip("second issue");
        assert!(ks.is_tripped());
    }
}
