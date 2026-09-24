//! The mesh: an append-only log, the tier wiring, and the cascade.

use crate::a2a::{AgentCard, Capabilities, Skill, Task, TaskState};
use crate::correlation::{correlate, CorrelationRefusal, CorrelationRule};
use crate::coverage::Coverage;
use crate::escalation::{escalation_event, EscalationGate, SeverityPolicy, Suppressed};
use crate::event::*;
use crate::fold::{fold, FoldError, Reduction};
use crate::react::{run_cycle, Reasoner};
use crate::store::{MemoryStore, MeshStore, StoreError};

/// One agent in the mesh.
pub struct Agent {
    pub id: String,
    pub tier: u8,
    pub how: Reduction,
    /// Agent ids this agent reduces. Empty at tier 0 (it reduces raw signals).
    pub children: Vec<String>,
    /// Tier-0 only: the signal class this agent owns. `None` means "every
    /// class", which is almost never what a per-signal-class agent wants.
    ///
    /// ⚠ Added after the first demo run showed both tier-0 agents reducing all
    /// six signals to the same number — a per-class agent that does not filter
    /// by class is just a duplicate of its sibling.
    pub signal_class: Option<String>,
    /// ⭐ M10. `Some` makes this agent able to escalate on its own; `None` —
    /// the default — leaves it exactly as it was, driven only by the cascade.
    pub severity: Option<SeverityPolicy>,
    /// ⭐ M11 — when `Some`, this agent is a **correlator**: it combines
    /// sibling branches (network + endpoint + identity) into one detection,
    /// and its population is the **union** of its branches' identities rather
    /// than the sum of their counts.
    ///
    /// ⚠ `None` is the ordinary aggregator and its arithmetic is untouched. A
    /// single-parent tier has no overlap to resolve, so switching it to sets
    /// would be cost with no correctness gain.
    pub correlates: Option<CorrelationSpec>,
}

/// How a correlator combines its branches.
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelationSpec {
    pub rule: CorrelationRule,
    /// ⚠ Below this many branches the correlator **refuses** rather than
    /// correlating over whatever happened to arrive. A detection built from
    /// two of three domains is not the same claim as one built from three,
    /// and silently making it is the failure M12 generalises.
    pub required_branches: usize,
}

impl Agent {
    /// An ordinary reducer — the shape every pre-M11 agent has.
    pub fn reducer(id: &str, tier: u8, how: Reduction, children: Vec<String>) -> Self {
        Agent {
            id: id.into(),
            tier,
            how,
            children,
            signal_class: None,
            severity: None,
            correlates: None,
        }
    }
}

impl Agent {
    pub fn card(&self) -> AgentCard {
        AgentCard {
            name: self.id.clone(),
            description: format!("tier-{} reducer ({:?})", self.tier, self.how),
            url: format!("http://mesh.local/agents/{}", self.id),
            version: "0.1.0".into(),
            protocol_version: crate::a2a::A2A_PROTOCOL_VERSION.into(),
            capabilities: Capabilities {
                streaming: false,
                push_notifications: true,
                extensions: vec![],
            },
            skills: vec![Skill {
                id: format!("reduce.tier{}", self.tier),
                name: format!("Reduce tier {}", self.tier),
                description: format!(
                    "Reduce {} input(s) to one value using {:?}",
                    if self.children.is_empty() {
                        "raw signal".to_string()
                    } else {
                        format!("{} child", self.children.len())
                    },
                    self.how
                ),
                tags: vec!["mesh".into(), format!("tier{}", self.tier)],
            }],
            security_schemes: vec!["cluster-mtls".into()],
        }
    }
}

