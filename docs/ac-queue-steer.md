# Design: queue + steer — user input while a turn is running

Status: **studied, design of record, not yet implemented** (2026-07-21). Grounded in a full read
of openai/codex `codex-rs` at commit `1836ae0612` (Apache-2.0): `core/src/session/input_queue.rs`,
`core/src/session/turn.rs`, `core/src/session/handlers.rs`, `core/src/tasks/mod.rs`,
`app-server-protocol/src/protocol/v2/turn.rs`, and the TUI composer/queue surfaces. File:line
references below are into that checkout. When AC implements this, this document is the contract.

## The problem

AC's `Session::run_turn` currently accepts input only between turns. Real hosts receive user
input *while the agent is working* — a correction ("no, use the other file"), an addition, an
interrupt. A runtime that cannot accept mid-turn input forces every host to choose between
blocking the user and aborting work in flight. Codex solved this with a small, sharp design
worth mirroring almost exactly.

## What codex does

### Steer-first: mid-turn input joins the *current* turn

There is no "queue" op in codex's core. User input submitted while a turn runs is **steered**
into it (`Session::steer_input`, session/mod.rs:3847): the items are appended to a turn-local
pending queue and nothing else happens immediately. The run loop drains that queue **only at
sampling-step boundaries** (turn.rs:227-235) — the in-flight model request always runs to
completion and in-flight tool calls are fully drained first (turn.rs:2475-2481). A steer never
tears mid-stream state.

Drained input is recorded into history as a **plain user message — no wrapper text, no "the
user interjected" framing** (session/mod.rs:3802-3820). The model just sees a new user message
after the latest tool results. On the wire, a turn that was steered is simply a turn containing
more than one user-message item between its explicit turn-started/turn-completed boundaries
(locked by test `uses_explicit_turn_boundaries_for_mid_turn_steering`).

### The drain rules (the subtle part)

`can_drain_pending_input` starts **false** and flips true after each completed sampling step
(turn.rs:191, 307). The deferral cases, from the governing comment (turn.rs:221-225):

1. At the start of a turn — the turn's own fresh input must be sampled before any steer.
2. For one request after a mid-turn auto-compaction *iff* the model still owes a tool
   continuation — the model resumes its work before seeing the steer.

And the turn-extension rule (turn.rs:322): `needs_follow_up = model_needs_follow_up ||
has_pending_input`. A steer that arrives during what would have been the **final** sampling step
keeps the turn alive for another round. One that arrives as the loop exits is caught by an outer
task re-loop (tasks/regular.rs:73-88); anything later still gets **recorded into history at task
end without being sampled** (tasks/mod.rs:568-629), so the next turn sees it — input is never
silently lost on the success path.

### Steering gates and preconditions

`steer_input` enforces (session/mod.rs:3847-3920):

- **No active turn** → `SteerInputError::NoActiveTurn(items)` — the error *returns the items* so
  the caller starts a fresh turn with them. The generic submit path is literally
  "try steer; on NoActiveTurn, spawn a turn" (handlers.rs:177-269).
- **Optional `expected_turn_id` precondition** → `ExpectedTurnMismatch { expected, actual }` —
  optimistic concurrency so a client never steers a turn other than the one it believes is
  running. The app-server `turn/steer` method makes this precondition **required**; a bare
  `turn/start` during a run silently downgrades to a steer with no precondition.
- **Non-steerable turn kinds**: review and compaction turns refuse steers with a typed error
  (`ActiveTurnNotSteerable { turn_kind }`). Clients hold the rejected steer and resubmit it as a
  fresh turn when the current one ends.

One wart to *not* copy: codex's TUI resyncs after `ExpectedTurnMismatch` by **parsing the
error-message string** for the actual turn id (tui/src/app.rs:663-685). The precondition failure
must carry the actual id as structured data.

### Interrupt semantics

`Op::Interrupt` → abort with a typed reason (`Interrupted | Replaced | ReviewEnded |
BudgetLimited`); starting a new turn always aborts a predecessor with `Replaced`
(tasks/mod.rs:320). The abort path (tasks/mod.rs:854-939):

- cancel token → 100 ms graceful window → hard task abort;
- **pending steered input is dropped** (input_queue.rs:120-124) — an interrupt means "stop,
  including what I said mid-flight"; codex's TUI compensates client-side by restoring dropped
  drafts into the composer;
- completed items survive: every finished response item was persisted as it streamed, only the
  unterminated tail is lost;
- for user-initiated interrupts, a **model-visible marker is recorded into history before the
  abort event is emitted** — "The user interrupted the previous turn on purpose. … If any
  tools/commands were aborted, they may have partially executed." — so the *next* turn's model
  knows the previous turn ended deliberately and possibly half-done, and the flush ordering lets
  clients re-read persisted state on receipt of the abort event.

### Queueing is a client concern

"Queue for the next turn" exists only in codex's clients: the TUI keeps its own queues (explicit
Tab-to-queue, steers rejected by turn-kind, drafts restored after interrupt) and submits one
merged message when the turn ends. Core never holds a next-turn queue for user input. The one
core-side nicety: long-blocking tools (e.g. `sleep`) subscribe to a watch channel that signals
steer activity and wake early — "Sleep interrupted by new input."

## What AC adopts

The mechanism maps onto AC's runtime cleanly, and it is app-agnostic throughout:

1. **`Session::steer(input) -> Result<(), SteerError>`** with codex's exact contract:
   `NoActiveTurn(input)` hands the items back; an optional expected-turn precondition fails with
   the actual turn id **as typed data**; turn kinds that cannot absorb a steer (future compaction
   tasks) refuse with a typed kind.
2. **A per-turn pending queue drained at step boundaries** in `run_turn`: drain-before-build at
   the top of each iteration, deferred until the turn's own input has sampled; steered items
   recorded as plain user messages, no framing text.
3. **`needs_follow_up |= has_pending_input`** — a steer during the final step extends the turn;
   task-end leftovers are recorded into history unsampled so the next turn sees them.
4. **Cancel drops pending steers** and records a deliberate-interrupt marker into history before
   the turn resolves as cancelled; partial completed items persist.
5. **Queueing stays host-side.** AC ships no next-turn queue; hosts that want one hold it
   themselves and submit on turn end (the codex TUI pattern). The kit's job ends at "steer or
   start".
6. **A steer-activity signal** tools can subscribe to via ctx (so a future wait-like tool can
   wake early), kept optional.

## Interaction with compaction

The one coupling point: after a **mid-turn** compaction, the drain of pending steers is deferred
for exactly one sampling request when the model owes a tool continuation — the handoff summary
plus the resumed continuation must land before new user input does. See
[ac-compaction.md](ac-compaction.md); the rule lives in the run loop, not in compaction.

## Deferred

- Client-side queue UX conventions (visible pending-steer previews, merge-on-interrupt) are host
  territory; documented here only so the split is explicit.
- An inter-agent mailbox (codex's second queue, with its current-turn/next-turn delivery phase
  machinery) is out of scope until AC has multi-agent sessions.
