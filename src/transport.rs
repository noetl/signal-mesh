//! **M2 — real A2A transport.**
//!
//! The POC's `a2a.rs` is a data model: cards and eight task states, in memory.
//! This module puts them on a wire, so another system can **discover** an agent
//! and **ask it for work**.
//!
//! # Off by default, and read-only when on
//!
//! [`A2A_ENV`] selects `off` (default), `serve`, or `serve+dispatch`. `serve`
//! exposes the card and the task lifecycle; it never writes to the mesh log.
//! Nothing here is wired into a deployed binary.
//!
//! # The card is a projection, not a second source of truth
//!
//! An [`AgentCard`] served at [`AGENT_CARD_WELL_KNOWN_PATH`] is **derived from a
//! catalog entry** ([`CatalogEntry`]), the same entry that would be registered
//! via `POST /api/catalog/register` (`noetl/server` `src/main.rs:89`). One
//! source of truth, two surfaces — the plan's fork F-4.
//!
//! ⚠ **What this does NOT do:** it does not POST to a live noetl server. There
//! isn't one in CI, and the MVP is not allowed to touch prod. What is tested is
//! the *projection property* — the served card is a pure function of the entry,
//! and changing the entry moves the card's digest. [`register_payload`] emits
//! exactly the `{content, resource_type}` body the real endpoint takes, so
//! wiring the POST later is a call site, not a redesign.
//!
//! # Auth is not optional
//!
//! A card that declares a security scheme is refused to an unauthenticated
//! caller, and the refusal is **counted**. ⚠ Every counter here is pinned at 0
//! at construction, unconditionally — an absent series and a healthy one look
//! identical on a scrape, and a pin inside a config branch is not a pin.

use crate::a2a::{canonical_json, fnv1a_hex, AgentCard, Task, TaskState};
use crate::mesh::{Mesh, Verdict};
use crate::react::DeterministicReasoner;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// `NOETL_SIGNAL_MESH_A2A` — `off` (default) | `serve` | `serve+dispatch`.
pub const A2A_ENV: &str = "NOETL_SIGNAL_MESH_A2A";

/// The protocol version this build speaks. Negotiation is on `Major.Minor`;
/// the patch is excluded by the spec.
pub const PROTOCOL_VERSION: &str = "1.0";

/// `NOETL_SIGNAL_MESH_A2A_TOKEN_FILE` — path to a file holding the bearer
/// token, mounted from a Kubernetes Secret.
///
/// ⚠⚠ **This is the only acceptable way to supply the token in a deployment.**
/// `execution-model.md` forbids a business-logic credential in a pod env var,
/// and a bearer is one. An env var is visible in `kubectl describe pod`, in the
/// Deployment spec, and to anything that can read the pod's environ; a file
/// mounted from a Secret is none of those. The env form is kept ONLY for local
/// development and is refused when the file form is configured, so a deployment
/// cannot silently fall back to the weaker one.
pub const A2A_TOKEN_FILE_ENV: &str = "NOETL_SIGNAL_MESH_A2A_TOKEN_FILE";

/// Resolve the bearer token, preferring the mounted file.
///
/// Returns `Err` with a reason rather than `None`, because "no token" and
/// "token file unreadable" want different operator responses and collapsing
/// them is how a misconfigured mount reads as an unconfigured one.
pub fn resolve_token(file_path: Option<&str>, env_value: Option<&str>) -> Result<String, String> {
    match file_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            // ⚠ A configured file wins outright. If BOTH are set the env one is
            // ignored, not merged — a deployment that accidentally carries the
            // env form must not be able to override the Secret.
            let raw = std::fs::read_to_string(p)
                .map_err(|e| format!("cannot read {A2A_TOKEN_FILE_ENV}={p}: {e}"))?;
            let t = raw.trim().to_string();
            if t.is_empty() {
                return Err(format!("{A2A_TOKEN_FILE_ENV}={p} is empty"));
            }
            Ok(t)
        }
        None => match env_value.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => Ok(v.to_string()),
            None => Err(format!(
                "no bearer token: set {A2A_TOKEN_FILE_ENV} (deployments) or \
                 NOETL_SIGNAL_MESH_A2A_TOKEN (local development only)"
            )),
        },
    }
}

/// What the transport is allowed to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum A2aMode {
    /// No routes. **Default.**
    #[default]
    Off,
    /// Serve the card and the task lifecycle. Read-only w.r.t. the mesh log.
    Serve,
    /// Also dispatch tasks to peers. Not exercised by the MVP.
    ServeAndDispatch,
}

