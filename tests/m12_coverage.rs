//! **M12 — failure semantics.**
//!
//! A missing child must surface as a *gap* — a smaller, HONEST denominator —
//! never as a quietly smaller one.
//!
//! ⚠⚠ The load-bearing test here is
//! `a_missing_child_barely_moves_the_value_which_is_why_the_marker_is_the_fix`.
//! It asserts the value moved by **less than 1%** while the marker flipped.
//! That is the argument for the whole milestone: no tolerance on the number can
//! separate "an answer about three children" from "an answer about two", so a
//! threshold cannot be the fix and a marker must be.

use signal_mesh::coverage::Coverage;
use signal_mesh::event::*;
use signal_mesh::fold::Reduction;
use signal_mesh::mesh::{Agent, Mesh};
use signal_mesh::react::DeterministicReasoner;

fn sig(device: &str, class: &str, v: f64) -> MeshEvent {
    MeshEvent::SignalObserved(SignalObserved {
        device_id: device.into(),
        signal_class: class.into(),
        value: v,
        device_seq: 0,
    })
}

fn tier0(id: &str, class: &str) -> Agent {
    Agent {
        id: id.into(),
        tier: 0,
        how: Reduction::WeightedMean,
        children: vec![],
        signal_class: Some(class.into()),
        severity: None,
        correlates: None,
    }
}

/// A tier-1 parent that DECLARES three tier-0 children, with `c` optionally
/// absent from the roster.
///
/// ⚠⚠ Withholding `c`'s *signals* does not produce a missing child, and the
/// first version of this suite got that wrong. Every declared agent in
/// `self.agents` runs each cascade, so a signal-less agent still emits — a
/// zero-`input_count` aggregate, which M11 already models as *known-empty* and
/// which a weighted mean correctly gives zero weight. `present` was 3 of 3 and
/// the suite reported no degradation, correctly.
///
/// A declared child is genuinely absent from `ctx.inputs` in exactly three
/// situations, and they are the ones worth testing:
///
/// 1. **declared but not deployed** — topology drift; this fixture;
/// 2. **it refused** — e.g. an M11 correlator that could not correlate, which
///    `continue`s without emitting;
/// 3. **it has not landed by the watermark** — the slow-child case.
fn three_child_mesh(armed: bool, deploy_c: bool) -> Mesh {
    let mut agents = vec![tier0("a", "ca"), tier0("b", "cb")];
    if deploy_c {
        agents.push(tier0("c", "cc"));
    }
    agents.push(Agent {
        id: "top".into(),
        tier: 1,
        how: Reduction::WeightedMean,
        // ⚠ Declared regardless of whether it is deployed. That IS the point:
        // the expectation is written down, so its absence is a fact rather
        // than an inference.
        children: vec!["a".into(), "b".into(), "c".into()],
        signal_class: None,
        severity: None,
        correlates: None,
    });
    Mesh::new("mesh-m12", agents, 1000.0).arm_coverage(armed)
}

fn top_aggregate(m: &Mesh) -> AggregateEmitted {
    m.log
        .records_up_to(u64::MAX)
        .expect("read")
        .into_iter()
        .filter_map(|r| match r.payload {
            MeshEvent::AggregateEmitted(a) if a.agent_id == "top" => Some(a),
            _ => None,
        })
        .next_back()
        .expect("top emitted")
}

// ------------------------------------------------------------ THE argument

