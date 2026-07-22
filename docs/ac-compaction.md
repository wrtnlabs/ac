# RFC: Context compaction

**Status:** implemented — specification of record (2026-07-22). The lifecycle
ships in the runtime's run loop over the append-only session log of
[ac-fork.md](ac-fork.md): the manual, pre-turn, and mid-turn triggers and the
*summarize* and *fresh-window* strategies are built, each appending a `κ` record
that carries `σ` and its trigger. The **model-switch** trigger is realized as
host-invoked manual compaction before a swap (context-compatibility is host and
provider knowledge, not the runtime's — §4); the **delegated** strategy remains
deferred (§7).
**Requires:** [ac-fork.md](ac-fork.md) (compaction is an event in the session log).
**Required by:** [ac-context.md](ac-context.md), [ac-hooks.md](ac-hooks.md).
**Interacts with:** [ac-queue-steer.md](ac-queue-steer.md) §4 (post-compaction drain deferral).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

Let `H = E(L)` be the effective history of [ac-fork.md](ac-fork.md) §3, and `τ(H)` its token measure and `W` the model's context window.
Within a session `τ` is monotonically increasing, `W` is constant; every sufficiently long task
crosses `τ → W`. At that boundary the naive options are both losses: truncate (destroy the
task's accumulated state) or refuse (destroy the task). Compaction is the third option — a
transformation `C : H → H′` with `τ(H′) ≪ τ(H)` that *preserves the task* — but "summarize the
transcript" underspecifies the four questions that decide whether the task actually survives:

1. **When** does `C` fire? (Too late is a hard failure; too eager burns quality and cost.)
2. **What survives verbatim** and what is summarized?
3. **Where** does the summary sit in `H′`? (Position is meaning, to a model.)
4. **What do observers see?** (Downstream machinery must not fork its behavior on how the
   context was reduced.)

This RFC fixes all four.

## 2. The central design commitment: compaction is a handoff

The transformation is framed — to the model performing it and to the model receiving it — as
**one agent handing off a task to another**, not as text shortening. The summarizing model is
instructed that it is producing a checkpoint *for another LLM that will resume the task*:
current progress, decisions made, constraints and preferences in force, what remains, and the
data needed to continue. The receiving model is told the summary is *the work of another model
that began this problem*, to be built upon without repeating it.

This framing is the load-bearing choice. A "shorten this" instruction yields prose recap —
optimized for a human skimmer; a handoff instruction yields decisions, invariants, and next
steps — optimized for resumption. Stated as the core requirement:

> **R1 (handoff completeness).** `C` MUST be specified such that an agent given only `H′` can
> continue the task an agent given `H` was performing: progress, decisions, constraints, and
> next steps are preserved *as content*, not recoverable only by inference.

## 3. Structure of the transformation

`C(H) = H′ = context′ ⧺ U ⧺ ⟨σ⟩` where:

- `σ` is the **summary** produced under R1 (possibly empty — §4, strategies);
- `U` is the sequence of **user messages of `H`, verbatim**, each independently capped at a
  fixed token bound;
- `context′` is the re-established once-per-window context (system-level material the host
  injects per context window, not per turn).

> **R2 (user-input fidelity).** What the *user* said is never summarized — paraphrase of
> instructions is corruption of instructions. (Injected fragments are excluded from `U` despite
> their user role — the recognition predicate of [ac-context.md](ac-context.md) identifies them.) The per-message cap exists only so a single
> pathological input cannot monopolize the fresh window; within it, survival is verbatim. What
> the *agent* did (sampling, tool traffic, intermediate outputs) is exactly the material `σ`
> compresses.

> **R3 (effectiveness).** `τ(H′)` MUST fall below the trigger threshold by a margin sufficient
> to preclude immediate re-triggering; a `C` that does not achieve this MUST surface as an
> error rather than loop.

**Placement.** When compaction interrupts a task in flight (mid-turn, §5), `σ` MUST be the
*final* item of `H′`, with `context′` inserted above the last user message: the resuming model
treats the most recent item as "where I am," and the checkpoint is exactly that. When
compaction completes between turns, placement is unconstrained and the next turn re-injects
`context′` in full.

**Record.** Per [ac-fork.md](ac-fork.md), `C` appends a compaction record `κ(H′)` to the
session log carrying `σ` *and the complete replacement history* `H′`. The projection applies
it; the pre-compaction events remain in the log (audit, fork-before-compaction); observers
receive the record itself, so what the context became is never ambiguous.

## 4. One lifecycle; triggers × strategies

Compaction is one lifecycle — pre-hooks → transformation → record → post-hooks — parameterized
on two orthogonal axes.

**Triggers** (predicates over session state):

| Trigger | Predicate | Notes |
| --- | --- | --- |
| Manual | user request | always available |
| Pre-turn | `τ(E(L)) ≥ β` before a turn's first step | clears the runway before new work |
| Mid-turn | after step `sᵢ`: `follow_up(sᵢ) ∧ τ ≥ β` | the one that saves long tasks: checkpoint, then *continue the same turn* |
| Model switch | successor model's context-compatibility differs from the predecessor's | compact **under the predecessor** before the successor's first step, so the new model starts from a handoff rather than a history shaped for another model |

`β ≤ W` is the **budget**. Implementations SHOULD support a budget scope that excludes a
stable cached prefix from `τ` — a large invariant prefix (cached at the provider) consumes no
marginal cost and ought not trigger compaction.

`τ` is measured from **provider-reported usage** (the authoritative token count of the last
round-trip), never client-side tokenization. Where a server figure does not yet exist — the
instant *after* a transformation, and on resume before the first round-trip — an intrinsic
size estimate stands in for `τ`. The estimate serves two bounded purposes only: it prevents a
stale pre-compaction figure from re-firing a trigger before the next real usage lands, and it
backs the R3 effectiveness check. It never overrides a real measurement.

**Strategies** (how `σ` is produced):

| Strategy | `σ` | Use |
| --- | --- | --- |
| Summarize | produced by the model under R1 | default |
| Fresh window | `σ = ∅` — `H′` is `context′ ⧺ U` alone | degenerate case; a deliberate reset |
| Delegated | produced by an external service under the same R1 contract | host/provider concern |

> **R4 (uniformity).** Triggers and strategies MUST be observationally equivalent except
> through the content of `σ` and the trigger recorded on `κ`. Hooks fire identically; the
> record has one shape; no consumer branches on "which kind" of compaction occurred. This is
> what keeps every downstream system — forking, resumption, analytics, clients — one code
> path.

## 5. Interaction with mid-turn input

After a **mid-turn** compaction, if the model still owes a tool continuation, pending steered
input defers for exactly one step ([ac-queue-steer.md](ac-queue-steer.md) §4): the model MUST
re-establish its interrupted work against `H′` before new user intent lands, or the steer is
interpreted against a context the model has not yet re-entered. This is the single coupling
between the two designs, and it lives in the run loop's drain discipline, not in `C`.

The steerability of a compaction depends on whether it *is* a turn or is *inside* one. A
**dedicated** compaction turn — a manual compaction, which is a turn in its own right — is
**non-steerable** ([ac-queue-steer.md](ac-queue-steer.md) §3): user intent cannot coherently
join a history transformation that constitutes the whole turn, so a steer is refused. An
**in-flight** compaction (pre-turn or mid-turn) is a phase *within* a regular, steerable turn;
a steer submitted while it runs is not refused but **deferred** by the rule above — it lands
against `H′`, once, after the model has re-established. Both readings share the same guarantee:
new user intent never lands on a half-transformed history.

## 6. Invariants

- **I1.** The log retains the full pre-compaction record; `C` changes only the projection.
  (Follows from [ac-fork.md](ac-fork.md) A1 + I2.)
- **I2.** A fork whose prefix contains `κ(H′)` reproduces the compacted view; a fork cut
  before it reproduces the pre-compaction view. No special-casing exists or is permitted.
- **I3.** User messages in `E(L)` after compaction are byte-identical to their originals, up
  to the per-message cap (R2).
- **I4.** In mid-turn compaction, `σ` is the terminal item of the replacement (placement
  rule); the turn then continues — compaction MUST NOT end a turn that owed follow-up.
- **I5.** `C` composes: `C(C(H))` is well-defined and each application appends its own record.
  Repeated compaction of a very long task is the designed steady state, bounded by R3.

## 7. Deferred

- The delegated strategy's transport — a service contract, reserved lifecycle slot, no design
  needed yet.
- The **injected-fragment recognition predicate** (R2). Until [ac-context.md](ac-context.md)
  supplies it, `U` is the set of user-role messages that carry text and no tool result — which
  correctly excludes tool traffic but also retains any runtime-injected user-role fragment (an
  interruption marker, a future context fragment) verbatim. A benign over-inclusion: such
  fragments are small and re-established per window anyway. The predicate lands with the context
  design that owns the persistent/reactive distinction.
- A metrics taxonomy (trigger/phase/strategy/outcome) — adopt the shape when a metrics seam
  exists; a telemetry dependency is not justified by it.
- Mid-stream reduction of individual oversized tool outputs — a different problem (bounded
  capture at the tool layer already addresses its worst case); revisit with evidence.
- **A single user input larger than the whole budget.** When the history is *only* user input
  (no agent traffic to summarize), compaction has nothing to do and the turn proceeds
  uncompacted — even if one pasted input alone exceeds `β`. The per-message cap (R2) that would
  bound it is not applied in this degenerate case, because "cap a lone user message" is a
  different operation from "compact a task," with its own fidelity cost. Rare in practice; the
  provider's own context-overflow error is the backstop. Revisit if it shows up.

---
*Provenance: this design distills the compaction system of a production agent runtime
(openai/codex, Apache-2.0), studied 2026-07-21, including its handoff framing, which this
document restates as R1. The distillation is behavioral — no code was carried over.*
