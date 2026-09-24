//! **M11 — the correlation tier.**
//!
//! The whole suite exists for ONE arithmetic error: three branches watching the
//! same workstation report `input_count: 1` each, and summing gives **3**. The
//! correlator then believes it has three independent observations of three
//! hosts when it has one host seen three ways — and it is *most* wrong exactly
//! when the branches agree, which is the moment the detection matters.
//!
//! ⚠⚠ Every test that asserts the union must be paired with a **disjoint**
//! control in the same test. A fixture whose branches happen not to overlap
//! makes union and sum identical, so it passes under the bug — that is the
//! "fixture that cannot exhibit the failure" trap from
//! `mutation-harness-self-traps`.

use signal_mesh::correlation::*;
use signal_mesh::event::*;
use signal_mesh::fold::TierInput;
use signal_mesh::mesh::{Agent, CorrelationSpec, Mesh};
use signal_mesh::react::DeterministicReasoner;

fn branch(agent: &str, value: f64, count: u32, ids: &[&str]) -> TierInput {
    TierInput {
        agent_id: agent.into(),
        value,
        input_count: count,
        up_to_seq: 0,
        population_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
    }
}

/// A pre-M11 aggregate: it carries no identity set at all.
fn legacy(agent: &str, value: f64, count: u32) -> TierInput {
    TierInput {
        agent_id: agent.into(),
        value,
        input_count: count,
        up_to_seq: 0,
        population_ids: None,
    }
}

// ---------------------------------------------------------------- the defect

/// ⭐⭐ THE test. Three domains, one workstation.
#[test]
fn branches_sharing_a_device_are_counted_once_and_disjoint_ones_are_not() {
    // Network, endpoint and identity all saw WS-42. One host, three views.
    let shared = vec![
        branch("net", 60.0, 1, &["WS-42"]),
        branch("endpoint", 60.0, 1, &["WS-42"]),
        branch("identity", 60.0, 1, &["WS-42"]),
    ];
    let c = correlate(&shared, CorrelationRule::Corroboration, 3).expect("correlates");
    eprintln!(
        "AC-M11.1 shared: branches={} population={} naive_sum={} ids={:?}",
        c.branches, c.population, c.naive_sum, c.identities
    );
    assert_eq!(c.population, 1, "one host seen three ways is ONE host");
    assert_eq!(
        c.naive_sum, 3,
        "the wrong answer is still reported, for contrast"
    );
    assert_eq!(c.naive_sum - c.population, 2, "exactly two double-counts");
    assert_eq!(c.identities, vec!["WS-42".to_string()]);

    // ⭐ The control. Same shape, disjoint hosts — sum and union MUST coincide,
    // which is what proves the assertion above is about overlap and not about
    // the union being unconditionally small.
    let disjoint = vec![
        branch("net", 60.0, 1, &["WS-1"]),
        branch("endpoint", 60.0, 1, &["WS-2"]),
        branch("identity", 60.0, 1, &["WS-3"]),
    ];
    let d = correlate(&disjoint, CorrelationRule::Corroboration, 3).expect("correlates");
    eprintln!(
        "AC-M11.1 disjoint: population={} naive_sum={}",
        d.population, d.naive_sum
    );
    assert_eq!(d.population, 3, "three distinct hosts are three");
    assert_eq!(d.naive_sum, d.population, "no overlap to collapse");

    // And the two fixtures must actually differ, or neither proved anything.
    assert_ne!(
        c.population, d.population,
        "fixtures must discriminate; if these match the suite is decorative"
    );
}

