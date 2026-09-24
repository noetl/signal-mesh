//! **M12 — failure semantics.** A missing child is a *gap*, never a quietly
//! smaller denominator.
//!
//! # The defect
//!
//! [`fold::population`](crate::fold::population) sums `input_count` over the
//! inputs that are **present**. A child that crashed, stalled or was
//! partitioned is simply absent from `ctx.inputs`, so the parent reduces over a
//! subset — and nothing anywhere says so.
//!
//! ⚠⚠ The reason a threshold cannot fix this is arithmetic: with three children
//! reporting similar values, dropping one moves a weighted mean by a fraction
//! of a percent. The verdict is **wrong in kind, not in magnitude** — it is an
//! answer about two thirds of the fleet wearing the name of an answer about all
//! of it. No tolerance on the value can separate those, which is why the marker
//! is the fix and `tests/m12_coverage.rs` asserts the value barely moved.
//!
//! # Why the inherited bit is stored rather than derived
//!
//! A parent can see its children's [`Coverage`] and work out that a child was
//! itself short. What it *cannot* see is a gap two tiers down: if C is complete
//! but C's own child D was missing, C's coverage reads clean and the taint dies
//! at C.
//!
//! So `inherited_degraded` is carried. It is the one denormalised bit here, and
//! it exists because transitivity genuinely cannot be recomputed from a single
//! tier of inputs. Everything else — `degraded`, the shortfall — is
//! **derived**, per `representation-drift.md`: when a value is both stored and
//! derivable, one of them will be wrong and it is rarely the loud one.
//!
//! # What M12 does NOT do
//!
//! It does not decide whether a degraded verdict is servable. That is a
//! deployment policy, and inventing one here would ship a knob nobody set. The
//! fold's job is to make the shortfall impossible to miss; the caller decides
//! what to do about it.
//!
//! ⚠ It also does not cover **tier 0**. An aggregator declares its children, so
//! "expected" is a real, written-down set. A tier-0 agent has no declared device
//! roster — nothing says which of 200 workstations *should* have reported — so
//! its expectation would have to be inferred from history, which is a different
//! design and a different milestone.

use serde::{Deserialize, Serialize};

/// How much of what an agent expected actually arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    /// Children the agent declares. ⚠ The *declared* set, not the observed one
    /// — the whole point is that it is written down in advance.
    pub expected: u32,
    /// Declared children that emitted an aggregate at or below the watermark.
    pub present: u32,
    /// Which ones did not. ⚠ Sorted, so a digest over an aggregate is stable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// ⚠⚠ At least one input was itself degraded. Stored, not derived — see the
    /// module docs.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherited_degraded: bool,
}

impl Coverage {
    /// Build a coverage report from a declared child set and the agents that
    /// actually reported.
    ///
    /// `degraded_inputs` is whether any *present* input carried a degraded
    /// coverage of its own.
    pub fn assess(expected: &[String], present: &[String], degraded_inputs: bool) -> Self {
        let mut missing: Vec<String> = expected
            .iter()
            .filter(|e| !present.contains(e))
            .cloned()
            .collect();
        missing.sort();
        missing.dedup();
        Coverage {
            expected: expected.len() as u32,
            // ⚠ Counted as "declared children that reported", NOT
            // `present.len()`. An input from an agent this parent never
            // declared must not paper over a missing declared one — it would
            // make `present == expected` while a child is still absent.
            present: expected.iter().filter(|e| present.contains(e)).count() as u32,
            missing,
            inherited_degraded: degraded_inputs,
        }
    }

    /// Is this value an answer about less than the whole declared population?
    pub fn degraded(&self) -> bool {
        self.present < self.expected || self.inherited_degraded
    }

    /// How many declared children did not report.
    pub fn shortfall(&self) -> u32 {
        self.expected.saturating_sub(self.present)
    }

    /// A stable, closed-set label for a metric or an alert.
    ///
    /// ⚠ The missing *ids* are deliberately not in it: an agent roster is
    /// unbounded and would make an unbounded label set.
    pub fn reason(&self) -> &'static str {
        match (self.shortfall() > 0, self.inherited_degraded) {
            (false, false) => "complete",
            (true, false) => "missing_children",
            (false, true) => "inherited",
            (true, true) => "missing_and_inherited",
        }
    }
}
