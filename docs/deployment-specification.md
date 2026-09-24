# signal-mesh — deployment specification

**Status:** M9. **Nothing here is deployed.** This page is the runtime contract
for `signal-mesh-serve`, written so an operator can size, configure, probe and
roll it back without reading the source.

⚠ **The env-var catalogue below is enforced by a test.**
`tests/m9_operability.rs` extracts every `NOETL_*` constant the crate declares
and fails if one is undocumented here, or documented here and read nowhere. A
deployment-spec page that drifts is worse than none: it is a confident wrong
answer.

---

## 1. What it is

One process serving one agent's A2A surface over an event log. It reduces device
signals through tiers of ReAct agents to **one number and one boolean**, and
every step is an event so the answer replays.

| | |
| :-- | :-- |
| binary | `signal-mesh-serve` |
| library | `signal-mesh` (also ships `signal-mesh-demo`, a deterministic fixture run) |
| language / edition | Rust 2021, MSRV 1.82 |
| state | its **own** EHDB dataset (`mesh_event_log`) — never the platform's D1 |
| writes to Postgres | **none, ever** |
| executes generated code | **no** |

---

## 2. Runtime contract — what it needs to start cleanly

1. `NOETL_SIGNAL_MESH=true`. Anything else and the process prints why and exits
   **0** without binding a port.
2. If `NOETL_SIGNAL_MESH_STORE=ehdb`, a **writable, mounted**
   `NOETL_SIGNAL_MESH_STORE_ROOT`. There is no default path — see §4.
3. If `NOETL_SIGNAL_MESH_A2A=serve`, a `NOETL_SIGNAL_MESH_A2A_TOKEN`. Without
   one every request would be refused while the process looked healthy, so it
   exits **2** instead.

It fails closed in every case above and says which one.

---

## 3. Network surface

| port | route | when |
| :-- | :-- | :-- |
| `NOETL_SIGNAL_MESH_A2A_ADDR` (default `127.0.0.1:8787`) | `GET /.well-known/agent-card.json` | `_A2A=serve` |
| " | `POST /a2a/tasks` | " |
| " | `GET /a2a/tasks/{id}` | " |
| " | `POST /a2a/tasks/{id}/resume` | " |
| " | `GET /metrics` | " |
| `NOETL_SIGNAL_MESH_METRICS_ADDR` (unset = **no listener**) | `GET /metrics` | independently of A2A |

⚠ The default A2A bind is **loopback**. Exposing the agent surface is a
deliberate act, and a card with no declared security scheme is refused rather
than served — "no scheme" means *not publishable outside the cluster*.

**Outbound dependencies: none.** The MVP calls no model, no catalog, no
database. That changes at M4 (model) and when the catalog POST is wired.

---

## 4. Environment variables

Every variable the binary reads, with the reason — not just the value.