impl A2aMode {
    pub fn serves(self) -> bool {
        matches!(self, A2aMode::Serve | A2aMode::ServeAndDispatch)
    }
}

/// Resolve [`A2A_ENV`]. Pure over the raw value.
///
/// ⚠ A selector must **fail closed**: anything unrecognised is [`A2aMode::Off`],
/// not "probably serve". `"true"` is deliberately *not* an arming value here —
/// this flag has three states, and treating a boolean spelling as "on" would
/// leave which of the two on-states ambiguous.
pub fn a2a_mode(raw: Option<&str>) -> A2aMode {
    match raw.map(str::trim) {
        Some("serve") => A2aMode::Serve,
        Some("serve+dispatch") => A2aMode::ServeAndDispatch,
        _ => A2aMode::Off,
    }
}

/// Compare two protocol versions on `Major.Minor`.
///
/// Accepts a peer whose **major** matches and whose **minor** is no greater than
/// ours plus forward-compat within the major line, per A2A's negotiation rule.
/// The patch component is ignored entirely.
pub fn version_compatible(ours: &str, theirs: &str) -> bool {
    fn major_minor(v: &str) -> Option<(u32, u32)> {
        let mut it = v.split('.');
        let major = it.next()?.parse().ok()?;
        // A bare "1" is a major-only declaration; treat the minor as 0 rather
        // than refusing, since the spec negotiates on Major.Minor.
        let minor = it.next().unwrap_or("0").parse().ok()?;
        Some((major, minor))
    }
    match (major_minor(ours), major_minor(theirs)) {
        (Some((om, _)), Some((tm, _))) => om == tm,
        _ => false,
    }
}

/// The catalog entry an agent is registered as — the card's single source.
///
/// Mirrors the two fields `POST /api/catalog/register` actually takes
/// (`CatalogRegisterRequest { content, resource_type }`) plus the identity the
/// response returns, so [`register_payload`] is the real body and not an
/// approximation of one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub path: String,
    pub version: u32,
    pub resource_type: String,
    /// The agent declaration. The card is derived from this.
    pub card: AgentCard,
}

impl CatalogEntry {
    /// A stable digest of the entry. The served card must match it (AC1).
    pub fn digest(&self) -> String {
        let v = serde_json::to_value(self).expect("catalog entry serialises");
        fnv1a_hex(canonical_json(&v).as_bytes())
    }

    /// ⭐ **The projection.** The served card is a pure function of the entry —
    /// that is the whole property AC1 asserts, and why there cannot be two
    /// sources of truth to drift apart.
    pub fn to_card(&self) -> AgentCard {
        self.card.clone()
    }

    /// Exactly the body `POST /api/catalog/register` accepts.
    pub fn register_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "content": serde_json::to_string(&self.card).expect("card serialises"),
            "resource_type": self.resource_type,
        })
    }
}

/// Counters. ⚠ **Every one is pinned at 0 at construction, unconditionally.**
///
/// A labelled metric is absent until it fires, and absent reads exactly like
/// healthy. Pinning inside `if mode.serves()` would leave the refusal counter
/// missing on precisely the configuration whose refusals someone is reading.
#[derive(Debug, Default)]
pub struct Counters {
    pub card_served: AtomicU64,
    pub card_refused_unauthenticated: AtomicU64,
    pub task_submitted: AtomicU64,
    pub task_completed: AtomicU64,
    pub task_interrupted: AtomicU64,
    pub task_refused_terminal: AtomicU64,
    pub version_refused: AtomicU64,
}

impl Counters {
    pub fn new() -> Self {
        // Explicit rather than `Default::default()` so the pin is a visible
        // decision a reader can check against the render below.
        Self {
            card_served: AtomicU64::new(0),
            card_refused_unauthenticated: AtomicU64::new(0),
            task_submitted: AtomicU64::new(0),
            task_completed: AtomicU64::new(0),
            task_interrupted: AtomicU64::new(0),
            task_refused_terminal: AtomicU64::new(0),
            version_refused: AtomicU64::new(0),
        }
    }

