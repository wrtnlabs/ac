# RFC: The agent loop — session, turn, step

**Status:** implemented — specification of record (2026-07-21).
**Requires:** [ac-provider.md](ac-provider.md) (the completion stream), [ac-tools.md](ac-tools.md)
(the registry a step dispatches into).
**Required by:** [ac-events.md](ac-events.md) (this loop is the stream's sole producer),
[ac-queue-steer.md](ac-queue-steer.md) (drains at the step boundaries defined here),
[ac-compaction.md](ac-compaction.md), [ac-serving.md](ac-serving.md) (every adapter consumes
this loop's stream).
**Interacts with:** [ac-hooks.md](ac-hooks.md) (forced chains ride the step-hook seam).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The loop is the kit's kernel: everything else — serving, skills, approvals, steering — is
defined relative to what it guarantees. Four requirements shape it:

- **R1 (host-agnostic).** The loop MUST own no persistence, no transport, no UI: it reads
  history the host hands it, emits a typed event stream the host consumes, and returns.
- **R2 (failures reach whoever can repair them).** A failure the model caused or can route
  around MUST reach the *model*, as data in its own history. A failure of the machinery that
  lets the model speak at all MUST reach the *host*, as a typed error. Blurring the line either
  hides recoverable errors from the model or asks the host to repair what only the model can.
- **R3 (history valid at every boundary).** The history is replayed to the provider on every
  request; a malformed history — above all, a tool call with no matching result — poisons
  every subsequent request in the session. No termination path may produce one.
- **R4 (bounded and cancellable).** A turn MUST NOT run forever, stall unboundedly once the
  provider has accepted a request, or continue computing for an audience that has left.

## 2. Model

A **session** owns an append-only history `H = ⟨m₁, m₂, …⟩`, a provider, a tool registry, a
shared run context, and an ordered list of step hooks. It is constructed empty or **resumed**
from host-loaded history (R1); after each turn the host reads `H` back and persists it
wherever it likes. A session runs **at most one turn at a time**, structurally.

A **turn** is the computation initiated by one user input: the input is appended to `H`, then
a sequence of **steps** `s₁ … sₙ` runs until the model stops requesting tools or a bound
fires. This is the same turn/step decomposition as [ac-queue-steer.md](ac-queue-steer.md) §2;
what that document treats as an atomic unit, this one opens up:

> `sᵢ` = build request → apply hooks → sample → record response → execute all tool calls →
> record all results.

`H` changes only inside a step's two record phases; between steps it is quiescent. The turn's
continuation condition is `follow_up(sᵢ)`: another step runs iff the response issued tool
calls. (Steering extends this condition; see its RFC.)

Alongside `H`, the loop emits an **event stream** — text and thinking deltas, tool calls and
results, citations, usage, turn completion — into a host-supplied sink. The stream is
observational and carries strictly more than `H`: thinking deltas, citations, and usage are
stream-only, never recorded — `H` holds only the final text, the tool calls, and their
results, so a session resumed from `H` carries no record of prior thinking. Mid-sampling
termination adds one further divergence: streamed *text* deltas that never entered `H` (§5).

## 3. The step pipeline

**Build.** The request is assembled fresh each step: the configured model and system prompt,
the entire current `H`, the registry's tool specifications (deterministically ordered — the
model sees a stable list), and any configured provider-executed server tools. Server tools
never touch the registry or the result-feedback path; a provider that cannot honor one
ignores it, and their citations arrive as stream annotations, forwarded as events.

**Hooks.** Each registered step hook then edits the request in place, in registration order,
each seeing its predecessors' edits — the request sampled is `hₙ(…h₂(h₁(base)))`, and later
hooks win conflicts. A hook receives the step ordinal and MAY swap the model, filter the tool
list, edit the system prompt, or force a tool choice. Edits are per-request only: `H` and the
session configuration are untouched, and the next step rebuilds from base — a hook wanting a
persistent effect re-applies it each step. The ordinal exists precisely so step-indexed
policies (force tool A at step 0, tool B at step 1, then release) are one stateless function;
forced opening chains ([ac-hooks.md](ac-hooks.md)) ride this seam, opaque to the loop.

**Sample.** Deltas forward to the sink as they arrive; tool-call requests accumulate. When the
stream ends, the assistant message — final text plus every tool call — enters `H` as one record.

**Execute.** If the response carried no tool calls the turn is over: a completion event with
the model's stop reason is emitted and the turn returns it. Otherwise every tool call is
dispatched **concurrently**, each on its own isolated task. Dispatch goes through the registry
by name: it resolves the name and, for compile-time-typed tools, validates arguments by
decoding them into the declared input — a failed decode becomes an error result. Runtime-
described tools receive the model's JSON verbatim and MUST report invalid input as error data
themselves; [ac-tools.md](ac-tools.md) specifies the two forms.

**Record.** Results are appended to `H` as a single message, **in the order the model issued
the calls** — never completion order — so `H` is deterministic under concurrency. Result
events emit in the same order. The loop then re-checks its bounds and begins the next step.

## 4. Errors as data

The dividing line of R2 is **the moment of dispatch**: once the model has issued a tool call,
nothing on the tool side of that call may terminate the turn. Every outcome is serialized into
the one result the model reads:

- an unknown tool name → an error result naming it;
- arguments that fail validation (registry-side for typed tools, tool-side for
  raw ones) → an error result carrying the failure;
- a tool that runs and fails (file not found, path-policy refusal, process error) → an error
  result, authored by the tool itself — tools have no failure channel *except* error data;
- a tool that **panics** → its isolated task absorbs the unwind and the loop converts it into
  an error result naming the tool. The turn continues; the crash is a fact the model reads.

Conversely, everything that prevents the model from issuing or completing a response is a
typed, turn-terminating error to the host: the provider's failure taxonomy (authentication,
rate limit, overload, oversized prompt, malformed stream), the idle-timeout guard, the
iteration bound, and cancellation. The host decides what a dead turn means — retry, surface,
resume — because only the host can. The event vocabulary carries an error event so a serving
layer MAY put termination on the wire; the loop reports it via its return value, never the sink.

