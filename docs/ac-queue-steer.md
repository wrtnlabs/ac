# RFC: Mid-turn input — steering

**Status:** implemented — specification of record (2026-07-22).
**Requires:** [ac-loop.md](ac-loop.md) (the step model this design extends). **Required by:** [ac-fork.md](ac-fork.md) §4 (boundary definition), [ac-compaction.md](ac-compaction.md) §5 (drain deferral).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

An agent turn is long-running: one user request can produce minutes of model sampling and tool
execution. Users produce input *during* that interval — corrections, additions, refinements —
and a runtime that accepts input only between turns forces every host into a bad dichotomy:
block the user until the turn ends, or abort work in flight and resample from scratch. Both
discard value. The design goal is a third path: **input submitted during a turn joins that
turn**, at a point where it cannot corrupt in-flight computation, without special framing that
would distort what the model sees.

Three requirements shape everything below:

- **R1 (no torn state).** Input MUST NOT interrupt a sampling request or a tool execution in
  progress; the model's view of its own step MUST remain internally consistent.
- **R2 (no silent loss).** Input accepted by the runtime MUST eventually become part of the
  history the model samples — except under explicit cancellation, where discarding is the
  user's stated intent.
- **R3 (neutrality).** Injected input MUST be indistinguishable, in the history the model
  reads, from input submitted at a turn boundary. Framing text ("the user interjected…")
  biases the model's interpretation and is prohibited.

## 2. Model

Let a **session** hold an append-only history `H = ⟨h₁, h₂, …⟩` of items (user messages,
assistant items, tool records, markers).

A **turn** `T` is a computation initiated by an input set `I₀`, consisting of a sequence of
**steps** `s₁ … sₙ`. A step is the atomic unit of agent progress:

> `sᵢ` = one sampling request over the current history, the model's response, and the complete
> execution of every tool call that response issued.

Steps are indivisible with respect to input (R1): the only points at which a turn's history may
change from the outside are the **step boundaries** — before `s₁`, between `sᵢ` and `sᵢ₊₁`, and
after `sₙ`.

Each active turn carries:

- `Q` — a FIFO queue of pending input (the *steer queue*), initially empty;
- `d ∈ {false, true}` — the *drainable* flag, initially **false**.

Two per-step predicates govern continuation:

- `follow_up(sᵢ)` — the loop specification's continuation predicate ([ac-loop.md](ac-loop.md)):
  true iff the model's response in `sᵢ` owes another step;
- the turn's continuation condition after `sᵢ`:  **continue iff `follow_up(sᵢ) ∨ Q ≠ ∅`.**

## 3. Operations

**steer(x)** — submit input `x` to the active turn.

```
steer(x):
  if no active turn        → error NO_ACTIVE_TURN, returning x to the caller
  if precondition supplied ∧ precondition ≠ active turn's identity
                           → error TURN_MISMATCH, carrying the actual identity as data
  if the active turn's class is non-steerable
                           → error NOT_STEERABLE, carrying the class as data
  otherwise                → Q ← Q ⧺ ⟨x⟩ ; signal steer-activity
```

Design notes on the error contract:

- `NO_ACTIVE_TURN` **returns the input** so the caller's generic submit path is simply
  "try steer; on NO_ACTIVE_TURN, start a turn with the returned items." One code path serves
  both cases; there is no time-of-check race between "is a turn running?" and the submit.
- The precondition is optimistic concurrency: a client that believes turn `t` is running can
  require its steer land in `t` and nowhere else. The failure MUST carry the actual identity
  as structured data — clients re-synchronize from the error itself, never by parsing message
  text.
- Non-steerable classes exist because some turns are not conversations: a compaction turn
  (see [ac-compaction.md](ac-compaction.md)) transforms history and cannot coherently absorb
  new user intent mid-transformation.

**cancel** — abort the active turn. See §5.

