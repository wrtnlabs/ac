# RFC: The session log, forking, and rewind

**Status:** design of record — accepted, not yet implemented (2026-07-21).
**Requires:** [ac-queue-steer.md](ac-queue-steer.md) (step atomicity, the interruption marker).
**Required by:** [ac-compaction.md](ac-compaction.md) (compaction is an event in this log),
[ac-context.md](ac-context.md), [ac-hooks.md](ac-hooks.md), [ac-serving.md](ac-serving.md).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

Hosts want three capabilities that a mutable row-store of messages cannot express cleanly:

1. **Branching** — "go back to an earlier point in this session and try something different,
   keeping both timelines."
2. **Rewind** — "drop the last *k* exchanges from what the model sees," without destroying the
   record that they happened.
3. **Deterministic reconstruction** — resume, audit, or replay any session purely from its own
   record.

All three are projections of one substrate decision: **the session record is an append-only
event log, and every derived view — including "what the model currently sees" — is a pure
function of that log.** Mutation is replaced by appending semantic markers; branching is
replaced by prefix duplication under a new identity. This RFC specifies the log, the
projection, and the two operations.

## 2. The log

A session is recorded as a log

> `L = ⟨m₀, e₁, e₂, …, eₙ⟩`

where `m₀` is a **metadata head** and each `eᵢ` is a timestamped, type-tagged event: a
conversation item, a turn-boundary event, a context marker, a compaction record, or a rewind
marker. The head carries the session's **identity** `ι` (globally unique, time-ordered) and its
**lineage** `λ` (the identity of the session it was forked from, if any).

Two axioms define the substrate:

- **A1 (append-only).** `L` grows only at its end. No event, once written, is modified or
  removed — including by rewind and compaction, which are themselves events.
- **A2 (self-sufficiency).** Every consumer — resume, fork, audit, projection — reads `L` and
  nothing else. Auxiliary stores (a sessions index, titles, metadata caches) are derived data
  and MUST be reconstructible from logs.

Robustness requirements on the reader: replay MUST tolerate individually corrupt lines
(skip and count, never abort the session), and when a log contains more than one metadata head
— which fork produces by construction, §4 — **the first head is canonical** and later heads are
inert data.

## 3. The projection

Define the **effective history** `E(L)`: the sequence of items the model would be given if a
turn started now. `E` is a left fold over `L` in which ordinary items accumulate and marker
events transform the accumulation:

- a **rewind marker** `ρ(k)` removes the last `k` turns from the accumulation;
- a **compaction record** `κ(H′)` replaces the accumulation with its embedded replacement
  history `H′` ([ac-compaction.md](ac-compaction.md));
- all other events accumulate or annotate.

> **I1 (determinism).** `E(L)` is a pure function of `L`. Two processes replaying the same log
> reach identical effective histories; there is no session state outside the log.

> **I2 (record/view separation).** `ρ` and `κ` change `E(L)` without changing any prior event.
> The record keeps everything; the view is computed. Consequently *all positional reasoning* —
> "the third user message," "the boundary of turn `t`" — MUST be performed against `E(L)`,
> never against raw positions in `L`, or rewound content resurfaces in the arithmetic.

**Rewind** is thereby fully specified: append `ρ(k)`. It MUST be refused while a turn is in
progress (the projection would change under a running computation, violating step atomicity
from [ac-queue-steer.md](ac-queue-steer.md) R1). It does not undo external effects — files
written, commands run — and MUST NOT claim to; it edits the model's view, nothing else.

## 4. Fork

### 4.1 Cut points

Let `B(L)` be the set of **canonical cut points** of `L`: the recorded starts of completed
turns, plus the end of the log. Forking is permitted **only at canonical cut points**:

- A user message that entered a turn by steering is *not* a cut point — it has no independent
  turn boundary, and a history cut mid-turn would split a step (violating step atomicity).
  Only a turn's initial input can head a branch.
- A cut point inside an in-progress turn does not exist yet; forking "through" a running turn
  is undefined and MUST be rejected.
- Positions given positionally (e.g. "before the *n*-th user message") are resolved against
  `E(L)` per I2.

### 4.2 The operation

For `c ∈ B(L)`:

> `fork(L, c) = L′ = ⟨m₀′⟩ ⧺ L[1..c)`  with fresh identity `ι′` and lineage `λ′ = ι`.

Properties, all REQUIRED:

- **I3 (source immutability).** `fork` reads `L` and writes only `L′`. The source session is
  never modified, locked, or annotated by being forked. Arbitrarily many forks of one source
  may exist concurrently.
- **I4 (atomic birth).** `L′`'s head and its entire copied prefix are persisted as one atomic
  append. No observer — including a crash-recovery replay — can see a half-copied fork.
- **I5 (identity).** `ι′` is fresh; the source's head, copied inside the prefix, is inert
  under the first-head-canonical rule (§2). Lineage `λ′` makes ancestry a queryable DAG; a
  fork of a fork chains lineage.
- **I6 (honesty at ragged edges).** If `c` is the end of a log whose final turn never
  completed, the copied prefix ends mid-turn. The fork MUST append the same
  deliberate-interruption marker a live cancellation would produce
  ([ac-queue-steer.md](ac-queue-steer.md) §5), so the branch's model sees an intentional cut,
  not an unexplained truncation.

A fork MAY be **ephemeral**: identical semantics with persistence elided — the natural
substrate for side-explorations that are discarded unless promoted.

### 4.3 What rides along

Because compaction records are ordinary events (A1), a copied prefix containing `κ(H′)`
replays exactly as the source did at that point: the branch inherits the compacted view. No
special case exists — this is I1 doing its job. Forking from a cut *before* a compaction
record yields the pre-compaction view, for free, by the same argument.

## 5. The transcript-editing pattern

The host capability this substrate exists to serve, stated once so the division of labor is
explicit: *select an earlier user message → fork before its turn → restore that message into
the composer of the branch for editing.* The original session is untouched (I3). Selections
that violate §4.1 (a steered message; an in-progress turn) are refused with typed errors;
selecting the very first turn degenerates to "start a new session" — a fork of an empty prefix
is one, and implementations SHOULD say so rather than materialize a trivial branch.

Everything above the arrow — selection UI, prompt restoration, presentation of lineage — is
host territory. The runtime's surface is exactly `fork(L, c)`, `ρ(k)`, and the typed
refusals.

## 6. Deferred

- **Copy-free forking.** `fork` by prefix *reference* — `L′` records `(ι, c)` instead of the
  copied prefix — trades I4's simplicity for storage sharing, and requires a lineage resolver
  with cycle detection and cut-bounds validation. The copy semantics above are deliberately a
  strict subset: a reference is a prefix that didn't need copying, so the contract survives
  the migration. Ship copies first.
- **Cold-storage compression and paginated replay** — storage engineering; no semantic
  content.
- **Garbage collection of forked prefixes** — a non-goal. Disk is cheap; the correctness of an
  append-only record is not.

---
*Provenance: this design distills the session-log and forking system of a production agent
runtime (openai/codex, Apache-2.0), studied 2026-07-21. The distillation is behavioral — no
code was carried over.*
