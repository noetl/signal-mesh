//! **M9 — operability.**
//!
//! Counters for the store paths, and one place that renders every series.
//!
//! # ⚠⚠ Counters are NOT flag-gated, and that is the point
//!
//! A counter behind a flag is **absent** when the flag is off, and an absent
//! series reads exactly like a healthy one on a scrape. So the counting is
//! always on and free; what M9 gates is the *listener* that exposes it
//! ([`METRICS_ADDR_ENV`], unset by default).
//!
//! Every series is **pinned at 0 at construction, unconditionally** — not
//! inside `if store_is_ehdb`, which would leave the dedupe counter missing on
//! precisely the configuration whose zero an operator is reading.
//!
//! # Why a separate listener at all
//!
//! Before M9 the only `/metrics` was on the A2A router, so a deployment could
//! not be observed without also exposing its agent surface. Those are different
//! decisions and now they are different flags.

use crate::transport::{A2aMode, Counters as TransportCounters};
use std::sync::atomic::{AtomicU64, Ordering};

/// `NOETL_SIGNAL_MESH_METRICS_ADDR` — bind a standalone metrics listener.
/// **Unset by default, which binds nothing.**
pub const METRICS_ADDR_ENV: &str = "NOETL_SIGNAL_MESH_METRICS_ADDR";

/// Store-path counters.
#[derive(Debug, Default)]
pub struct StoreCounters {
    /// Records actually written.
    pub appended: AtomicU64,
    /// ⭐ Idempotent redeliveries acknowledged at an existing position. A
    /// working dedupe and a broken writer both show a flat `appended`; only
    /// this series tells them apart (ai-meta#313).
    pub deduped: AtomicU64,
    /// Writes refused for exceeding the payload ceiling.
    pub refused_too_large: AtomicU64,
    /// Writes refused by the engine.
    pub refused_engine: AtomicU64,
    /// Serialisation / deserialisation failures.
    pub refused_codec: AtomicU64,
    /// Bounded prefix reads performed.
    pub reads: AtomicU64,
    /// Records returned by those reads — the denominator for the prune claim.
    pub records_read: AtomicU64,
    /// Durability barriers crossed.
    pub checkpoints: AtomicU64,
}

impl StoreCounters {
    /// Explicit zeros rather than `Default::default()`, so the pin is a visible
    /// decision a reader can check against [`Self::render`].
    pub fn new() -> Self {
        Self {
            appended: AtomicU64::new(0),
            deduped: AtomicU64::new(0),
            refused_too_large: AtomicU64::new(0),
            refused_engine: AtomicU64::new(0),
            refused_codec: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            records_read: AtomicU64::new(0),
            checkpoints: AtomicU64::new(0),
        }
    }

    pub(crate) fn incr(c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub(crate) fn add(c: &AtomicU64, n: u64) {
        c.fetch_add(n, Ordering::Relaxed);
    }
    fn g(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// Prometheus text exposition for the store paths.
    ///
    /// ⚠ `store` is a label rather than a separate metric name, so a dashboard
    /// does not have to know which backend is configured to find the series.
    pub fn render(&self, store: &str) -> String {
        format!(
            "# HELP signal_mesh_store_append_total Records written, by outcome.\n\
             # TYPE signal_mesh_store_append_total counter\n\
             signal_mesh_store_append_total{{store=\"{store}\",outcome=\"appended\"}} {}\n\
             signal_mesh_store_append_total{{store=\"{store}\",outcome=\"deduped\"}} {}\n\
             # HELP signal_mesh_store_refused_total Writes refused, by reason.\n\
             # TYPE signal_mesh_store_refused_total counter\n\
             signal_mesh_store_refused_total{{store=\"{store}\",reason=\"payload-too-large\"}} {}\n\
             signal_mesh_store_refused_total{{store=\"{store}\",reason=\"engine\"}} {}\n\
             signal_mesh_store_refused_total{{store=\"{store}\",reason=\"codec\"}} {}\n\
             # TYPE signal_mesh_store_read_total counter\n\
             signal_mesh_store_read_total{{store=\"{store}\"}} {}\n\
             # HELP signal_mesh_store_records_read_total Records returned by bounded reads.\n\
             # TYPE signal_mesh_store_records_read_total counter\n\
             signal_mesh_store_records_read_total{{store=\"{store}\"}} {}\n\
             # HELP signal_mesh_store_checkpoint_total Durability barriers crossed.\n\
             # TYPE signal_mesh_store_checkpoint_total counter\n\
             signal_mesh_store_checkpoint_total{{store=\"{store}\"}} {}\n",
            Self::g(&self.appended),
            Self::g(&self.deduped),
            Self::g(&self.refused_too_large),
            Self::g(&self.refused_engine),
            Self::g(&self.refused_codec),
            Self::g(&self.reads),
            Self::g(&self.records_read),
            Self::g(&self.checkpoints),
        )
    }

    /// ⚠⚠ **Pin BOTH stores, always.** The `store` label is a closed set of two,
    /// so an operator reading `store="ehdb"` on a memory-configured process must
    /// see `0`, not nothing. Pinning only the configured one reintroduces the
    /// absent-is-not-zero bug on exactly the value someone is checking.
    pub fn render_all_stores(&self, active: &str) -> String {
        let mut out = String::new();
        for store in ["memory", "ehdb"] {
            if store == active {
                out.push_str(&self.render(store));
            } else {
                out.push_str(&StoreCounters::new().render(store));
            }
        }
        out
    }
}

/// Everything this process exposes, in one body.
pub fn render_all(
    store: &StoreCounters,
    active_store: &str,
    transport: &TransportCounters,
    mode: A2aMode,
) -> String {
    let mut s = transport.render(mode);
    s.push_str(&store.render_all_stores(active_store));
    s
}
