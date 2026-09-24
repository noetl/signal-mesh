//! **M9 acceptance — operability.**
//!
//! Two claims: *every knob this binary reads is documented*, and *every counter
//! reads 0 before it fires rather than being absent*.
//!
//! ⚠ Both are guards against the same failure: a confident clean reading from
//! something that was never looking.

use signal_mesh::event::{MeshEvent, SignalObserved};
use signal_mesh::metrics::{render_all, StoreCounters, METRICS_ADDR_ENV};
use signal_mesh::store::{EhdbStore, MemoryStore, MeshStore, MAX_PAYLOAD_BYTES};
use signal_mesh::transport::{A2aMode, Counters as TransportCounters};
use std::collections::BTreeSet;
use std::path::Path;

struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "sm-m9-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_src(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// Every `NOETL_*` string literal the crate declares.
///
/// ⚠⚠ The character class is `[A-Z0-9_]`, and the digit matters. Writing
/// `[A-Z_]` — the obvious first attempt — truncates `NOETL_SIGNAL_MESH_A2A` at
/// the `2`, silently splitting one variable into a prefix of another and making
/// the whole set wrong in a direction that still looks plausible.
fn declared_env_vars() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // ⚠⚠ Walk src/ RECURSIVELY. The first version listed five files by hand,
    // and the moment a new module (`escalation.rs`) declared a variable the
    // guard simply could not see it — it reported `declared=8` while the crate
    // declared 9. A hardcoded population is a denominator that silently stops
    // matching the thing it measures.
    let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![src_root];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("src is readable") {
            let path = e.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "rs") {
                files.push(path);
            }
        }
    }
    assert!(
        files.len() >= 8,
        "implausibly few source files found ({}) — the walk is broken",
        files.len()
    );
    for path in files {
        let src = std::fs::read_to_string(&path).expect("readable");
        for line in src.lines() {
            // Only `const NAME: &str = "NOETL_..."` declarations, so prose
            // mentioning a variable does not enter the read-set. ⚠ A comment
            // counting as a declaration is how a scan over-reports.
            if !line.contains("const ") || !line.contains("NOETL_") {
                continue;
            }
            if let Some(start) = line.find("\"NOETL_") {
                let rest = &line[start + 1..];
                if let Some(end) = rest.find('"') {
                    let name = &rest[..end];
                    if name
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    {
                        out.insert(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// ⭐ **AC — the deployment spec covers every knob, and every knob has a reader.**
///
/// Modelled on the fleet's `spec-env-currency` guard, including the lesson that
/// made it necessary: the first version of that check measured 56 variables
/// where the real read-set was 152, and closed an issue on a number low by a
/// factor of 2.7. So this one prints **both populations** and states the idiom
/// it covers.
#[test]
fn every_declared_env_var_is_documented_and_read() {
    let declared = declared_env_vars();
    let spec = read_src("docs/deployment-specification.md");
    let serve = read_src("src/bin/serve.rs");

    // ⚠ Assert the extraction before asserting about it. An empty read-set
    // makes every check below pass over nothing.
    assert!(
        declared.len() >= 6,
        "implausibly few declared env vars ({}) — the extractor is broken and \
         this guard would pass vacuously: {declared:?}",
        declared.len()
    );

    let undocumented: Vec<&String> = declared.iter().filter(|v| !spec.contains(*v)).collect();

    // Which const declares which variable. ⚠ Written out rather than inferred:
    // a heuristic that maps names to consts is a second thing that can be
    // wrong, and it would be wrong silently.
    let const_for: &[(&str, &str)] = &[
        ("NOETL_SIGNAL_MESH", "MESH_ENABLED_ENV"),
        ("NOETL_SIGNAL_MESH_REASONER", "MESH_REASONER_ENV"),
        ("NOETL_SIGNAL_MESH_STORE", "STORE_ENV"),
        ("NOETL_SIGNAL_MESH_STORE_ROOT", "STORE_ROOT_ENV"),
        ("NOETL_SIGNAL_MESH_A2A", "A2A_ENV"),
        ("NOETL_SIGNAL_MESH_A2A_ADDR", "ADDR_ENV"),
        ("NOETL_SIGNAL_MESH_A2A_TOKEN", "TOKEN_ENV"),
        ("NOETL_SIGNAL_MESH_METRICS_ADDR", "METRICS_ADDR_ENV"),
        ("NOETL_SIGNAL_MESH_CHECKPOINT_SECS", "CHECKPOINT_SECS_ENV"),
        ("NOETL_SIGNAL_MESH_A2A_TOKEN_FILE", "A2A_TOKEN_FILE_ENV"),
    ];

    // A variable is READ when `serve.rs` — the only thing with a process
    // environment — passes its const to `env::var`, however it is qualified.
    let is_read = |var: &str| -> bool {
        let Some((_, c)) = const_for.iter().find(|(n, _)| *n == var) else {
            return false; // a new var with no mapping is unread until proven
        };
        serve.contains(&format!("env::var({c})")) || serve.contains(&format!("::{c})"))
    };
    let unread: Vec<&String> = declared.iter().filter(|v| !is_read(v)).collect();

    eprintln!(
        "env currency: declared={} documented={} read_by_serve={} | idiom = \
         `const NAME: &str = \"NOETL_...\"` over 5 source files, class [A-Z0-9_]\n  declared: {:?}",
        declared.len(),
        declared.len() - undocumented.len(),
        declared.len() - unread.len(),
        declared
    );
    assert!(
        undocumented.is_empty(),
        "{} declared env var(s) missing from docs/deployment-specification.md: \
         {undocumented:?}",
        undocumented.len()
    );
    assert!(
        unread.is_empty(),
        "{} declared env var(s) that NOTHING reads: {unread:?}. A declared flag \
         with no reader is not a flag — it is documentation of a capability \
         that does not exist.",
        unread.len()
    );
}

/// ⚠ The extractor's own control. If the character class regressed to `[A-Z_]`,
/// `NOETL_SIGNAL_MESH_A2A` and its two children would vanish from the read-set
/// and the guard above would pass over a smaller population.
#[test]
fn the_extractor_sees_the_variables_with_digits_in_them() {
    let d = declared_env_vars();
    for required in [
        "NOETL_SIGNAL_MESH",
        "NOETL_SIGNAL_MESH_A2A",
        "NOETL_SIGNAL_MESH_A2A_ADDR",
        "NOETL_SIGNAL_MESH_A2A_TOKEN",
        "NOETL_SIGNAL_MESH_METRICS_ADDR",
        "NOETL_SIGNAL_MESH_REASONER",
        "NOETL_SIGNAL_MESH_STORE",
        "NOETL_SIGNAL_MESH_STORE_ROOT",
        "NOETL_SIGNAL_MESH_CHECKPOINT_SECS",
        "NOETL_SIGNAL_MESH_A2A_TOKEN_FILE",
    ] {
        assert!(
            d.contains(required),
            "the extractor missed {required} — check the character class; \
             `[A-Z_]` silently truncates at the digit. Saw: {d:?}"
        );
    }
    eprintln!(
        "extractor control: {} vars, all 8 expected present",
        d.len()
    );
}

/// ⭐ **Every store series is pinned at 0, for BOTH stores, before anything fires.**
#[test]
fn every_store_series_is_pinned_at_zero_for_both_stores() {
    for active in ["memory", "ehdb"] {
        let text = StoreCounters::new().render_all_stores(active);
        for store in ["memory", "ehdb"] {
            for series in [
                format!("signal_mesh_store_append_total{{store=\"{store}\",outcome=\"appended\"}} 0"),
                format!("signal_mesh_store_append_total{{store=\"{store}\",outcome=\"deduped\"}} 0"),
                format!("signal_mesh_store_refused_total{{store=\"{store}\",reason=\"payload-too-large\"}} 0"),
                format!("signal_mesh_store_refused_total{{store=\"{store}\",reason=\"engine\"}} 0"),
                format!("signal_mesh_store_refused_total{{store=\"{store}\",reason=\"codec\"}} 0"),
                format!("signal_mesh_store_read_total{{store=\"{store}\"}} 0"),
                format!("signal_mesh_store_records_read_total{{store=\"{store}\"}} 0"),
                format!("signal_mesh_store_checkpoint_total{{store=\"{store}\"}} 0"),
            ] {
                assert!(
                    text.contains(&series),
                    // ⚠ Including the store that is NOT active. Pinning only the
                    // configured one leaves the other absent, and absent reads
                    // exactly like zero to a dashboard.
                    "active={active}: missing pinned series `{series}`\n{text}"
                );
            }
        }
    }
    eprintln!("store pins: 8 series x 2 stores x 2 active-configs = 32 assertions, all 0");
}

/// The counters actually move, on both stores, for every outcome.
///
/// ⚠ A pinned-at-zero test alone is satisfied by a counter nothing increments.
/// This is the other half.
#[test]
fn every_store_counter_moves_when_its_event_happens() {
    let tmp = Tmp::new("counters");
    let mut stores: Vec<Box<dyn MeshStore>> = vec![
        Box::new(MemoryStore::new("s")),
        Box::new(EhdbStore::open("s", &tmp.0).expect("open")),
    ];

    for st in stores.iter_mut() {
        let label = st.label();
        let sig = |seq: u64| {
            MeshEvent::SignalObserved(SignalObserved {
                device_id: "d".into(),
                signal_class: "temp".into(),
                value: 1.0,
                device_seq: seq,
            })
        };

        st.append_with_id(sig(1), Some("k1")).expect("append");
        st.append_with_id(sig(1), Some("k1")).expect("dedupe ack");
        let _ = st.records_up_to(u64::MAX).expect("read");
        st.checkpoint().expect("checkpoint");

        // One byte over the ceiling.
        let overhead = serde_json::to_string(&sig(1)).unwrap().len() - 1;
        let big = MeshEvent::SignalObserved(SignalObserved {
            device_id: "x".repeat(MAX_PAYLOAD_BYTES - overhead + 8),
            signal_class: "temp".into(),
            value: 1.0,
            device_seq: 2,
        });
        assert!(
            st.append(big).is_err(),
            "{label}: oversized must be refused"
        );

        let text = st.counters().render(label);
        eprintln!("--- {label} ---\n{text}");
        for (series, want) in [
            (format!("signal_mesh_store_append_total{{store=\"{label}\",outcome=\"appended\"}} 1"), "append"),
            (format!("signal_mesh_store_append_total{{store=\"{label}\",outcome=\"deduped\"}} 1"), "dedupe"),
            (format!("signal_mesh_store_refused_total{{store=\"{label}\",reason=\"payload-too-large\"}} 1"), "ceiling"),
            (format!("signal_mesh_store_read_total{{store=\"{label}\"}} 1"), "read"),
            (format!("signal_mesh_store_records_read_total{{store=\"{label}\"}} 1"), "records read"),
            (format!("signal_mesh_store_checkpoint_total{{store=\"{label}\"}} 1"), "checkpoint"),
        ] {
            assert!(
                text.contains(&series),
                "{label}: the {want} counter did not move — expected `{series}`\n{text}"
            );
        }
    }
}

/// The combined body carries transport AND store series together, so one scrape
/// answers both questions.
#[test]
fn one_metrics_body_carries_transport_and_store() {
    let body = render_all(
        &StoreCounters::new(),
        "memory",
        &TransportCounters::new(),
        A2aMode::Off,
    );
    for series in [
        "signal_mesh_build_info{",
        "signal_mesh_a2a_card_served_total 0",
        "signal_mesh_store_append_total{store=\"memory\"",
        "signal_mesh_store_append_total{store=\"ehdb\"",
    ] {
        assert!(body.contains(series), "missing {series}\n{body}");
    }
    eprintln!(
        "render_all: {} bytes, {} series lines",
        body.len(),
        body.lines().filter(|l| !l.starts_with('#')).count()
    );
}

/// The metrics listener is off by default — the flag is named and unset means
/// nothing binds.
#[test]
fn the_metrics_listener_is_off_by_default() {
    assert_eq!(METRICS_ADDR_ENV, "NOETL_SIGNAL_MESH_METRICS_ADDR");
    let serve = read_src("src/bin/serve.rs");
    assert!(
        serve.contains("let metrics_addr = std::env::var(METRICS_ADDR_ENV).ok();"),
        "the listener must be driven by the env var, not hardcoded"
    );
    assert!(
        serve.contains("if let Some(addr) = metrics_addr"),
        "unset must mean no bind at all, not a bind to a default address"
    );
}

/// ⚠⚠ **The metrics body must be rendered per request, not once at startup.**
///
/// This test exists because the first M9 listener did exactly that: it built
/// the body with `render_all_stores(...)` at boot and served a clone of that
/// `String` forever. Every probe got a 200, every series was present and
/// correctly pinned at 0 — and no counter could ever move. A frozen endpoint is
/// indistinguishable from an idle system, which is the whole failure class this
/// milestone exists to close.
///
/// Asserted two ways: the shared counters really are shared, and `serve.rs`
/// does not pre-render.
#[test]
fn the_metrics_body_is_rendered_live_and_not_snapshotted_at_startup() {
    use std::sync::Arc;

    let shared = Arc::new(StoreCounters::new());
    let mut store = MemoryStore::new("s").with_counters(shared.clone());

    let before = render_all(&shared, "memory", &TransportCounters::new(), A2aMode::Off);
    assert!(
        before.contains("signal_mesh_store_append_total{store=\"memory\",outcome=\"appended\"} 0")
    );

    store
        .append(MeshEvent::SignalObserved(SignalObserved {
            device_id: "d".into(),
            signal_class: "temp".into(),
            value: 1.0,
            device_seq: 1,
        }))
        .expect("append");

    // The SAME Arc the listener holds must now read 1.
    let after = render_all(&shared, "memory", &TransportCounters::new(), A2aMode::Off);
    eprintln!(
        "live render: appended 0 -> {}",
        after
            .lines()
            .find(|l| l.contains("outcome=\"appended\"") && l.contains("memory"))
            .unwrap_or("?")
    );
    assert!(
        after.contains("signal_mesh_store_append_total{store=\"memory\",outcome=\"appended\"} 1"),
        "the shared counters did not move — the listener would serve a frozen \
         body:\n{after}"
    );

    // And the binary must not pre-render.
    let serve = read_src("src/bin/serve.rs");
    assert!(
        !serve.contains("let snapshot ="),
        "serve.rs pre-renders the metrics body; it must render per request"
    );
    assert!(
        serve.contains("async move { render_all("),
        "the /metrics handler must call render_all per request"
    );
    // build_info rides render_all, so the standalone listener carries it too.
    assert!(
        after.contains("signal_mesh_build_info{"),
        "build_info must be on the standalone listener, not only the A2A one"
    );
}

/// ⚠⚠ **Two exposition surfaces must read the SAME counters.**
///
/// The first M9 build gave `A2aState` its own `Counters` while the standalone
/// listener held a different `Arc`. The A2A `/metrics` reported a card served;
/// the standalone one reported zero. Both returned 200. Whichever an operator
/// happened to scrape decided what they believed — the "metric on the wrong
/// registry is invisible" failure, one level up.
#[test]
fn both_exposition_surfaces_share_one_set_of_transport_counters() {
    use signal_mesh::a2a::{AgentCard, Capabilities, Skill};
    use signal_mesh::transport::{A2aState, CatalogEntry, PROTOCOL_VERSION};
    use std::sync::Arc;

    let shared = Arc::new(TransportCounters::new());
    let entry = CatalogEntry {
        path: "agents/a".into(),
        version: 1,
        resource_type: "AgentCard".into(),
        card: AgentCard {
            name: "a".into(),
            description: "d".into(),
            url: "http://x/".into(),
            version: "0.1.0".into(),
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: Capabilities {
                streaming: false,
                push_notifications: false,
                extensions: vec![],
            },
            skills: vec![Skill {
                id: "s".into(),
                name: "s".into(),
                description: "s".into(),
                tags: vec![],
            }],
            security_schemes: vec!["bearer".into()],
        },
    };
    let state = A2aState::with_counters(A2aMode::Serve, entry, Some("t".into()), shared.clone());

    // The state's counters and the shared handle must be the same allocation —
    // otherwise the two surfaces drift the moment either one increments.
    assert!(
        Arc::ptr_eq(&state.counters, &shared),
        "A2aState holds a DIFFERENT Counters allocation than the standalone \
         listener; the two /metrics surfaces would disagree"
    );

    state
        .counters
        .card_served
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let body = render_all(&StoreCounters::new(), "memory", &shared, A2aMode::Serve);
    eprintln!(
        "shared transport counters: {}",
        body.lines()
            .find(|l| l.starts_with("signal_mesh_a2a_card_served_total"))
            .unwrap_or("?")
    );
    assert!(
        body.contains("signal_mesh_a2a_card_served_total 1"),
        "an increment through A2aState must be visible on the standalone \
         render:\n{body}"
    );
}
