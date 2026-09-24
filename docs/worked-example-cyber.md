# Worked example — a cyber detection, end to end

The industrial example (temp / vibration / pressure) shows the **shape**. It
does not show the **hard part**, because a temperature sensor has no adversary
and its signals do not have to be combined across domains to mean anything.

This is the security worked example the external review asked for.
**Event-driven escalation (M10)** and the **correlation tier (M11)** have since
landed and are marked ✅ below; what remains missing is marked ⛔. This document
is the target, and the honest statement of what is still missing from it.

⚠ Both land **flag-gated and off by default**, and M11's arm is a constructor
rather than an env var until the `/mesh/*` routes in PR #5 land. "Built" is not
"reachable in a deployment" — see the state table at the foot of this page.

---

## The detection

> A workstation begins beaconing to a C2 host. Within minutes an unusual admin
> token is minted for the same user, and a lateral SMB connection opens to a
> file server. Individually each is noise. Together they are an incident.

That sentence is the requirement, and it is exactly what the current mesh
cannot express: nothing escalates on its own, and nothing may read across
branches.

---

## The tiers

```mermaid
flowchart TB
    subgraph SRC["Signal sources — three domains"]
        N["network<br/>netflow / DNS"]
        E["endpoint<br/>process / module load"]
        I["identity<br/>token mint / group change"]
    end

    subgraph T0["Tier 0 — one agent per signal class"]
        A["beacon-periodicity<br/>deterministic"]
        B["proc-anomaly<br/>deterministic"]
        C["token-anomaly<br/>deterministic"]
    end

    subgraph T1["Tier 1 — per-domain aggregation"]
        D["net-risk"]
        F["host-risk"]
        G["id-risk"]
    end

    X["✅ M11 correlator<br/>reads THREE branches<br/>population = UNION of devices"]
    V(["verdict<br/>one number + one boolean"])

    N --> A
    E --> B
    I --> C
    A --> D
    B --> F
    C --> G
    D --> X
    F --> X
    G --> X
    X --> V

    A -. "⛔ M10 escalation<br/>severity crossed, pushes UP" .-> X
```

Two edges carry the whole review in them:

- **The dotted edge** is M10. Today it does not exist, so `beacon-periodicity`
  cannot tell anyone anything until the synthesizer next asks. Beaconing is
  detected on the *next scheduled cascade*, not when it starts.
- **`X` reading three branches** is M11. Today
  `ctx.inputs.retain(|i| children.contains(&i.agent_id))` (`mesh.rs:257`)
  restricts every aggregator to its direct children, so `net-risk`, `host-risk`
  and `id-risk` can never meet.

---

## ⚠⚠ The arithmetic trap this example exposes

All three branches observe **the same workstation**. A correlator that summed
its inputs' `input_count` would count that host three times and inflate its own
confidence exactly when the signals agree — the worst possible moment to be
wrong, because agreement is what makes it an incident.

So M11's population is the **union of contributing device identities**, not the
sum of counts. That is why M11 changes the event model (aggregates carry a
population *identity set*, not just a number) rather than merely relaxing a
filter.

Building it surfaced two consequences this section had not drawn out:

- **The weights move too.** Reporting the union upward is not enough. Weighting
  a branch by `input_count` reintroduces the same double-count one level down —
  a branch that saw WS-42 a hundred times would drown three branches that each
  saw a different host once. The weight is `|identities|`.
- **An empty identity set is two different facts.** `endpoint` reporting *"I ran
  and saw nothing"* is a real observation; a pre-M11 aggregate carrying no set
  at all is an unknown that could overlap its siblings. The first build
  conflated them and refused a silent branch as a legacy writer — the wrong
  cause, in the record an analyst reads. They are now `Some(vec![])` and `None`.
  A silent branch also does **not** satisfy a 3-of-3 requirement: *"network AND
  endpoint AND identity fired"* must not be satisfiable by two that fired and
  one that said nothing.

The correlator reports `naive_sum` alongside `population`, so the trap is
visible in the data rather than only in this paragraph: for the detection above,
`naive_sum = 3`, `population = 1`, and the difference **is** the double-count.

The industrial example never surfaces this: temp and vibration agents own
*disjoint* devices, so sum and union coincide. **A worked example that cannot
exhibit the failure cannot justify the design** — which is precisely why this
one was needed.

---

## Why an LLM belongs at `X` and nowhere below it

`beacon-periodicity` is an interval-variance calculation. `token-anomaly` is a
comparison against a baseline. Neither wants a model, and putting one there
costs a call per device per append.

`X` is different: *"is this one incident or three coincidences?"* is a judgement
over heterogeneous evidence, it runs only on escalation, and there are four of
them. Low volume, high value — see §7.2 of the production plan.

---

## What has to be true before this runs

| need | milestone | state |
| :-- | :-- | :-- |
| tier-0 pushes upward on severity | **M10** | ✅ built, flag-gated off; HTTP route rides PR #5 |
| a role that reads multiple branches | **M11** | ✅ built, flag-gated off; arm is a constructor until PR #5 |
| a missing branch degrades rather than skews | **M12** | ✅ built, flag-gated off; aggregation path only, not tier 0 |
| per-agent streams so 200 agents do not fold one log | M5 | mechanism yes, partitioning no |
| a model at the correlator | M4 | ⛔ not built |

⚠ M10 and M11 exist in code and are proven by test; neither is reachable in a
running deployment until the `/mesh/*` routes land. **"Exists" and "is on the
path" are independent questions** — cite the row, not the milestone number.

⚠ M12 now marks a verdict computed over fewer children than were declared, and
carries the taint upward through tiers that are locally complete. Two limits
worth stating rather than discovering:

- It covers the **aggregation** path. A tier-0 agent declares no device roster,
  so "which of the 200 workstations should have reported" is still unanswerable
  — a silent collector remains invisible at tier 0.
- It **marks**; it does not decide. Whether a degraded detection is actionable
  is a policy this milestone deliberately does not set.
