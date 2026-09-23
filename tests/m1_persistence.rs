//! **M1 acceptance — real EHDB persistence.**
//!
//! The claim under test is one sentence: *an answer written by the mesh
//! survives the process that produced it.* Everything here exists to make that
//! checkable rather than plausible.
//!
//! ⚠ Every test that reports a count **prints it**. A guard that says "fewer"
//! without saying fewer-than-what is consistent with having measured nothing.

use signal_mesh::event::{AggregateEmitted, MeshEvent, SignalObserved};
use signal_mesh::fold::{fold, Reduction};
use signal_mesh::mesh::{Agent, Mesh};
use signal_mesh::react::DeterministicReasoner;
use signal_mesh::store::{
    store_kind, EhdbStore, MemoryStore, MeshStore, StoreError, StoreKind, MAX_PAYLOAD_BYTES,
};

/// A scratch directory that cleans itself up.
struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "sm-m1-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The demo's fixture, so the durable path is compared against a number that
/// is already pinned in the README and the spec.
fn fixture() -> Vec<Agent> {
    vec![
        Agent {
            id: "t0-temp".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("temp".into()),
        },
        Agent {
            id: "t0-vibe".into(),
            tier: 0,
            how: Reduction::WeightedMean,
            children: vec![],
            signal_class: Some("vibe".into()),
        },
        Agent {
            id: "t1-site".into(),
            tier: 1,
            how: Reduction::WeightedMean,
            children: vec!["t0-temp".into(), "t0-vibe".into()],
            signal_class: None,
        },
        Agent {
            id: "t2-fleet".into(),
            tier: 2,
            how: Reduction::Max,
            children: vec!["t1-site".into()],
            signal_class: None,
        },
    ]
}

fn feed(m: &mut Mesh) {
    for (i, (dev, class, val)) in [
        ("dev-01", "temp", 41.0),
        ("dev-02", "temp", 47.0),
        ("dev-03", "temp", 44.0),
        ("dev-04", "vibe", 61.0),
        ("dev-05", "vibe", 59.0),
        ("dev-06", "vibe", 63.0),
    ]
    .iter()
    .enumerate()
    {
        m.observe_signal(dev, class, *val, i as u64 + 1)
            .expect("collector append");
    }
}

// ---------------------------------------------------------------- AC 1

/// **AC1 — the durable path and the in-memory path agree.**
///
/// If they disagree, one of them is wrong and the POC's pinned verdict tells
/// us which.
#[test]
fn ehdb_backed_cascade_matches_the_in_memory_verdict() {
    let tmp = Tmp::new("ac1");

    let mut mem = Mesh::new("mesh-1", fixture(), 50.0);
    feed(&mut mem);
    let mem_v = mem
        .cascade(mem.log.head(), &DeterministicReasoner)
        .expect("memory cascade");

    let store = EhdbStore::open("mesh-1", &tmp.0).expect("open engine");
    let mut db = Mesh::with_store(Box::new(store), fixture(), 50.0);
    feed(&mut db);
    let db_v = db
        .cascade(db.log.head(), &DeterministicReasoner)
        .expect("ehdb cascade");

    eprintln!(
        "AC1: memory={:.4}/{} ehdb={:.4}/{} | records mem={} ehdb={}",
        mem_v.value,
        mem_v.decision,
        db_v.value,
        db_v.decision,
        mem.log.record_count().unwrap(),
        db.log.record_count().unwrap(),
    );
    assert_eq!(mem_v, db_v, "the store must not change the answer");
    assert!(
        (db_v.value - 52.5).abs() < 1e-9,
        "the fixture's pinned verdict is 52.5000, got {}",
        db_v.value
    );
    assert!(db_v.decision);
}

// ---------------------------------------------------------------- AC 2