/// A tiered mesh.
pub struct Mesh {
    /// ⚠ A **seam**, not a field. `MemoryStore` is the default and stays
    /// compiled and tested, because a rollback that is not exercised is a plan.
    pub log: Box<dyn MeshStore>,
    pub agents: Vec<Agent>,
    pub threshold: f64,
    /// ⭐ M10. Dedupe + backpressure for self-initiated escalations.
    pub gate: EscalationGate,
    /// Counted suppressions — ⚠ a suppressed escalation must be visible.
    pub suppressed_nominal: u64,
    pub suppressed_duplicate: u64,
    pub suppressed_backpressure: u64,
    pub escalations: u64,
    /// ⚠ M10 is armed per-mesh. A flag that gates nothing is not a flag — the
    /// crate's own env-currency guard exists to catch exactly that, and caught
    /// this one.
    pub escalation_armed: bool,
    /// ⭐ M12 arm. Default **false**: without it an aggregate carries
    /// `coverage: None`, which readers must treat as "not assessed" — never as
    /// "complete".
    pub coverage_armed: bool,
    /// ⭐ M11 arm. Default **false**: an agent carrying a `CorrelationSpec`
    /// reduces exactly as it did before until this is set, so the flag can be
    /// turned off in production without redeploying a topology.
    pub correlation_armed: bool,
}

/// Why a cascade did not produce a verdict.
///
/// ⚠ Two variants, not one flattened string. A fold refusal and a store refusal
/// want different responses — one is a freshness/shape problem, the other is a
/// durability problem — and a caller that cannot tell them apart will retry the
/// wrong one.
#[derive(Debug, Clone, PartialEq)]
pub enum MeshError {
    Fold(FoldError),
    Store(StoreError),
}

impl From<FoldError> for MeshError {
    fn from(e: FoldError) -> Self {
        MeshError::Fold(e)
    }
}
impl From<StoreError> for MeshError {
    fn from(e: StoreError) -> Self {
        MeshError::Store(e)
    }
}
impl std::fmt::Display for MeshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeshError::Fold(e) => write!(f, "fold refused: {e:?}"),
            MeshError::Store(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for MeshError {}

/// What one cascade produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub value: f64,
    pub decision: bool,
    /// ⭐ M12 — this number is an answer about less than the declared
    /// population. ⚠ `false` when coverage was never assessed; read
    /// [`Verdict::coverage`] to tell "complete" from "not measured".
    pub degraded: bool,
    /// The closed-set reason, for a label. Empty when not degraded.
    pub degraded_reason: String,
    /// The top agent's coverage. `None` = not assessed, NOT complete.
    pub coverage: Option<Coverage>,
    pub up_to_seq: u64,
    /// Events appended during this cascade.
    pub events_appended: usize,
}

impl Mesh {
    /// In-memory mesh — the default, and what every deterministic test uses.
    pub fn new(stream: &str, agents: Vec<Agent>, threshold: f64) -> Self {
        Self::with_store(Box::new(MemoryStore::new(stream)), agents, threshold)
    }

    /// Mesh over any store. `EhdbStore` makes the answers durable (M1).
    pub fn with_store(log: Box<dyn MeshStore>, agents: Vec<Agent>, threshold: f64) -> Self {
        Self {
            log,
            agents,
            threshold,
            // 32 in flight: enough that a real incident storm is not clipped,
            // small enough that a chattering agent cannot become the outage.
            gate: EscalationGate::new(32),
            suppressed_nominal: 0,
            suppressed_duplicate: 0,
            suppressed_backpressure: 0,
            escalations: 0,
            escalation_armed: false,
            correlation_armed: false,
            coverage_armed: false,
        }
    }

    /// Arm M10 event-driven escalation. See [`Mesh::escalation_armed`].
    ///
    /// ⚠ A builder method alongside `arm_correlation` / `arm_coverage` rather
    /// than a public field write. Three arms set three different ways is three
    /// chances to set one of them wrong.
    pub fn arm_escalation(mut self, on: bool) -> Self {
        self.escalation_armed = on;
        self
    }

    /// Arm M12 coverage reporting. See [`Mesh::coverage_armed`].
    pub fn arm_coverage(mut self, on: bool) -> Self {
        self.coverage_armed = on;
        self
    }

