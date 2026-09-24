//! **M10 acceptance — event-driven escalation.**
//!
//! The claim: *a tier-0 agent can raise the alarm without being asked.*
//! Everything here exists to make that checkable, and to pin the three
//! properties that stop it becoming an outage — dedupe, backpressure, and a
//! suppression that is counted rather than silent.

use signal_mesh::escalation::{
    escalation_armed, EscalationGate, Severity, SeverityPolicy, Suppressed,
};
use signal_mesh::event::MeshEvent;
use signal_mesh::fold::Reduction;
use signal_mesh::mesh::{Agent, Mesh};
use signal_mesh::react::DeterministicReasoner;

fn policy() -> SeverityPolicy {
    SeverityPolicy::new(70.0, 90.0).expect("valid policy")
}

/// Tier 0 escalates; the tier above it is the push target.
fn agents() -> Vec<Agent> {
    vec![
        Agent {
            id: "t0-beacon".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("net".into()),
            severity: Some(policy()),
        },
        Agent {
            id: "t1-net".into(),
            tier: 1,
            how: Reduction::Max,
            children: vec!["t0-beacon".into()],
            signal_class: None,
            severity: None,
        },
    ]
}

// ------------------------------------------------------------ the flag

#[test]
fn escalation_is_off_unless_exactly_true() {
    assert!(!escalation_armed(None));
    for raw in ["", "false", "1", "yes", "TRUE", "True", "on"] {
        assert!(
            !escalation_armed(Some(raw)),
            "{raw:?} must not arm escalation"
        );
    }
    assert!(escalation_armed(Some("true")));
    assert!(escalation_armed(Some(" true ")), "trimmed");
}

// ------------------------------------------------------ severity bands

#[test]
fn bands_are_inclusive_at_the_boundary() {
    let p = policy();
    assert_eq!(p.classify(69.999), Severity::Nominal);
    // ⚠ `>=`, not `>`. A threshold documented as "elevated at 70" that does not
    // fire at exactly 70 is a boundary bug nobody finds until it matters.
    assert_eq!(
        p.classify(70.0),
        Severity::Elevated,
        "the boundary must fire"
    );
    assert_eq!(p.classify(89.999), Severity::Elevated);
    assert_eq!(
        p.classify(90.0),
        Severity::Critical,
        "the boundary must fire"
    );
    assert_eq!(p.classify(1e9), Severity::Critical);
}

/// ⚠ NaN compares false against everything, so a naive threshold chain returns
/// Nominal — a detector that goes quiet exactly when its input is broken.
#[test]
fn nan_is_critical_not_quiet() {
    assert_eq!(policy().classify(f64::NAN), Severity::Critical);
}

/// An inverted policy is refused, because its effect — everything reads
/// critical — looks like a working detector having a bad day.
#[test]
fn an_inverted_policy_is_refused() {
    assert!(SeverityPolicy::new(90.0, 70.0).is_err());
    assert!(SeverityPolicy::new(f64::NAN, 90.0).is_err());
    assert!(SeverityPolicy::new(70.0, 90.0).is_ok());
}

#[test]
fn only_bands_above_nominal_escalate() {
    assert!(!Severity::Nominal.escalates());
    assert!(Severity::Elevated.escalates());
    assert!(Severity::Critical.escalates());
}

// ------------------------------------------------- the gate: dedupe

/// ⭐⭐ **The load-bearing property.** A beaconing device appends continuously;
/// without dedupe it escalates on every append forever and the escalation path
/// becomes the outage.
#[test]
fn the_same_band_at_the_same_watermark_fires_once() {
    let mut g = EscalationGate::new(32);
    assert_eq!(g.admit("a", Severity::Critical, 10), Ok(()));
    assert_eq!(
        g.admit("a", Severity::Critical, 10),
        Err(Suppressed::Duplicate),
        "the same badness at the same point in the log is ONE event"
    );

    // ⚠ CONTROL — three things that are genuinely new must still fire, or
    // "dedupe works" would be indistinguishable from "nothing ever fires".
    assert_eq!(
        g.admit("a", Severity::Elevated, 10),
        Ok(()),
        "a different band is news"
    );
    assert_eq!(
        g.admit("a", Severity::Critical, 11),
        Ok(()),
        "a new watermark is news"
    );
    assert_eq!(
        g.admit("b", Severity::Critical, 10),
        Ok(()),
        "a different agent is news"
    );
}

