//! Serve one agent's A2A surface over HTTP.
//!
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
use signal_mesh::transport::{a2a_mode, router, A2aMode, A2aState, CatalogEntry, PROTOCOL_VERSION};
use std::sync::Arc;

const ADDR_ENV: &str = "NOETL_SIGNAL_MESH_A2A_ADDR";
const TOKEN_ENV: &str = "NOETL_SIGNAL_MESH_A2A_TOKEN";

#[tokio::main]
async fn main() {
    let mode = a2a_mode(
        std::env::var(signal_mesh::transport::A2A_ENV)
            .ok()
            .as_deref(),
    );
    if mode == A2aMode::Off {
        println!(
            "signal-mesh A2A transport is OFF.\n\
             Set {}=serve to expose the agent card and the task lifecycle.\n\
             Nothing was bound; no port is listening.",
            signal_mesh::transport::A2A_ENV
        );
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
    let state = Arc::new(A2aState::new(mode, entry, token));
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    println!("signal-mesh A2A [{mode:?}] on http://{addr}");
    println!("  card     GET  /.well-known/agent-card.json");
    println!("  submit   POST /a2a/tasks");
    println!("  resume   POST /a2a/tasks/{{id}}/resume");
    println!("  metrics  GET  /metrics");
    axum::serve(listener, app).await.expect("server runs");
}
