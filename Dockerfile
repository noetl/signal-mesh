# signal-mesh — the A2A surface and its durable store.
#
# ⚠ Two stages. The builder needs the full toolchain plus git (ehdb-core and
# ehdb-l0 are git dependencies pinned to tag v0.3.0, not crates.io packages),
# and none of that belongs in the runtime.
FROM rust:1.90-bookworm AS build
WORKDIR /src

# git for the tagged ehdb dependencies; ca-certificates so the fetch verifies.
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests

# --locked, so the image is built against the committed Cargo.lock and not
# whatever the registry resolves today. A build that silently re-resolves is a
# build whose dependency set is a function of the clock.
RUN cargo build --release --locked --bin signal-mesh-serve \
 && strip target/release/signal-mesh-serve

FROM debian:bookworm-slim AS runtime

# ⚠ ca-certificates only. No curl, no shell tooling beyond the base — the fleet
# has repeatedly read "curl returned 0" as a healthy probe when curl was simply
# absent, so this image does not pretend to carry one.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# Non-root, and the uid is fixed so a PVC's ownership is predictable across
# restarts rather than whatever the runtime happens to assign.
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin mesh
COPY --from=build /src/target/release/signal-mesh-serve /usr/local/bin/signal-mesh-serve

USER 10001:10001
# ⚠ No ENV defaults for any NOETL_SIGNAL_MESH_* variable, deliberately. The
# binary fails closed on every one it needs, and an ENV default here would be a
# second place for a value to come from — the exact drift the deployment spec
# exists to prevent. In particular there is no default store root: an unmounted
# default silently becomes the container's writable layer.
ENTRYPOINT ["/usr/local/bin/signal-mesh-serve"]
