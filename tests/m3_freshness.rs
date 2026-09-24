//! **M3 — bounded-staleness reads, wired.**
//!
//! `fold::admits` shipped with M1: implemented, documented, six tests, and
//! **zero production callers**. An inert gate — the same class as M10/M11/M12
//! before the wiring increment, and the class this repo keeps re-finding.
//!
//! ⚠⚠ Wiring it is not enough on its own. The cascade always read at
//! `log.head()`, where the staleness is 0 *by construction*, so a gate called
//! there can never refuse and is indistinguishable from a gate that is not
//! there. AC3 exists for exactly that: **a gate that has never refused is
//! indistinguishable from one that cannot.** `POST /mesh/cascade` therefore
//! accepts a watermark, and these tests drive it past the head.

use ehdb_core::plan::ReadConsistency;
use signal_mesh::escalation::SeverityPolicy;
use signal_mesh::event::*;
use signal_mesh::fold::{FreshnessRefusal, Reduction};
use signal_mesh::mesh::{Agent, Mesh, MeshError};
use signal_mesh::react::DeterministicReasoner;

fn agents() -> Vec<Agent> {
    vec![
        Agent {
            id: "t0".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("temp".into()),
            severity: SeverityPolicy::new(60.0, 90.0).ok(),
            correlates: None,
        },
        Agent {
            id: "t1".into(),
            tier: 1,
            how: Reduction::Max,
            children: vec!["t0".into()],
            signal_class: None,
            severity: None,
            correlates: None,
        },
    ]
}

fn mesh(armed: bool, policy: ReadConsistency, rate: u64) -> Mesh {
    let mut m = Mesh::new("mesh-m3", agents(), 50.0).arm_freshness(armed, policy, rate);
    for (d, v) in [("d1", 50.0), ("d2", 60.0)] {
        m.log
            .append(MeshEvent::SignalObserved(SignalObserved {
                device_id: d.into(),
                signal_class: "temp".into(),
                value: v,
                device_seq: 0,
            }))
            .expect("append");
    }
    m
}

/// How far past the head to read. The fold then reports a real staleness.
fn beyond(m: &Mesh, n: u64) -> u64 {
    m.log.head() + n
}

// ------------------------------------------------------------------- AC2

/// ⭐⭐ **AC2** — a tier reading at a watermark the log has not reached
/// **refuses**, rather than reducing a short prefix.
#[test]
fn a_read_past_the_head_refuses_instead_of_reducing_a_short_prefix() {
    let mut m = mesh(true, ReadConsistency::Strong, 1);
    let at = beyond(&m, 5);
    let before = m.log.record_count().expect("count");

    let err = m
        .cascade(at, &DeterministicReasoner)
        .expect_err("must refuse");
    eprintln!("AC-M3.1 refusal at {at} (head {}): {err}", m.log.head());
    assert!(
        matches!(
            err,
            MeshError::Freshness(FreshnessRefusal::TooStale {
                staleness: 5,
                allowed: 0
            })
        ),
        "expected TooStale{{5,0}}, got {err:?}"
    );

    // ⚠⚠ And NOTHING was written. "Refuses rather than reduces" is a claim
    // about side effects, not about the return value: a gate checked after the
    // reduction would return this same error with the short answer already
    // appended, and the assertion above could not tell the difference.
    assert_eq!(
        m.log.record_count().expect("count"),
        before,
        "a refused cascade must append no events"
    );
}

/// The control: the same mesh, reading at the head, proceeds normally.
#[test]
fn the_same_mesh_at_the_head_cascades_normally() {
    let mut m = mesh(true, ReadConsistency::Strong, 1);
    let at = m.log.head();
    let v = m.cascade(at, &DeterministicReasoner).expect("must admit");
    eprintln!(
        "AC-M3.2 at head: value={} events={}",
        v.value, v.events_appended
    );
    assert!(v.events_appended > 0);
}

// ------------------------------------------------------------------- AC3

/// ⭐⭐ **AC3, the positive control.** A lagged read produces a refusal, and the
/// *same* read under `Strong` produces a **different, also-correct** refusal.
///
/// ⚠ Two policies that refuse identically would be one policy wearing two
/// names — the assertion is that they disagree in the right direction, not
/// merely that both say no.
#[test]
fn bounded_and_strong_refuse_the_same_lagged_read_differently() {
    let lag = 5;

    // Strong: no budget at all. Refuses with allowed = 0.
    let mut strong = mesh(true, ReadConsistency::Strong, 1);
    let e_strong = strong
        .cascade(beyond(&strong, lag), &DeterministicReasoner)
        .expect_err("strong refuses");

    // Bounded with a 3ms budget at 1 seq/ms = 3 sequence units. 5 > 3.
    let mut bounded = mesh(
        true,
        ReadConsistency::Bounded {
            max_staleness_millis: 3,
        },
        1,
    );
    let e_bounded = bounded
        .cascade(beyond(&bounded, lag), &DeterministicReasoner)
        .expect_err("bounded refuses");

    eprintln!("AC-M3.3 strong={e_strong}  bounded={e_bounded}");
    assert!(matches!(
        e_strong,
        MeshError::Freshness(FreshnessRefusal::TooStale { allowed: 0, .. })
    ));
    assert!(matches!(
        e_bounded,
        MeshError::Freshness(FreshnessRefusal::TooStale { allowed: 3, .. })
    ));
    assert_ne!(
        format!("{e_strong}"),
        format!("{e_bounded}"),
        "two policies that refuse identically are one policy with two names"
    );

    // ⭐ And the other half of the control: the SAME bounded policy ADMITS a
    // lag inside its budget. A gate that refuses everything is as useless as
    // one that refuses nothing.
    let mut ok = mesh(
        true,
        ReadConsistency::Bounded {
            max_staleness_millis: 8,
        },
        1,
    );
    let at = beyond(&ok, lag);
    let v = ok
        .cascade(at, &DeterministicReasoner)
        .expect("8 >= 5 admits");
    eprintln!("AC-M3.4 bounded(8) admitted lag {lag}: value={}", v.value);
}