/// ⭐⭐ Why the marker is the fix and a threshold is not.
#[test]
fn a_missing_child_barely_moves_the_value_which_is_why_the_marker_is_the_fix() {
    // All three declared children are deployed and reporting.
    let mut full = three_child_mesh(true, true);
    for (d, c) in [("d1", "ca"), ("d2", "cb"), ("d3", "cc")] {
        full.log.append(sig(d, c, 50.0)).expect("append");
    }
    let h = full.log.head();
    let v_full = full.cascade(h, &DeterministicReasoner).expect("cascade");

    // Same declaration; `c` is not deployed. Nothing else differs.
    let mut short = three_child_mesh(true, false);
    for (d, c) in [("d1", "ca"), ("d2", "cb")] {
        short.log.append(sig(d, c, 50.0)).expect("append");
    }
    let h2 = short.log.head();
    let v_short = short.cascade(h2, &DeterministicReasoner).expect("cascade");

    let drift = (v_full.value - v_short.value).abs() / v_full.value.abs().max(1e-9);
    eprintln!(
        "AC-M12.1 full={:.6} short={:.6} relative_drift={:.4}% degraded={}",
        v_full.value,
        v_short.value,
        drift * 100.0,
        v_short.degraded
    );

    // ⚠⚠ THE point: the value is essentially unchanged...
    assert!(
        drift < 0.01,
        "expected the value to barely move (it is a mean of similar numbers); \
         relative drift was {:.4}% — if this fixture makes the value move a lot, \
         it no longer demonstrates why a threshold cannot catch the gap",
        drift * 100.0
    );
    // ⚠ And the CONTROL that keeps the line above from being vacuous: the two
    // meshes really did behave differently. Without this, "the value did not
    // move" is equally consistent with the cascade never having run.
    assert_ne!(
        v_full.coverage.as_ref().map(|c| c.present),
        v_short.coverage.as_ref().map(|c| c.present),
        "the fixtures must differ in what they measured, or nothing is proven"
    );
    // ...while the claim being made is completely different.
    assert!(!v_full.degraded, "three of three is not degraded");
    assert!(v_short.degraded, "two of three IS degraded");
    assert_eq!(v_short.degraded_reason, "missing_children");

    let cov = v_short.coverage.expect("assessed");
    assert_eq!(cov.expected, 3);
    assert_eq!(cov.present, 2);
    assert_eq!(
        cov.missing,
        vec!["c".to_string()],
        "named, not just counted"
    );
    assert_eq!(cov.shortfall(), 1);
}

// ------------------------------------------------------------- the arm

/// ⚠ The OFF test. An unarmed mesh reports `coverage: None`, and `None` must
/// never be read as "complete".
#[test]
fn an_unarmed_mesh_reports_not_assessed_not_complete() {
    let mut m = three_child_mesh(false, false);
    for (d, c) in [("d1", "ca"), ("d2", "cb")] {
        m.log.append(sig(d, c, 50.0)).expect("append");
    }
    let h = m.log.head();
    let v = m.cascade(h, &DeterministicReasoner).expect("cascade");

    assert!(v.coverage.is_none(), "unarmed assesses nothing");
    assert!(!v.degraded, "and therefore cannot claim degradation");
    assert!(top_aggregate(&m).coverage.is_none());

    // ⚠ The discriminating control: the SAME shortfall, armed, IS degraded.
    // Without this the assertion above is equally consistent with the arm being
    // broken in the on position.
    let mut armed = three_child_mesh(true, false);
    for (d, c) in [("d1", "ca"), ("d2", "cb")] {
        armed.log.append(sig(d, c, 50.0)).expect("append");
    }
    let h2 = armed.log.head();
    assert!(
        armed
            .cascade(h2, &DeterministicReasoner)
            .expect("cascade")
            .degraded
    );
}

#[test]
fn a_complete_population_is_marked_complete_not_merely_unmarked() {
    let mut m = three_child_mesh(true, true);
    for (d, c) in [("d1", "ca"), ("d2", "cb"), ("d3", "cc")] {
        m.log.append(sig(d, c, 50.0)).expect("append");
    }
    let h = m.log.head();
    let v = m.cascade(h, &DeterministicReasoner).expect("cascade");
    let cov = v.coverage.expect("assessed");
    eprintln!(
        "AC-M12.2 complete: {}/{} reason={}",
        cov.present,
        cov.expected,
        cov.reason()
    );
    assert_eq!(cov.reason(), "complete");
    assert!(cov.missing.is_empty());
    assert!(!cov.degraded());
}

// ------------------------------------------------------------- transitivity