/// Partial overlap — the case a set-vs-count bug can still survive if the test
/// only ever uses full overlap or none.
#[test]
fn partial_overlap_collapses_only_the_shared_identities() {
    let inputs = vec![
        branch("net", 10.0, 2, &["WS-1", "WS-2"]),
        branch("endpoint", 20.0, 2, &["WS-2", "WS-3"]),
    ];
    let c = correlate(&inputs, CorrelationRule::Max, 2).expect("correlates");
    eprintln!(
        "AC-M11.2 population={} naive_sum={} ids={:?}",
        c.population, c.naive_sum, c.identities
    );
    assert_eq!(c.population, 3, "WS-1,2,3");
    assert_eq!(c.naive_sum, 4, "2 + 2");
    assert_eq!(c.identities, vec!["WS-1", "WS-2", "WS-3"]);
}

// ------------------------------------------------------------- the refusals

/// ⚠⚠ The fallback that must never exist. An aggregate written before M11
/// carries no identities; falling back to `input_count` IS the double-count.
#[test]
fn an_input_with_no_identities_is_refused_never_counted_by_its_count() {
    let inputs = vec![
        branch("net", 60.0, 5, &["WS-1"]),
        legacy("legacy", 90.0, 5), // pre-M11 aggregate
    ];
    let e = correlate(&inputs, CorrelationRule::Max, 2).expect_err("must refuse");
    assert_eq!(
        e,
        CorrelationRefusal::UnknownPopulation {
            agent_id: "legacy".into()
        }
    );
    eprintln!("AC-M11.3 refusal: {e}");
    assert!(e.to_string().contains("overlap"), "the reason says WHY");
}

/// ⚠ The refusal must land BEFORE any value is computed. A correlator that
/// computes first and refuses second still leaks a number through any path
/// that logs it.
#[test]
fn a_single_unknown_branch_refuses_the_whole_correlation() {
    // Two perfectly good branches; the third is unknown. A partial answer over
    // the good two would look entirely reasonable — and be a different claim.
    let inputs = vec![
        branch("net", 60.0, 1, &["WS-1"]),
        branch("endpoint", 60.0, 1, &["WS-2"]),
        legacy("identity", 60.0, 1),
    ];
    assert!(correlate(&inputs, CorrelationRule::Corroboration, 3).is_err());

    // Control: drop the unknown branch and the SAME call succeeds, proving the
    // refusal is caused by the unknown input and not by the rule or the arity.
    let ok = correlate(&inputs[..2], CorrelationRule::Corroboration, 2).expect("controls");
    assert_eq!(ok.population, 2);
}

#[test]
fn too_few_branches_is_a_refusal_not_a_quiet_correlation_over_what_arrived() {
    let inputs = vec![branch("net", 99.0, 1, &["WS-1"])];
    let e = correlate(&inputs, CorrelationRule::Corroboration, 3).expect_err("must refuse");
    assert_eq!(
        e,
        CorrelationRefusal::TooFewBranches {
            present: 1,
            required: 3
        }
    );
    // ⚠ Note what the bug would have produced: a single 99 branch, scaled by
    // 1, emitted as a three-domain detection. Confident, and wrong.
    assert!(correlate(&inputs, CorrelationRule::Corroboration, 1).is_ok());
}

/// ⚠⚠ The distinction the first battery run forced into the event model.
///
/// A branch that RAN and saw nothing (`Some(vec![])`) is a real observation.
/// A branch written before M11 (`None`) is an unknown. A bare `Vec` cannot
/// tell them apart, and conflating them made a silent domain agent get
/// reported as a legacy writer — the wrong cause, in the record an operator
/// reads.
#[test]
fn a_branch_that_ran_and_saw_nothing_is_not_an_unknown_branch() {
    let ran_empty = branch("identity", 0.0, 0, &[]);
    let never_wrote = legacy("identity", 0.0, 0);

    // The unknown is refused, by name.
    assert_eq!(
        correlate(
            &[branch("net", 60.0, 1, &["WS-1"]), never_wrote],
            CorrelationRule::Max,
            1
        )
        .expect_err("unknown refuses"),
        CorrelationRefusal::UnknownPopulation {
            agent_id: "identity".into()
        }
    );

    // The known-empty is NOT refused — it contributes nothing and weighs
    // nothing, which is exactly what "this domain was silent" means.
    let c = correlate(
        &[branch("net", 60.0, 1, &["WS-1"]), ran_empty.clone()],
        CorrelationRule::WeightedMean,
        1,
    )
    .expect("known-empty is a real observation, not a refusal");
    eprintln!(
        "AC-M11.10 known-empty: population={} value={} branches={}",
        c.population, c.value, c.branches
    );
    assert_eq!(c.population, 1, "only WS-1 is in the union");
    assert_eq!(c.value, 60.0, "a zero-weight branch does not drag the mean");
    assert_eq!(c.branches, 2, "it is still a branch that reported");

    // ⚠⚠ But it does NOT satisfy the arity requirement. "network AND endpoint
    // both fired" must not be satisfiable by one that fired and one that said
    // nothing — that is the stronger claim's name on a weaker claim.
    assert_eq!(
        correlate(
            &[branch("net", 60.0, 1, &["WS-1"]), ran_empty],
            CorrelationRule::Corroboration,
            2
        )
        .expect_err("a silent branch does not corroborate"),
        CorrelationRefusal::TooFewBranches {
            present: 1,
            required: 2
        }
    );
}