## 5. Cancellation and bounds

Three independent paths stop a turn early. What each guarantees about `H` follows from one
rule: **within the step sequence, the record phases are the only writers** — a path that
fires before a record phase leaves `H` exactly as the last completed phase left it.

- **The cancellation token** (host-triggered, shared with every tool through the tool
  context). Observed at the step boundary, while awaiting the provider connection, and
  between stream events — always with priority over forward progress. Cancellation mid-
  sampling discards the partial response entirely: no half-recorded assistant message, `H`
  ends at the previous boundary (R3). Cancellation is **not** observed between dispatch and
  record: once tool calls are dispatched, the loop waits for their results, records them, and
  terminates at the next boundary — so `H` never ends with an unanswered call. Cancellation
  MUST come through the token: a host that instead abandons an in-flight turn, dropping its
  computation mid-await, voids I2 and I3 — `H` would end with unanswered calls (R3). Tools
  that block SHOULD observe the shared token themselves; the runtime does not preempt them.
- **The idle timeout** (default five minutes; disable-able). Bounds *silence*, not duration:
  the clock arms once the provider accepts the request, re-arms on every stream event, and a
  stalled or never-closing stream terminates the turn with a timeout error. The connection
  await itself is bounded only by cancellation — a provider or host SHOULD apply its own
  connect timeout. Discard semantics identical to mid-sampling cancellation.
- **The dropped sink.** A closed sink means nobody is listening: an implicit cancel at the
  next step boundary. Mid-step, sends are discarded — at most one full step runs unobserved.

The **iteration bound** (default 16) limits sampling requests per turn, checked before each
build: a turn that exhausts it terminates in error with the model still owing a continuation,
`H` intact through the last completed step — the host MAY continue with a fresh turn on the same
session. The initiating input, appended at turn start, persists in `H` however the turn ends.

## 6. Invariants

- **I1 (append-only).** A turn only ever appends to `H`. No termination path edits or removes
  a recorded item; work recorded before the failure survives it.
- **I2 (one result per call).** Every tool call the model issues receives exactly one result —
  success, tool-authored error, validation error, unknown-name error, or panic notice —
  before the next sampling request. There is no path on which a call goes unanswered.
- **I3 (boundary atomicity).** Beyond the initiating input appended at turn start, `H`
  changes only in a step's record phases. A turn terminated mid-sampling leaves `H` exactly
  as it stood at the last boundary; no partial assistant content is ever recorded.
- **I4 (issue-order determinism).** Concurrent tool results are recorded and emitted in the
  order the model issued the calls. Two runs with identical responses produce identical `H`
  regardless of tool completion order.
- **I5 (no orphan sampling).** After cancellation is observed, no further sampling request is
  issued.
- **I6 (bounded).** A turn issues at most the configured number of sampling requests, and
  once the provider accepts a request, no inter-event silence exceeds the idle timeout.
- **I7 (hook purity).** Hooks transform only the outgoing request of their own step; `H` and
  session configuration are invisible to them as write targets. Removing all hooks changes no
  recorded history except through the model's own behavior.
- **I8 (completion honesty).** The turn-complete event is emitted iff the turn returns a stop
  reason; error terminations emit no completion event.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| Step sequencing, request assembly, bounds, record phases | loop |
| History persistence, resume source, what a dead turn means | host |
| Per-step request edits (model swap, tool filter, forced choice) | hooks |
| Name resolution, typed-input validation, panic containment | registry + task isolation |
| Failure the model reads | the tool, as error data |
| Cancellation trigger | host, through the token only — never turn abandonment (§5) |
| Event delivery beyond the sink | host / serving adapters |

## 8. Deferred

- **Mid-turn input** — designed in [ac-queue-steer.md](ac-queue-steer.md); its drain
  discipline lands at the step boundaries specified here, changing nothing inside a step.
- **Preemptive tool abort** — cancellation during execution is cooperative (the shared
  token); forcibly killing a dispatched tool while preserving I2 is future work.
- **Context-window management** — compaction is a distinct turn class over this same loop
  ([ac-compaction.md](ac-compaction.md)); the loop carries no truncation logic of its own.