| Variable | Default | Required | What it does, and why |
| :-- | :-- | :-- | :-- |
| `NOETL_SIGNAL_MESH` | *(unset)* = off | **yes, to start** | Master arm. Only the exact string `true` arms it — the house convention shared with `seal_max_age`, writer fencing and the repair sweep, so arming is deliberate and greppable. ⚠ Until M9 this flag had **no reader at all**. |
| `NOETL_SIGNAL_MESH_STORE` | `memory` | no | `ehdb` selects real durable persistence. A **selector, not a boolean**: anything unrecognised is `memory`, so it fails closed rather than guessing. |
| `NOETL_SIGNAL_MESH_STORE_ROOT` | *(none)* | **yes when `_STORE=ehdb`** | Where the engine keeps its substrate and local parts. ⚠⚠ **Deliberately no default.** `LocalFsSubstrate::new` calls `create_dir_all`, so an unmounted default silently becomes the container's ephemeral layer — healthy-looking right up to the eviction. Refusing to start is the honest behaviour. Mount a PVC. |
| `NOETL_SIGNAL_MESH_REASONER` | *(unset)* = deterministic | no | Selects the reasoner. Anything other than unset/`deterministic` is **refused**, because the model-backed path is M4 and is not built into this binary; silently reasoning arithmetically while configured for a model is a wrong answer that looks like a working one. |
| `NOETL_SIGNAL_MESH_A2A` | *(unset)* = off | no | `serve` or `serve+dispatch`. ⚠ `off` means the **routes do not exist**, not a handler that declines — the rollback is the absence of the surface. `true` is deliberately not an arming value: this flag has three states. |
| `NOETL_SIGNAL_MESH_A2A_ADDR` | `127.0.0.1:8787` | no | A2A bind address. Loopback by default. |
| `NOETL_SIGNAL_MESH_A2A_TOKEN_FILE` | *(none)* | **yes in any deployment** | Path to a file holding the bearer token, mounted from a Kubernetes Secret. ⚠⚠ **This is the only acceptable form in a cluster.** `execution-model.md` forbids a business-logic credential in a pod env var, and a bearer is one — an env var is visible in `kubectl describe pod`, in the Deployment spec, and to anything that can read the pod's environ. A configured file **wins outright** over the env form rather than merging, so a stray env var cannot override the Secret. |
| `NOETL_SIGNAL_MESH_CHECKPOINT_SECS` | *(none)* | **yes when `_STORE=ehdb`** | Seconds between durability barriers. ⚠⚠ **No default, and the process refuses to start without it**, because a durable store with no checkpoint survives *process* loss and not *node* loss and nothing running would say so. **Why a timer and not a lower `seal_max_records`:** the record threshold bounds the tail in **appends**, and the dangerous case is when appends *stop* — a half-full part then sits on one local disk indefinitely, so the quieter the mesh the worse the exposure. A timer bounds it in **seconds**, which is the unit node-loss cost is actually measured in, independent of traffic. `ehdb-l0` already shows the trap: `seal_max_age` exists but is only consulted on append. |
| `NOETL_SIGNAL_MESH_A2A_TOKEN` | *(none)* | **yes when `_A2A=serve`** | Bearer token callers must present. ⚠ Env is acceptable here only because this is a **platform** credential for a dev harness. A business-logic secret belongs in the keychain, referenced by alias — see `execution-model.md`. |
| `NOETL_SIGNAL_MESH_A2A_ADDR` (write routes) | — | — | ⚠ When `_A2A=serve` the service also exposes **`POST /mesh/signals`**, **`POST /mesh/cascade`** and **`GET /mesh/replay`** on the same port and behind the same bearer. These are the only write paths into the store; without them the service opens a durable volume, checkpoints it, and never writes a byte. |
| `NOETL_SIGNAL_MESH_ESCALATION` | *(unset)* = off | no | ⭐ **M10 — event-driven escalation.** Arms the path by which a tier-0 agent pushes upward on its own instead of waiting for the scheduled cascade. Only the exact string `true` arms it. ⚠ Additive: the scheduled cascade is untouched and remains the correctness baseline. A *suppressed* escalation returns a reason rather than an error — a non-event is a normal outcome, and it is counted.

`POST /mesh/escalate` is the endpoint. ⚠ A *suppressed* escalation is **200**,
not an error — most values are nominal and most repeats are duplicates, so a
4xx would train a caller to treat the ordinary case as a failure and stop
reading the reason. The body carries `armed` explicitly, because an unarmed
mesh suppresses as `nominal`, which is otherwise indistinguishable from a
working detector with nothing to report. |
| `NOETL_SIGNAL_MESH_CORRELATION` | *(unset)* = off | no | ⭐ **M11 — the correlation tier.** Arms `t1-site` to correlate across its branches, taking its population as the **union** of contributing identities rather than the sum of their counts. Only the exact string `true` arms it. ⚠ With it off, `t1-site` reduces exactly as before — the `CorrelationSpec` it carries is inert, not absent. |
| `NOETL_SIGNAL_MESH_COVERAGE` | *(unset)* = off | no | ⭐ **M12 — failure semantics.** Assesses each aggregator against its *declared* children and marks a verdict `degraded` when fewer reported. Only the exact string `true` arms it. ⚠ **Observability only — it must not move the value**, and a test asserts that. With it off an aggregate carries `coverage: None`, which means *not assessed*, never "complete". |
| `NOETL_SIGNAL_MESH_METRICS_ADDR` | *(unset)* = **no listener** | no | Standalone metrics bind. ⭐ Independent of `_A2A` on purpose: before M9 the only `/metrics` rode the A2A router, so a deployment could not be observed without also exposing its agent surface. |

### Variables this component does **not** read

- `NOETL_EHDB_READ_CONSISTENCY` / `NOETL_EHDB_MAX_STALENESS_MS` — ⚠ **these do
  not exist anywhere in `noetl/ehdb` or `noetl/server`** (see the production
  plan §2.3). M3 introduces them. The MVP uses `Strong` reads only.
- `NOETL_SLM_*` — the stepgen gate is M8.

---

## 5. Resource sizing

⚠ **These are starting points, not measurements.** No production workload has
been stated (production plan §2.4), and M7 is the milestone that replaces them
with numbers.

| | request | limit | why |
| :-- | :-- | :-- | :-- |
| CPU | 100m | 500m | The cascade is serial and arithmetic; CPU is not the constraint at MVP scale. |
| memory | 256Mi | 512Mi | Dominated by the unsealed part buffer: `seal_max_bytes` is 8 MiB per shard by default, and the MVP runs one shard. |
| disk (`_STORE_ROOT`) | 1Gi | — | ⚠ Size against **write rate × retention**, not record count. The cmdbus PVC filled because manifest size grows with part count while part count grows with writes — a product of two growing quantities. Alert on the **ratio** of manifest bytes to part bytes, not on a free-space threshold. |

