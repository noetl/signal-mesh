# signal-mesh

A tiered **A2A / ReAct** agent mesh over an [EHDB](https://github.com/noetl/ehdb)-shaped
event log.

Two documents live here, and they are not the same document:

| | |
| :-- | :-- |
| [`docs/architecture/a2a-signal-mesh-blueprint.md`](docs/architecture/a2a-signal-mesh-blueprint.md) | **The team blueprint.** Diagram-forward. Start here. Source of truth — the copy on the wiki is downstream of this file. |
| [`docs/spec/a2a-react-signal-mesh.md`](docs/spec/a2a-react-signal-mesh.md) | **The implementation/proof spec.** Grounding, `file:line` evidence, and what the POC does and does not prove (§11). |

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
