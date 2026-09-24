//! Serve one agent's A2A surface over HTTP.
//!
//!     NOETL_SIGNAL_MESH=true \
//!     NOETL_SIGNAL_MESH_A2A=serve \
//!     NOETL_SIGNAL_MESH_A2A_TOKEN=dev-token \
//!     cargo run --bin signal-mesh-serve
//!
//! Then:
//!
//!     curl -H 'Authorization: Bearer dev-token' \
//!          localhost:8787/.well-known/agent-card.json
//!
//! ⚠ **Off by default.** With the flag unset this prints why and exits 0 —
//! it does not bind a port. The rollback for M2 is the absence of the surface.
//!
//! ⚠ The token is read from the environment because this is a dev harness. A
//! deployment resolves its credential through the keychain by alias; a bearer
//! in a pod env var is exactly what `execution-model.md` forbids for anything
//! that is not a platform credential.

use signal_mesh::a2a::{AgentCard, Capabilities, Skill};
use signal_mesh::correlation::{correlation_armed, CorrelationRule, CORRELATION_ENV};
use signal_mesh::coverage::{coverage_armed, COVERAGE_ENV};
use signal_mesh::escalation::{escalation_armed, SeverityPolicy, ESCALATION_ENV};
use signal_mesh::fold::Reduction;
use signal_mesh::mesh::CorrelationSpec;
use signal_mesh::mesh::{Agent, Mesh};
use signal_mesh::metrics::{render_all, StoreCounters, METRICS_ADDR_ENV};
use signal_mesh::store::{
    store_kind, EhdbStore, MemoryStore, MeshStore, StoreKind, CHECKPOINT_SECS_ENV, STORE_ROOT_ENV,
};
use signal_mesh::transport::{
    a2a_mode, resolve_token, router, A2aMode, A2aState, CatalogEntry, Counters, A2A_TOKEN_FILE_ENV,
    PROTOCOL_VERSION,
};
use signal_mesh::{mesh_armed, MESH_ENABLED_ENV, MESH_REASONER_ENV};
use std::sync::Arc;

const ADDR_ENV: &str = "NOETL_SIGNAL_MESH_A2A_ADDR";
const TOKEN_ENV: &str = "NOETL_SIGNAL_MESH_A2A_TOKEN";

