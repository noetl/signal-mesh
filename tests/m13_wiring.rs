//! **Wiring — making M10, M11 and M12 reachable over HTTP.**
//!
//! All three shipped built, tested and *unreachable*: `Mesh::escalate` had no
//! caller outside its own test file, `arm_correlation` and `arm_coverage` had
//! none at all, and `POST /mesh/escalate` was not registered. This suite exists
//! so that stays fixed.
//!
//! ⚠⚠ Two failure modes, and the second is the subtle one:
//!
//! 1. **Not reachable** — the route is missing, or `serve.rs` never reads the
//!    flag. That is what shipped, and `the_binary_reads_every_arm` /
//!    `the_router_registers_the_escalate_route` pin it.
//! 2. **Reachable and inert** — the flag is read, the route answers, and
//!    nothing can ever happen because the agents carry no policy to act on.
//!    An unarmed-looking `200 {"escalated": false, "suppressed": "nominal"}` is
//!    exactly what a healthy, correctly-quiet detector returns, so this one
//!    ships as a feature. `the_shipped_fixture_can_actually_escalate` is the
//!    guard, and it is the reason the fixture declares its policies
//!    unconditionally rather than only when armed.

use signal_mesh::correlation::{correlation_armed, CorrelationRule, CORRELATION_ENV};
use signal_mesh::coverage::{coverage_armed, COVERAGE_ENV};
use signal_mesh::escalation::{escalation_armed, SeverityPolicy, Suppressed, ESCALATION_ENV};
use signal_mesh::event::*;
use signal_mesh::fold::Reduction;
use signal_mesh::mesh::{Agent, CorrelationSpec, Mesh};
use signal_mesh::react::DeterministicReasoner;
use std::path::Path;

