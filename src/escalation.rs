//! **M10 — event-driven escalation.**
//!
//! The mesh's cascade is **top-initiated**: a dispatcher submits a Task
//! downward to every agent and answers flow back up (`mesh.rs`, blueprint §5
//! `TOP->>T1` / `T1->>T0`). That is *periodic scoring*. The requirement it does
//! not meet is *"escalate the moment a device starts beaconing to C2"* — a
//! tier-0 agent today cannot say anything until it is asked.
//!
//! This module adds the missing direction: a tier-0 agent that crosses a
//! severity threshold **pushes a Task upward on its own**.
//!
//! # Additive, never a replacement
//!
//! ⚠ The scheduled cascade is untouched and remains the correctness baseline.
//! Escalation is a second, faster path to the *same* tiers — it does not
//! compute the verdict, it asks for one sooner. A design where escalation
//! replaced the cascade would make the answer depend on arrival order, which is
//! exactly what the per-tier watermark exists to prevent.
//!
//! # The three things that make it safe
//!
//! **Trigger** — a severity band over the reduced value, evaluated at `act`
//! where the value already exists. Bands rather than a bare threshold, so
//! "worse" is expressible without re-firing on every wobble.
//!
//! **Dedupe** — ⚠⚠ the load-bearing one. A beaconing device appends
//! continuously; without dedupe it escalates on *every append, forever*, and
//! the escalation path becomes the outage. An escalation is keyed on
//! `(agent, band, watermark)` and the same key fires **once**.
//!
//! # ⚠ HTTP exposure lands separately
//!
//!  is NOT in this change. The  write routes live
//! on the Phase-1 deployment branch, which is unmerged, and duplicating them
//! here would create two definitions of the same surface to reconcile later.
//! This PR is the capability and its guarantees; the endpoint is one route on
//! whichever of the two merges second. Nothing here is reachable over HTTP yet,
//! and that is stated rather than implied.
//!
//! **Backpressure** — bound the escalation path, never the ingest. ⚠ A
//! suppressed escalation is a **counted gap**, never a silently smaller
//! denominator — the same rule the collector follows, and the one
//! `fold.rs::population` currently breaks for missing children (M12).

use crate::event::MeshEvent;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Is event-driven escalation armed? Pure over the raw value.
///
/// ⚠ There is deliberately **no `ESCALATION_ENV` constant here yet.** One was
/// written, and the crate's own env-currency guard refused it: the binary on
/// this branch has no mesh to arm, so the constant would have been a declared
/// flag that nothing reads — *documentation of a capability that does not
/// exist*, which is the precise failure that guard was built to catch. It
/// caught its author. The constant lands with the HTTP exposure, in the change
/// that gives it a reader, and it will be named
/// `NOETL_SIGNAL_MESH_ESCALATION`.
pub fn escalation_armed(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("true"))
}

/// How bad a reduced value is.
///
/// ⚠ Bands, not a raw threshold. A bare `value > t` re-fires on every append
/// while the value hovers above `t`; a band changes only when severity
/// genuinely changes, so "it got worse" is expressible and "it is still bad"
/// is not an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
    /// Below every threshold. **Never escalates.**
    Nominal,
    Elevated,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Nominal => "nominal",
            Severity::Elevated => "elevated",
            Severity::Critical => "critical",
        }
    }

    /// Only a band above nominal is worth waking anyone for.
    pub fn escalates(self) -> bool {
        !matches!(self, Severity::Nominal)
    }
}

/// The per-agent severity policy.
#[derive(Debug, Clone, PartialEq)]
pub struct SeverityPolicy {
    pub elevated_at: f64,
    pub critical_at: f64,
}