/// ⭐⭐ **AC2a — the answer survives the PROCESS that produced it.**
///
/// Write, drop the engine, re-`open` the same root, re-fold, compare digests.
/// This is the ordinary restart: the pod came back on the same disk.
///
/// ⚠ The positive control is the whole point. A digest comparison that cannot
/// fail proves nothing, so we perturb one value and require the digest to move.
#[test]
fn an_answer_survives_a_process_restart() {
    let tmp = Tmp::new("ac2a");

    let (before_digest, before_verdict, watermark, count) = {
        let store = EhdbStore::open("mesh-1", &tmp.0).expect("open");
        let mut m = Mesh::with_store(Box::new(store), fixture(), 50.0);
        feed(&mut m);
        let wm = m.log.head();
        let v = m.cascade(wm, &DeterministicReasoner).expect("cascade");
        let prefix = m.log.records_up_to(wm).expect("prefix");
        let ctx = fold(&prefix, "mesh-1", wm).expect("fold");
        let n = m.log.record_count().expect("count");
        (ctx.digest(), v, wm, n)
        // `m`, and the engine inside it, drop here. The process that produced
        // the answer is gone.
    };

    let reopened = EhdbStore::open("mesh-1", &tmp.0).expect("re-open same root");
    let after = reopened.records_up_to(watermark).expect("prefix");
    let after_digest = fold(&after, "mesh-1", watermark).expect("refold").digest();

    eprintln!(
        "AC2a: wrote={count} records, watermark={watermark}, refolded={} | \
         digest before={before_digest} after={after_digest}",
        after.len()
    );
    // ⚠ Assert the extraction BEFORE asserting about it: a digest over nothing
    // equals a digest over nothing.
    assert!(
        !after.is_empty(),
        "re-open read ZERO records — every comparison below would be vacuous"
    );
    assert_eq!(
        before_digest, after_digest,
        "the context must survive intact"
    );

    // ⚠ Same trap as AC2b: the digest covers only the prefix at the watermark.
    // Assert the whole set and the verdict, or "survived" means "six signals
    // survived" while the 29 events the mesh actually produced did not.
    let all_after = reopened.records_up_to(u64::MAX).expect("everything");
    assert_eq!(
        all_after.len(),
        count,
        "re-open recovered {} of {count} records",
        all_after.len()
    );
    assert_eq!(
        all_after.iter().find_map(|r| match &r.payload {
            MeshEvent::VerdictSynthesised(v) => Some(v.value),
            _ => None,
        }),
        Some(52.5),
        "the verdict itself must survive a process restart"
    );

    // The verdict is re-derivable, not just the context.
    let mut replayed = Mesh::with_store(
        Box::new(EhdbStore::open("mesh-1", &tmp.0).expect("re-open 2")),
        fixture(),
        50.0,
    );
    let replay_v = replayed
        .cascade(watermark, &DeterministicReasoner)
        .expect("replay");
    assert!(
        (replay_v.value - before_verdict.value).abs() < 1e-9,
        "re-running at the same watermark must reproduce the verdict: {} vs {}",
        before_verdict.value,
        replay_v.value
    );

    // ⚠ POSITIVE CONTROL — the digest must move when an input changes.
    let mut perturbed = after.clone();
    let mut touched = false;
    for r in perturbed.iter_mut() {
        if let MeshEvent::SignalObserved(sig) = &mut r.payload {
            sig.value += 1.0;
            touched = true;
            break;
        }
    }
    assert!(touched, "the control needs a signal to perturb");
    let control = fold(&perturbed, "mesh-1", watermark)
        .expect("control folds")
        .digest();
    eprintln!("AC2a control: perturbed digest={control}");
    assert_ne!(
        after_digest, control,
        "the digest did not move when an input changed — it could not detect a \
         restart that lost data either"
    );
}