/// Backpressure bounds the escalation path — and a duplicate must not consume
/// budget, or a chattering agent starves a quiet one.
#[test]
fn backpressure_bounds_the_path_and_duplicates_do_not_consume_budget() {
    let mut g = EscalationGate::new(2);
    assert_eq!(g.admit("a", Severity::Critical, 1), Ok(()));
    assert_eq!(g.admit("b", Severity::Critical, 1), Ok(()));
    assert_eq!(g.in_flight(), 2);
    assert_eq!(
        g.admit("c", Severity::Critical, 1),
        Err(Suppressed::Backpressure)
    );

    // A duplicate while full is reported as a DUPLICATE, not backpressure —
    // the two want different operator responses.
    assert_eq!(
        g.admit("a", Severity::Critical, 1),
        Err(Suppressed::Duplicate)
    );

    g.release();
    assert_eq!(g.in_flight(), 1);
    assert_eq!(g.admit("c", Severity::Critical, 1), Ok(()), "budget freed");
}

// ------------------------------------------ the mesh-level behaviour

/// ⭐ **The capability the review said was missing.** An escalation happens
/// with **no cascade call at all**.
#[test]
fn a_tier0_agent_escalates_without_being_asked() {
    let mut m = Mesh::new("mesh-1", agents(), 50.0);
    m.escalation_armed = true;
    let before = m.log.record_count().expect("count");

    let out = m.escalate("t0-beacon", 95.0, 4, 7).expect("store ok");
    let seq = out.expect("critical must escalate");

    let all = m.log.records_up_to(u64::MAX).expect("read");
    let esc: Vec<_> = all
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::Escalated(e) => Some(e),
            _ => None,
        })
        .collect();

    eprintln!(
        "M10: records {before} -> {} | escalations={} seq={seq}",
        all.len(),
        esc.len()
    );
    assert_eq!(esc.len(), 1, "exactly one escalation event");
    let e = esc[0];
    assert_eq!(e.agent_id, "t0-beacon");
    assert_eq!(e.tier, 0);
    assert_eq!(e.severity, "critical");
    assert_eq!(e.input_count, 4, "the population travels with the alarm");
    assert_eq!(
        e.to_agent, "t1-net",
        "it must push to the agent that declares it a child, not to a fixed target"
    );
    assert_eq!(m.escalations, 1);
}

/// ⚠ **The discriminating control.** Below threshold produces nothing — without
/// this, "it escalated" is consistent with escalating on everything.
#[test]
fn a_nominal_value_does_not_escalate() {
    let mut m = Mesh::new("mesh-1", agents(), 50.0);
    m.escalation_armed = true;
    let out = m.escalate("t0-beacon", 12.0, 4, 7).expect("store ok");
    assert_eq!(out, Err(Suppressed::Nominal));
    let all = m.log.records_up_to(u64::MAX).expect("read");
    assert!(
        !all.iter()
            .any(|r| matches!(r.payload, MeshEvent::Escalated(_))),
        "a nominal value must append NOTHING"
    );
    assert_eq!(m.escalations, 0);
    assert_eq!(
        m.suppressed_nominal, 1,
        "and the non-event is still counted"
    );
}

/// An agent with no severity policy is unchanged by M10 existing.
#[test]
fn an_agent_without_a_policy_never_escalates() {
    let mut m = Mesh::new("mesh-1", agents(), 50.0);
    m.escalation_armed = true;
    assert_eq!(
        m.escalate("t1-net", 99.0, 4, 7).expect("store ok"),
        Err(Suppressed::Nominal),
        "t1-net declares no severity policy, so it cannot escalate"
    );
    assert_eq!(m.escalations, 0);
}

/// ⚠ Every suppression is counted. A suppressed escalation must be a visible
/// gap, never silence — the same rule the collector follows.
#[test]
fn every_suppression_is_counted() {
    let mut m = Mesh::new("mesh-1", agents(), 50.0);
    m.escalation_armed = true;
    m.escalate("t0-beacon", 10.0, 1, 7).unwrap().unwrap_err(); // nominal
    m.escalate("t0-beacon", 95.0, 1, 7).unwrap().unwrap(); // fires
    m.escalate("t0-beacon", 95.0, 1, 7).unwrap().unwrap_err(); // duplicate
    eprintln!(
        "M10 counters: fired={} nominal={} dup={} backpressure={}",
        m.escalations, m.suppressed_nominal, m.suppressed_duplicate, m.suppressed_backpressure
    );
    assert_eq!(m.escalations, 1);
    assert_eq!(m.suppressed_nominal, 1);
    assert_eq!(m.suppressed_duplicate, 1);
    assert_eq!(m.suppressed_backpressure, 0);
}

