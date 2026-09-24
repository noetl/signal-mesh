//! **M1 — where mesh events actually live.**
//!
//! The POC kept its log in a `Vec` (`mesh.rs:8`, *"An in-memory stand-in for
//! EHDB's D1 event log"*). This module puts the same events in a real
//! [`L0Engine`], so an answer survives the process that produced it.
//!
//! # The store is a seam, not a replacement
//!
//! [`MemoryStore`] and [`EhdbStore`] both implement [`MeshStore`], and the
//! in-memory one stays the **default**. That is not sentiment: a rollback for
//! `NOETL_SIGNAL_MESH_STORE` has to be a path that is still compiled and still
//! tested, or it is a plan rather than a rollback.
//!
//! # Three decisions worth stating
//!
//! **1. Its own dataset, never the platform's D1.** `MeshDataset::NAME` is
//! `mesh_event_log`, not `d1_event_log`. The production plan's fork F-5 settled
//! this, and it also dissolves the plan's §2.4 blocker (*"can a non-server
//! process share D1 with the server"*): the mesh never opens the platform's
//! dataset, so the question does not arise for the MVP.
//!
//! **2. `index_key` is the stream, not an execution** (plan fork F-1). That is
//! what makes [`L0Engine::read_index_after`] a pruned prefix read rather than a
//! full scan, and it is why the mesh does not need per-agent storage of its own.
//! ⚠ With the MVP's single fixed stream the prune is *exercised but not
//! stressed* — [`tests/m1_persistence.rs`] proves it discriminates between two
//! streams; proving it scales is M7's job, not this module's claim.
//!
//! **3. The payload ceiling is 1 MiB, which is the smaller of two.** L0 frames
//! cap at 64 MiB (`ehdb-l0` `frame.rs:25`), but the worker's event-log client
//! caps one payload at 1 MiB (`noetl/worker` `src/ehdb/eventlog.rs:72`). A write
//! path sized against the engine would pass every local test and fail the day it
//! went through the worker. We enforce the **smaller** one, here, at the seam —
//! and [`MAX_PAYLOAD_BYTES`] carries the reason so nobody raises it to 64 MiB
//! because "the engine allows it".

use crate::event::{MeshEvent, Record};
use crate::metrics::StoreCounters;
use ehdb_l0::dataset::{shard_for_execution, Dataset};
use ehdb_l0::engine::{L0Config, L0Engine};
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

/// `NOETL_SIGNAL_MESH_STORE` — `memory` (default) or `ehdb`.
pub const STORE_ENV: &str = "NOETL_SIGNAL_MESH_STORE";

/// `NOETL_SIGNAL_MESH_STORE_ROOT` — where [`EhdbStore`] keeps its substrate and
/// local parts. Required when the store is `ehdb`; there is deliberately **no
/// default path**.
///
/// ⚠⚠ A default would be the `/data` trap the fleet already paid for:
/// `LocalFsSubstrate::new` calls `create_dir_all`, so an unmounted path
/// silently becomes the container's ephemeral layer and the engine looks
/// healthy right up to the eviction. Refusing to start is the honest behaviour.
pub const STORE_ROOT_ENV: &str = "NOETL_SIGNAL_MESH_STORE_ROOT";

/// `NOETL_SIGNAL_MESH_CHECKPOINT_SECS` — how often to cross the durability
/// barrier. **Required when the store is `ehdb`; there is no default.**
///
/// ⚠⚠ **Why a TIMER and not a lower `seal_max_records`.** Both bound the
/// unsealed tail, but in different units, and only one matches the risk.
///
/// `seal_max_records` bounds it in **appends**: the part seals once N records
/// land. The dangerous case is precisely when appends *stop* — a half-full part
/// then sits on one local disk indefinitely, and the quieter the mesh the worse
/// the exposure. `ehdb-l0` already demonstrates the trap: `seal_max_age` exists
/// but `seal_aged_parts` is only consulted on append, so the age trigger is
/// inert on exactly the shard it was added for unless something drives it.
///
/// A timer bounds it in **seconds**, which is the unit "how much can node loss
/// cost me" is actually measured in, and it is independent of traffic. So the
/// mesh drives `checkpoint()` on an interval and states the window plainly:
/// lose the node, lose at most this many seconds of appends.
pub const CHECKPOINT_SECS_ENV: &str = "NOETL_SIGNAL_MESH_CHECKPOINT_SECS";

