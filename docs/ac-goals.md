# RFC: Goals — bounded autonomous objectives

**Status:** design of record — proposed (2026-07-23). **Not implemented.** This RFC
specifies a *goal*: a persistent, bounded objective a session pursues across turns without
per-turn prompting, until it is met, blocked, paused, or its budget is spent. A goal is **not
a subsystem of the kit and not a skill** — it is a composition over existing seams. This
document names the two generic mechanisms the core kit is missing (a lifecycle trigger and
idle continuation), an opt-in engine that composes them with context injection and the session
log, and the host surface — and draws the boundary between the three.
**Requires:** [ac-hooks.md](ac-hooks.md) (the lifecycle phase this is the first consumer of);
[ac-context.md](ac-context.md) §4–§5 (the reactive fragment the objective is injected as);
[ac-fork.md](ac-fork.md) §3 (`E(L)`, the effective history the continue/stop verdict derives
from); [ac-queue-steer.md](ac-queue-steer.md) §4 (the step/idle boundary continuation shares
with steer); [ac-loop.md](ac-loop.md) §2 (session/turn/idle). **Required by:** nothing yet.
**Interacts with:** [ac-provider.md](ac-provider.md) (usage truth feeds budget accounting);
[ac-subagents.md](ac-subagents.md) (a goal MAY delegate — deferred, §8); [ac-tools.md](ac-tools.md)
(the goal-status tool).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The loop pursues one turn at a time: the model samples, tools run, the turn settles, the
session goes idle and waits for the next input. A *long-running objective* — "keep working
until the suite is green," "draft, review, and revise the section" — does not fit that shape.
Expressed as a skill (injected text), it is advice the model may drop after one turn and cannot
be enforced, budgeted, paused, or recovered after a restart. Five failures a text instruction
cannot avoid:

- **R1 (persistence beyond the turn).** An objective must outlive the turn that set it and
  seed future turns. Turn-scoped injection expires by construction ([ac-context.md](ac-context.md)
  𝒯); a standing instruction (𝒲) is not *state* — it cannot be paused, marked complete, or
  accounted against.
- **R2 (bounded autonomy).** Self-continuation without a spend bound is a runaway loop. An
  objective MUST carry a budget (tokens, wall-time, or turns), and continuation MUST cease —
  deterministically, at the kernel of the mechanism, not by the model's goodwill — when the
  bound is crossed.
- **R3 (record fidelity / resume safety).** The goal, its remaining budget, and the *decision
  to continue or stop* MUST be reconstructable from the session log alone. A resumed or forked
  session must reach the same verdict at every boundary as one that never stopped (mirrors
  [ac-hooks.md](ac-hooks.md) R3).
- **R4 (least surface).** A host that does not want goals MUST pay nothing: no goal type in the
  core kit, no goal state on every session, no goal vocabulary in the loop. The concept is
  opt-in or it violates the one rule ([architecture.md](architecture.md) §2).
