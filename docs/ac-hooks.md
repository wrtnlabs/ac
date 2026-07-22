# RFC: The lifecycle-phase taxonomy

**Status:** machinery implemented — specification of record (2026-07-22). The single step
hook is now one phase of a `HookRegistry` (frozen at session construction, ordered per phase,
R5). Two phases ship wired into the loop: **step-prepare** (the live request-editing hook,
unchanged in authority — §2) and **observation** (tool start/finish, immutable input and no
return, so it cannot mutate what it watches — I4/I6). The runtime ships the RFC's worked
**stateless forced chain** (`ForcedChainHook`): it forces a tool until the *effective history*
(`request.messages`) shows a successful result of it, deriving the verdict from `E(L)` and
never from a process-local flag — so resume and fork are correct for free (§3, I5), proven by
an integration test that resumes past the bind and does not re-force. The two **contributing**
phases — *session-context* (durable per-window fragments) and *turn-input* (per-turn mention
injections) — are deferred: their contributions enter history as *marked* fragments
([ac-context.md](ac-context.md) R1), so they land with ac-context's window/turn cadence drivers
(deferred there for the same reason) and a concrete host consumer; the **lifecycle** phase
lands with its first consumer. Defining a phase ahead of a caller would be authority without a
use — the taxonomy's value is authority-by-shape at the point of use.
**Requires:** [ac-loop.md](ac-loop.md) (the live step hook this generalizes);
[ac-fork.md](ac-fork.md) §3 (the effective-history projection verdicts derive from).
**Required by:** nothing yet. **Interacts with:** [ac-provider.md](ac-provider.md) §3
(step-prepare is its pre-flight mutation seam); [ac-context.md](ac-context.md) (fragment
classes); [ac-queue-steer.md](ac-queue-steer.md) §2; [ac-compaction.md](ac-compaction.md) §3.

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The loop today exposes one extension seam: a **step hook** that runs before every sampling
request and edits the outgoing request — swap the model, filter the tool list, rewrite the
system prompt, force a tool choice. Hooks compose in registration order, each seeing its
predecessors' edits; the request is rebuilt from scratch each step, so an edit lives for one
request. The right seam for exactly one job — per-request shaping — and as the *only* seam it
receives every other kind of host logic at the wrong lifetime. Four concrete failures:

- **Durable material, re-derived per step.** A capability catalog (the available skills, say)
  belongs in the context once per window; a per-step hook recomputes it on every request, and
  any variation between steps breaks the provider's prefix cache — cost scales with steps.
- **Flags as shadow history.** A hook that must act "until X has happened" — force a binding
  tool until the bind succeeds — grows a flag: process state, not session state. After a
  resume, history says the bind happened while the flag says it didn't; after a fork cut
  before the bind, the reverse. The kit's forced-chain demonstration carries this flag today.
- **Invisible input.** Contributing *items to a turn* (the full body of an explicitly
  mentioned skill) through a request-editing hook means the model samples text the log never
  recorded. Replay, fork, and compaction then reconstruct a history the model never saw —
  violating the log's self-sufficiency ([ac-fork.md](ac-fork.md) A2).
- **Observers with edit authority.** Attribution and accounting (which skill was used, token
  telemetry) need to *see* tool traffic and contribute nothing. Run through the editing hook,
  discipline is the only thing between "observes the request" and "mutates it."

The fix is **phase honesty**: split the one hook into phases — one per lifetime that actually
exists in the loop — and give each exactly the authority its purpose needs.

- **R1 (right lifetime).** Every contribution is scoped to a phase whose lifetime — window,
  turn, or request — the *runtime* enforces; no contributor manages its own expiry.
- **R2 (record fidelity).** Everything the model samples is reachable from the log plus the
  contributor set: persisting contributions enter history; edits are pure, re-derivable.
- **R3 (resume safety).** Phases derive verdicts from the effective history, never from
  process-local state accumulated across steps.
- **R4 (least authority).** A phase's interface makes over-reach unrepresentable: observation
  receives nothing mutable.
