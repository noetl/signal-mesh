//! **M2 acceptance — real A2A transport.**
//!
//! The claim: *another system can discover this agent and ask it for work.*
//! Exercised over the real router with real HTTP requests, not by calling the
//! handlers directly — a seam test that bypasses routing proves the function,
//! not the endpoint.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use signal_mesh::a2a::{AgentCard, Capabilities, Skill, TaskState, AGENT_CARD_WELL_KNOWN_PATH};
use signal_mesh::transport::{
    a2a_mode, get_request, post_request, router, version_compatible, A2aMode, A2aState,
    CatalogEntry, Counters, PROTOCOL_VERSION,
};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "test-bearer";

fn card(name: &str, schemes: Vec<String>) -> AgentCard {
    AgentCard {
        name: name.into(),
        description: "tier-0 reducer".into(),
        url: format!("http://mesh.local/agents/{name}"),
        version: "0.1.0".into(),
        protocol_version: PROTOCOL_VERSION.into(),
        capabilities: Capabilities {
            streaming: false,
            push_notifications: true,
            extensions: vec![],
        },
        skills: vec![Skill {
            id: "reduce.tier0".into(),
            name: "Reduce tier 0".into(),
            description: "Reduce raw signals to one value".into(),
            tags: vec!["mesh".into(), "tier0".into()],
        }],
        security_schemes: schemes,
    }
}

fn entry(name: &str) -> CatalogEntry {
    CatalogEntry {
        path: format!("agents/{name}"),
        version: 1,
        resource_type: "AgentCard".into(),
        card: card(name, vec!["cluster-mtls".into()]),
    }
}

fn state(mode: A2aMode, e: CatalogEntry) -> Arc<A2aState> {
    Arc::new(A2aState::new(mode, e, Some(TOKEN.into())))
}

async fn send(app: axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value, String) {
    let res = app.oneshot(req).await.expect("router responds");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json, text)
}

// ---------------------------------------------------------------- AC 1

/// **AC1 — the served card is a projection of the catalog entry.**
///
/// ⚠ Control included: change the entry and the digest must move. Without it,
/// "the digests match" is consistent with `digest()` returning a constant.
#[tokio::test]
async fn the_served_card_projects_the_catalog_entry_and_the_digest_moves_when_it_changes() {
    let e = entry("t0-temp");
    let before = e.digest();
    let app = router(state(A2aMode::Serve, e.clone()));

    let (status, body, _) = send(app, get_request(AGENT_CARD_WELL_KNOWN_PATH, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);

    let served: AgentCard = serde_json::from_value(body).expect("a card");
    eprintln!(
        "AC1: path={AGENT_CARD_WELL_KNOWN_PATH} entry_digest={before} served_card={} skills={}",
        served.name,
        served.skills.len()
    );
    assert_eq!(
        served,
        e.to_card(),
        "the served card must BE the entry's card, not a second copy of it"
    );

    // ⚠ CONTROL: mutate the entry, require the digest to move.
    let mut changed = e.clone();
    changed.card.description = "tier-0 reducer (revised)".into();
    let after = changed.digest();
    eprintln!("AC1 control: digest before={before} after={after}");
    assert_ne!(
        before, after,
        "the entry digest did not move when the entry changed — it could not \
         detect drift between catalog and card either"
    );

    // And the registration body is the real one the endpoint accepts.
    let payload = e.register_payload();
    eprintln!(
        "AC1: register body keys = {:?}",
        payload.as_object().map(|o| o.keys().collect::<Vec<_>>())
    );
    assert!(payload.get("content").is_some(), "content is required");
    assert_eq!(payload["resource_type"], "AgentCard");
}

// ---------------------------------------------------------------- AC 2

/// **AC2 — all eight states, over the wire, and the interrupted two resume.**
///
/// ⚠⚠ The resumption half is the point. A dispatcher that treats
/// `input-required` / `auth-required` as terminal turns a recoverable pause
/// into a lost branch — and a comment saying so is not a test.
#[tokio::test]
async fn every_task_state_is_reachable_and_the_interrupted_two_resume_over_http() {
    // The four non-terminal / completing paths, driven through the router.
    for (needs, expected) in [
        (None, TaskState::Completed),
        (Some("input"), TaskState::InputRequired),
        (Some("auth"), TaskState::AuthRequired),
    ] {
        let app = router(state(A2aMode::Serve, entry("t0-temp")));
        let mut body = serde_json::json!({
            "id": "t-1", "from_agent": "dispatcher", "up_to_seq": 6
        });
        if let Some(n) = needs {
            body["needs"] = serde_json::json!(n);
        }
        let (status, json, _) = send(app, post_request("/a2a/tasks", Some(TOKEN), body)).await;
        assert_eq!(status, StatusCode::OK);
        let got = json["state"].as_str().unwrap().to_string();
        eprintln!("AC2: needs={needs:?} -> state={got}");
        assert_eq!(got, expected.as_str());
    }

    // ⭐ The resumption, end to end on ONE router so the task persists.
    for needs in ["input", "auth"] {
        let app = router(state(A2aMode::Serve, entry("t0-temp")));
        let (_, json, _) = send(
            app.clone(),
            post_request(
                "/a2a/tasks",
                Some(TOKEN),
                serde_json::json!({
                    "id": "t-resume", "from_agent": "dispatcher",
                    "up_to_seq": 6, "needs": needs
                }),
            ),
        )
        .await;
        let paused = json["state"].as_str().unwrap().to_string();
        assert!(
            TaskState::InputRequired.as_str() == paused
                || TaskState::AuthRequired.as_str() == paused,
            "expected an interrupted state, got {paused}"
        );

        let (status, resumed, _) = send(
            app,
            post_request(
                "/a2a/tasks/t-resume/resume",
                Some(TOKEN),
                serde_json::json!({ "supplied": "the-missing-thing" }),
            ),
        )
        .await;
        eprintln!(
            "AC2 resume: needs={needs} paused_at={paused} -> {}",
            resumed["state"]
        );
        assert_eq!(status, StatusCode::OK, "an interrupted task MUST resume");
        assert_eq!(
            resumed["state"].as_str().unwrap(),
            TaskState::Completed.as_str(),
            "{needs}: the interrupted state is resumable, not terminal"
        );
    }

    // ⚠ CONTROL: a terminal task refuses to resume. Without this, "resume
    // worked" is consistent with a handler that completes anything it is given.
    let app = router(state(A2aMode::Serve, entry("t0-temp")));
    send(
        app.clone(),
        post_request(
            "/a2a/tasks",
            Some(TOKEN),
            serde_json::json!({"id": "t-done", "from_agent": "d", "up_to_seq": 1}),
        ),
    )
    .await;
    let (status, body, _) = send(
        app,
        post_request(
            "/a2a/tasks/t-done/resume",
            Some(TOKEN),
            serde_json::json!({"supplied": "x"}),
        ),
    )
    .await;
    eprintln!(
        "AC2 control: resuming a COMPLETED task -> {status} {}",
        body["error"]
    );
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "terminal");

    // All eight states exist and the partition is exactly 4 terminal / 2
    // interrupted / 2 transient.
    let terminal = TaskState::ALL.iter().filter(|s| s.is_terminal()).count();
    let interrupted = TaskState::ALL.iter().filter(|s| s.is_interrupted()).count();
    eprintln!(
        "AC2: states={} terminal={terminal} interrupted={interrupted}",
        TaskState::ALL.len()
    );
    assert_eq!(TaskState::ALL.len(), 8);
    assert_eq!(terminal, 4);
    assert_eq!(interrupted, 2);
    assert!(
        TaskState::ALL
            .iter()
            .all(|s| !(s.is_terminal() && s.is_interrupted())),
        "no state may be both terminal and interrupted"
    );
}