/// ⭐⭐ **AC2b — the answer survives the NODE, but only after a checkpoint.**
///
/// ⚠⚠ **This test found the real thing.** Written first without the checkpoint,
/// it failed with *"cold-load: no durable manifest for dataset
/// mesh_event_log"*. The engine seals at 1024 records / 8 MiB by default
/// (`ehdb-l0` `engine.rs:53,55`) and one cascade appends ~29 — so nothing had
/// reached the substrate and the mesh was durable only against process loss,
/// with nothing anywhere saying so. That is the unsealed-tail property the
/// production plan flagged, met in practice.
///
/// So this asserts BOTH halves: without a checkpoint the cold load **fails**,
/// and with one it **reproduces the digest**. The negative half is what stops
/// a future change from quietly making `checkpoint()` a no-op.
#[test]
fn an_answer_survives_node_loss_only_after_a_checkpoint() {
    // --- the negative half: no checkpoint, no durable manifest.
    let bare = Tmp::new("ac2b-bare");
    {
        let store = EhdbStore::open("mesh-1", &bare.0).expect("open");
        let mut m = Mesh::with_store(Box::new(store), fixture(), 50.0);
        feed(&mut m);
        m.cascade(m.log.head(), &DeterministicReasoner)
            .expect("cascade");
    }
    let without = EhdbStore::cold_load("mesh-1", &bare.0).map(|_| ());
    eprintln!("AC2b negative: cold_load without a checkpoint -> {without:?}");
    assert!(
        matches!(without, Err(StoreError::Engine(_))),
        "a cascade smaller than the seal threshold leaves NOTHING on the \
         substrate; if this ever starts succeeding, the seal policy changed and \
         the checkpoint contract below needs rereading"
    );

    // --- the positive half: checkpoint, then cold-load.
    let tmp = Tmp::new("ac2b");
    let (before_digest, watermark, total_written) = {
        let store = EhdbStore::open("mesh-1", &tmp.0).expect("open");
        let mut m = Mesh::with_store(Box::new(store), fixture(), 50.0);
        feed(&mut m);
        let wm = m.log.head();
        m.cascade(wm, &DeterministicReasoner).expect("cascade");
        let prefix = m.log.records_up_to(wm).expect("prefix");
        let d = fold(&prefix, "mesh-1", wm).expect("fold").digest();
        // ⭐ The durability barrier. Everything above is local until this line.
        m.checkpoint().expect("checkpoint");
        (d, wm, m.log.record_count().expect("count"))
    };

    let cold = EhdbStore::cold_load("mesh-1", &tmp.0).expect("cold load after checkpoint");
    let after = cold.records_up_to(watermark).expect("prefix");
    let after_digest = fold(&after, "mesh-1", watermark).expect("refold").digest();
    let all_after = cold.records_up_to(u64::MAX).expect("everything");

    eprintln!(
        "AC2b positive: wrote={total_written} cold-loaded={} (prefix at wm={watermark}: {}) | \
         digest before={before_digest} after={after_digest}",
        all_after.len(),
        after.len()
    );
    assert!(!after.is_empty(), "cold load read ZERO records");
    assert_eq!(
        before_digest, after_digest,
        "a checkpointed mesh must cold-load to the identical context"
    );

    // ⚠⚠ The digest above covers only the PREFIX at the watermark — six signals.
    // Matching on those alone is consistent with losing all 29 cascade events,
    // which is most of what the mesh actually produced. Assert the whole set,
    // and assert the verdict specifically: it is the answer, and it is the last
    // record written, so it is the one most likely to be lost.
    assert_eq!(
        all_after.len(),
        total_written,
        "cold load recovered {} of {total_written} records — a prefix-only \
         digest match would have hidden this",
        all_after.len()
    );
    let verdict_value = all_after.iter().find_map(|r| match &r.payload {
        MeshEvent::VerdictSynthesised(v) => Some(v.value),
        _ => None,
    });
    eprintln!("AC2b: recovered verdict = {verdict_value:?}");
    assert_eq!(
        verdict_value,
        Some(52.5),
        "the VERDICT itself must survive node loss — it is the answer, and it \
         is the last record written"
    );
}

// ---------------------------------------------------------------- AC 3