- **R5 (deterministic composition).** Contributors within a phase compose in registration order (§3).

## 2. Model

Three nested intervals structure the loop: **window** ⊇ **turn** ⊇ **step**. Steps and turns
are as defined in [ac-loop.md](ac-loop.md) §2; a **context window** is a maximal
interval sharing one re-established context — it opens at session start and re-opens at each
compaction, its opening being where `context′` of [ac-compaction.md](ac-compaction.md) §3 is
produced. A resume opens no window: contributions are history and replay from the log. `E(L)`
is the effective history ([ac-fork.md](ac-fork.md) §3).

A **phase** is a triple (trigger, authority, lifetime). The taxonomy:

| Phase | Fires | Contributes | Contribution lives for | Sees others' output |
| --- | --- | --- | --- | --- |
| session-context | once per context window | durable instruction fragments, recorded as marked history items | the window — stripped at compaction | no — fragments concatenate |
| turn-input | once per turn, before `s₁` | items appended to the turn's input, recorded into history | forever (it is history) | no — items concatenate |
| step-prepare | once per sampling request | edits to the outgoing request: model, tool filter, system prompt, tool choice | that one request | yes — edits fold in order |
| observation | tool start/finish, usage checkpoints, skill invocations | nothing | — | — |
| lifecycle | session start/resume/end; turn start/end/abort | extension-private state seeding and flush | — | — |

The firing schedule, abstractly (`H` is the session's append-only history, `E(L)` the
effective history of [ac-fork.md](ac-fork.md) §3):

```
window open (session start | compaction, the prior window's fragments stripped):
  H ← H ⧺ (⧺ contribute_window(E(L))         over session-context contributors, in order)
turn start (input I₀):
  H ← H ⧺ I₀ ⧺ (⧺ contribute_turn(I₀, E(L)) over turn-input contributors, in order)
step i:
  ρ ← base_request(E(L))
  for h in step-prepare contributors, in order:   ρ ← h(i, ρ, E(L))
  sample ρ; run tools                        — observation sees each start and finish
```

Per-phase contract — what each MUST NOT do:

- **session-context** fragments are recorded, marked, window-class history items in the sense
  of [ac-context.md](ac-context.md): appended to `H` at window open, stripped at compaction,
  re-produced over the rebuilt window. Idempotence is recognition-dedupe over `E(L)` — a
  fragment already recognized is not re-emitted. MUST NOT read turn- or step-local state.
- **turn-input** MUST NOT edit the request, prior history, or other contributors' items. Its
  contributions become recorded items of the same kind and role as user input — no event-level
  special casing, no editorial framing prose ([ac-queue-steer.md](ac-queue-steer.md) R3) — but
  their text MUST carry the in-band markers of [ac-context.md](ac-context.md) R1, so
  compaction, transcript projection, and dedupe can exclude them from "what the user said."
- **step-prepare** MUST be a pure function of (step index, request, effective history). It
  MUST NOT write history and MUST NOT carry state from step to step (§3). This phase is the
  live hook, unchanged: the request is rebuilt each step, so edits expire by construction.
- **observation** MUST NOT contribute to context or mutate anything model-visible; it MAY emit
  host events, metrics, and extension-private state. Pairing is not guaranteed — cancellation
  can win before dispatch accepts a call — so a finish MAY arrive without its start.
- **lifecycle** MUST NOT be used as a covert contributing phase. It brackets scopes — seed
  caches on start, reconcile on resume, flush on stop — and contributes no model-visible
  content. Stop and abort callbacks MUST tolerate a missing matching start.

## 3. Mechanics

**Registration.** One registry, one ordered list per phase, frozen at session construction. An
extension needing several phases registers several narrow contributors — never one omnipotent
one. The worked example is a skills system: **session-context** (the catalog — name,
description, locator per skill — once per window), **turn-input** (the full body of each
explicitly mentioned skill, entering history), **observation** (attributing implicit
invocations — the model reading a skill document on its own), **lifecycle** (seeding the
resolver cache) — nothing in step-prepare: skills are injected text, not request surgery.