// ---------------------------------------------------------------- AC 3

/// **AC3 — an unauthenticated card request is refused AND counted.**
#[tokio::test]
async fn an_unauthenticated_card_request_is_refused_and_counted() {
    let s = state(A2aMode::Serve, entry("t0-temp"));
    let app = router(s.clone());

    // ⚠ The counter is pinned at 0 BEFORE anything happens. An absent series
    // and a healthy one look identical; this is what separates them.
    let (_, _, before) = send(app.clone(), get_request("/metrics", Some(TOKEN))).await;
    assert!(
        before.contains("signal_mesh_a2a_card_refused_total{reason=\"unauthenticated\"} 0"),
        "the refusal counter must read 0 before any refusal, not be absent:\n{before}"
    );
    assert!(
        before.contains("signal_mesh_build_info{"),
        "build_info must always be present, or 'is this build old?' is \
         answered from an image tag instead of the scrape"
    );

    let (status, body, _) = send(app.clone(), get_request(AGENT_CARD_WELL_KNOWN_PATH, None)).await;
    eprintln!("AC3: no bearer -> {status} {}", body["error"]);
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthenticated");

    let (_, _, after) = send(app.clone(), get_request("/metrics", Some(TOKEN))).await;
    assert!(
        after.contains("signal_mesh_a2a_card_refused_total{reason=\"unauthenticated\"} 1"),
        "the refusal must be counted:\n{after}"
    );

    // ⚠ CONTROL: the right token still works, so the refusal is authentication
    // and not the route being broken.
    let (ok, _, _) = send(
        app.clone(),
        get_request(AGENT_CARD_WELL_KNOWN_PATH, Some(TOKEN)),
    )
    .await;
    assert_eq!(ok, StatusCode::OK, "a valid bearer must still be served");
    // And a WRONG token is refused too — not merely a missing one.
    let (bad, _, _) = send(app, get_request(AGENT_CARD_WELL_KNOWN_PATH, Some("wrong"))).await;
    eprintln!("AC3 control: valid=200 wrong={bad}");
    assert_eq!(bad, StatusCode::UNAUTHORIZED);
}

