# signal-mesh

A tiered **A2A / ReAct** agent mesh over an [EHDB](https://github.com/noetl/ehdb)-shaped
event log.

Two documents live here, and they are not the same document:

| | |
| :-- | :-- |
| [`docs/architecture/a2a-signal-mesh-blueprint.md`](docs/architecture/a2a-signal-mesh-blueprint.md) | **The team blueprint.** Diagram-forward. Start here. Source of truth — the copy on the wiki is downstream of this file. |
| [`docs/spec/a2a-react-signal-mesh.md`](docs/spec/a2a-react-signal-mesh.md) | **The implementation/proof spec.** Grounding, `file:line` evidence, and what the POC does and does not prove (§11). |
| [`docs/production-implementation-plan.md`](docs/production-implementation-plan.md) | **The production scope.** How the POC becomes deployable: M1–M9, each flag-gated and default-off, with a dependency graph, a flag matrix, and an MVP cut. Plan only — nothing built. |

⚠ **POC only.** Not wired into any production binary, not deployed, nothing
enabled. It executes no generated code. Read §11 of the spec — *"what the POC
does NOT prove"* — before drawing conclusions from it: there is no durability,
no real A2A transport, and no scale evidence.

## Relationship to EHDB

This crate depends on `ehdb-core` as a **library**, pinned by git tag:

```toml
ehdb-core = { git = "https://github.com/noetl/ehdb", tag = "v0.3.0" }
```

The NoETL crates are not published to crates.io, so a git dependency at a tag
is the pin. It is a tag and not a branch on purpose — a branch dependency makes
every build a different build.

## Run it

```bash
cargo run --bin signal-mesh-demo
```

Deterministic: same output every run. No clock, no network, no model.

You should see, in order:

1. **Agent Cards published** — four agents across three tiers, each with a
   content digest, and the RFC 8615 well-known path.
2. **Collector appends** six device signals (two signal classes).
3. **The cascade** at a named watermark, tier by tier.
4. **The ReAct trace** — `observe` / `reason` / `act` for every agent, each one
   an event in the log.
5. **The verdict** — ONE number and ONE boolean.
6. **A replay check** — re-folding the same prefix reproduces the same context.

Expected verdict with the shipped fixture: `52.5000`, `true` (threshold `50.0`).

## Durability (M1)

By default the mesh keeps its log in memory, exactly as the POC did. Set the
store selector to put it in a real EHDB `L0Engine`:

```bash
NOETL_SIGNAL_MESH_STORE=ehdb   # anything else, including unset, means memory
```

⚠ **Two different failure modes, and only one of them is free:**

| failure | recovery | needs `checkpoint()`? |
| :-- | :-- | :-- |
| the **process** died, disk intact | re-open the same root | no |
| the **node** died, disk gone | cold-load from the substrate | **yes** |

The engine seals at 1024 records / 8 MiB by default, and one cascade over the
shipped fixture appends 29 — so without an explicit `checkpoint()` a small mesh
has written **nothing** to the substrate and a cold load fails outright. That is
the unsealed-tail property, and `tests/m1_persistence.rs` asserts *both* halves:
the cold load fails without a checkpoint and reproduces the verdict with one.

## A2A transport (M2)

Off by default. `off` means **no routes**, not a handler that declines:

```bash
NOETL_SIGNAL_MESH_A2A=serve \
NOETL_SIGNAL_MESH_A2A_TOKEN=dev-token \
cargo run --bin signal-mesh-serve

curl -H 'Authorization: Bearer dev-token' \
     localhost:8787/.well-known/agent-card.json
```

| route | |
| :-- | :-- |
| `GET /.well-known/agent-card.json` | the card, projected from a catalog entry (RFC 8615) |
| `POST /a2a/tasks` | submit; negotiates on `Major.Minor` |
| `POST /a2a/tasks/{id}/resume` | ⭐ resume an **interrupted** task |
| `GET /metrics` | counters, every series pinned at 0 |

⚠ `input-required` and `auth-required` are **interrupted, not terminal** — they
resume. Treating them as failures is the most common way a dispatcher
mis-implements A2A, and `tests/m2_transport.rs` fails if you do.

⚠ A card declaring **no** security scheme is refused rather than served: "no
scheme" means *not publishable outside the cluster*, not *open*.

## Operability (M9)

```bash
NOETL_SIGNAL_MESH_METRICS_ADDR=127.0.0.1:9797   # unset = no listener at all
```

⭐ Independent of `_A2A` on purpose: before M9 the only `/metrics` rode the A2A
router, so a deployment could not be observed without also exposing its agent
surface.

Every series is pinned at 0 **for both stores**, including the one that is not
configured — absent reads exactly like zero. `signal_mesh_build_info` is always
1, so *"does this pod predate that metric?"* is answerable from the scrape
rather than from an image tag.

📄 **[`docs/deployment-specification.md`](docs/deployment-specification.md)** —
the runtime contract: every env var with the *why*, ports, sizing, probes,
durability, rollback. ⚠ A test fails the build if a declared variable is
undocumented there, or documented and read nowhere.

## Test it

```bash
cargo test
```

Deterministic, no network. Includes `tests/target_hygiene.rs`, which asserts
every declared build target resolves to a **git-tracked** file — the guard for
the failure class where a crate builds on the machine that wrote it and fails
on every clean checkout. See the header comment in that file for the history.

## History

Split out of [`noetl/ehdb`](https://github.com/noetl/ehdb) on 2026-09-21 with
`git filter-repo`; the six commits that built the crate and the two documents
are preserved here with their original authorship and dates.