#[test]
fn no_inputs_refuses_rather_than_returning_zero() {
    assert_eq!(
        correlate(&[], CorrelationRule::Max, 0).expect_err("must refuse"),
        CorrelationRefusal::NoInputs
    );
}

// ------------------------------------------------------------- the weighting

/// ⚠⚠ Weights are DISTINCT IDENTITIES, not `input_count`. Weighting by event
/// count reintroduces the double-count one level down: a chatty branch that saw
/// one host a hundred times would drown three branches that each saw a
/// different host once.
#[test]
fn weighted_mean_weights_by_identities_not_by_event_count() {
    let inputs = vec![
        // One host, screaming.
        branch("chatty", 0.0, 100, &["WS-1"]),
        // Three hosts, one observation each.
        branch("quiet", 100.0, 1, &["WS-2", "WS-3", "WS-4"]),
    ];
    let c = correlate(&inputs, CorrelationRule::WeightedMean, 2).expect("correlates");
    eprintln!("AC-M11.4 weighted_mean value={:.6}", c.value);

    // identity-weighted: (0*1 + 100*3) / 4 == 75
    assert!(
        (c.value - 75.0).abs() < 1e-9,
        "expected 75.0 (identity-weighted), got {}",
        c.value
    );
    // count-weighted would be (0*100 + 100*1)/101 ≈ 0.990 — the number this
    // test exists to reject. Asserted explicitly so a future edit that flips
    // the weight back cannot pass by widening a tolerance.
    assert!(
        (c.value - 100.0 / 101.0).abs() > 1.0,
        "value collapsed to the count-weighted answer"
    );
    assert_eq!(c.population, 4);
    // ⚠ And naive_sum must be Σ|identities| (1+3), NOT Σinput_count (101).
    // Without this line a defect that flips naive_sum back to counts survives
    // every other test in the file, because every other fixture happens to
    // have input_count == |identities|.
    assert_eq!(
        c.naive_sum, 4,
        "naive_sum is an identity count, not an event count"
    );
}

#[test]
fn max_propagates_nan_rather_than_swallowing_it() {
    // ⚠ f64::max(NaN, x) == x, so a naive fold reports a clean number for a
    // branch that produced garbage. M10 classifies NaN as Critical; it has to
    // be able to see it.
    let inputs = vec![
        branch("net", f64::NAN, 1, &["WS-1"]),
        branch("endpoint", 3.0, 1, &["WS-2"]),
    ];
    let c = correlate(&inputs, CorrelationRule::Max, 2).expect("correlates");
    assert!(c.value.is_nan(), "NaN was swallowed: got {}", c.value);

    // Control: without the NaN, Max is an ordinary max.
    let ok = correlate(
        &[
            branch("net", 1.0, 1, &["WS-1"]),
            branch("endpoint", 3.0, 1, &["WS-2"]),
        ],
        CorrelationRule::Max,
        2,
    )
    .expect("correlates");
    assert_eq!(ok.value, 3.0);
}

