//! **M11 — the correlation tier.**
//!
//! Today an aggregator sees only its direct children
//! (`mesh.rs` `ctx.inputs.retain(|i| children.contains(&i.agent_id))`) and a
//! tier-0 agent owns exactly one `signal_class`. So network, endpoint and
//! identity signals can never meet, and *"a beacon **and** an unusual token
//! **and** lateral movement"* — one detection — cannot be expressed.
//!
//! A correlator reads **multiple branches**. That is the capability, and it is
//! also precisely what breaks the weighting invariant if done naively.
//!
//! # ⚠⚠ Why population becomes a SET, not a count
//!
//! Three branches watching the same workstation each report `input_count: 1`.
//! Summing gives **3**, so the correlator believes it has three independent
//! observations when it has one host seen three ways — and it inflates its
//! confidence *exactly when the branches agree*, which is the moment the
//! signals are most meaningful and the error most expensive.
//!
//! So a correlator's population is the **union of contributing identities**.
//! That forces the event-model change: an aggregate has to carry *which*
//! identities it summarises, not merely how many.
//!
//! ⚠ The industrial example cannot exhibit this. Temp and vibration agents own
//! **disjoint** devices, so sum and union coincide and the bug is invisible —
//! which is why `docs/worked-example-cyber.md` exists.
//!
//! # ⚠ An input with UNKNOWN identities is refused, not guessed
//!
//! An aggregate written before this change carries no identity set. A
//! correlator cannot prove such an input does not overlap its siblings, so it
//! **refuses** rather than falling back to the count — the fallback is exactly
//! the double-count. Same posture as `ReplicaCandidate::freshness_is_known`:
//! unknown is not a soft yes.
//!
//! # ⚠ The count path is untouched
//!
//! Ordinary parent→child aggregation still uses `input_count`, and must: a
//! single-parent tier has no overlap to resolve, and switching it to sets would
//! be cost with no correctness gain. M11 adds a role; it does not re-plumb the
//! tiers that were already right.

use crate::fold::TierInput;
use std::collections::BTreeSet;

/// Why a correlation could not be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrelationRefusal {
    /// An input's identity set is **unknown** (`None` — a pre-M11 writer), so
    /// overlap with its siblings cannot be ruled out.
    ///
    /// ⚠ This is the one that must never be "handled" by falling back to
    /// `input_count`. That fallback is the double-count this module exists to
    /// prevent.
    ///
    /// ⚠⚠ Distinct from a branch that ran and saw nothing (`Some(vec![])`),
    /// which is a real observation and is NOT refused — it simply contributes
    /// no identities and no weight.
    UnknownPopulation { agent_id: String },
    /// Nothing to correlate.
    NoInputs,
    /// Fewer **contributing** branches than the correlator declares it needs.
    ///
    /// ⚠⚠ `present` counts branches with a non-empty known population. A
    /// branch that ran and saw nothing is a branch reporting "not here", and
    /// letting it satisfy a 3-of-3 conjunction would turn "network AND
    /// endpoint AND identity fired" into a weaker claim wearing the stronger
    /// claim's name.
    ///
    /// ⚠ Surfaced rather than silently correlating over what happened to
    /// arrive — see M12.
    TooFewBranches { present: usize, required: usize },
}

impl std::fmt::Display for CorrelationRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownPopulation { agent_id } => write!(
                f,
                "input from {agent_id} carries no population identities; overlap \
                 with sibling branches cannot be ruled out, so it will not be \
                 counted by weight"
            ),
            Self::NoInputs => write!(f, "no inputs to correlate"),
            Self::TooFewBranches { present, required } => {
                write!(f, "{present} of {required} required branches present")
            }
        }
    }
}