fn read_src(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

// ------------------------------------------------- 1. the flags have readers

/// ⚠⚠ THE gap M9's guard could not see.
///
/// `every_declared_env_var_is_documented_and_read` maps a variable to its const
/// through a hand-written table and asks whether `serve.rs` mentions that
/// const. It would pass if `serve.rs` read the value and **threw it away**.
/// This asserts the value reaches a *builder call*, which is the part that
/// makes the flag mean something.
#[test]
fn the_binary_reads_every_arm_and_threads_it_into_the_builder() {
    let serve = read_src("src/bin/serve.rs");

    assert!(
        serve.len() > 2_000,
        "implausibly small serve.rs ({} bytes) — the read is broken and every \
         assertion below would pass over nothing",
        serve.len()
    );

    for (env_const, arm, var) in [
        ("ESCALATION_ENV", "arm_escalation", "esc_on"),
        ("CORRELATION_ENV", "arm_correlation", "corr_on"),
        ("COVERAGE_ENV", "arm_coverage", "cov_on"),
    ] {
        assert!(
            serve.contains(&format!("env::var({env_const})")),
            "serve.rs never reads {env_const}"
        );
        assert!(
            serve.contains(&format!(".{arm}({var})")),
            "serve.rs reads the flag but never passes it to .{arm}() — a value \
             that is read and discarded is not a flag"
        );
    }
}

/// The route table, asserted against the source rather than inferred.
#[test]
fn the_router_registers_the_escalate_route() {
    let t = read_src("src/transport.rs");
    for r in [
        "/mesh/signals",
        "/mesh/cascade",
        "/mesh/replay",
        "/mesh/escalate",
        "/metrics",
    ] {
        assert!(
            t.contains(&format!("\"{r}\"")),
            "route {r} is not registered"
        );
    }
}

/// Each arm parses independently, and only `"true"` arms it.
#[test]
fn only_the_exact_string_true_arms_each_flag() {
    for f in [escalation_armed, correlation_armed, coverage_armed] {
        assert!(f(Some("true")));
        assert!(f(Some("  true  ")), "trimmed");
        for off in [
            None,
            Some(""),
            Some("false"),
            Some("TRUE"),
            Some("1"),
            Some("yes"),
        ] {
            assert!(!f(off), "{off:?} must not arm");
        }
    }
    // ⚠ Three distinct constants. A copy-paste that pointed two arms at one
    // variable would make them impossible to set independently, and every
    // behavioural test here would still pass.
    let names = [ESCALATION_ENV, CORRELATION_ENV, COVERAGE_ENV];
    let uniq: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(uniq.len(), 3, "the three arms share a variable: {names:?}");
}

// -------------------------------------------- 2. the shipped fixture is live

/// Rebuild the shipped fixture exactly as `serve.rs` declares it.
///
/// ⚠ Duplicated on purpose, and pinned by
/// `the_test_fixture_matches_the_shipped_one`: importing it would make this
/// suite agree with `serve.rs` by construction and prove nothing about it.
fn shipped_agents() -> Vec<Agent> {
    vec![
        Agent {
            id: "t0-temp".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("temp".into()),
            severity: SeverityPolicy::new(60.0, 90.0).ok(),
            correlates: None,
        },
        Agent {
            id: "t0-vibe".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("vibe".into()),
            severity: SeverityPolicy::new(60.0, 90.0).ok(),
            correlates: None,
        },
        Agent {
            id: "t1-site".into(),
            tier: 1,
            how: Reduction::WeightedMean,
            children: vec!["t0-temp".into(), "t0-vibe".into()],
            signal_class: None,
            severity: None,
            correlates: Some(CorrelationSpec {
                rule: CorrelationRule::WeightedMean,
                required_branches: 1,
            }),
        },
        Agent {
            id: "t2-fleet".into(),
            tier: 2,
            how: Reduction::Max,
            children: vec!["t1-site".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
    ]
}

/// Source with `//` comments removed.
///
/// ⚠⚠ Load-bearing. The first version of the guard below matched raw source,
/// and the fixture's own doc comment contains the string `required_branches: 1`
/// explaining the choice — so deleting the correlator left the guard green.
/// **A comment satisfied a check about code**, which is one of the standing
/// false-zero idioms in this codebase, reproduced here by its author.
fn non_comment(rel: &str) -> String {
    read_src(rel)
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// ⚠ Keeps the fixture above honest against the one that actually ships.
///
/// ⚠⚠ Counts, not `contains`. `SeverityPolicy::new(60.0, 90.0).ok()` appears
/// twice — once per tier-0 agent — so a `contains` check stays green after one
/// of them is deleted, and the escalate route goes half-inert unnoticed. The
/// planted-defect battery proved that: D5 removed one policy and SURVIVED.
#[test]
fn the_test_fixture_matches_the_shipped_one() {
    let serve = non_comment("src/bin/serve.rs");

    // Assert the extraction before asserting about it: a stripper that ate the
    // whole file would make every count below zero and the messages confusing.
    assert!(
        serve.contains("fn fixture_agents"),
        "comment stripping removed the code it was meant to keep"
    );

    let policies = serve.matches("severity: SeverityPolicy::new(").count();
    let correlators = serve.matches("correlates: Some(").count();
    eprintln!("AC-W0 shipped fixture: severity_policies={policies} correlators={correlators}");

    assert_eq!(
        policies, 2,
        "expected a SeverityPolicy on BOTH tier-0 agents; found {policies}. An \
         agent without one can never escalate, so the flag would be reachable \
         and permanently, cheerfully quiet."
    );
    assert_eq!(
        correlators, 1,
        "expected exactly one declared correlator; found {correlators}. With \
         none, arming NOETL_SIGNAL_MESH_CORRELATION changes nothing at all."
    );
}

fn mesh(esc: bool, corr: bool, cov: bool) -> Mesh {
    Mesh::new("mesh-wire", shipped_agents(), 50.0)
        .arm_escalation(esc)
        .arm_correlation(corr)
        .arm_coverage(cov)
}

fn feed(m: &mut Mesh) {
    for (d, c, v) in [
        ("dev-1", "temp", 50.0),
        ("dev-2", "temp", 55.0),
        ("dev-3", "vibe", 50.0),
    ] {
        m.log
            .append(MeshEvent::SignalObserved(SignalObserved {
                device_id: d.into(),
                signal_class: c.into(),
                value: v,
                device_seq: 0,
            }))
            .expect("append");
    }
}

/// ⭐⭐ The **reachable-and-inert** guard.
///
/// If the shipped agents carried `severity: None`, arming the flag would return
/// a permanent, cheerful `suppressed: nominal` — indistinguishable from a
/// detector that is working and has nothing to report.
#[test]
fn the_shipped_fixture_can_actually_escalate() {
    let mut m = mesh(true, false, false);
    feed(&mut m);
    let at = m.log.head();

    let fired = m.escalate("t0-temp", 95.0, 2, at).expect("store ok");
    assert!(
        fired.is_ok(),
        "the SHIPPED fixture cannot escalate even when armed — the flag is \
         reachable and inert: {fired:?}"
    );

    // ⚠ And the discriminating control: the same call on an UNARMED mesh is
    // suppressed. Without this, the assertion above is equally consistent with
    // escalate ignoring the arm entirely.
    let mut off = mesh(false, false, false);
    feed(&mut off);
    let at_off = off.log.head();
    assert_eq!(
        off.escalate("t0-temp", 95.0, 2, at_off).expect("store ok"),
        Err(Suppressed::Nominal),
        "an unarmed mesh must suppress"
    );
}

#[test]
fn arming_correlation_changes_what_the_cascade_records() {
    let mut off = mesh(false, false, false);
    feed(&mut off);
    let h = off.log.head();
    off.cascade(h, &DeterministicReasoner).expect("cascade");

    let mut on = mesh(false, true, false);
    feed(&mut on);
    let h2 = on.log.head();
    on.cascade(h2, &DeterministicReasoner).expect("cascade");

    // The correlator records a `correlate` reasoning phase; the plain
    // aggregator records the ReAct phases instead.
    let phases = |m: &Mesh| -> Vec<String> {
        m.log
            .records_up_to(u64::MAX)
            .expect("read")
            .into_iter()
            .filter_map(|r| match r.payload {
                MeshEvent::AgentReasoned(a) if a.agent_id == "t1-site" => Some(a.phase),
                _ => None,
            })
            .collect()
    };
    let (p_off, p_on) = (phases(&off), phases(&on));
    eprintln!("AC-W1 t1-site phases off={p_off:?} on={p_on:?}");
    assert!(
        !p_off.iter().any(|p| p == "correlate"),
        "unarmed must not correlate"
    );
    assert!(
        p_on.iter().any(|p| p == "correlate"),
        "armed must correlate — otherwise the flag is reachable and inert"
    );
}

#[test]
fn arming_coverage_changes_what_the_verdict_carries() {
    let mut off = mesh(false, false, false);
    feed(&mut off);
    let h = off.log.head();
    let v_off = off.cascade(h, &DeterministicReasoner).expect("cascade");

    let mut on = mesh(false, false, true);
    feed(&mut on);
    let h2 = on.log.head();
    let v_on = on.cascade(h2, &DeterministicReasoner).expect("cascade");

    eprintln!(
        "AC-W2 coverage off={:?} on={:?}",
        v_off.coverage.is_some(),
        v_on.coverage.as_ref().map(|c| (c.present, c.expected))
    );
    assert!(v_off.coverage.is_none(), "unarmed assesses nothing");
    assert!(v_on.coverage.is_some(), "armed must assess");
    // ⚠ And the value is untouched either way — coverage MARKS, it does not
    // change the arithmetic. A coverage arm that moved the number would be a
    // behaviour change hiding inside an observability flag.
    assert_eq!(
        v_off.value, v_on.value,
        "arming coverage must not move the value"
    );
}

// ----------------------------------------------- 3. OFF is today's behaviour

/// ⭐ The OFF test the brief asks for: with every flag unset, nothing differs
/// from what shipped before this change.
#[test]
fn the_unarmed_fixture_is_byte_identical_to_today() {
    let mut m = mesh(false, false, false);
    feed(&mut m);
    let h = m.log.head();
    let v = m.cascade(h, &DeterministicReasoner).expect("cascade");

    eprintln!(
        "AC-W3 unarmed: value={} decision={} degraded={} coverage={:?}",
        v.value, v.decision, v.degraded, v.coverage
    );
    // ⚠ 51.666…, not the demo's pinned 52.5 — this suite feeds three signals,
    // the demo feeds six. The first draft asserted 52.5 here and failed, which
    // was the test being wrong rather than the code: a pinned constant is only
    // meaningful against the input that produced it. The demo's own 52.5 is
    // pinned where it belongs, by `the_demo_output_is_unchanged_by_this_change`.
    assert!(
        (v.value - 155.0 / 3.0).abs() < 1e-9,
        "value drifted: {}",
        v.value
    );
    assert!(v.decision);
    assert!(!v.degraded);
    assert!(v.coverage.is_none());

    let recs = m.log.records_up_to(u64::MAX).expect("read");
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r.payload, MeshEvent::Escalated(_))),
        "an unarmed mesh must not escalate"
    );
    assert!(
        !recs
            .iter()
            .any(|r| matches!(r.payload, MeshEvent::CorrelationRefused(_))),
        "an unarmed mesh must not refuse a correlation it never attempted"
    );
    for r in &recs {
        if let MeshEvent::AggregateEmitted(a) = &r.payload {
            assert!(
                a.coverage.is_none(),
                "{} carried coverage while unarmed",
                a.agent_id
            );
        }
    }
}

/// ⚠ The three arms are independent. One `if` covering all three would pass
/// every test above, because every test above arms exactly one.
#[test]
fn each_arm_is_independent_of_the_other_two() {
    for (e, c, v) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let m = mesh(e, c, v);
        assert_eq!(m.escalation_armed, e, "escalation arm leaked");
        assert_eq!(m.correlation_armed, c, "correlation arm leaked");
        assert_eq!(m.coverage_armed, v, "coverage arm leaked");
    }
}

