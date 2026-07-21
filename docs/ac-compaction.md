# Design: compaction — context summarization as a first-class turn lifecycle

Status: **studied, design of record, not yet implemented** (2026-07-21). Grounded in a read of
openai/codex `codex-rs` at commit `1836ae0612` (Apache-2.0): `core/src/compact.rs`,
`core/src/compact_token_budget.rs`, `core/src/compact_remote*.rs`,
`core/src/compact_model_fallback.rs`, `core/src/session/turn.rs` (triggers), and
`prompts/templates/compact/`. Referenced by [ac-fork.md](ac-fork.md) (compaction markers in the
event log) and [ac-queue-steer.md](ac-queue-steer.md) (post-compaction drain deferral). When AC
implements compaction, this document is the contract.

## The problem

Long sessions exhaust the context window mid-task. Truncation loses the work; refusing loses
the user. The naive fix — "summarize the transcript when it gets big" — underspecifies
everything that matters: *when* it fires, *what survives verbatim*, *where* the summary sits in
the rebuilt history, and *what downstream machinery observes*. Codex's compaction answers all
four precisely, and the answers are the design worth mirroring.

## What codex does

### Compaction is a handoff, not a shortening

The summarization prompt (prompts/templates/compact/prompt.md) opens: *"You are performing a
CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the
task"* — progress, decisions, constraints, next steps, critical data. And when the summary
re-enters the fresh window it is prefixed (summary_prefix.md): *"Another language model started
to solve this problem and produced a summary of its thinking process…"*. Framing compaction as
an **agent-to-agent handoff** — the model is literally told the summary came from another LLM —
is why the summaries carry decisions and next steps rather than prose recap. This framing is the
single most load-bearing design choice in the system.

### One lifecycle, four triggers

Every variant runs the same lifecycle: pre-compact hooks → strategy → post-compact hooks, a
compaction item recorded into the turn stream, and a `Compacted` event carrying the summary
message **and the full replacement history** so clients and the event log see exactly what the
context became. Triggers:

- **Manual** — the user asks.
- **Pre-turn** — before sampling starts, the token budget is checked; exhausted → compact first
  (turn.rs:800-830).
- **Mid-turn** — after *every* sampling step, while the model still needs follow-up on a long
  task: budget exhausted → compact and continue the same turn (turn.rs:342-379). This is the
  "agent wraps up its long task with a checkpoint summary" behavior visible in clients.
- **Model switch** — every model declares a *compaction compatibility hash*; when a session's
  model changes and the hashes differ, codex compacts **through the previous model first**
  (compact_model_fallback.rs) so the new model starts from a clean handoff instead of an
  incompatible history. No other runtime does this.

### One lifecycle, three strategies

- **Local summarization** — the model summarizes its own history under the handoff prompt.
- **Remote** — a backend performs the summarization server-side (`compact_remote_v2`).
- **Token-budget** — *no summarization at all*: install a fresh context window. Still modeled as
  compaction so hooks, items, and analytics observe the identical lifecycle
  (compact_token_budget.rs:20-24 states this explicitly).

Strategy is invisible to everything downstream — that uniformity is the point.

### Placement is trained-behavior-aware

Mid-turn compaction injects re-established initial context *above* the last real user message so
the **summary lands as the last item** — "the model is trained to see the compaction summary as
the last item in history after mid-turn compaction" (compact.rs:56-64). Pre-turn/manual
compaction instead clears and lets the next regular turn fully re-inject initial context —
which is why once-per-context-window prompt material (e.g. a skills catalog) re-injects after
compaction, not per turn.

### Selective survival

Actual **user messages survive compaction verbatim** (each capped — 20k tokens — so one giant
paste can't monopolize the fresh window); it is the *work* — tool calls, reasoning, outputs —
that gets summarized away. The budget check itself has a cache-aware scope option
(`BodyAfterPrefix`): measure against the window *excluding the cached prefix*, so a large stable
prefix doesn't trigger needless compaction.

## What AC adopts

Compaction as a projection over the session log, not a mutation of it — which is why this
design depends on [ac-fork.md](ac-fork.md)'s substrate:

1. **A compaction item in the event log** carrying the summary and the replacement history. The
   log keeps everything; the *effective* history after a compaction item is its replacement.
   Fork, resume, and clients all read the same marker.
2. **The trigger taxonomy** — manual / pre-turn / mid-turn / model-switch — with mid-turn as
   the one that matters most (it is what lets a long task finish instead of dying at the window
   edge). Model-switch compaction enters when AC models carry compatibility metadata; the seam
   (compact-through-the-previous-provider) costs nothing to leave room for.
3. **The strategy split** with the same uniformity rule: local summarization first;
   fresh-window (no-summarize) as the degenerate strategy; remote is a host/provider concern
   behind the same lifecycle.
4. **The handoff framing** — AC ships the compaction prompt and summary prefix (adapted from
   codex's, Apache-2.0) the way it ships the skills catalog text: model-facing text that *is*
   the mechanism.
5. **Placement + survival rules** as stated: mid-turn summary lands last; user messages survive
   verbatim, capped; once-per-window host context re-injects after compaction (hosts hook this
   — the seam already exists conceptually in how the system prompt is host-supplied).
6. **Steer interaction**: after a mid-turn compaction, pending steered input defers exactly one
   sampling request when the model owes a tool continuation ([ac-queue-steer.md](ac-queue-steer.md)).

## Deferred

- Remote/server-side strategy — needs a backend; the lifecycle slot is reserved.
- Compaction analytics taxonomy (trigger/phase/strategy/status) — adopt the *shape* when AC
  grows a metrics seam; do not grow a telemetry dependency for it.
- Auto-compaction of tool outputs mid-stream (independent of window pressure) — different
  problem (output truncation already handles the worst of it); revisit with evidence.