#[test]
fn corroboration_scales_with_the_number_of_agreeing_branches() {
    let one = correlate(
        &[branch("net", 60.0, 1, &["A"])],
        CorrelationRule::Corroboration,
        1,
    )
    .expect("correlates");
    let three = correlate(
        &[
            branch("net", 60.0, 1, &["A"]),
            branch("endpoint", 60.0, 1, &["B"]),
            branch("identity", 60.0, 1, &["C"]),
        ],
        CorrelationRule::Corroboration,
        3,
    )
    .expect("correlates");
    eprintln!("AC-M11.5 one={} three={}", one.value, three.value);
    assert_eq!(one.value, 60.0, "a single branch is never amplified");
    assert_eq!(three.value, 180.0, "three agreeing branches corroborate");
}

// ---------------------------------------------------------------- end-to-end

fn sig(device: &str, class: &str, v: f64, n: u64) -> MeshEvent {
    MeshEvent::SignalObserved(SignalObserved {
        device_id: device.into(),
        signal_class: class.into(),
        value: v,
        device_seq: n,
    })
}

fn tier0(id: &str, class: &str) -> Agent {
    Agent {
        id: id.into(),
        tier: 0,
        how: signal_mesh::fold::Reduction::WeightedMean,
        children: vec![],
        signal_class: Some(class.into()),
        correlates: None,
        severity: None,
    }
}

/// Three tier-0 domain agents over ONE workstation, and a tier-1 correlator.
fn cyber_mesh(armed: bool) -> Mesh {
    let agents = vec![
        tier0("net", "network"),
        tier0("endpoint", "endpoint"),
        tier0("identity", "identity"),
        Agent {
            id: "correlator".into(),
            tier: 1,
            how: signal_mesh::fold::Reduction::WeightedMean,
            children: vec!["net".into(), "endpoint".into(), "identity".into()],
            signal_class: None,
            correlates: Some(CorrelationSpec {
                rule: CorrelationRule::Corroboration,
                required_branches: 3,
            }),
            severity: None,
        },
    ];
    Mesh::new("mesh-m11", agents, 100.0).arm_correlation(armed)
}

fn feed(m: &mut Mesh, devices: &[&str]) {
    for d in devices {
        for (i, class) in ["network", "endpoint", "identity"].iter().enumerate() {
            m.log
                .append(sig(d, class, 60.0, i as u64))
                .expect("append signal");
        }
    }
}

#[test]
fn the_correlator_emits_the_union_as_its_population() {
    let mut m = cyber_mesh(true);
    feed(&mut m, &["WS-42"]);
    let head = m.log.head();
    m.cascade(head, &DeterministicReasoner).expect("cascade");

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    let agg = recs
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::AggregateEmitted(a) if a.agent_id == "correlator" => Some(a),
            _ => None,
        })
        .next_back()
        .expect("the correlator emitted");
    eprintln!(
        "AC-M11.6 correlator: value={} input_count={} population_ids={:?}",
        agg.value, agg.input_count, agg.population_ids
    );
    assert_eq!(
        agg.input_count, 1,
        "three branches, one host — the population passed UPWARD is the union"
    );
    assert_eq!(
        agg.population_ids.as_deref(),
        Some(&["WS-42".to_string()][..])
    );

    // ⭐ Control on the same code path: two hosts must give 2, so the `1` above
    // is the union and not a constant.
    let mut m2 = cyber_mesh(true);
    feed(&mut m2, &["WS-42", "WS-43"]);
    let h2 = m2.log.head();
    m2.cascade(h2, &DeterministicReasoner).expect("cascade");
    let recs2 = m2.log.records_up_to(u64::MAX).expect("read");
    let agg2 = recs2
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::AggregateEmitted(a) if a.agent_id == "correlator" => Some(a),
            _ => None,
        })
        .next_back()
        .expect("emitted");
    assert_eq!(agg2.input_count, 2, "two hosts is two");
}