/// ⭐⭐ **Additive, not a replacement.** The scheduled cascade must be
/// bit-for-bit unaffected by escalation existing — including the pinned
/// fixture verdict.
#[test]
fn escalation_does_not_change_the_scheduled_cascade() {
    let fixture = || {
        vec![
            Agent {
                id: "t0-temp".into(),
                tier: 0,
                how: Reduction::WeightedMean,
                children: vec![],
                signal_class: Some("temp".into()),
                severity: None,
            },
            Agent {
                id: "t0-vibe".into(),
                tier: 0,
                how: Reduction::WeightedMean,
                children: vec![],
                signal_class: Some("vibe".into()),
                severity: None,
            },
            Agent {
                id: "t1-site".into(),
                tier: 1,
                how: Reduction::WeightedMean,
                children: vec!["t0-temp".into(), "t0-vibe".into()],
                signal_class: None,
                severity: None,
            },
            Agent {
                id: "t2-fleet".into(),
                tier: 2,
                how: Reduction::Max,
                children: vec!["t1-site".into()],
                signal_class: None,
                severity: None,
            },
        ]
    };
    let feed = |m: &mut Mesh| {
        for (i, (d, c, v)) in [
            ("dev-01", "temp", 41.0),
            ("dev-02", "temp", 47.0),
            ("dev-03", "temp", 44.0),
            ("dev-04", "vibe", 61.0),
            ("dev-05", "vibe", 59.0),
            ("dev-06", "vibe", 63.0),
        ]
        .iter()
        .enumerate()
        {
            m.observe_signal(d, c, *v, i as u64 + 1).expect("append");
        }
    };

    let mut plain = Mesh::new("mesh-1", fixture(), 50.0);
    plain.escalation_armed = true;
    feed(&mut plain);
    let a = plain
        .cascade(plain.log.head(), &DeterministicReasoner)
        .expect("cascade");

    // The same run, on a mesh whose tier-0 agents CAN escalate.
    let mut armed_agents = fixture();
    armed_agents[0].severity = Some(policy());
    armed_agents[1].severity = Some(policy());
    let mut armed = Mesh::new("mesh-1", armed_agents, 50.0);
    armed.escalation_armed = true;
    feed(&mut armed);
    let b = armed
        .cascade(armed.log.head(), &DeterministicReasoner)
        .expect("cascade");

    eprintln!("M10 additive: plain={:.4} armed={:.4}", a.value, b.value);
    assert_eq!(
        a, b,
        "declaring a severity policy must not change the cascade"
    );
    assert!(
        (a.value - 52.5).abs() < 1e-9,
        "and the pinned fixture verdict holds"
    );
}

/// ⚠⚠ **The flag's own discriminating control.**
///
/// Every other test in this file arms the mesh, so deleting the arming check
/// entirely changed nothing and the planted-defect battery reported it as
/// SURVIVED. A default-off flag with no test for the OFF state is not
/// default-off; it is untested.
#[test]
fn an_unarmed_mesh_never_escalates() {
    let mut m = Mesh::new("mesh-1", agents(), 50.0);
    assert!(!m.escalation_armed, "off is the default");

    let out = m.escalate("t0-beacon", 999.0, 4, 7).expect("store ok");
    assert_eq!(
        out,
        Err(Suppressed::Nominal),
        "a critical value on an UNARMED mesh must not escalate"
    );
    let all = m.log.records_up_to(u64::MAX).expect("read");
    assert!(
        !all.iter()
            .any(|r| matches!(r.payload, MeshEvent::Escalated(_))),
        "and it must append nothing"
    );
    assert_eq!(m.escalations, 0);

    // ⚠ CONTROL: the identical call on an armed mesh DOES fire, so this test
    // is about the flag and not about the value being unescalatable.
    m.escalation_armed = true;
    assert!(
        m.escalate("t0-beacon", 999.0, 4, 7)
            .expect("store ok")
            .is_ok(),
        "the same call must fire once armed"
    );
    assert_eq!(m.escalations, 1);
}