- **R5 (authority separation).** *Completion* is a judgment only the model can make ("the
  objective is met"); *limit-reached* is a fact only the system can assert ("the budget is
  spent"). Neither may forge the other, and neither may be a side effect of injected text.

The design follows the pattern of ultra ([ac-ultra.md](ac-ultra.md)): the kit ships generic
mechanisms; a specific *way of working* is a composition, and the composition lives outside the
core.

## 2. Model

Session, turn, and *idle* are as defined in [ac-loop.md](ac-loop.md) §2; `E(L)` is the
effective history of [ac-fork.md](ac-fork.md) §3.

A **goal** is a record `γ = (objective, status, budget, spent)` bound to a session, written to
and recovered from the session log. `objective` is model-facing text; `budget` is a spend
ceiling in one or more dimensions (tokens, seconds, turns); `spent` is the accumulated cost.

The **status lattice** `Σ` partitions into *active* and *terminal*:

> active = { Active, Paused, Blocked }  terminal = { Complete, LimitReached(kind) }

- **Active** — the goal drives continuation.
- **Paused** — suspended by the host; no continuation; resumable to Active.
- **Blocked** — the model reports it cannot proceed; no continuation until the block is
  externally cleared (resume → Active).
- **Complete** — the model judges the objective met. Terminal.
- **LimitReached(kind)** — the system observed budget/usage exhaustion (`kind ∈ {budget,
  usage}`). Terminal.

Only the **model** may move a goal to Complete or Blocked (via a tool). Only the **system** may
move it to LimitReached (via accounting). Only the **host** may Pause/Resume/replace/clear it.
This three-authority split is R5 made structural. A session has **at most one** active goal
(multi-goal deferred, §8).

## 3. The two kit mechanisms

Goals need exactly two capabilities the core kit does not yet provide. Both are generic —
reusable beyond goals — and this RFC is the consumer that justifies landing them.

**M1 — the lifecycle trigger (the deferred lifecycle phase).** [ac-hooks.md](ac-hooks.md) §2
defines a `lifecycle` phase (session start/resume/end; turn start/end/abort) that "brackets
scopes… and contributes no model-visible content," deferred there "until its first consumer."
Goals is that consumer. A lifecycle contributor fires at **turn settle** (a turn ended and the
session is now idle) and at **resume**, MAY read session state and seed extension-private state
— but, per ac-hooks §2, contributes no model-visible bytes itself. It is the *trigger*, not the
*content*.

**M2 — idle continuation.** The lifecycle trigger can decide "another turn is owed," but the kit
has no way for it to *start* one. Steer ([ac-queue-steer.md](ac-queue-steer.md)) injects input
into a *running* turn; its NoActiveTurn path already returns the items so a caller may start a
turn. M2 is the inbound complement: a lifecycle contributor MAY request that the runtime
**start a fresh turn seeded with contributed input, iff the session is idle and no user input
is queued**. The started turn is an ordinary turn — its seed is ordinary input recorded in the
log and sampled at step 0 — so continuation is replayable and fork/resume-sound. Continuation
MUST NOT preempt a running turn or a queued user turn (user input always wins the idle race).

Model-visible content is neither M1's nor M2's business: it rides ac-context (§4). The
separation is deliberate — M1 says *when*, ac-context says *what*, M2 says *start*.

## 4. The engine (an opt-in extension)

The goal object, its lattice, budget accounting, the continue/stop predicate, the injection
content, and the goal tool compose the two mechanisms with ac-context and the session log. This
is **not** part of the core kit: it depends on core crates (context, log, the loop seams, the
tool trait) and **no core crate depends on it**; the generic host ([architecture.md](architecture.md)
§2, "assemblable with no application attached") does not compile it. It is the first member of
an **extensions tier** — the kit's answer to "reusable but not universal." A host that wants
goals wires it; a host that does not never sees it.

Its behavior:

- **Drive (M1 at turn-settle).** Read `γ` from the log-backed store. If `status = Active` ∧
  `spent < budget` ∧ session idle ∧ no queued user input: render the **continuation fragment**
  — a reactive (ℛ) ac-context fragment (marked, filtered from "what the user said,"
  [ac-context.md](ac-context.md) R1) carrying the objective and remaining budget — and request
  M2 continuation seeded with it. Otherwise contribute nothing.
- **Account (M1 at tool-finish / turn-usage).** Add observed cost (provider usage truth,
  [ac-provider.md](ac-provider.md); wall-time) to `spent`. Crossing `budget` sets
  `status = LimitReached` and emits a one-shot limit fragment; the *next* drive sees a terminal
  status and does not continue (R2). Accounting is the kernel that enforces the bound — not the
  model.
- **Verdict is stateless (R3).** "Continue vs stop" is a pure function of `E(L)` + the logged
  `γ`, never of a process-local status cache: a resumed session re-derives Active-and-within-
  budget from the log and continues; one whose log shows Complete/LimitReached does not; a fork
  inherits the snapshot as an independent `γ`. This is the [ac-hooks.md](ac-hooks.md)
  `ForcedChainHook` discipline applied to a driver — the anti-pattern it avoids is a mutable
  status field taken as ground truth, which desyncs on resume and fork.
- **Completion/blocking (R5, model authority).** A goal-status tool lets the model set Complete
  or Blocked and only those; it cannot Pause/Resume or forge LimitReached. A host SHOULD require
  corroboration for Blocked (e.g. the same block observed across consecutive continuation turns)
  so a transient obstacle does not end the goal.

## 5. The host surface

The host owns everything a client touches: setting/replacing an objective, Pause/Resume/clear,
the budget policy (default ceilings, dimensions), the opt-in feature gate, and the command or
RPC through which a user drives all of this. It also decides where a goal may not run — e.g. a
goal MUST be withheld from a sub-agent spawned for a bounded sub-task
([ac-subagents.md](ac-subagents.md)), lest a child inherit a self-continuing objective. None of
this is kit vocabulary.

## 6. Invariants

- **I1 (bounded).** No Active goal continues past its budget: accounting flips status to
  LimitReached at the crossing, and the next drive is a no-op. Autonomy is finite by
  construction (R2).
- **I2 (idle-only, user-first).** A continuation turn starts only when the session is idle with
  no queued user input; a pending user turn always wins. Continuation never preempts (composes
  with [ac-queue-steer.md](ac-queue-steer.md)).
- **I3 (identifiable injection).** Every continuation/limit fragment is a marked ac-context
  fragment, excluded from the verbatim-user set and transcript projection
  ([ac-context.md](ac-context.md) R1) — the model's own objective is never mistaken for the
  user's words.
- **I4 (resume/fork equivalence).** A session resumed from its log and one that never stopped
  reach identical goal verdicts at every boundary; a fork carries an independent snapshot, never
  shared mutable status (R3).
- **I5 (authority partition).** Complete/Blocked are reachable only through the model tool;
  LimitReached only through accounting; Pause/Resume/clear only through the host. No path forges
  another's transition (R5).
- **I6 (zero core cost).** Removing the extension removes goals entirely: no core type, no
  per-session field, no loop branch names a goal (R4). M1 and M2 remain, generic and
  goal-agnostic.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| Lifecycle trigger (M1), idle continuation (M2) | kit (loop + hooks) |
| Marked fragment injection, recognition | kit (ac-context) |
| Goal persistence, resume, fork | kit (ac-fork / store) |
| Goal object, status lattice, budget accounting, continue/stop predicate, continuation content, status tool | extension |
| Objective set/replace, pause/resume/clear, budget policy, feature gate, command/RPC surface, where goals may not run | host |

## 8. Deferred

- **Multiple concurrent goals** per session — a scheduler and an inter-goal budget split; one
  active goal first.
- **Delegated goals** — a goal decomposed across sub-agents (goals × [ac-subagents.md](ac-subagents.md)).
  The seam composes; evidence first.
- **Goal-first sessions** — a session whose first recorded item is the objective, before any
  user message; an ordering choice on the log, additive to this design.
- **Objective spill** — very large objectives or attachments materialized out-of-band; a host
  storage detail.
- **Richer budget kinds** and adaptive ceilings; fixed per-dimension ceilings first.
- **Host-owned engine** — a host MAY implement the engine itself instead of the shared
  extension; the mechanism/host boundary (M1, M2, ac-context, the log) is identical either way.
  The extension exists to spare every host that work, not to mandate it.

---
*Provenance: this design distills the thread-goal subsystem of a production agent runtime
(openai/codex, Apache-2.0), studied 2026-07-23 — its persisted goal record, six-state status
machine, budget accounting, idle auto-continuation, and marked context injection. The
distillation is behavioral; no code was carried over. The mechanism/composition split and the
`E(L)`-derived verdict follow this kit's own ultra ([ac-ultra.md](ac-ultra.md)) and hooks
([ac-hooks.md](ac-hooks.md)) precedents.*