/// ⚠ The OFF test. M10's arming gate shipped without one and the battery
/// reported SURVIVED for a planted defect in the armed path, because nothing
/// pinned the unarmed behaviour.
#[test]
fn an_unarmed_mesh_ignores_the_spec_entirely() {
    let mut m = cyber_mesh(false);
    feed(&mut m, &["WS-42"]);
    let head = m.log.head();
    m.cascade(head, &DeterministicReasoner).expect("cascade");

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r.payload, MeshEvent::CorrelationRefused(_))),
        "an unarmed mesh must not refuse"
    );
    let agg = recs
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::AggregateEmitted(a) if a.agent_id == "correlator" => Some(a),
            _ => None,
        })
        .next_back()
        .expect("still emits, by the ordinary path");
    // The ordinary aggregator sums its children's counts — the pre-M11
    // behaviour, deliberately unchanged while the flag is off.
    eprintln!("AC-M11.7 unarmed input_count={}", agg.input_count);
    assert_eq!(agg.input_count, 3, "unarmed = the old count-summing path");
}

/// ⚠⚠ A refused correlation emits NO aggregate — but it DOES emit a record.
#[test]
fn a_refused_correlation_is_recorded_and_emits_no_aggregate() {
    // Only two of three domains ever report, so `required_branches: 3` refuses.
    let mut m = cyber_mesh(true);
    for (i, class) in ["network", "endpoint"].iter().enumerate() {
        m.log
            .append(sig("WS-42", class, 60.0, i as u64))
            .expect("append");
    }
    let head = m.log.head();
    m.cascade(head, &DeterministicReasoner).expect("cascade");

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    let refusal = recs
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::CorrelationRefused(c) => Some(c),
            _ => None,
        })
        .next_back()
        .expect("the refusal is a first-class record");
    eprintln!(
        "AC-M11.8 refused: reason={} detail={}",
        refusal.reason, refusal.detail
    );
    assert_eq!(refusal.reason, "too_few_branches");
    assert_eq!(refusal.agent_id, "correlator");
    assert!(
        !recs.iter().any(|r| matches!(
            &r.payload,
            MeshEvent::AggregateEmitted(a) if a.agent_id == "correlator"
        )),
        "a refusing correlator must not also emit a value"
    );
    // And the task is recorded as failed, not completed.
    assert!(
        recs.iter().any(|r| matches!(
            &r.payload,
            MeshEvent::TaskTransitioned(t) if t.to_agent == "correlator" && t.state == "failed"
        )),
        "the A2A task must end failed, not silently completed"
    );
}

/// ⚠⚠ The defect my own M11 change introduced and a `debug_assert` caught:
/// `mesh.rs` narrowed `ctx.signals` to a tier-0 agent's own class while leaving
/// `signal_ids` holding EVERY device — so a per-class agent's population would
/// have been the identities of signals it did not reduce.
#[test]
fn a_tier0_class_filter_narrows_identities_alongside_values() {
    let mut m = cyber_mesh(true);
    m.log.append(sig("WS-NET", "network", 60.0, 0)).expect("a");
    m.log.append(sig("WS-EP", "endpoint", 60.0, 0)).expect("b");
    m.log.append(sig("WS-ID", "identity", 60.0, 0)).expect("c");
    let head = m.log.head();
    m.cascade(head, &DeterministicReasoner).expect("cascade");

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    let net = recs
        .iter()
        .filter_map(|r| match &r.payload {
            MeshEvent::AggregateEmitted(a) if a.agent_id == "net" => Some(a),
            _ => None,
        })
        .next_back()
        .expect("net emitted");
    eprintln!("AC-M11.9 net population_ids={:?}", net.population_ids);
    assert_eq!(
        net.population_ids.as_deref(),
        Some(&["WS-NET".to_string()][..]),
        "the network agent's population is the network devices ONLY"
    );
}