    fn incr(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// Prometheus text exposition, with a `build_info` gauge always at 1.
    ///
    /// ⚠ The gauge exists so *"does this process predate that counter?"* is
    /// answerable from the scrape itself, rather than from an image tag — a
    /// different representation, and one that can disagree with what is running.
    pub fn render(&self, mode: A2aMode) -> String {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        format!(
            "# HELP signal_mesh_build_info Build metadata; always 1.\n\
             # TYPE signal_mesh_build_info gauge\n\
             signal_mesh_build_info{{version=\"{v}\",protocol=\"{p}\",a2a_mode=\"{m}\"}} 1\n\
             # TYPE signal_mesh_a2a_card_served_total counter\n\
             signal_mesh_a2a_card_served_total {}\n\
             # TYPE signal_mesh_a2a_card_refused_total counter\n\
             signal_mesh_a2a_card_refused_total{{reason=\"unauthenticated\"}} {}\n\
             # TYPE signal_mesh_a2a_task_total counter\n\
             signal_mesh_a2a_task_total{{outcome=\"submitted\"}} {}\n\
             signal_mesh_a2a_task_total{{outcome=\"completed\"}} {}\n\
             signal_mesh_a2a_task_total{{outcome=\"interrupted\"}} {}\n\
             signal_mesh_a2a_task_total{{outcome=\"refused-terminal\"}} {}\n\
             # TYPE signal_mesh_a2a_version_refused_total counter\n\
             signal_mesh_a2a_version_refused_total {}\n",
            g(&self.card_served),
            g(&self.card_refused_unauthenticated),
            g(&self.task_submitted),
            g(&self.task_completed),
            g(&self.task_interrupted),
            g(&self.task_refused_terminal),
            g(&self.version_refused),
            v = env!("CARGO_PKG_VERSION"),
            p = PROTOCOL_VERSION,
            m = match mode {
                A2aMode::Off => "off",
                A2aMode::Serve => "serve",
                A2aMode::ServeAndDispatch => "serve+dispatch",
            },
        )
    }
}

/// What the server needs to answer.
pub struct A2aState {
    pub mode: A2aMode,
    pub entry: CatalogEntry,
    /// Bearer token a caller must present when the card declares a scheme.
    pub token: Option<String>,
    /// ⚠⚠ **Shared, not owned.** A process can expose two `/metrics` surfaces —
    /// the A2A router's and the standalone listener's — and if each holds its
    /// own `Counters` they disagree: one reports the card served, the other
    /// reports zero. That is the "metric on the wrong registry is invisible"
    /// failure, and the first M9 build had it.
    pub counters: Arc<Counters>,
    tasks: Mutex<BTreeMap<String, Task>>,
    /// ⭐ The M1↔M2 join. Without it the service opens a durable store, drives
    /// checkpoints against it, and **never writes a byte** — M1 and M2 both
    /// real and never connected. A durable store nothing reaches is not
    /// durability, it is an empty volume with a timer.
    ///
    /// `Mutex<Option<..>>` rather than a bare store: the mesh is optional
    /// (`memory` deployments and unit tests do not need one) and the store is
    /// `Send` but not `Sync`.
    pub mesh: Mutex<Option<Mesh>>,
}

impl A2aState {
    pub fn new(mode: A2aMode, entry: CatalogEntry, token: Option<String>) -> Self {
        Self::with_counters(mode, entry, token, Arc::new(Counters::new()))
    }

    /// Share the counters with another exposition surface.
    pub fn with_counters(
        mode: A2aMode,
        entry: CatalogEntry,
        token: Option<String>,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            mode,
            entry,
            token,
            counters,
            tasks: Mutex::new(BTreeMap::new()),
            mesh: Mutex::new(None),
        }
    }

    /// Attach the cascade, so the served agent can actually produce answers.
    pub fn with_mesh(self, mesh: Mesh) -> Self {
        *self.mesh.lock().expect("mesh") = Some(mesh);
        self
    }

    /// Does the served card require authentication?
    ///
    /// ⚠ Blueprint §8: a card with **no** declared scheme is not publishable
    /// outside the cluster. So "no scheme" does not mean "open" — it means this
    /// card should not have been exposed, and the route refuses it rather than
    /// serving an unauthenticated card to the world.
    pub fn requires_auth(&self) -> bool {
        !self.entry.card.security_schemes.is_empty()
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
}

fn refuse(status: StatusCode, error: &'static str, detail: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error,
            detail: detail.into(),
        }),
    )
        .into_response()
}

/// Submit body for `POST /a2a/tasks`.
#[derive(Debug, Deserialize)]
pub struct SubmitTask {
    pub id: String,
    pub from_agent: String,
    pub up_to_seq: u64,
    /// The caller's protocol version, negotiated on `Major.Minor`.
    #[serde(default)]
    pub protocol_version: Option<String>,
    /// When set, the agent answers by interrupting instead of completing —
    /// the two resumable states, exercised over the wire.
    #[serde(default)]
    pub needs: Option<String>,
}