---

## 6. Health probes

There is no dedicated health route yet. Use:

| probe | target | notes |
| :-- | :-- | :-- |
| readiness | `GET /metrics` on `_METRICS_ADDR` | 200 with a non-empty body means the process is up and its counters are pinned. |
| liveness | same | ⚠ Set `failureThreshold` and `periodSeconds` so a slow checkpoint cannot be killed mid-flush. A `CrashLoopBackOff` with `exitCode=0` and `SIGTERM` is a **liveness kill, not a crash**. |

⚠ A `/metrics` body that is **empty** is not healthy — it means the exporter is
answering but nothing registered. This build pins every series, so an empty body
indicates a build that predates M9. Check `signal_mesh_build_info`.

---

## 7. Observability

`signal_mesh_build_info{version,protocol,a2a_mode}` is **always 1**, so *"does
this pod predate that metric?"* is answerable from the scrape rather than from a
Deployment's image tag — a different representation, and one that can disagree
with what is running.

**Every series is pinned at 0 at construction, unconditionally**, including for
the store that is *not* configured. An absent series and a healthy one look
identical, and a pin inside `if store_is_ehdb` leaves the dedupe counter missing
on exactly the configuration whose zero someone is reading.

| series | reading |
| :-- | :-- |
| `signal_mesh_store_append_total{store,outcome="appended"}` | records actually written |
| `…{outcome="deduped"}` | ⭐ idempotent redeliveries. A working dedupe and a **broken writer** both show a flat `appended`; only this separates them. |
| `signal_mesh_store_refused_total{store,reason}` | `payload-too-large` / `engine` / `codec` |
| `signal_mesh_store_read_total{store}` | bounded prefix reads |
| `signal_mesh_store_records_read_total{store}` | records returned — the **denominator** for the index-prune claim |
| `signal_mesh_store_checkpoint_total{store}` | durability barriers crossed |
| `signal_mesh_a2a_card_served_total` / `…card_refused_total{reason}` | card requests |
| `signal_mesh_a2a_task_total{outcome}` | `submitted` / `completed` / `interrupted` / `refused-terminal` |
| `signal_mesh_a2a_version_refused_total` | incompatible peers |

**What to alert on:** `refused_total{reason="engine"}` rising, and
`checkpoint_total` **flat while `append_total` climbs** — that is the unsealed
tail growing with nothing pushing it to the substrate.

---

## 8. Durability — read this before sizing anything

⚠⚠ **Two failure modes, and only one is free.**

| failure | recovery | needs a checkpoint? |
| :-- | :-- | :-- |
| the **process** died, disk intact | re-open the same root | no |
| the **node** died, disk gone | cold-load from the substrate | **yes** |

The engine seals at **1024 records / 8 MiB**; one cascade over the shipped
fixture appends **29**. So without an explicit `checkpoint()` a small mesh has
written *nothing* to the substrate and a cold load fails outright. The mesh
inherits EHDB's unsealed-tail property (RF=1); it does not fix it and must not
claim to.

---

## 9. Rollback

| change | rollback |
| :-- | :-- |
| the whole component | unset `NOETL_SIGNAL_MESH` — exits 0, binds nothing |
| durable store | unset `NOETL_SIGNAL_MESH_STORE`; the in-memory path stays compiled and tested |
| A2A surface | unset `NOETL_SIGNAL_MESH_A2A`; the routes cease to exist |
| metrics listener | unset `NOETL_SIGNAL_MESH_METRICS_ADDR` |

Every rollback is an env change and a restart. No migration, no data rewrite —
the log is append-only.

---

## 10. Validation before any cluster

1. `cargo test` — 40 tests, deterministic, no network, no socket bound.
2. `cargo run --bin signal-mesh-demo` — must print **52.5000 / true**.
3. Start `signal-mesh-serve` with `_A2A=serve` and curl the card, submit a task
   with `needs=input`, resume it, and read `/metrics`.
4. ⚠ **Kill the process and re-curl.** A probe that keeps answering after the
   server dies is measuring something else.
5. kind before prod. ⚠ kind **cannot** measure dispatch latency — a saturating
   burst measures the pool, not the bus.

---

## 11. Related

- [`production-implementation-plan.md`](production-implementation-plan.md) — M1–M9, forks, the MVP cut
- [`architecture/a2a-signal-mesh-blueprint.md`](architecture/a2a-signal-mesh-blueprint.md) — the team reference
- [`spec/a2a-react-signal-mesh.md`](spec/a2a-react-signal-mesh.md) — §11, what the POC does **not** prove