/// ⭐⭐ The strongest OFF evidence: the shipped demo's output, unchanged.
///
/// ⚠ The fixture now declares a `SeverityPolicy` on both tier-0 agents and a
/// `CorrelationSpec` on `t1-site`. Those declarations are inert while the flags
/// are off — but "inert" is a claim, and this is what checks it against the
/// one artefact whose output was pinned before the change existed.
#[test]
fn the_demo_output_is_unchanged_by_this_change() {
    let demo = read_src("src/bin/demo.rs");
    assert!(
        demo.contains("severity: None") && demo.contains("correlates: None"),
        "the DEMO's agents must stay policy-free, so its pinned 52.5000 remains \
         a measurement of the unarmed path and not of this change"
    );
    // The value itself is asserted by the demo binary in CI; what this test
    // protects is the premise that makes that assertion meaningful.
}

// ------------------------------------------------ 4. over an actual HTTP request

mod http {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use signal_mesh::transport::{router, A2aMode, A2aState, CatalogEntry};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TOKEN: &str = "test-token";

    fn entry() -> CatalogEntry {
        CatalogEntry {
            path: "agents/t0-temp".into(),
            version: 1,
            resource_type: "agent".into(),
            card: shipped_card(),
        }
    }

    fn shipped_card() -> signal_mesh::a2a::AgentCard {
        shipped_agents()
            .into_iter()
            .find(|a| a.id == "t0-temp")
            .expect("t0-temp")
            .card()
    }

