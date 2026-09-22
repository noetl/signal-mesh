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
use signal_mesh::metrics::{render_all, StoreCounters, METRICS_ADDR_ENV};
use signal_mesh::store::{
    store_kind, EhdbStore, MemoryStore, MeshStore, StoreKind, STORE_ROOT_ENV,
};
use signal_mesh::transport::{
    a2a_mode, router, A2aMode, A2aState, CatalogEntry, Counters, PROTOCOL_VERSION,
};
use signal_mesh::{mesh_armed, MESH_ENABLED_ENV, MESH_REASONER_ENV};
use std::sync::Arc;

const ADDR_ENV: &str = "NOETL_SIGNAL_MESH_A2A_ADDR";
const TOKEN_ENV: &str = "NOETL_SIGNAL_MESH_A2A_TOKEN";

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
    // ⭐ The metrics listener is INDEPENDENT of the A2A surface. Before M9 the
    // only `/metrics` rode the A2A router, so a deployment could not be
    // observed without also exposing its agent surface. Those are different
    // decisions, so they are different flags.
    let metrics_addr = std::env::var(METRICS_ADDR_ENV).ok();
    if let Some(addr) = metrics_addr.clone() {
        let label = store.label();
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
            store.label(),
            signal_mesh::transport::A2A_ENV
        );
        if metrics_addr.is_some() {
            // Keep the metrics listener alive; the A2A surface stays absent.
            futures_hold().await;
        }
        return;
    }

    let token = std::env::var(TOKEN_ENV).ok();
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
    let state = Arc::new(A2aState::with_counters(
        mode,
        entry,
        token,
        transport_counters.clone(),
    ));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("signal-mesh A2A [{mode:?}] on http://{addr}");
    println!("  card     GET  /.well-known/agent-card.json");
    println!("  submit   POST /a2a/tasks");
    println!("  resume   POST /a2a/tasks/{{id}}/resume");
    println!("  metrics  GET  /metrics");
    println!("  store    {}", store.label());
    axum::serve(listener, app).await.expect("server runs");
}

/// Park forever so a metrics-only deployment stays up.
async fn futures_hold() {
    let (_tx, rx) = tokio::sync::oneshot::channel::<()>();
    let _ = rx.await;
}