    /// Arm the M11 correlation tier. See [`Mesh::correlation_armed`].
    pub fn arm_correlation(mut self, on: bool) -> Self {
        self.correlation_armed = on;
        self
    }

    /// ⭐ **M10 — the direction the mesh did not have.**
    ///
    /// A tier-0 agent evaluates its own reduced value and, if the severity band
    /// warrants it, PUSHES upward without waiting to be asked. Returns the
    /// sequence of the escalation event, or why it was suppressed.
    ///
    /// ⚠ Additive: this does not compute a verdict and does not touch the
    /// per-tier watermark. It asks for a cascade sooner; it never replaces one.
    /// ⚠⚠ `at_seq` is the watermark of the DATA that triggered this, supplied
    /// by the caller — it is deliberately **not** `self.log.head()`.
    ///
    /// The first implementation read the head here, and the first run of
    /// `every_suppression_is_counted` caught why that is wrong: appending the
    /// escalation event **advances the head**, so the next escalation for the
    /// same badness computes a different key and dedupe never fires. A
    /// beaconing agent would then escalate on every append forever — precisely
    /// the outage the gate exists to prevent, reintroduced by the gate's own
    /// side effect. An escalation is *about* a state of the world at a
    /// watermark, so the key has to be that watermark.
    pub fn escalate(
        &mut self,
        agent_id: &str,
        value: f64,
        input_count: u32,
        at_seq: u64,
    ) -> Result<Result<u64, Suppressed>, StoreError> {
        if !self.escalation_armed {
            // Unarmed is not an error; it is the default posture.
            self.suppressed_nominal += 1;
            return Ok(Err(Suppressed::Nominal));
        }
        let Some((tier, policy, parent)) = self.agents.iter().find_map(|a| {
            if a.id != agent_id {
                return None;
            }
            let p = a.severity.clone()?;
            // Who to push to: the first agent declaring this one as a child.
            let parent = self
                .agents
                .iter()
                .find(|c| c.children.iter().any(|x| x == agent_id))
                .map(|c| c.id.clone())
                .unwrap_or_else(|| "synthesizer".to_string());
            Some((a.tier, p, parent))
        }) else {
            // No policy = not an escalating agent. Nominal, not an error.
            self.suppressed_nominal += 1;
            return Ok(Err(Suppressed::Nominal));
        };

        let severity = policy.classify(value);
        let at = at_seq;
        match self.gate.admit(agent_id, severity, at) {
            Err(why) => {
                match why {
                    Suppressed::Nominal => self.suppressed_nominal += 1,
                    Suppressed::Duplicate => self.suppressed_duplicate += 1,
                    Suppressed::Backpressure => self.suppressed_backpressure += 1,
                }
                Ok(Err(why))
            }
            Ok(()) => {
                let seq = self.log.append(escalation_event(
                    agent_id,
                    tier,
                    &parent,
                    severity,
                    value,
                    input_count,
                    at,
                ))?;
                self.escalations += 1;
                Ok(Ok(seq))
            }
        }
    }

    /// Publish every agent's card. In production the card is registered in the
    /// noetl catalog (`POST /api/catalog/register`, `server/src/main.rs:89`) —
    /// the same "catalog as carrier" pattern the SLM F2 decision used for
    /// generated steps. Here we record the digest so topology is replayable.
    pub fn publish_cards(&mut self) -> Result<(), StoreError> {
        let pubs: Vec<MeshEvent> = self
            .agents
            .iter()
            .map(|a| {
                MeshEvent::AgentCardPublished(AgentCardPublished {
                    agent_id: a.id.clone(),
                    tier: a.tier,
                    card_digest: a.card().digest(),
                })
            })
            .collect();
        for p in pubs {
            self.log.append(p)?;
        }
        Ok(())
    }

    /// Push everything written so far past the durability barrier.
    ///
    /// ⚠ A no-op on the in-memory store, and a real seal + upload on the EHDB
    /// one. Call it when an answer must survive losing the node, not merely the
    /// process — the two are different failure modes and `store::EhdbStore`
    /// spells out why.
    pub fn checkpoint(&mut self) -> Result<(), StoreError> {
        self.log.checkpoint()
    }