There is deliberately **no queue operation**. "Hold this for the next turn" requires no runtime
support: a host that wants a queue holds the input itself and submits at the boundary it
observes. The runtime's obligation ends at steer-or-start (§7).

## 4. The drain discipline

Pending input enters history only at step boundaries, under the drainable flag:

```
at each step boundary, before building the next sampling request:
  if d ∧ Q ≠ ∅ :  H ← H ⧺ Q ;  Q ← ∅        (each item appended as a plain user message)

d transitions:
  d ← false   at turn start
  d ← true    after each completed step
  d ← ¬follow_up(sᵢ)  immediately after a mid-turn compaction following sᵢ
```

The three rules encode three theorems of ordering:

- **Initial deferral.** `d = false` at turn start means the turn's own `I₀` is always sampled
  before any steer — a steer submitted in the instant between turn creation and first sampling
  cannot preempt the input that created the turn.
- **Post-compaction deferral.** After a mid-turn compaction, if the model still owes a tool
  continuation, exactly one further step runs before the drain: the model re-establishes its
  interrupted work against the compacted history before new intent lands. (Rationale in
  [ac-compaction.md](ac-compaction.md) §5.)
- **Late-steer extension.** Because continuation is `follow_up ∨ Q ≠ ∅`, a steer arriving
  during what would have been the final step keeps the turn alive for one more step, in which
  it is drained and sampled. Input never has to "catch" a turn.

**Terminal flush (R2).** If the turn ends with `Q ≠ ∅` — a steer raced the final boundary —
the residue is appended to `H` *unsampled*. It was not part of this turn's computation, but the
next turn's model sees it in exactly the position it arrived. On the non-cancel path, accepted
input therefore always reaches history:

> **I1.** For every accepted steer `x`, either `x` is sampled within its turn, or `x ∈ H` when
> the turn resolves. A steer accepted at step boundary `i` is sampled no later than step
> `i + 2` (the +2 absorbed only by the post-compaction deferral) or is flushed terminally.

## 5. Cancellation

`cancel` is the one path that discards pending input, because discarding is its meaning: the
user said *stop, including what I just typed*.

- `Q` is cleared. Hosts SHOULD compensate in their own layer (restore the discarded text to the
  composer for editing) — the runtime does not resurrect it.
- Items completed before the cancel persist; only the unterminated tail of the in-flight step
  is lost. Persistence of completed work MUST NOT depend on the turn ending cleanly.
- A **deliberate-interruption marker** is appended to `H` *before* the turn resolves as
  cancelled: a statement, visible to the next turn's model, that the previous turn was ended on
  purpose and its actions may have partially executed. Without it, the model misreads a
  truncated turn as an anomaly and re-attempts completed work; with it, interruption becomes
  usable steering ("stop" is itself information).

## 6. Observability

Turn boundaries are explicit events; steered input is a plain user message within them (R3).
Consequently a consumer needs no dedicated "steer" signal:

> **I2.** A turn whose interval contains more than one user message was steered; the positions
> of those messages in `H` are exactly the step boundaries at which they were drained.

Long-blocking tools (waits, sleeps) MAY subscribe to the steer-activity signal and return
early — new user intent is a better wake condition than a timer.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| Steer queue, drain discipline, flush, cancel semantics | runtime |
| Turn-identity precondition | runtime (verify) / client (supply) |
| Next-turn queueing, queue editing UX, merge-on-submit | host |
| Restoring cancelled drafts | host |
| Non-steerable turn classes | runtime (declare per turn kind) |

## 8. Deferred

- Inter-agent mail (a second input class with its own delivery-phase rules) — out of scope
  until multi-agent sessions exist.
- Priorities or coalescing within `Q` — FIFO in arrival order is the contract; evidence first.

---
*Provenance: this design distills the mid-turn input system of a production agent runtime
(openai/codex, Apache-2.0), studied 2026-07-21. The distillation is behavioral — no code was
carried over.*