/// ⚠⚠ **The smaller of the two ceilings, on purpose.**
///
/// `ehdb-l0` accepts a 64 MiB frame (`frame.rs:25` `MAX_FRAME_BODY_BYTES`). The
/// worker's event-log client refuses a payload over 1 MiB
/// (`noetl/worker` `src/ehdb/eventlog.rs:72` `MAX_PAYLOAD_BYTES_CEILING`).
///
/// Enforcing 64 MiB here would make every local test pass and fail only once a
/// record travelled through the worker — which is ai-meta#343's shape, one cap
/// serving two directions. Enforce the tighter one at the seam, refuse loudly,
/// and let the caller chunk.
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576;

/// Why a write did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The serialized payload exceeds [`MAX_PAYLOAD_BYTES`].
    PayloadTooLarge { bytes: usize, cap: usize },
    /// The engine refused. Carries its message rather than swallowing it: a
    /// blanket `Err(_) => Ok(None)` is how a guard stops guarding.
    Engine(String),
    /// The payload could not be serialized or read back.
    Codec(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::PayloadTooLarge { bytes, cap } => write!(
                f,
                "mesh payload {bytes} B exceeds the {cap} B ceiling (the worker \
                 event-log client's cap, which is tighter than the L0 frame's) \
                 — chunk the record, do not raise the cap"
            ),
            StoreError::Engine(m) => write!(f, "l0 engine refused: {m}"),
            StoreError::Codec(m) => write!(f, "mesh payload codec: {m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Which backing store a deployment selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StoreKind {
    /// The POC's `Vec`. **Default**, and it stays compiled so the rollback is real.
    #[default]
    Memory,
    /// A real `L0Engine` over a durable substrate.
    Ehdb,
}

/// Resolve [`STORE_ENV`]. Pure over the raw value so the default is testable
/// without touching process env — and so the "only the exact string arms it"
/// convention is a property a mutation can be written against.
///
/// ⚠ Unlike the boolean flags, this one is a *selector*, so an unrecognised
/// value must not silently mean `Ehdb`. Anything that is not exactly `"ehdb"`
/// is [`StoreKind::Memory`].
pub fn store_kind(raw: Option<&str>) -> StoreKind {
    match raw.map(str::trim) {
        Some("ehdb") => StoreKind::Ehdb,
        _ => StoreKind::Memory,
    }
}

/// One mesh record as the engine stores it.
///
/// ⚠ `stream` rather than `execution_id`: D1 reuses `execution_id` as its index
/// dimension, but a mesh record does not belong to an execution. Naming the
/// field for what it is keeps the dataset honest instead of borrowing a column
/// whose meaning is somebody else's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeshRecord {
    /// The engine-assigned sort key.
    pub seq: u64,
    /// The agent stream — partition **and** index dimension (fork F-1).
    pub stream: String,
    /// The serialized [`MeshEvent`].
    pub payload: String,
    /// **The idempotency key.** A redelivery with the same key is acknowledged
    /// at its existing position rather than appended twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

/// The mesh's own dataset. ⚠ **Not D1** — see the module header.
#[derive(Debug, Clone, Copy)]
pub struct MeshDataset;

/// Dataset id. Deliberately not `d1_event_log`.
pub const DATASET_MESH_EVENT_LOG: &str = "mesh_event_log";

impl Dataset for MeshDataset {
    type Record = MeshRecord;
    const NAME: &'static str = DATASET_MESH_EVENT_LOG;

    fn sort_key(r: &MeshRecord) -> u64 {
        r.seq
    }
    fn partition(r: &MeshRecord, shard_count: u32) -> u32 {
        shard_for_execution(&r.stream, shard_count)
    }
    fn index_key(r: &MeshRecord) -> &str {
        &r.stream
    }
    fn read_partition(stream: &str, shard_count: u32) -> u32 {
        shard_for_execution(stream, shard_count)
    }
    /// Opt in to dedupe. Without this the engine has no idempotency key and a
    /// redelivered signal appends a second time, silently.
    fn dedupe_key(r: &MeshRecord) -> Option<&str> {
        r.event_id.as_deref()
    }
    /// The writer owns the ordering key.
    fn assign_sort_key(mut r: MeshRecord, writer_seq: u64) -> MeshRecord {
        r.seq = writer_seq;
        r
    }
}

/// What the cascade needs from a log, and nothing more.
///
/// Kept this narrow on purpose: every method here is one the cascade already
/// called on the POC's `Vec`, so swapping the implementation cannot quietly
/// change what the cascade does.
/// ⚠ `Send` is required so the checkpoint driver can own a store on its own
/// thread. It is deliberately NOT `Sync`: nothing shares a store across threads,
/// and requiring `Sync` would push every implementation toward interior
/// mutability it does not need.
pub trait MeshStore: Send {
    fn stream(&self) -> &str;

    /// Append one event. `event_id` is the idempotency key; `None` opts that
    /// record out of dedupe, exactly as a D1 record without one does.
    fn append_with_id(
        &mut self,
        payload: MeshEvent,
        event_id: Option<&str>,
    ) -> Result<u64, StoreError>;

    /// Every record in this stream at or below `up_to_seq`, ascending.
    ///
    /// ⚠ This is the **bounded** read the whole design rests on. It is a
    /// prefix, never a tail, and the caller names the bound.
    fn records_up_to(&self, up_to_seq: u64) -> Result<Vec<Record>, StoreError>;

    /// Highest sequence this store has assigned.
    fn head(&self) -> u64;

    /// How many records this stream holds. ⚠ Distinct from [`Self::head`]: a
    /// deduplicated redelivery advances neither, but a gap in the sequence
    /// would separate them, and conflating the two is how a working dedupe gets
    /// reported as a parity divergence (ai-meta#313).
    fn record_count(&self) -> Result<usize, StoreError>;

    /// Append without an idempotency key.
    fn append(&mut self, payload: MeshEvent) -> Result<u64, StoreError> {
        self.append_with_id(payload, None)
    }

    /// This store's counters. ⚠ On the trait rather than the concrete types so
    /// a renderer does not have to know which backend is configured — which is
    /// the same reason `store` is a metric label and not a metric name.
    fn counters(&self) -> &StoreCounters;

    /// The value of the `store` label for this implementation.
    fn label(&self) -> &'static str;

    /// Push everything written so far past the durability barrier.
    ///
    /// ⚠ The default is a **no-op, and honestly so**: `MemoryStore` has no
    /// durability to reach, and a default that pretended otherwise would make
    /// "checkpointed" mean nothing on the default path. `EhdbStore` overrides
    /// it — see [`EhdbStore::checkpoint`] for why a small mesh needs it.
    fn checkpoint(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Serialized size gate, shared by both stores so the ceiling cannot differ
/// between the default path and the one under test.
fn encode(payload: &MeshEvent, c: &StoreCounters) -> Result<String, StoreError> {
    let s = serde_json::to_string(payload).map_err(|e| {
        StoreCounters::incr(&c.refused_codec);
        StoreError::Codec(e.to_string())
    })?;
    if s.len() > MAX_PAYLOAD_BYTES {
        // ⚠ Counted at the refusal, not at the call site. A caller that forgets
        // to count is a caller whose refusals are invisible.
        StoreCounters::incr(&c.refused_too_large);
        return Err(StoreError::PayloadTooLarge {
            bytes: s.len(),
            cap: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(s)
}

/// The POC's log, behind the trait. **Default.**
#[derive(Debug, Default)]
pub struct MemoryStore {
    pub stream: String,
    pub records: Vec<Record>,
    seen: Vec<String>,
    next_seq: u64,
    counters: Arc<StoreCounters>,
}

impl MemoryStore {
    /// Share this store's counters with a metrics listener.
    pub fn with_counters(mut self, c: Arc<StoreCounters>) -> Self {
        self.counters = c;
        self
    }

    pub fn new(stream: &str) -> Self {
        Self {
            stream: stream.to_string(),
            records: Vec::new(),
            seen: Vec::new(),
            next_seq: 1,
            counters: Arc::new(StoreCounters::new()),
        }
    }
}

impl MeshStore for MemoryStore {
    fn stream(&self) -> &str {
        &self.stream
    }

    fn append_with_id(
        &mut self,
        payload: MeshEvent,
        event_id: Option<&str>,
    ) -> Result<u64, StoreError> {
        // The size gate runs on BOTH paths. A ceiling enforced only by the
        // engine would not exist in the default configuration.
        let _ = encode(&payload, &self.counters)?;
        if let Some(id) = event_id {
            if let Some(pos) = self.seen.iter().position(|s| s == id) {
                // Acknowledge at the existing position — the same contract as
                // `L0Engine::append_record_reporting` returning `appended=false`.
                StoreCounters::incr(&self.counters.deduped);
                return Ok(self.records[pos].seq);
            }
            self.seen.push(id.to_string());
        } else {
            self.seen.push(String::new());
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.records.push(Record {
            seq,
            stream: self.stream.clone(),
            payload,
        });
        StoreCounters::incr(&self.counters.appended);
        Ok(seq)
    }

    fn records_up_to(&self, up_to_seq: u64) -> Result<Vec<Record>, StoreError> {
        let out: Vec<Record> = self
            .records
            .iter()
            .filter(|r| r.seq <= up_to_seq)
            .cloned()
            .collect();
        StoreCounters::incr(&self.counters.reads);
        StoreCounters::add(&self.counters.records_read, out.len() as u64);
        Ok(out)
    }

    fn head(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    fn record_count(&self) -> Result<usize, StoreError> {
        Ok(self.records.len())
    }

    fn counters(&self) -> &StoreCounters {
        &self.counters
    }

    fn label(&self) -> &'static str {
        "memory"
    }

    fn checkpoint(&mut self) -> Result<(), StoreError> {
        // ⚠ Counted even though it does nothing. An operator comparing the two
        // stores needs to see that the barrier was REACHED and was a no-op,
        // which is different from never having been called.
        StoreCounters::incr(&self.counters.checkpoints);
        Ok(())
    }
}

/// A real `L0Engine` over a durable substrate.
pub struct EhdbStore {
    stream: String,
    engine: L0Engine<MeshDataset>,
    head: u64,
    counters: Arc<StoreCounters>,
}

impl EhdbStore {
    /// Open (or re-open) the mesh's dataset rooted at `root`.
    ///
    /// ⚠ `shard_count` stays at the engine default (1). The MVP is one fixed
    /// stream, so more shards would add no pruning and would hide the fact that
    /// sharding is M5's milestone, not a thing that happened by accident here.
    pub fn open(stream: &str, root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref();
        let substrate: Arc<dyn DurableSubstrate> = Arc::new(
            LocalFsSubstrate::new(root.join("substrate"))
                .map_err(|e| StoreError::Engine(e.to_string()))?,
        );
        let config = L0Config::for_dataset(DATASET_MESH_EVENT_LOG, root.join("local"));
        let engine = L0Engine::<MeshDataset>::open(config, substrate)
            .map_err(|e| StoreError::Engine(e.to_string()))?;
        let mut s = Self {
            stream: stream.to_string(),
            engine,
            head: 0,
            counters: Arc::new(StoreCounters::new()),
        };
        s.head = s.recover_head()?;
        Ok(s)
    }

    /// **Cold-load** the same dataset into a fresh engine — the restart path.
    ///
    /// ⭐ This is the M1 acceptance criterion's mechanism. `cold_load`
    /// *"Reproduces the exact record set + global sequence of the origin"*
    /// (`ehdb-l0` `engine.rs:524`), which is precisely "the answer survived the
    /// process that produced it".
    pub fn cold_load(stream: &str, root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = root.as_ref();
        let substrate: Arc<dyn DurableSubstrate> = Arc::new(
            LocalFsSubstrate::new(root.join("substrate"))
                .map_err(|e| StoreError::Engine(e.to_string()))?,
        );
        let config = L0Config::for_dataset(DATASET_MESH_EVENT_LOG, root.join("cold"));
        let engine = L0Engine::<MeshDataset>::cold_load(config, substrate)
            .map_err(|e| StoreError::Engine(e.to_string()))?;
        let mut s = Self {
            stream: stream.to_string(),
            engine,
            head: 0,
            counters: Arc::new(StoreCounters::new()),
        };
        s.head = s.recover_head()?;
        Ok(s)
    }

    /// Highest sequence already in this stream. ⚠ Derived from the log, never
    /// carried in a counter beside it — a counter is a second copy, and the
    /// copy is the one that goes wrong.
    fn recover_head(&self) -> Result<u64, StoreError> {
        Ok(self.raw(0)?.last().map(|r| r.seq).unwrap_or(0))
    }

    /// The pruned indexed prefix read. This is the call fork F-1 exists for.
    fn raw(&self, after_seq: u64) -> Result<Vec<MeshRecord>, StoreError> {
        self.engine
            .read_index_after(&self.stream, after_seq)
            .map_err(|e| StoreError::Engine(e.to_string()))
    }

    /// Share this store's counters with a metrics listener.
    pub fn with_counters(mut self, c: Arc<StoreCounters>) -> Self {
        self.counters = c;
        self
    }

    /// ⭐⭐ **The durability barrier.** Seal every pending part and block until
    /// the uploader has shipped them to the substrate.
    ///
    /// ⚠⚠ **Without this, a small mesh is not durable beyond its own disk, and
    /// nothing says so.** `ehdb-l0` seals on 1024 records / 8 MiB by default
    /// (`engine.rs:53,55`); one cascade over the shipped fixture appends ~29.
    /// So the records are fsynced locally and the substrate has **no manifest
    /// at all** — `cold_load` fails with *"no durable manifest for dataset
    /// mesh_event_log"*. That is the unsealed-tail property the production plan
    /// flagged (RF=1, window unbounded by default), met head-on in the first
    /// test that asked for it.
    ///
    /// So the mesh distinguishes two failure modes rather than conflating them:
    ///
    /// | failure | recovery | needs a checkpoint? |
    /// | :-- | :-- | :-- |
    /// | the **process** died, disk intact | [`EhdbStore::open`] on the same root | no |
    /// | the **node** died, disk gone | [`EhdbStore::cold_load`] from the substrate | **yes** |
    ///
    /// `flush_and_wait_uploads` is documented as *"a durability barrier — used
    /// before a cold-load equality check"* (`engine.rs:1008`), which is exactly
    /// this call site.
    pub fn checkpoint_impl(&mut self) -> Result<(), StoreError> {
        let r = self
            .engine
            .flush_and_wait_uploads()
            .map_err(|e| StoreError::Engine(e.to_string()));
        if r.is_ok() {
            StoreCounters::incr(&self.counters.checkpoints);
        }
        r
    }

    /// Which partition this stream reads from — exposed so a test can assert
    /// the prune is a real partition decision rather than a filter.
    pub fn shard(&self) -> u32 {
        self.engine.shard_for(&self.stream)
    }
}

impl MeshStore for EhdbStore {
    fn stream(&self) -> &str {
        &self.stream
    }

    fn append_with_id(
        &mut self,
        payload: MeshEvent,
        event_id: Option<&str>,
    ) -> Result<u64, StoreError> {
        let encoded = encode(&payload, &self.counters)?;
        let record = MeshRecord {
            // Overwritten by `assign_sort_key`; the writer owns the order.
            seq: 0,
            stream: self.stream.clone(),
            payload: encoded,
            event_id: event_id.map(str::to_string),
        };
        // ⚠⚠ `append_writer_assigned_reporting`, not `append_record`. The plain
        // return cannot distinguish "appended" from "already present", and the
        // difference is load-bearing: a deduplicated redelivery returns an OLDER
        // position and does not advance the count (ai-meta#313). We need the
        // writer-assigned form because the mesh does not mint its own sequences.
        let (seq, appended) = self
            .engine
            .append_writer_assigned_reporting(record)
            .map_err(|e| {
                StoreCounters::incr(&self.counters.refused_engine);
                StoreError::Engine(e.to_string())
            })?;
        if appended {
            self.head = self.head.max(seq);
            StoreCounters::incr(&self.counters.appended);
        } else {
            // ⭐ The series that separates a working dedupe from a broken writer.
            StoreCounters::incr(&self.counters.deduped);
        }
        Ok(seq)
    }

    fn records_up_to(&self, up_to_seq: u64) -> Result<Vec<Record>, StoreError> {
        let mut out = Vec::new();
        for r in self.raw(0)? {
            if r.seq > up_to_seq {
                continue;
            }
            let payload: MeshEvent = serde_json::from_str(&r.payload).map_err(|e| {
                StoreCounters::incr(&self.counters.refused_codec);
                StoreError::Codec(e.to_string())
            })?;
            out.push(Record {
                seq: r.seq,
                stream: r.stream,
                payload,
            });
        }
        out.sort_by_key(|r| r.seq);
        StoreCounters::incr(&self.counters.reads);
        StoreCounters::add(&self.counters.records_read, out.len() as u64);
        Ok(out)
    }

    fn head(&self) -> u64 {
        self.head
    }

    fn record_count(&self) -> Result<usize, StoreError> {
        Ok(self.raw(0)?.len())
    }

    fn counters(&self) -> &StoreCounters {
        &self.counters
    }

    fn label(&self) -> &'static str {
        "ehdb"
    }

    fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.checkpoint_impl()
    }
}