/// **AC3 — a redelivered signal is acknowledged, not appended twice.**
///
/// The denominator is printed because "no duplicates" over an empty log is also
/// true.
#[test]
fn replaying_the_same_batch_appends_n_records_not_2n() {
    let tmp = Tmp::new("ac3");
    let store = EhdbStore::open("mesh-1", &tmp.0).expect("open");
    let mut m = Mesh::with_store(Box::new(store), fixture(), 50.0);

    feed(&mut m);
    let after_first = m.log.record_count().expect("count");
    let head_first = m.log.head();

    feed(&mut m); // byte-identical redelivery
    let after_second = m.log.record_count().expect("count");

    eprintln!(
        "AC3: offered=12 (6 twice) appended_after_first={after_first} \
         appended_after_second={after_second} head={head_first}->{}",
        m.log.head()
    );
    assert_eq!(after_first, 6, "six signals, six records");
    assert_eq!(
        after_second, after_first,
        "the redelivery must be acknowledged at existing positions, not appended"
    );
    assert_eq!(
        m.log.head(),
        head_first,
        "a deduplicated redelivery must not advance the head"
    );

    // ⚠ CONTROL: a genuinely new device_seq DOES append. Without this, the
    // assert above passes for a store that silently drops every write.
    m.observe_signal("dev-07", "temp", 40.0, 99)
        .expect("append");
    let after_new = m.log.record_count().expect("count");
    eprintln!("AC3 control: after a genuinely new signal={after_new}");
    assert_eq!(
        after_new,
        after_first + 1,
        "a new signal must still append — otherwise dedupe is just data loss"
    );
}

// ---------------------------------------------------------------- AC 4

/// **AC4 — the indexed prefix read prunes by stream (fork F-1).**
///
/// Two streams in one engine; reading one must not see the other. ⚠ With the
/// MVP's single fixed stream this is *exercised but not stressed* — proving it
/// scales is M7, and this test does not claim it.
#[test]
fn the_indexed_read_prunes_by_stream_and_prints_both_counts() {
    let tmp = Tmp::new("ac4");

    let mut a = EhdbStore::open("stream-a", &tmp.0).expect("open a");
    for i in 0..5u64 {
        a.append_with_id(
            MeshEvent::SignalObserved(SignalObserved {
                device_id: format!("a{i}"),
                signal_class: "temp".into(),
                value: i as f64,
                device_seq: i,
            }),
            Some(&format!("a:{i}")),
        )
        .expect("append a");
    }
    let mut b = EhdbStore::open("stream-b", &tmp.0).expect("open b");
    for i in 0..7u64 {
        b.append_with_id(
            MeshEvent::SignalObserved(SignalObserved {
                device_id: format!("b{i}"),
                signal_class: "vibe".into(),
                value: i as f64,
                device_seq: i,
            }),
            Some(&format!("b:{i}")),
        )
        .expect("append b");
    }

    let only_a = a.records_up_to(u64::MAX).expect("read a");
    let only_b = b.records_up_to(u64::MAX).expect("read b");
    let total = only_a.len() + only_b.len();

    eprintln!(
        "AC4: total written={total} | stream-a read={} (wrote 5) | stream-b read={} (wrote 7) \
         | shards a={} b={}",
        only_a.len(),
        only_b.len(),
        a.shard(),
        b.shard()
    );
    assert_eq!(only_a.len(), 5, "stream-a must see exactly its own records");
    assert_eq!(only_b.len(), 7, "stream-b must see exactly its own records");
    assert!(
        only_a.len() < total,
        "the indexed read must return FEWER than everything written \
         ({} of {total})",
        only_a.len()
    );
    assert!(
        only_a.iter().all(|r| r.stream == "stream-a"),
        "a pruned read leaked a foreign stream's records"
    );
}

// ---------------------------------------------------------------- AC 5

/// **AC5 — an unknown kind still folds and still advances the watermark.**
///
/// A rolling upgrade is tier-by-tier, so a lower tier will emit kinds an upper
/// tier does not know. A fold that refuses turns that into an outage.
#[test]
fn an_unknown_kind_round_trips_through_the_real_store() {
    let tmp = Tmp::new("ac5");
    let mut s = EhdbStore::open("mesh-1", &tmp.0).expect("open");

    let raw = r#"{"kind":"mesh.from.the.future","whatever":1}"#;
    let unknown: MeshEvent = serde_json::from_str(raw).expect("unknown kind deserialises");
    let seq_unknown = s.append(unknown).expect("append unknown");
    let seq_known = s
        .append(MeshEvent::AggregateEmitted(AggregateEmitted {
            agent_id: "t0".into(),
            tier: 0,
            value: 3.0,
            input_count: 2,
            up_to_seq: seq_unknown,
        }))
        .expect("append known");

    let prefix = s.records_up_to(u64::MAX).expect("read");
    let ctx = fold(&prefix, "mesh-1", seq_known).expect("fold must not refuse an unknown kind");
    eprintln!(
        "AC5: records={} unknown_at={seq_unknown} known_at={seq_known} folded_through={}",
        prefix.len(),
        ctx.folded_through
    );
    assert_eq!(prefix.len(), 2, "both records round-tripped");
    assert_eq!(
        ctx.folded_through, seq_known,
        "the unknown kind must not stall the watermark"
    );
    assert_eq!(ctx.inputs.len(), 1, "the known aggregate still folded");
}