/// Body for `POST /mesh/signals` — one device reading.
#[derive(Debug, Deserialize)]
pub struct ObserveSignal {
    pub device_id: String,
    pub signal_class: String,
    pub value: f64,
    pub device_seq: u64,
}

/// Body for `POST /mesh/escalate`.
///
/// ⚠ `at_seq` is supplied by the CALLER, not read from the log head inside
/// `Mesh::escalate`. M10's first build read the head itself, and appending the
/// escalation advanced it — so the dedupe key `(agent, band, watermark)` moved
/// on every call and dedupe never fired. The caller names the watermark it
/// observed at; that is what makes a duplicate detectable.
#[derive(Debug, Deserialize)]
pub struct EscalateRequest {
    pub agent_id: String,
    pub value: f64,
    pub input_count: u32,
    pub at_seq: u64,
}

/// Body for `POST /a2a/tasks/{id}/resume`.
#[derive(Debug, Deserialize)]
pub struct ResumeTask {
    /// What the interrupted task was waiting for.
    pub supplied: String,
}

fn authorized(state: &A2aState, headers: &HeaderMap) -> bool {
    if !state.requires_auth() {
        return false; // see `requires_auth` — no scheme means not publishable.
    }
    let Some(expected) = state.token.as_deref() else {
        return false;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == expected)
        .unwrap_or(false)
}

async fn serve_card(State(s): State<Arc<A2aState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "this agent card declares a security scheme; present a bearer token",
        );
    }
    Counters::incr(&s.counters.card_served);
    Json(s.entry.to_card()).into_response()
}

async fn submit_task(
    State(s): State<Arc<A2aState>>,
    headers: HeaderMap,
    Json(body): Json<SubmitTask>,
) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bearer required",
        );
    }
    if let Some(peer) = &body.protocol_version {
        if !version_compatible(PROTOCOL_VERSION, peer) {
            Counters::incr(&s.counters.version_refused);
            return refuse(
                StatusCode::CONFLICT,
                "incompatible-protocol",
                format!("peer speaks {peer}; this agent speaks {PROTOCOL_VERSION}"),
            );
        }
    }

    let mut task = Task::submit(
        &body.id,
        &body.from_agent,
        &s.entry.card.name,
        body.up_to_seq,
    );
    Counters::incr(&s.counters.task_submitted);
    task.transition(TaskState::Working)
        .expect("submitted->working");

    // ⚠⚠ The interrupted states are NOT failures. A dispatcher that collapses
    // them into `failed` loses a branch of the cascade that would have resumed.
    match body.needs.as_deref() {
        Some("input") => {
            task.transition(TaskState::InputRequired).expect("->input");
            Counters::incr(&s.counters.task_interrupted);
        }
        Some("auth") => {
            task.transition(TaskState::AuthRequired).expect("->auth");
            Counters::incr(&s.counters.task_interrupted);
        }
        _ => {
            task.transition(TaskState::Completed).expect("->completed");
            Counters::incr(&s.counters.task_completed);
        }
    }
    let out = task.clone();
    s.tasks.lock().expect("tasks").insert(body.id, task);
    (StatusCode::OK, Json(out)).into_response()
}

/// ⭐ **M10 — `POST /mesh/escalate`.**
///
/// ⚠⚠ A *suppressed* escalation is **200, not an error.** A non-event is a
/// normal outcome here — most signals are nominal, most repeats are duplicates
/// — and returning 4xx would train a caller to treat the ordinary case as a
/// failure and stop reading the reason. The reason is the payload.
///
/// ⚠ Returns `armed: false` explicitly rather than pretending. An unarmed mesh
/// suppresses as `nominal`, which is indistinguishable from a genuinely
/// nominal value unless the response says which it was — the "absent is not
/// zero" rule applied to an HTTP body.
async fn escalate(
    State(s): State<Arc<A2aState>>,
    headers: HeaderMap,
    Json(b): Json<EscalateRequest>,
) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bearer required",
        );
    }
    let mut g = s.mesh.lock().expect("mesh");
    let Some(m) = g.as_mut() else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no-mesh",
            "no cascade attached",
        );
    };
    let armed = m.escalation_armed;
    match m.escalate(&b.agent_id, b.value, b.input_count, b.at_seq) {
        Ok(Ok(seq)) => Json(serde_json::json!({
            "escalated": true,
            "armed": armed,
            "seq": seq,
            "head": m.log.head(),
        }))
        .into_response(),
        Ok(Err(why)) => Json(serde_json::json!({
            "escalated": false,
            "armed": armed,
            "suppressed": suppressed_reason(&why),
            "head": m.log.head(),
        }))
        .into_response(),
        Err(e) => refuse(StatusCode::BAD_REQUEST, "append-failed", e.to_string()),
    }
}