    /// Ingest one device reading (the collector's job).
    pub fn observe_signal(
        &mut self,
        device_id: &str,
        class: &str,
        value: f64,
        device_seq: u64,
    ) -> Result<u64, StoreError> {
        // The device's own sequence is the natural idempotency key: a redelivered
        // reading is the SAME reading, and the engine acknowledges it at its
        // existing position instead of inflating the denominator.
        let event_id = format!("sig:{device_id}:{device_seq}");
        self.log.append_with_id(
            MeshEvent::SignalObserved(SignalObserved {
                device_id: device_id.to_string(),
                signal_class: class.to_string(),
                value,
                device_seq,
            }),
            Some(&event_id),
        )
    }

    /// Run the cascade bottom-up at a named watermark.
    ///
    /// # The watermark advances per tier, and it has to
    ///
    /// ⚠⚠ The first version of this fixed ONE watermark for the whole cascade.
    /// That is wrong in a way worth recording: tier 0 emits its aggregates at
    /// sequences *above* the entry watermark, so every tier above read a prefix
    /// that structurally could not contain them and reduced zero inputs to
    /// 0.0. A single global watermark makes the upper tiers blind to the very
    /// cascade they are part of.
    ///
    /// So each tier reads at the log head **as of the moment the tier below
    /// finished**, and that per-tier watermark is recorded on every aggregate.
    /// Determinism is preserved because the watermark is still *named* rather
    /// than "latest": replaying the log reproduces the same per-tier bounds.
    ///
    /// Within a tier, all agents share one watermark, so siblings cannot see
    /// each other's output and the tier is order-independent.
    pub fn cascade(
        &mut self,
        up_to_seq: u64,
        reasoner: &dyn Reasoner,
    ) -> Result<Verdict, MeshError> {
        let before = self.log.record_count()?;
        let mut tiers: Vec<u8> = self.agents.iter().map(|a| a.tier).collect();
        tiers.sort_unstable();
        tiers.dedup();

        let mut last_value = 0.0;
        // ⭐ M12 — the coverage of whichever agent produced `last_value`. The
        // taint reaches here through `inherited_degraded`, so this one value
        // carries a gap from any depth.
        let mut last_coverage: Option<Coverage> = None;
        let mut top_seq = up_to_seq;
        // Watermark for the tier currently executing. Starts at the caller's
        // bound; advances to the head after each tier completes.
        let mut tier_watermark = up_to_seq;

        for tier in tiers {
            let ids: Vec<String> = self
                .agents
                .iter()
                .filter(|a| a.tier == tier)
                .map(|a| a.id.clone())
                .collect();

            for id in ids {
                let (how, children, agent_tier, signal_class, spec) = {
                    let a = self.agents.iter().find(|a| a.id == id).expect("agent");
                    (
                        a.how,
                        a.children.clone(),
                        a.tier,
                        a.signal_class.clone(),
                        a.correlates.clone(),
                    )
                };
                let correlating = self.correlation_armed && spec.is_some();

                // A2A Task: the tier above asks this agent for a value AS OF the
                // watermark. Recorded so the exchange is auditable.
                let task_id = format!("task-{id}-{tier_watermark}");
                let mut task = Task::submit(&task_id, "dispatcher", &id, tier_watermark);
                self.log
                    .append(MeshEvent::TaskTransitioned(TaskTransitioned {
                        task_id: task_id.clone(),
                        from_agent: "dispatcher".into(),
                        to_agent: id.clone(),
                        state: task.state.as_str().into(),
                    }))?;
                task.transition(TaskState::Working)
                    .expect("submitted->working");
                self.log
                    .append(MeshEvent::TaskTransitioned(TaskTransitioned {
                        task_id: task_id.clone(),
                        from_agent: "dispatcher".into(),
                        to_agent: id.clone(),
                        state: task.state.as_str().into(),
                    }))?;

                // Bounded read of everything at or below the watermark, then
                // narrow to this agent's children.
                // ⭐ ONE bounded read per agent, and everything below derives
                // from it. On `EhdbStore` this is `read_index_after` — the
                // index-pruned prefix (fork F-1), not a scan.
                let stream = self.log.stream().to_string();
                let prefix = self.log.records_up_to(tier_watermark)?;
                let mut ctx = fold(&prefix, &stream, tier_watermark)?;
                if !children.is_empty() {
                    // An aggregator reduces its children only, never raw signals.
                    ctx.inputs.retain(|i| children.contains(&i.agent_id));
                    ctx.signals.clear();
                    ctx.signal_ids.clear();
                } else {
                    // A tier-0 agent reduces only its own signal class.
                    ctx.inputs.clear();
                    if let Some(class) = &signal_class {
                        // ⚠ signals and signal_ids are positionally aligned
                        // (fold::TierContext). Narrowing one without the other
                        // silently hands this agent the identity set of signals
                        // it did NOT reduce — collect the pair in one pass.
                        let (vals, ids): (Vec<f64>, Vec<String>) = prefix
                            .iter()
                            .filter_map(|r| match &r.payload {
                                MeshEvent::SignalObserved(sg) if &sg.signal_class == class => {
                                    Some((sg.value, sg.device_id.clone()))
                                }
                                _ => None,
                            })
                            .unzip();
                        ctx.signals = vals;
                        ctx.signal_ids = ids;
                    }
                }

                let observed: Vec<u64> = prefix.iter().map(|r| r.seq).collect();

                // ⭐ M11 — carried on every aggregate so a correlator above can
                // union rather than sum. Computed before the reduction so it
                // describes exactly the context that produced the value.
                // ⭐ M12 — assessed against the DECLARED children, before the
                // reduction, so the shortfall describes exactly the context
                // that produced the value.
                //
                // ⚠ Tier-0 agents get `None`: they declare no roster, and
                // `None` means "not assessed", never "complete".
                let cov = if self.coverage_armed && !children.is_empty() {
                    let present: Vec<String> =
                        ctx.inputs.iter().map(|i| i.agent_id.clone()).collect();
                    let inherited = ctx
                        .inputs
                        .iter()
                        .any(|i| i.coverage.as_ref().is_some_and(Coverage::degraded));
                    Some(Coverage::assess(&children, &present, inherited))
                } else {
                    None
                };

                let mut pop_ids = crate::fold::population_identities(&ctx);

                // ⭐ M11 — the correlator path. It does NOT run the ordinary
                // ReAct reduction: a recorded thought that says "weighted mean
                // of 3 children" beside an emitted value that is a union-
                // weighted correlation is a trace that lies, and a trace that
                // lies is worse than no trace.
                let mut correlated = None;
                if correlating {
                    let s = spec.as_ref().expect("correlating implies spec");
                    match correlate(&ctx.inputs, s.rule, s.required_branches) {
                        Ok(c) => {
                            self.log.append(MeshEvent::AgentReasoned(AgentReasoned {
                                agent_id: id.clone(),
                                tier: agent_tier,
                                phase: "correlate".into(),
                                thought: format!(
                                    "{:?} over {} branch(es): population={} (union) \
                                     naive_sum={} overlap={} value={:.6}",
                                    s.rule,
                                    c.branches,
                                    c.population,
                                    c.naive_sum,
                                    c.naive_sum.saturating_sub(c.population),
                                    c.value,
                                ),
                                observed_seqs: observed.clone(),
                            }))?;
                            pop_ids = c.identities.clone();
                            correlated = Some(c);
                        }
                        Err(why) => {
                            // ⚠ A refused correlation emits NO aggregate — but
                            // it does emit a record. Skipping silently is how a
                            // missing branch becomes an invisible hole in the
                            // denominator one tier up (M12).
                            self.log
                                .append(MeshEvent::CorrelationRefused(CorrelationRefused {
                                    agent_id: id.clone(),
                                    tier: agent_tier,
                                    reason: refusal_code(&why).into(),
                                    detail: why.to_string(),
                                    up_to_seq: tier_watermark,
                                }))?;
                            task.transition(TaskState::Failed).expect("working->failed");
                            self.log
                                .append(MeshEvent::TaskTransitioned(TaskTransitioned {
                                    task_id,
                                    from_agent: "dispatcher".into(),
                                    to_agent: id.clone(),
                                    state: task.state.as_str().into(),
                                }))?;
                            continue;
                        }
                    }
                }

                let turns = if correlating {
                    Vec::new()
                } else {
                    run_cycle(&id, agent_tier, &ctx, how, reasoner)
                };
                let mut value = 0.0;
                let mut pop = 0u32;
                for t in &turns {
                    self.log.append(MeshEvent::AgentReasoned(AgentReasoned {
                        agent_id: id.clone(),
                        tier: agent_tier,
                        phase: t.phase.as_str().into(),
                        thought: t.thought.clone(),
                        observed_seqs: observed.clone(),
                    }))?;
                    if let Some(v) = t.value {
                        value = v;
                    }
                    if let Some(p) = t.population {
                        pop = p;
                    }
                }

                if let Some(c) = &correlated {
                    value = c.value;
                    pop = c.population;
                }

                let emitted = self
                    .log
                    .append(MeshEvent::AggregateEmitted(AggregateEmitted {
                        agent_id: id.clone(),
                        tier: agent_tier,
                        value,
                        input_count: pop,
                        population_ids: Some(pop_ids.clone()),
                        coverage: cov.clone(),
                        up_to_seq: tier_watermark,
                    }))?;
                top_seq = emitted;
                last_value = value;
                last_coverage = cov;

                task.transition(TaskState::Completed)
                    .expect("working->completed");
                self.log
                    .append(MeshEvent::TaskTransitioned(TaskTransitioned {
                        task_id,
                        from_agent: "dispatcher".into(),
                        to_agent: id.clone(),
                        state: task.state.as_str().into(),
                    }))?;
            }
            // The tier finished; the next tier may see what it emitted.
            tier_watermark = self.log.head();
        }

        let decision = last_value >= self.threshold;
        // ⚠⚠ The marker rides the VERDICT, not only the aggregate that caused
        // the gap. A caller reads the verdict; a shortfall recorded three tiers
        // down and never surfaced here has been logged, not surfaced.
        let degraded = last_coverage.as_ref().is_some_and(Coverage::degraded);
        let degraded_reason = if degraded {
            last_coverage
                .as_ref()
                .map(|c| c.reason())
                .unwrap_or("")
                .to_string()
        } else {
            String::new()
        };
        self.log
            .append(MeshEvent::VerdictSynthesised(VerdictSynthesised {
                agent_id: "top".into(),
                value: last_value,
                decision,
                threshold: self.threshold,
                up_to_seq: top_seq,
                degraded,
                degraded_reason: degraded_reason.clone(),
            }))?;

        Ok(Verdict {
            value: last_value,
            degraded,
            degraded_reason,
            coverage: last_coverage,
            decision,
            up_to_seq: top_seq,
            events_appended: self.log.record_count()? - before,
        })
    }
}

/// A short, stable code per refusal — the label a metric or an alert keys on.
///
/// ⚠ Separate from `Display`. The detail string carries agent ids and counts
/// and would make an unbounded label set; the code is the closed one that can
/// be pinned at 0 (`representation-drift.md`, "absent is not zero").
fn refusal_code(r: &CorrelationRefusal) -> &'static str {
    match r {
        CorrelationRefusal::UnknownPopulation { .. } => "unknown_population",
        CorrelationRefusal::NoInputs => "no_inputs",
        CorrelationRefusal::TooFewBranches { .. } => "too_few_branches",
    }
}