**Composition.** Within a phase, registration order is the composition order (R5). The two
contributing phases concatenate: contributor *k*'s output follows contributor *k−1*'s, no
contributor sees another's, and reordering permutes concatenation only. The editing phase
folds: each step-prepare contributor receives the request as edited by its predecessors, so
later registration wins conflicts — hosts MUST register policy-bearing hooks (a forced chain)
after cosmetic ones (a model router), and the runtime MUST NOT reorder. Observation and
lifecycle run in registration order too; nothing model-visible may depend on it (I4).

**Stateless derivation.** The doctrine, stated once: *a phase implementation is a function of
what the record says, not of what the process remembers.* The forced chain restated: instead
of a flag set by the binding tool, the step-prepare hook evaluates a predicate over `E(L)` —
"does the history contain a successful result of the binding tool?" — and forces the tool
choice while it is false. Resume and fork are then correct for free: a resumed session whose
log shows the bind does not re-force; a branch cut before the bind forces again; there is no
second source of truth to desynchronize (R3). Caches are permitted only as memoization of
such a derivation, scoped to a phase boundary and rebuilt there; the private stores lifecycle
seeds are exactly this — runtime-enforced lifetimes, never ground truth.

**Neighboring seams.** The step boundaries the drain discipline acts at
([ac-queue-steer.md](ac-queue-steer.md) §4) are the boundaries step-prepare fires at; the
once-per-window `context′` that compaction re-establishes carries the session-context phase's
re-produced fragments. Tool *definitions* are not contributed here — the tool registry owns
that seam ([ac-tools.md](ac-tools.md)); step-prepare only filters and forces.

## 4. Invariants

- **I1 (confinement).** No contribution outlives its phase: a session-context fragment does
  not survive a window rebuild unless re-produced — the compaction strip enforces this; a
  step-prepare edit affects one request; a turn-input item persists because it is history.
- **I2 (request determinism).** The request for step *i* is a pure function of (contributor
  set, registration order, log prefix, *i*) — rebuildable from the log, byte-for-byte.
- **I3 (resume equivalence).** A session resumed from its log and one that never stopped yield
  identical phase verdicts at every subsequent boundary — kill and resume between any two
  steps; the request stream is unchanged.
- **I4 (passivity).** Removing every observation or lifecycle contributor changes no
  model-visible byte of any request or history item.
- **I5 (forced-chain re-derivation).** A precondition-gating hook forces iff `E(L)` lacks the
  precondition's success record — fresh session, mid-turn, after a resume, every fork branch.
- **I6 (authority by shape).** Each phase's interface carries only the surface its row in §2
  grants — an observation input contains nothing mutable; a turn-input contributor cannot
  reach the request. Over-reach is a type error, not a review finding.

## 5. Division of responsibility

| Concern | Owner |
| --- | --- |
| Phase boundaries — when each fires | kit |
| Phase authority — what each may touch | this RFC; runtime enforces by interface shape |
| Contribution content | extension |
| Registration order | host |
| Recording contributed fragments and items into history | kit |
| Scoped private stores — creation, expiry | kit |
| Scoped private stores — contents (caches under R3) | extension |

## 6. Deferred

- **Approval claims** — a contributor answering an approval prompt holds decision authority,
  not contribution authority; [ac-approvals.md](ac-approvals.md)'s domain.
- **Output post-processing** — an ordered contributor mutating parsed output items before
  emission. The studied runtime has one; no consumer here yet — evidence first.
- **Change-detected re-rendering** — diffing a durable section against its previous snapshot,
  emitting only deltas. An optimization of session-context; adopt when measured.
- **A config-changed phase** — reserved; today configuration is fixed at construction.

---
*Provenance: this design distills the extension-contributor system of a production agent
runtime (openai/codex, Apache-2.0), studied 2026-07-21, whose skills extension — registered as
six contributor kinds — is the worked example behind §3. The distillation is behavioral — no
code was carried over.*