// ---------------------------------------------------- the payload ceiling

/// ⚠⚠ **Both ceilings, and we enforce the smaller.**
///
/// Tested at the exact boundary. An off-by-one here is the difference between
/// "works locally, fails through the worker" and a loud local refusal.
#[test]
fn the_payload_ceiling_is_the_workers_1mib_not_the_frames_64mib() {
    assert_eq!(
        MAX_PAYLOAD_BYTES, 1_048_576,
        "the cap must be the worker event-log client's, not the L0 frame's 64 MiB"
    );

    // Build a payload whose SERIALIZED length lands exactly on the cap.
    let overhead = serde_json::to_string(&MeshEvent::SignalObserved(SignalObserved {
        device_id: String::new(),
        signal_class: "temp".into(),
        value: 1.0,
        device_seq: 1,
    }))
    .unwrap()
    .len();
    let at_cap = MeshEvent::SignalObserved(SignalObserved {
        device_id: "x".repeat(MAX_PAYLOAD_BYTES - overhead),
        signal_class: "temp".into(),
        value: 1.0,
        device_seq: 1,
    });
    let over_cap = MeshEvent::SignalObserved(SignalObserved {
        device_id: "x".repeat(MAX_PAYLOAD_BYTES - overhead + 1),
        signal_class: "temp".into(),
        value: 1.0,
        device_seq: 2,
    });
    let at_len = serde_json::to_string(&at_cap).unwrap().len();
    let over_len = serde_json::to_string(&over_cap).unwrap().len();
    eprintln!("ceiling: cap={MAX_PAYLOAD_BYTES} at_cap={at_len} over_cap={over_len}");
    assert_eq!(
        at_len, MAX_PAYLOAD_BYTES,
        "the boundary fixture must be exact"
    );
    assert_eq!(over_len, MAX_PAYLOAD_BYTES + 1);

    // Both stores enforce it, or the default configuration has no ceiling.
    for (name, mut store) in [
        (
            "memory",
            Box::new(MemoryStore::new("s")) as Box<dyn MeshStore>,
        ),
        ("ehdb", {
            let tmp = Tmp::new("cap");
            let s = EhdbStore::open("s", &tmp.0).expect("open");
            std::mem::forget(tmp); // the dir outlives the loop body
            Box::new(s) as Box<dyn MeshStore>
        }),
    ] {
        store
            .append(at_cap.clone())
            .unwrap_or_else(|e| panic!("{name}: exactly at the cap must be ACCEPTED, got {e}"));
        match store.append(over_cap.clone()) {
            Err(StoreError::PayloadTooLarge { bytes, cap }) => {
                assert_eq!(bytes, MAX_PAYLOAD_BYTES + 1);
                assert_eq!(cap, MAX_PAYLOAD_BYTES);
            }
            other => panic!("{name}: one byte over the cap must be REFUSED, got {other:?}"),
        }
    }
}

// ------------------------------------------------------------- the flag

/// The store selector is off by default, and only the exact string arms it.
#[test]
fn the_ehdb_store_is_off_unless_explicitly_selected() {
    assert_eq!(store_kind(None), StoreKind::Memory, "default is memory");
    assert_eq!(store_kind(Some("ehdb")), StoreKind::Ehdb);
    assert_eq!(store_kind(Some(" ehdb ")), StoreKind::Ehdb, "trimmed");
    // ⚠ A selector must not fail open. Anything unrecognised is the default.
    for raw in ["", "EHDB", "true", "1", "memory", "ehd", "ehdbx", "yes"] {
        assert_eq!(
            store_kind(Some(raw)),
            StoreKind::Memory,
            "{raw:?} must not select the durable store"
        );
    }
}