/// The MVP's FIXED tier set — the demo's four agents, so a deployed cascade
/// produces the same pinned `52.5000 / true` the README and tests assert.
/// ⚠ Fixed on purpose: runtime fan-out is M5, not this milestone.
///
/// ⚠⚠ **The tier-0 agents carry a `SeverityPolicy` and `t1-site` carries a
/// `CorrelationSpec` unconditionally, not only when the flags are set.**
///
/// The alternative — build the policy and the spec only when armed — reads
/// safer and is worse. It makes the topology a function of the environment, so
/// the shape under test is not the shape deployed, and it hides the thing that
/// actually matters: *a flag with nothing to act on is reachable and inert.*
/// Arming `NOETL_SIGNAL_MESH_ESCALATION` against agents that all carry
/// `severity: None` would return `200 {"escalated": false,
/// "suppressed": "nominal"}` forever, which is exactly what a working,
/// correctly-quiet detector returns. That is the failure this repo keeps
/// finding, and it would have been shipped as a feature.
///
/// Declaring them costs nothing while the flags are off: `Mesh::escalate`
/// tests `escalation_armed` before it looks at any policy, and `correlating`
/// is `correlation_armed && spec.is_some()`. The pinned demo output is
/// unchanged — asserted by `the_unarmed_fixture_is_byte_identical_to_today`.
fn fixture_agents() -> Vec<Agent> {
    vec![
        Agent {
            id: "t0-temp".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("temp".into()),
            // Bands, not a bare threshold — see SeverityPolicy.
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
            // ⚠ `required_branches: 1`, not 2. The fixture's two tier-0 agents
            // own disjoint signal classes and either may legitimately be
            // silent; demanding both would make the armed fixture refuse on
            // ordinary input and look broken. The refusal path is proven in
            // tests/m11_correlation.rs, not by crippling the demo.
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

#[tokio::main]
async fn main() {
    // ⚠⚠ The master arm is read HERE, and before M9 it was read nowhere.
    // `mesh_armed` existed, was pure, and was tested — and no call site passed
    // it the process environment, so `NOETL_SIGNAL_MESH` was one of THREE inert
    // flags of six. A declared flag with no reader is not a flag;
    // `tests/m9_operability.rs` now fails the build if another one appears.
    if !mesh_armed(std::env::var(MESH_ENABLED_ENV).ok().as_deref()) {
        println!(
            "signal-mesh is NOT armed.\n\
             Set {MESH_ENABLED_ENV}=true (exactly that string) to start.\n\
             Nothing was bound; no port is listening."
        );
        return;
    }

    // Read so the flag is reachable. The model-backed reasoner is M4 and is not
    // built, so anything but the default is REFUSED rather than ignored —
    // silently reasoning arithmetically while configured for a model is the
    // shape where a wrong answer looks like a working one.
    match std::env::var(MESH_REASONER_ENV).ok().as_deref() {
        None | Some("") | Some("deterministic") => {}
        Some(other) => {
            eprintln!(
                "{MESH_REASONER_ENV}={other} requests a model-backed reasoner, \
                 which is M4 and is not built into this binary. Refusing to start."
            );
            std::process::exit(2);
        }
    }

    let selected = store_kind(std::env::var(signal_mesh::store::STORE_ENV).ok().as_deref());
    // ⚠ Shared, so the metrics listener reads the SAME counters the store
    // increments. A listener holding its own copy renders the startup values
    // forever — an endpoint that answers 200 with numbers that cannot move.
    let mut checkpoint_secs: Option<u64> = None;
    let store_counters = Arc::new(StoreCounters::new());
    let transport_counters = Arc::new(Counters::new());
    let store: Box<dyn MeshStore> = match selected {
        StoreKind::Memory => {
            Box::new(MemoryStore::new("mesh-1").with_counters(store_counters.clone()))
        }
        StoreKind::Ehdb => {
            // ⚠ No default root, on purpose. See `STORE_ROOT_ENV`.
            let Ok(root) = std::env::var(STORE_ROOT_ENV) else {
                eprintln!(
                    "{}=ehdb requires {STORE_ROOT_ENV}. There is no default path \
                     on purpose: an unmounted default silently becomes the \
                     container's ephemeral layer and looks healthy until eviction.",
                    signal_mesh::store::STORE_ENV
                );
                std::process::exit(2);
            };
            // ⚠⚠ A durable store with no checkpoint interval survives PROCESS
            // loss and not NODE loss, and nothing in the running system would
            // say so. Refuse rather than ship that asymmetry silently.
            let secs: u64 = match std::env::var(CHECKPOINT_SECS_ENV) {
                Ok(v) => match v.trim().parse() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        eprintln!(
                            "{CHECKPOINT_SECS_ENV}={v:?} is not a positive integer. With \
                             the ehdb store this bounds how many SECONDS of appends node \
                             loss costs; there is no safe default."
                        );
                        std::process::exit(2);
                    }
                },
                Err(_) => {
                    eprintln!(
                        "{}=ehdb requires {CHECKPOINT_SECS_ENV}. Without it the engine \
                         seals only on 1024 records / 8 MiB, one cascade appends ~29, and \
                         the tail lives on ONE local disk until a part fills — durable \
                         against process loss, not node loss.",
                        signal_mesh::store::STORE_ENV
                    );
                    std::process::exit(2);
                }
            };
            checkpoint_secs = Some(secs);
            match EhdbStore::open("mesh-1", &root) {
                Ok(s) => Box::new(s.with_counters(store_counters.clone())),
                Err(e) => {
                    eprintln!("cannot open the ehdb store at {root}: {e}");
                    std::process::exit(2);
                }
            }
        }
    };

    let mode = a2a_mode(
        std::env::var(signal_mesh::transport::A2A_ENV)
            .ok()
            .as_deref(),
    );

    let store_label = store.label();

    // ⭐ The three arms. Each is off unless its variable is exactly `"true"`.
    // ⚠ Read here, in the one place with a process environment, and threaded
    // through the builder — not read inside the mesh. A library that reads its
    // own env is a library you cannot test two ways in one process.
    let esc_on = escalation_armed(std::env::var(ESCALATION_ENV).ok().as_deref());
    let corr_on = correlation_armed(std::env::var(CORRELATION_ENV).ok().as_deref());
    let cov_on = coverage_armed(std::env::var(COVERAGE_ENV).ok().as_deref());
    let mesh = Mesh::with_store(store, fixture_agents(), 50.0)
        .arm_escalation(esc_on)
        .arm_correlation(corr_on)
        .arm_coverage(cov_on);

    let metrics_addr = std::env::var(METRICS_ADDR_ENV).ok();
    if let Some(addr) = metrics_addr.clone() {
        let label = store_label;
        // ⚠⚠ Cloned Arcs, NOT a rendered string. Rendering once at startup
        // produces an endpoint that returns 200 with frozen numbers — healthy
        // to every probe and useless to every operator.
        let sc = store_counters.clone();
        let tc = transport_counters.clone();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/metrics",
                axum::routing::get(move || {
                    let (sc, tc) = (sc.clone(), tc.clone());
                    async move { render_all(&sc, label, &tc, mode) }
                }),
            );
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => {
                    println!("signal-mesh metrics on http://{addr}/metrics");
                    let _ = axum::serve(l, app).await;
                }
                Err(e) => eprintln!("metrics listener cannot bind {addr}: {e}"),
            }
        });
    }

    if mode == A2aMode::Off {
        println!(
            "signal-mesh A2A transport is OFF ({}={:?}, store={}).\n\
             Set {}=serve to expose the agent card and the task lifecycle.",
            METRICS_ADDR_ENV,
            metrics_addr,
            store_label,
            signal_mesh::transport::A2A_ENV
        );
        if metrics_addr.is_some() {
            // Keep the metrics listener alive; the A2A surface stays absent.
            futures_hold().await;
        }
        return;
    }

    let token = match resolve_token(
        std::env::var(A2A_TOKEN_FILE_ENV).ok().as_deref(),
        std::env::var(TOKEN_ENV).ok().as_deref(),
    ) {
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if token.is_none() {
        // ⚠ Fail closed and say so. Serving a card that declares a scheme with
        // no token configured would refuse every request while looking like a
        // working deployment.
        eprintln!("{TOKEN_ENV} is unset — every request would be refused. Refusing to start.");
        std::process::exit(2);
    }

    let entry = CatalogEntry {
        path: "agents/t0-temp".into(),
        version: 1,
        resource_type: "AgentCard".into(),
        card: AgentCard {
            name: "t0-temp".into(),
            description: "tier-0 reducer (weighted mean over the temp class)".into(),
            url: "http://localhost:8787/a2a".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: Capabilities {
                streaming: false,
                push_notifications: true,
                extensions: vec![],
            },
            skills: vec![Skill {
                id: "reduce.tier0".into(),
                name: "Reduce tier 0".into(),
                description: "Reduce the temp signal class to one value at a named watermark"
                    .into(),
                tags: vec!["mesh".into(), "tier0".into()],
            }],
            security_schemes: vec!["bearer".into()],
        },
    };

    let addr = std::env::var(ADDR_ENV).unwrap_or_else(|_| "127.0.0.1:8787".into());
    let state = Arc::new(
        A2aState::with_counters(mode, entry, token, transport_counters.clone()).with_mesh(mesh),
    );

    // ⭐ The checkpoint driver, on a dedicated thread, driving the SAME mesh the
    // request handlers use.
    //
    // ⚠ An earlier draft gave the thread its own handle and then MOVED the mesh
    // into the serving state — leaving the thread looking at `None` and
    // checkpointing nothing, forever, with the log line still claiming a 30s
    // window. Driving through the shared state is what makes the timer real,
    // and `store_checkpoint_total{store="ehdb"}` climbing is what proves it.
    //
    // ⚠ A thread, not a tokio task: `checkpoint()` seals and uploads
    // synchronously, and parking a runtime worker to do that is precisely what
    // starves the prod writer (noetl/ai-meta#351 — 2 runtime threads vs 4
    // permitted blocking ops).
    if let Some(secs) = checkpoint_secs {
        println!("signal-mesh checkpoint every {secs}s (the node-loss window)");
        let st = state.clone();
        std::thread::Builder::new()
            .name("mesh-checkpoint".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                let mut g = st.mesh.lock().expect("mesh");
                if let Some(m) = g.as_mut() {
                    if let Err(e) = m.checkpoint() {
                        // ⚠ Logged AND counted. A barrier that only bumps a
                        // counter on success reads identically to one that
                        // never ran.
                        eprintln!("checkpoint failed: {e}");
                    }
                }
            })
            .expect("checkpoint thread spawns");
    }
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("signal-mesh A2A [{mode:?}] on http://{addr}");
    println!("  card     GET  /.well-known/agent-card.json");
    println!("  submit   POST /a2a/tasks");
    // ⚠ Print the ARM STATE, not just the route. A route that is listening and
    // a capability that is armed are different facts, and an operator reading
    // a startup banner should not have to infer the second from the first.
    println!("  escalate POST /mesh/escalate   [escalation armed={esc_on}]");
    println!("  arms     correlation={corr_on} coverage={cov_on}");
    println!("  resume   POST /a2a/tasks/{{id}}/resume");
    println!("  metrics  GET  /metrics");
    println!("  store    {store_label}");
    axum::serve(listener, app).await.expect("server runs");
}

/// Park forever so a metrics-only deployment stays up.
async fn futures_hold() {
    let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
    let _ = rx.await;
}