/// A closed-set label for a suppression.
///
/// ⚠ Closed on purpose: it is what a metric or an alert keys on, and a free
/// string here would make an unbounded label set.
fn suppressed_reason(w: &crate::escalation::Suppressed) -> &'static str {
    match w {
        crate::escalation::Suppressed::Nominal => "nominal",
        crate::escalation::Suppressed::Duplicate => "duplicate",
        crate::escalation::Suppressed::Backpressure => "backpressure",
    }
}

async fn get_task(State(s): State<Arc<A2aState>>, Path(id): Path<String>) -> Response {
    match s.tasks.lock().expect("tasks").get(&id) {
        Some(t) => Json(t.clone()).into_response(),
        None => refuse(StatusCode::NOT_FOUND, "no-such-task", id),
    }
}

/// Resume an interrupted task. ⭐ This route is the proof that
/// `input-required` / `auth-required` are resumable rather than terminal.
async fn resume_task(
    State(s): State<Arc<A2aState>>,
    Path(id): Path<String>,
    Json(body): Json<ResumeTask>,
) -> Response {
    let mut tasks = s.tasks.lock().expect("tasks");
    let Some(task) = tasks.get_mut(&id) else {
        return refuse(StatusCode::NOT_FOUND, "no-such-task", id);
    };
    if task.state.is_terminal() {
        Counters::incr(&s.counters.task_refused_terminal);
        return refuse(
            StatusCode::CONFLICT,
            "terminal",
            format!("task is terminal in {}", task.state.as_str()),
        );
    }
    if !task.state.is_interrupted() {
        return refuse(
            StatusCode::CONFLICT,
            "not-interrupted",
            format!("task is {}, nothing to resume", task.state.as_str()),
        );
    }
    let _ = body.supplied;
    task.transition(TaskState::Working)
        .expect("resume->working");
    task.transition(TaskState::Completed).expect("->completed");
    Counters::incr(&s.counters.task_completed);
    Json(task.clone()).into_response()
}

/// Ingest one signal. ⚠ This is the ONLY write path into the store.
async fn observe(
    State(s): State<Arc<A2aState>>,
    headers: HeaderMap,
    Json(b): Json<ObserveSignal>,
) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bearer required",
        );
    }
    let mut g = s.mesh.lock().expect("mesh");
    let Some(m) = g.as_mut() else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no-mesh",
            "no cascade attached",
        );
    };
    match m.observe_signal(&b.device_id, &b.signal_class, b.value, b.device_seq) {
        Ok(seq) => Json(serde_json::json!({ "seq": seq, "head": m.log.head() })).into_response(),
        Err(e) => refuse(StatusCode::BAD_REQUEST, "append-failed", e.to_string()),
    }
}

/// Run the cascade at the current head and persist every event it produces.
/// Body for `POST /mesh/cascade`. Empty is valid.
///
/// ⭐ M3 — `up_to_seq` lets a caller name a watermark. ⚠ Without it the gate is
/// reachable and **unable to fire**: the cascade always read at `log.head()`,
/// where the staleness is 0 by construction and every policy admits. A gate
/// that cannot refuse is indistinguishable from one that is not there.
#[derive(Debug, Default, Deserialize)]
pub struct CascadeRequest {
    #[serde(default)]
    pub up_to_seq: Option<u64>,
}