/// ⚠⚠ A gap TWO tiers down. `mid` is complete in its own right, so its own
/// numbers read clean — the taint survives only because it is carried.
#[test]
fn degradation_propagates_through_a_tier_that_is_itself_complete() {
    let agents = vec![
        tier0("a", "ca"),
        // ⚠ `b` is DECLARED by mid below but deliberately NOT deployed — that
        // is what makes it absent from mid's inputs. Giving it no signals
        // would not: it would still run and emit a zero-weight aggregate.
        // `mid` declares two children; only `a` is there, so mid is short.
        Agent {
            id: "mid".into(),
            tier: 1,
            how: Reduction::WeightedMean,
            children: vec!["a".into(), "b".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
        // `top` declares exactly one child, `mid`, which DOES report. top's own
        // coverage is 1/1 — complete. The only way top can know is inheritance.
        Agent {
            id: "top".into(),
            tier: 2,
            how: Reduction::WeightedMean,
            children: vec!["mid".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
    ];
    let mut m = Mesh::new("mesh-m12-deep", agents, 1000.0).arm_coverage(true);
    m.log.append(sig("d1", "ca", 50.0)).expect("append");
    let h = m.log.head();
    let v = m.cascade(h, &DeterministicReasoner).expect("cascade");

    let cov = v.coverage.clone().expect("assessed");
    eprintln!(
        "AC-M12.3 top: {}/{} missing={:?} inherited={} reason={}",
        cov.present,
        cov.expected,
        cov.missing,
        cov.inherited_degraded,
        cov.reason()
    );
    assert_eq!(cov.expected, 1, "top declared one child");
    assert_eq!(cov.present, 1, "and that child reported — locally complete");
    assert!(cov.missing.is_empty(), "top is missing nothing of its own");
    assert!(
        cov.inherited_degraded,
        "the gap is one tier further down; without the carried bit it dies at mid"
    );
    assert!(v.degraded, "and it must reach the verdict a caller reads");
    assert_eq!(v.degraded_reason, "inherited");
}

// ----------------------------------------------------------- the unit itself

#[test]
fn an_undeclared_reporter_cannot_paper_over_a_missing_declared_child() {
    // `x` is not a declared child. Counting `present.len()` would give 2 of 2
    // and hide that `b` never reported.
    let c = Coverage::assess(&["a".into(), "b".into()], &["a".into(), "x".into()], false);
    eprintln!(
        "AC-M12.4 {}/{} missing={:?}",
        c.present, c.expected, c.missing
    );
    assert_eq!(c.present, 1, "only declared children count as present");
    assert_eq!(c.missing, vec!["b".to_string()]);
    assert!(c.degraded());
}

#[test]
fn the_reason_code_is_a_closed_set_and_names_both_causes() {
    let complete = Coverage::assess(&["a".into()], &["a".into()], false);
    let missing = Coverage::assess(&["a".into(), "b".into()], &["a".into()], false);
    let inherited = Coverage::assess(&["a".into()], &["a".into()], true);
    let both = Coverage::assess(&["a".into(), "b".into()], &["a".into()], true);
    assert_eq!(complete.reason(), "complete");
    assert_eq!(missing.reason(), "missing_children");
    assert_eq!(inherited.reason(), "inherited");
    assert_eq!(both.reason(), "missing_and_inherited");
    // ⚠ The set is CLOSED — an agent roster is unbounded and an id in the label
    // would make the label set unbounded with it. Asserted as membership in the
    // four literals rather than as "does not contain an id": the first draft
    // wrote the latter as `!reason().contains('a')` and it failed on the word
    // "and" in `missing_and_inherited`, which is a test measuring its own
    // spelling rather than the property.
    const CLOSED: [&str; 4] = [
        "complete",
        "missing_children",
        "inherited",
        "missing_and_inherited",
    ];
    for c in [&complete, &missing, &inherited, &both] {
        assert!(
            CLOSED.contains(&c.reason()),
            "reason outside the closed set"
        );
    }
}

#[test]
fn missing_is_sorted_and_deduped_so_a_digest_over_an_aggregate_is_stable() {
    let one = Coverage::assess(&["z".into(), "a".into(), "m".into()], &[], false);
    let two = Coverage::assess(&["m".into(), "z".into(), "a".into()], &[], false);
    assert_eq!(one.missing, vec!["a", "m", "z"]);
    assert_eq!(one, two, "declaration order must not change the record");
}

/// The event must round-trip, including through a reader that predates M12.
#[test]
fn coverage_round_trips_and_a_pre_m12_aggregate_still_deserialises() {
    let with = MeshEvent::AggregateEmitted(AggregateEmitted {
        agent_id: "top".into(),
        tier: 1,
        value: 1.0,
        input_count: 2,
        population_ids: Some(vec!["d1".into()]),
        coverage: Some(Coverage::assess(
            &["a".into(), "b".into()],
            &["a".into()],
            false,
        )),
        up_to_seq: 9,
    });
    let json = serde_json::to_string(&with).expect("ser");
    assert_eq!(with, serde_json::from_str::<MeshEvent>(&json).expect("de"));

    // A pre-M12 aggregate: no `coverage` key at all.
    let old = r#"{"kind":"mesh.aggregate.emitted","agent_id":"top","tier":1,
                  "value":1.0,"input_count":2,"up_to_seq":9}"#;
    match serde_json::from_str::<MeshEvent>(old).expect("pre-M12 still folds") {
        MeshEvent::AggregateEmitted(a) => {
            assert!(
                a.coverage.is_none(),
                "absent must deserialise to None (= not assessed), never to a \
                 synthesised complete coverage"
            );
        }
        other => panic!("wrong arm: {other:?}"),
    }

    // And a complete coverage is NOT skipped on the wire — "assessed and
    // complete" has to be distinguishable from "not assessed".
    let complete = serde_json::to_string(&MeshEvent::AggregateEmitted(AggregateEmitted {
        agent_id: "t".into(),
        tier: 1,
        value: 1.0,
        input_count: 1,
        population_ids: None,
        coverage: Some(Coverage::assess(&["a".into()], &["a".into()], false)),
        up_to_seq: 1,
    }))
    .expect("ser");
    assert!(
        complete.contains("coverage"),
        "a complete coverage must still be written, or it is indistinguishable \
         from never having been measured: {complete}"
    );
}

/// ⭐⭐ The RUNTIME gap, not a topology one. Everything is deployed; the child
/// refuses while the cascade is running.
///
/// This is where M11 and M12 meet: a correlator that cannot correlate
/// `continue`s without emitting (`CorrelationRefused`), so its parent finds it
/// absent from `ctx.inputs`. Without M12 the parent would reduce over nothing
/// and emit a confident zero.
#[test]
fn a_child_that_refuses_at_runtime_leaves_its_parent_short() {
    use signal_mesh::correlation::CorrelationRule;
    use signal_mesh::mesh::CorrelationSpec;

    let agents = vec![
        tier0("a", "ca"),
        tier0("b", "cb"),
        Agent {
            id: "corr".into(),
            tier: 1,
            how: Reduction::WeightedMean,
            children: vec!["a".into(), "b".into()],
            signal_class: None,
            severity: None,
            correlates: Some(CorrelationSpec {
                rule: CorrelationRule::Corroboration,
                // Both branches must contribute; `b` will be silent.
                required_branches: 2,
            }),
        },
        Agent {
            id: "top".into(),
            tier: 2,
            how: Reduction::WeightedMean,
            children: vec!["corr".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
    ];
    let mut m = Mesh::new("mesh-m12-refuse", agents, 1000.0)
        .arm_correlation(true)
        .arm_coverage(true);
    // Only `a`'s class reports. `b` runs and is known-empty, so the correlator
    // has one contributing branch of the two it requires.
    m.log.append(sig("d1", "ca", 50.0)).expect("append");
    let h = m.log.head();
    let v = m.cascade(h, &DeterministicReasoner).expect("cascade");

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    assert!(
        recs.iter()
            .any(|r| matches!(&r.payload, MeshEvent::CorrelationRefused(c)
                              if c.agent_id == "corr" && c.reason == "too_few_branches")),
        "the correlator must have refused, or this test proves nothing"
    );

    let cov = v.coverage.clone().expect("assessed");
    eprintln!(
        "AC-M12.5 top after runtime refusal: {}/{} missing={:?} reason={} value={}",
        cov.present,
        cov.expected,
        cov.missing,
        cov.reason(),
        v.value
    );
    assert_eq!(cov.expected, 1);
    assert_eq!(cov.present, 0, "the refusing child never emitted");
    assert_eq!(cov.missing, vec!["corr".to_string()]);
    assert!(
        v.degraded,
        "a verdict over zero of one declared child is degraded"
    );
    assert_eq!(v.degraded_reason, "missing_children");

    // ⚠⚠ And the reason the marker matters: the VALUE is a perfectly ordinary
    // number. Nothing about it says it was computed over nothing.
    assert!(
        v.value.is_finite(),
        "the point is that the value looks fine: {}",
        v.value
    );

    // ⭐ Control: give `b` a signal and the same topology produces a complete
    // verdict, proving the shortfall above is the refusal and not the wiring.
    let agents2 = vec![
        tier0("a", "ca"),
        tier0("b", "cb"),
        Agent {
            id: "corr".into(),
            tier: 1,
            how: Reduction::WeightedMean,
            children: vec!["a".into(), "b".into()],
            signal_class: None,
            severity: None,
            correlates: Some(CorrelationSpec {
                rule: CorrelationRule::Corroboration,
                required_branches: 2,
            }),
        },
        Agent {
            id: "top".into(),
            tier: 2,
            how: Reduction::WeightedMean,
            children: vec!["corr".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
    ];
    let mut ok = Mesh::new("mesh-m12-ok", agents2, 1000.0)
        .arm_correlation(true)
        .arm_coverage(true);
    ok.log.append(sig("d1", "ca", 50.0)).expect("append");
    ok.log.append(sig("d2", "cb", 50.0)).expect("append");
    let h2 = ok.log.head();
    let v2 = ok.cascade(h2, &DeterministicReasoner).expect("cascade");
    assert!(!v2.degraded, "both branches present: not degraded");
    assert_eq!(v2.coverage.expect("assessed").present, 1);
}