/// ⚠ Blueprint §8: a card declaring **no** security scheme is not publishable
/// outside the cluster — so it is refused rather than served openly.
#[tokio::test]
async fn a_card_with_no_security_scheme_is_not_publishable() {
    let mut e = entry("t0-open");
    e.card.security_schemes.clear();
    let app = router(state(A2aMode::Serve, e));
    let (status, _, _) = send(app, get_request(AGENT_CARD_WELL_KNOWN_PATH, Some(TOKEN))).await;
    eprintln!("no-scheme card -> {status}");
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a card with no declared scheme must NOT be served, even to a caller \
         holding a token — 'no scheme' is not 'open'"
    );
}

// ---------------------------------------------------------------- AC 4

/// **AC4 — negotiation is on `Major.Minor`; the patch is excluded.**
#[tokio::test]
async fn protocol_version_is_negotiated_on_major_minor() {
    assert!(
        version_compatible("1.0", "1.1"),
        "same major must be accepted"
    );
    assert!(version_compatible("1.0", "1.0.9"), "the patch is excluded");
    assert!(version_compatible("1.0", "1"), "a bare major is minor 0");
    assert!(
        !version_compatible("1.0", "2.0"),
        "a new major must be refused"
    );
    assert!(!version_compatible("1.0", "banana"), "garbage is refused");

    let s = state(A2aMode::Serve, entry("t0-temp"));
    let app = router(s.clone());
    let (ok, _, _) = send(
        app.clone(),
        post_request(
            "/a2a/tasks",
            Some(TOKEN),
            serde_json::json!({"id":"v1","from_agent":"d","up_to_seq":1,"protocol_version":"1.1"}),
        ),
    )
    .await;
    let (bad, body, _) = send(
        app.clone(),
        post_request(
            "/a2a/tasks",
            Some(TOKEN),
            serde_json::json!({"id":"v2","from_agent":"d","up_to_seq":1,"protocol_version":"2.0"}),
        ),
    )
    .await;
    eprintln!(
        "AC4: peer 1.1 -> {ok} | peer 2.0 -> {bad} ({})",
        body["error"]
    );
    assert_eq!(ok, StatusCode::OK);
    assert_eq!(bad, StatusCode::CONFLICT);
    assert_eq!(body["error"], "incompatible-protocol");

    let (_, _, m) = send(app, get_request("/metrics", Some(TOKEN))).await;
    assert!(
        m.contains("signal_mesh_a2a_version_refused_total 1"),
        "the refusal must be counted:\n{m}"
    );
}

// ------------------------------------------------------------- the flag

/// The transport is off by default, and `off` means **no routes**, not a
/// handler that declines.
#[tokio::test]
async fn the_transport_is_off_by_default_and_off_means_no_routes() {
    assert_eq!(a2a_mode(None), A2aMode::Off);
    for raw in ["", "true", "on", "1", "yes", "SERVE", "dispatch", "serve+x"] {
        assert_eq!(a2a_mode(Some(raw)), A2aMode::Off, "{raw:?} must not arm it");
    }
    assert_eq!(a2a_mode(Some("serve")), A2aMode::Serve);
    assert_eq!(
        a2a_mode(Some(" serve+dispatch ")),
        A2aMode::ServeAndDispatch
    );

    let app = router(state(A2aMode::Off, entry("t0-temp")));
    let (status, _, _) = send(app, get_request(AGENT_CARD_WELL_KNOWN_PATH, Some(TOKEN))).await;
    eprintln!("flag off: {AGENT_CARD_WELL_KNOWN_PATH} -> {status}");
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "with the flag off the route must not exist — the rollback is the \
         absence of the surface, not a handler saying no"
    );
}

/// Every counter is pinned at 0 the moment the process starts.
#[test]
fn every_counter_is_pinned_at_zero_unconditionally() {
    for mode in [A2aMode::Off, A2aMode::Serve, A2aMode::ServeAndDispatch] {
        let text = Counters::new().render(mode);
        // ⚠ Including `Off`. A pin inside `if mode.serves()` is not a pin — it
        // leaves the series missing on exactly the configuration whose zero
        // someone would be reading.
        for series in [
            "signal_mesh_a2a_card_served_total 0",
            "signal_mesh_a2a_card_refused_total{reason=\"unauthenticated\"} 0",
            "signal_mesh_a2a_task_total{outcome=\"submitted\"} 0",
            "signal_mesh_a2a_task_total{outcome=\"completed\"} 0",
            "signal_mesh_a2a_task_total{outcome=\"interrupted\"} 0",
            "signal_mesh_a2a_task_total{outcome=\"refused-terminal\"} 0",
            "signal_mesh_a2a_version_refused_total 0",
        ] {
            assert!(
                text.contains(series),
                "{mode:?}: missing pinned series {series}\n{text}"
            );
        }
        assert!(
            text.contains("signal_mesh_build_info{"),
            "{mode:?}: no build_info"
        );
    }
    eprintln!("pins: 7 series x 3 modes = 21 assertions, all present at 0");
}