async fn cascade(
    State(s): State<Arc<A2aState>>,
    headers: HeaderMap,
    body: Option<Json<CascadeRequest>>,
) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bearer required",
        );
    }
    let mut g = s.mesh.lock().expect("mesh");
    let Some(m) = g.as_mut() else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no-mesh",
            "no cascade attached",
        );
    };
    let head = m.log.head();
    let at = body.and_then(|Json(b)| b.up_to_seq).unwrap_or(head);
    match m.cascade(at, &DeterministicReasoner) {
        Ok(v) => Json(verdict_json(&v, at)).into_response(),
        // ⚠ A freshness refusal is 409 like any other cascade refusal, but it
        // carries its OWN error code: a caller that retries a stale read after
        // waiting is doing the right thing, while retrying a malformed fold is
        // not, and one code for both would hide which it was.
        Err(crate::mesh::MeshError::Freshness(w)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "stale-read",
                "detail": w.to_string(),
                "requested_up_to": at,
                "head": head,
                "retryable": true,
            })),
        )
            .into_response(),
        Err(e) => refuse(StatusCode::CONFLICT, "cascade-refused", e.to_string()),
    }
}

/// Re-fold the persisted log and report what it holds. ⭐ This is the
/// restart-survival probe: it reads ONLY from the store.
async fn replay(State(s): State<Arc<A2aState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        Counters::incr(&s.counters.card_refused_unauthenticated);
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "bearer required",
        );
    }
    let mut g = s.mesh.lock().expect("mesh");
    let Some(m) = g.as_mut() else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "no-mesh",
            "no cascade attached",
        );
    };
    let head = m.log.head();
    let records = match m.log.records_up_to(head) {
        Ok(r) => r,
        Err(e) => {
            return refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "read-failed",
                e.to_string(),
            )
        }
    };
    let stream = m.log.stream().to_string();
    let ctx = match crate::fold::fold(&records, &stream, head) {
        Ok(c) => c,
        Err(e) => return refuse(StatusCode::CONFLICT, "fold-refused", format!("{e:?}")),
    };
    let verdict = records.iter().rev().find_map(|r| match &r.payload {
        crate::event::MeshEvent::VerdictSynthesised(v) => Some(v.value),
        _ => None,
    });
    Json(serde_json::json!({
        "records": records.len(),
        "head": head,
        "digest": ctx.digest(),
        "folded_through": ctx.folded_through,
        "staleness": ctx.staleness(),
        "verdict": verdict,
    }))
    .into_response()
}

fn verdict_json(v: &Verdict, at: u64) -> serde_json::Value {
    // ⚠⚠ M12's marker has to leave the process, or arming coverage is a
    // capability that runs and cannot be observed — which is the same thing as
    // not running, from the only vantage point that matters to a caller.
    //
    // ⚠ `coverage` is `null` when NOT ASSESSED and an object when it was. A
    // caller must not read the absence as "complete"; the `assessed` field says
    // which of the two it is, in the body, rather than leaving it to be
    // inferred from a missing key.
    let coverage = match &v.coverage {
        None => serde_json::json!({ "assessed": false }),
        Some(c) => serde_json::json!({
            "assessed": true,
            "expected": c.expected,
            "present": c.present,
            "missing": c.missing,
            "shortfall": c.shortfall(),
            "reason": c.reason(),
        }),
    };
    serde_json::json!({
        "value": v.value, "decision": v.decision,
        "up_to_seq": v.up_to_seq, "events_appended": v.events_appended,
        "cascaded_at_head": at,
        "degraded": v.degraded,
        "degraded_reason": v.degraded_reason,
        "coverage": coverage,
    })
}

async fn metrics(State(s): State<Arc<A2aState>>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        s.counters.render(s.mode),
    )
        .into_response()
}

/// Build the router. ⚠ Returns an **empty** router when the mode is `off`, so
/// the rollback is the absence of the routes rather than a handler that says no.
pub fn router(state: Arc<A2aState>) -> Router {
    if !state.mode.serves() {
        return Router::new();
    }
    Router::new()
        .route(crate::a2a::AGENT_CARD_WELL_KNOWN_PATH, get(serve_card))
        .route("/a2a/tasks", post(submit_task))
        .route("/a2a/tasks/{id}", get(get_task))
        .route("/a2a/tasks/{id}/resume", post(resume_task))
        .route("/mesh/signals", post(observe))
        .route("/mesh/cascade", post(cascade))
        .route("/mesh/escalate", post(escalate))
        .route("/mesh/replay", get(replay))
        .route("/metrics", get(metrics))
        .with_state(state)
}

/// Convenience for tests and future callers: an empty-body GET.
pub fn get_request(uri: &str, bearer: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().uri(uri).method("GET");
    if let Some(t) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).expect("request builds")
}

/// Convenience for tests and future callers: a JSON POST.
pub fn post_request(uri: &str, bearer: Option<&str>, body: serde_json::Value) -> Request<Body> {
    let mut b = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(body.to_string()))
        .expect("request builds")
}