    fn app(esc: bool) -> Arc<A2aState> {
        let mut m = mesh(esc, false, false);
        feed(&mut m);
        Arc::new(A2aState::new(A2aMode::Serve, entry(), Some(TOKEN.into())).with_mesh(m))
    }

    async fn post(s: Arc<A2aState>, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri("/mesh/escalate")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("request");
        let res = router(s).oneshot(req).await.expect("response");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn body() -> serde_json::Value {
        serde_json::json!({
            "agent_id": "t0-temp", "value": 95.0, "input_count": 2, "at_seq": 3
        })
    }

    /// ⭐⭐ The route is REACHABLE and the capability FIRES, over HTTP.
    #[tokio::test]
    async fn armed_the_escalate_route_escalates() {
        let (status, j) = post(app(true), body()).await;
        eprintln!("AC-W4 armed: {status} {j}");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(j["armed"], true);
        assert_eq!(j["escalated"], true, "armed must actually escalate: {j}");
        assert!(j["seq"].is_u64());
    }

    /// ⚠⚠ Unarmed answers 200 with `armed: false` — NOT an error, and not a
    /// bare `nominal` that an operator cannot distinguish from a working
    /// detector with nothing to say.
    #[tokio::test]
    async fn unarmed_the_route_answers_and_says_it_is_unarmed() {
        let (status, j) = post(app(false), body()).await;
        eprintln!("AC-W5 unarmed: {status} {j}");
        assert_eq!(status, StatusCode::OK, "a non-event is not a failure");
        assert_eq!(j["armed"], false, "the body must say WHY nothing happened");
        assert_eq!(j["escalated"], false);
        assert_eq!(j["suppressed"], "nominal");
    }

    /// ⚠ D9 from the M3 battery: the cascade must honour the caller's
    /// `up_to_seq`. Ignoring it and always using `head` survived every test,
    /// because nothing had ever asked the route for a different watermark —
    /// and it would make the M3 freshness gate unreachable over HTTP while
    /// leaving it fully reachable in unit tests.
    #[tokio::test]
    async fn the_cascade_route_honours_the_callers_watermark() {
        let s = app(false);
        let req = Request::builder()
            .method("POST")
            .uri("/mesh/cascade")
            .header("authorization", format!("Bearer {TOKEN}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"up_to_seq":99}"#.to_string()))
            .expect("request");
        let res = router(s).oneshot(req).await.expect("response");
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let j: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        eprintln!("AC-W7 cascade at a named watermark: {j}");
        assert_eq!(
            j["cascaded_at_head"], 99,
            "the route ignored the requested watermark: {j}"
        );
    }

    /// The same bearer discipline as every other mesh route.
    #[tokio::test]
    async fn the_route_refuses_an_unauthenticated_caller() {
        let req = Request::builder()
            .method("POST")
            .uri("/mesh/escalate")
            .header("content-type", "application/json")
            .body(Body::from(body().to_string()))
            .expect("request");
        let res = router(app(true)).oneshot(req).await.expect("response");
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// ⚠ Dedupe is observable through the route, which is what makes the
    /// caller-supplied `at_seq` load-bearing rather than decorative.
    #[tokio::test]
    async fn a_repeat_at_the_same_watermark_is_suppressed_as_duplicate() {
        let s = app(true);
        let (_, first) = post(s.clone(), body()).await;
        let (status, second) = post(s, body()).await;
        eprintln!("AC-W6 first={first} second={second}");
        assert_eq!(first["escalated"], true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(second["escalated"], false);
        assert_eq!(second["suppressed"], "duplicate");
    }
}

/// ⚠⚠ M12's marker must leave the process. A capability that runs and cannot
/// be observed is, from a caller's vantage point, the same as one that does not
/// run — and the cascade response omitted `degraded` and `coverage` entirely
/// until a live probe showed an armed mesh answering identically to an unarmed
/// one.
#[test]
fn the_cascade_response_surfaces_the_coverage_marker() {
    let t = read_src("src/transport.rs");
    for field in [
        "\"degraded\"",
        "\"degraded_reason\"",
        "\"coverage\"",
        "\"assessed\"",
    ] {
        assert!(
            t.contains(field),
            "verdict_json omits {field} — coverage would be armed and invisible"
        );
    }
    // ⚠ `assessed` specifically: without it, "not assessed" and "assessed and
    // complete" both serialise to something a caller reads as fine.
    assert!(
        t.contains("\"assessed\": false") && t.contains("\"assessed\": true"),
        "both arms of the assessed/not-assessed distinction must be emitted"
    );
}