/// The exchange rate is load-bearing, not decoration.
#[test]
fn the_seq_per_milli_rate_changes_the_verdict_on_the_same_read() {
    let policy = ReadConsistency::Bounded {
        max_staleness_millis: 3,
    };
    // 3ms x 1 seq/ms = 3 < 5 -> refuse
    let mut slow = mesh(true, policy, 1);
    assert!(slow
        .cascade(beyond(&slow, 5), &DeterministicReasoner)
        .is_err());
    // 3ms x 2 seq/ms = 6 >= 5 -> admit
    let mut fast = mesh(true, policy, 2);
    assert!(fast
        .cascade(beyond(&fast, 5), &DeterministicReasoner)
        .is_ok());
}

/// `Exact` is refused rather than approximated — this POC has no clock.
#[test]
fn exact_is_refused_rather_than_approximated() {
    let mut m = mesh(true, ReadConsistency::Exact { at_millis: 1 }, 1);
    let e = m
        .cascade(m.log.head(), &DeterministicReasoner)
        .expect_err("no clock");
    assert!(matches!(
        e,
        MeshError::Freshness(FreshnessRefusal::ExactUnsupported)
    ));
}

// ------------------------------------------------------------- the OFF test

/// ⭐ Unarmed, the gate is not consulted at all — today's behaviour, including
/// for a read the armed gate would refuse.
#[test]
fn an_unarmed_mesh_reduces_a_short_prefix_exactly_as_before() {
    let mut m = mesh(false, ReadConsistency::Strong, 1);
    let at = beyond(&m, 5);
    let v = m
        .cascade(at, &DeterministicReasoner)
        .expect("unarmed must not refuse");
    eprintln!(
        "AC-M3.5 unarmed past head: value={} up_to={}",
        v.value, v.up_to_seq
    );
    assert!(v.events_appended > 0, "it still cascaded");

    // ⚠ The discriminating control: the identical read, armed, refuses. Without
    // it, "unarmed did not refuse" is equally consistent with the gate being
    // broken in the on position.
    let mut armed = mesh(true, ReadConsistency::Strong, 1);
    assert!(armed
        .cascade(beyond(&armed, 5), &DeterministicReasoner)
        .is_err());
}

/// ⚠ Default construction is unarmed and Strong — the rollback posture.
#[test]
fn the_default_posture_is_unarmed_and_strong() {
    let m = Mesh::new("d", agents(), 50.0);
    assert!(!m.freshness_armed);
    assert_eq!(m.read_consistency, ReadConsistency::Strong);
    assert_eq!(m.seq_per_milli, 1);
}

/// The policy is ehdb's, read through ehdb — not re-parsed here.
#[test]
fn the_policy_comes_from_ehdbs_own_reader() {
    let serve = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/serve.rs"),
    )
    .expect("serve.rs");
    assert!(
        serve.contains("read_consistency_from_env()"),
        "serve.rs must call ehdb's reader — one name for one concept"
    );
    // ⚠ The NAME may appear in an operator-facing error message — that is the
    // point of the message. What must not appear is a second READ of it, which
    // is what "one name for one concept" actually forbids.
    assert!(
        !serve.contains("env::var(\"NOETL_EHDB"),
        "serve.rs reads an ehdb variable directly instead of going through \
         ehdb's own reader; that is a second place for it to be spelled"
    );
    // ⚠⚠ And it must FAIL CLOSED. Asserting the message alone is not enough:
    // a planted defect that kept `refusing to start` and replaced the exit with
    // a Strong fallback SURVIVED this check. The message is not the behaviour.
    // Anchor on what FOLLOWS the message instead.
    let i = serve
        .find("refusing to start")
        .expect("a malformed policy must be reported");
    let arm = &serve[i..(i + 300).min(serve.len())];
    assert!(
        arm.contains("process::exit"),
        "the malformed-policy arm reports and then CONTINUES; it must stop the \
         binary, or an operator's typo silently becomes `strong`:\n{arm}"
    );
}

/// ⚠ D8 from the battery: `serve.rs` must thread the FLAG, not a literal.
/// Replacing `fresh_on` with `false` survived every behavioural test here,
/// because they all construct the mesh directly and never go through `main`.
#[test]
fn the_binary_threads_the_freshness_flag_not_a_literal() {
    let serve = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/serve.rs"),
    )
    .expect("serve.rs");
    assert!(
        serve.contains("env::var(FRESHNESS_ENV)"),
        "serve.rs never reads the freshness flag"
    );
    assert!(
        serve.contains(".arm_freshness(fresh_on, policy, seq_per_milli)"),
        "serve.rs reads the flag but does not pass it to .arm_freshness() — a \
         value that is read and discarded is not a flag"
    );
}