impl SeverityPolicy {
    /// ⚠ Refuses an inverted policy rather than silently ordering the bounds.
    /// `critical < elevated` is a configuration error whose effect —
    /// everything reads critical — looks like a working detector having a bad
    /// day.
    pub fn new(elevated_at: f64, critical_at: f64) -> Result<Self, String> {
        if !(elevated_at.is_finite() && critical_at.is_finite()) {
            return Err("severity thresholds must be finite".into());
        }
        if critical_at < elevated_at {
            return Err(format!(
                "critical_at ({critical_at}) is below elevated_at ({elevated_at}); \
                 every value would read critical"
            ));
        }
        Ok(Self {
            elevated_at,
            critical_at,
        })
    }

    /// Classify a reduced value.
    ///
    /// ⚠ `>=`, not `>`. A threshold documented as "elevated at 70" that does
    /// not fire at exactly 70 is a boundary bug nobody notices until the one
    /// time it matters.
    pub fn classify(&self, value: f64) -> Severity {
        if value.is_nan() {
            // ⚠ NaN is not "fine". It compares false against everything, so a
            // naive chain would silently return Nominal — a detector that goes
            // quiet exactly when its input is broken.
            return Severity::Critical;
        }
        if value >= self.critical_at {
            Severity::Critical
        } else if value >= self.elevated_at {
            Severity::Elevated
        } else {
            Severity::Nominal
        }
    }
}

/// One escalation, as recorded on the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Escalated {
    pub agent_id: String,
    pub tier: u8,
    pub severity: String,
    pub value: f64,
    /// The population behind the value, carried so the receiver can weigh it
    /// without re-folding.
    pub input_count: u32,
    /// The watermark the escalating agent had reached.
    pub up_to_seq: u64,
    /// Who it was pushed to.
    pub to_agent: String,
}

/// Why an escalation did not happen. ⚠ Counted, never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppressed {
    /// Severity was nominal — the ordinary case, not a problem.
    Nominal,
    /// Same `(agent, band, watermark)` already fired.
    Duplicate,
    /// The in-flight budget is full.
    Backpressure,
}

/// The dedupe + backpressure gate.
///
/// ⚠⚠ Without this a beaconing device escalates on every append forever. The
/// key is `(agent, band, watermark)`: the same badness at the same point in the
/// log is **one** event, while a band change or a new watermark is genuinely
/// new information.
#[derive(Debug)]
pub struct EscalationGate {
    seen: BTreeSet<(String, &'static str, u64)>,
    in_flight: usize,
    max_in_flight: usize,
}

impl EscalationGate {
    pub fn new(max_in_flight: usize) -> Self {
        Self {
            seen: BTreeSet::new(),
            in_flight: 0,
            max_in_flight,
        }
    }

    /// Decide whether this escalation may fire, recording it if so.
    pub fn admit(
        &mut self,
        agent_id: &str,
        severity: Severity,
        up_to_seq: u64,
    ) -> Result<(), Suppressed> {
        if !severity.escalates() {
            return Err(Suppressed::Nominal);
        }
        let key = (agent_id.to_string(), severity.as_str(), up_to_seq);
        if self.seen.contains(&key) {
            return Err(Suppressed::Duplicate);
        }
        // ⚠ Backpressure is checked AFTER dedupe on purpose: a duplicate must
        // not consume budget, or a chattering agent would starve a genuinely
        // new escalation from a quiet one.
        if self.in_flight >= self.max_in_flight {
            return Err(Suppressed::Backpressure);
        }
        self.seen.insert(key);
        self.in_flight += 1;
        Ok(())
    }

    /// The receiver acknowledged one escalation.
    pub fn release(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
}

/// Build the event for an admitted escalation.
pub fn escalation_event(
    agent_id: &str,
    tier: u8,
    to_agent: &str,
    severity: Severity,
    value: f64,
    input_count: u32,
    up_to_seq: u64,
) -> MeshEvent {
    MeshEvent::Escalated(Escalated {
        agent_id: agent_id.to_string(),
        tier,
        severity: severity.as_str().to_string(),
        value,
        input_count,
        up_to_seq,
        to_agent: to_agent.to_string(),
    })
}