/// How a correlator combines branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationRule {
    /// Mean weighted by each branch's **distinct identity count**, with the
    /// correlator's population being the union.
    ///
    /// ⚠⚠ The weight is `|identities|`, NOT `input_count`. Weighting by event
    /// count reintroduces the double-count one level down: a branch that saw
    /// one workstation six times would outweigh a branch that saw three
    /// workstations once each. The unit of independent evidence is the
    /// identity — that is the whole premise of this module, and it has to hold
    /// for the weights as well as for the reported population.
    WeightedMean,
    /// The worst branch wins. Useful when any one domain firing is enough.
    ///
    /// ⚠ NaN propagates rather than being swallowed: `f64::max(NaN, x) == x`,
    /// so a naive fold would drop a NaN branch and report a clean number.
    /// `SeverityPolicy` classifies NaN as Critical (M10) and it must still be
    /// able to see it here.
    Max,
    /// ⭐ The detection-shaped one: the branch mean scaled by **how many
    /// distinct branches contributed**. One branch at 90 is a lead; three
    /// branches at 60 each is an incident.
    ///
    /// ⚠ Unbounded above, and deliberately so — it is a corroboration score,
    /// not a probability. What bounds the *response* is the `SeverityPolicy`
    /// band the correlator's value falls into, not this function.
    Corroboration,
}

/// What one correlation produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Correlated {
    pub value: f64,
    /// ⭐ The union — NOT the sum of the inputs' counts.
    pub population: u32,
    /// What summing the branches' populations would have given. Kept so the
    /// overlap is **observable rather than asserted**: `naive_sum - population`
    /// is exactly the number of double-counted identities, and a battery that
    /// reads it does not have to trust that the union ran.
    ///
    /// ⚠ This is Σ|branch identity set|, not Σ`input_count` — conflating the
    /// two would fold event-chattiness into a number that is supposed to
    /// isolate one thing: branch overlap.
    pub naive_sum: u32,
    /// How many distinct branches contributed.
    pub branches: usize,
    /// The union itself, so the tier above can correlate again without
    /// re-deriving it.
    pub identities: Vec<String>,
}

/// Correlate across branches.
///
/// ⚠ Every input must carry identities. See [`CorrelationRefusal::UnknownPopulation`].
pub fn correlate(
    inputs: &[TierInput],
    rule: CorrelationRule,
    required_branches: usize,
) -> Result<Correlated, CorrelationRefusal> {
    if inputs.is_empty() {
        return Err(CorrelationRefusal::NoInputs);
    }

    // ⚠ UNKNOWN is refused BEFORE anything is computed. A value derived from a
    // population we cannot characterise is worse than no value, because it
    // looks like one.
    let mut union: BTreeSet<&str> = BTreeSet::new();
    let mut naive_sum: u32 = 0;
    let mut contributing = 0usize;
    for i in inputs {
        let ids = match &i.population_ids {
            None => {
                return Err(CorrelationRefusal::UnknownPopulation {
                    agent_id: i.agent_id.clone(),
                })
            }
            Some(ids) => ids,
        };
        // ⚠ Per-branch weight is DISTINCT identities, so a branch that
        // reported one host ten times weighs the same as one that reported it
        // once. `input_count` is deliberately not read here.
        let distinct: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        if !distinct.is_empty() {
            contributing += 1;
        }
        naive_sum = naive_sum.saturating_add(distinct.len() as u32);
        union.extend(distinct);
    }

    // ⚠ Checked AFTER the unknown scan (an unknown branch is a worse problem
    // and should be named as such) but BEFORE any value is produced.
    if contributing < required_branches {
        return Err(CorrelationRefusal::TooFewBranches {
            present: contributing,
            required: required_branches,
        });
    }

    let weight = |i: &TierInput| {
        i.population_ids
            .iter()
            .flatten()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            .len() as f64
    };

    let value = match rule {
        // ⚠ NaN-propagating on purpose — see CorrelationRule::Max.
        CorrelationRule::Max => {
            if inputs.iter().any(|i| i.value.is_nan()) {
                f64::NAN
            } else {
                inputs
                    .iter()
                    .map(|i| i.value)
                    .fold(f64::NEG_INFINITY, f64::max)
            }
        }
        CorrelationRule::WeightedMean => {
            let w: f64 = inputs.iter().map(weight).sum();
            if w == 0.0 {
                0.0
            } else {
                inputs.iter().map(|i| i.value * weight(i)).sum::<f64>() / w
            }
        }
        CorrelationRule::Corroboration => {
            let mean = inputs.iter().map(|i| i.value).sum::<f64>() / inputs.len() as f64;
            mean * inputs.len() as f64
        }
    };

    Ok(Correlated {
        value,
        population: union.len() as u32,
        naive_sum,
        branches: inputs.len(),
        identities: union.into_iter().map(str::to_string).collect(),
    })
}
