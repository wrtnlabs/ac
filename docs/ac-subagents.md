# RFC: The sub-agent seam — delegation as an injected capability

**Status:** design of record — proposed (2026-07-23). Decisions below are locked pending review
sign-off; implementation has not begun. This document discharges the deferrals that name it:
[architecture.md](architecture.md) §7 ("multi-agent orchestration … earns its own document"),
[ac-events.md](ac-events.md) §10 ("nested streams for sub-agents"), and [ac-ultra.md](ac-ultra.md)
§"where each half lands" ("the sub-agent seam itself … a distinct subsystem with no spec yet").
**Requires:** [ac-loop.md](ac-loop.md) (a child run is another instance of this loop; its
failures-are-data rule is the parent-facing contract), [ac-fork.md](ac-fork.md) §3 (a child is a
*fresh root*, explicitly not a fork), [ac-tools.md](ac-tools.md) (the injected-capability pattern,
the registry, and the containment algebra a child inherits).
**Interacts with:** [ac-sandbox.md](ac-sandbox.md) (the launcher seam this mirrors; read-only child
containment), [ac-ultra.md](ac-ultra.md) (the orchestration tier this seam unlocks),
[ac-context.md](ac-context.md) (the delegation-mode standing instruction, owned there not here),
[ac-provider.md](ac-provider.md) (per-child model and effort), [ac-serving.md](ac-serving.md) (the
host store and the spawner→child link).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A single agent run has one context window and one thread of control. Three needs push past that,
and all three are served by the same primitive — an agent that can hand a scoped task to *another*
agent and get back only the answer:

- **Fan-out parallelism.** Independent sub-tasks — draft N artifacts, investigate N leads — run
  concurrently instead of as one serial chain, collapsing wall-clock to the slowest single branch.
- **Context hygiene.** Exploration that would burn the parent's window — reading source material,
  long tool output, sibling context — is spent in a *child's* window, and the parent receives only
  the synthesis. The parent's context does not fill with the child's working notes, so compaction
  is deferred, not accelerated.
- **Scoped, enforceable exploration.** A read-only researcher's containment can be narrowed at the
  kernel and in-process, not merely asked for in a prompt — a strictly stronger guarantee than a
  permission hint.

The primitive has requirements the rest of this document satisfies:

- **R1 (injected spawning).** The kit ships the *seam* — the delegation capability, the tool that
  invokes it, and the shape of an agent definition — and MAY ship an injectable **reference
  spawner**, since assembling and driving a child run is kit machinery (the same loop, one level
  down), exactly as the kernel launcher is kit-owned in [ac-sandbox.md](ac-sandbox.md) and merely
  *installed* by the host. What the kit MUST NOT do is hardwire a child-run mechanism into the loop
  or reach into a store or UI itself: where a child session is **persisted**, how it links to its
  spawner, and how it is surfaced are the host's. What "spawn" means at the edges is a deployment
  decision.
- **R2 (recursion is structural by default).** A child MUST NOT spawn by default, and the *kit's*
  guarantee is structural — the absence of the capability from a child's context, with no counter to
  bypass. Depth beyond one is not the kit's to provide: a host MAY opt into it under its own counted
  bound (§4, §6), which the kit neither ships nor forbids.
- **R3 (scoped independence).** A child is an independent run with its own context window; the
  parent's context MUST carry only the delegation and its returned result — never the child's
  internal turns. This is why a child is a fresh session, not a fork (§3).
- **R4 (containment never widens).** A child's reach MUST be no wider than the parent's, and MAY be
  narrower; no child act may widen it.
- **R5 (failure is data).** Every child outcome — completed, aborted, errored, or a host that could
  not even assemble the child — MUST reach the parent model as tool-result data, never as a
  turn-terminating fault ([ac-loop.md](ac-loop.md) R2).
- **R6 (cancellation cascades one way).** Parent cancellation MUST propagate to the child; a child's
  abort or bound MUST NOT terminate the parent's turn.

## 2. Model

**The capability.** Delegation is an **injected context capability** — the same pattern as the OS
sandbox launcher ([ac-sandbox.md](ac-sandbox.md)): the run context carries an optional **spawner**;
`None` means delegation is unavailable here, and a delegation tool then refuses as data (§5). The
kit expresses only *intent* across the seam: a **spawn request** — which agent to run, the child's
initial input, an optional per-child model override, an optional per-child effort override (reserved
and inert until effort lands on the wire, §6), and the parent's cancellation signal — and a **spawn
result** — the child's session identifier, its final text, and a status (completed, aborted, or
errored). The request carries no depth or parent handle: a host that opts into bounded recursion
tracks depth in its own spawner state (§4), so the agnostic request stays minimal. The seam is pure
data: it names no session, rollout, or provider type, because it lives in the tool contract, beneath
the runtime.

**What the spawner does.** To run a child, a spawner assembles a complete, independent run and drives
it to completion: a provider; a **tool surface filtered** to the child's agent definition and
**never carrying the delegation tool**; a configuration (the child's model and system prompt, its
compaction eligibility, and any provider-executed **server tools** — which never enter the registry
([ac-provider.md](ac-provider.md)) and so are governed by the host's assembled config, *not* by the
definition's tool scope); a **fresh child context** whose containment is no wider than the parent's,
whose spawner slot is `None`, whose read-before-write ledger is its own, and whose cancellation token
is *derived from* the parent's so cancel flows down but not up; a **fresh root session log**; and its
own event sink. The
child's event stream is produced by the child's own turn, into that sink — nothing in the runtime
couples it to the parent's stream (§5).

**The delegation tool.** A `task`-style tool is the model-facing surface: it reads the spawner from
its context, refuses as error data when absent, issues one spawn request, and returns the child's
final text wrapped in a result envelope carrying the child's session id (the id is the model-visible
handle a later resume would name — resume itself is deferred, §9). Its description carries the
delegation discipline the model needs — launch independent tasks concurrently in one step, do not
duplicate delegated work, the result is not shown to the user so summarize what matters — as tool
text, not as a system-prompt constant.

**Agent definitions.** An **agent definition** is host-supplied data: a name and description (what
the parent model reads when choosing whom to delegate to), an optional system prompt, a **tool
scope** (the allow/deny filter over the registry), an optional default model, and a **read-only**
flag. "Sub-agent" is not a type: a definition is just a definition, and "sub" exists only
relationally — in the tool scope that omits delegation, the fresh child context, and the host's
spawner→child link. The kit ships the *shape*; the definitions themselves are host content, like a
skill catalog ([ac-tools.md](ac-tools.md) R3, host-owned trust).

## 3. Fresh session, not a fork

A child could in principle be a **fork** of the parent's log ([ac-fork.md](ac-fork.md)) — reuse the
lineage machinery, chain ancestry. It MUST NOT be. Three properties of the fork substrate each
independently disqualify it:

- **Direction.** A fork *copies the parent's entire prefix into the child*: the child's effective
  history begins with the parent's conversation up to the cut. That is the exact opposite of the
  clean, scoped window a sub-agent exists to provide (R3). A child's only inbound context is its
  explicit task prompt.
- **Mechanics.** A fork is legal only at a *completed-turn* boundary. A delegation happens
  **mid-turn**, inside the tool call — where a fork is either rejected (the turn is in progress) or,
  forced, drags the in-flight turn and an interruption marker into the child. A sub-agent has no
  well-formed fork point.
- **Lifecycle.** Fork lineage is built to *persist and dangle* — it makes ancestry a queryable DAG
  and drives the branching / transcript-editing surface, where a branch is a peer timeline the user
  keeps. A sub-agent has the opposite lifecycle: the spawner **owns** the child, which is
  cascade-deleted with it and hidden from the recents and branching views. Overloading one lineage
  field with two opposite lifecycles is the conflation a typed log exists to prevent.

Therefore a child is a **fresh root session** — no fork lineage — and the **delegation parentage
lives in host metadata**, the opaque per-session blob the store keeps verbatim and never interprets
([ac-serving.md](ac-serving.md)), never in the fork-lineage field. The kit's log stays flat by
doctrine: it grows no lineage column and no "spawned-by" head field for this. That parentage remains
recoverable from the *parent's* own recorded delegation — which names the child's id (§5) — so the
host-metadata link is a cache, not ground truth, and the log self-sufficiency of
[ac-fork.md](ac-fork.md) (A2) holds. A child is owned solely by its spawning session: a fork of the
parent inherits the *recorded* delegation and its final text, but not ownership of — nor a
resumption right over — the child (moot while resume is deferred, §9). [ac-fork.md](ac-fork.md) needs
no change of substance — only a fence sentence noting that its lineage field is peer-branch lineage
and is explicitly not the sub-agent mechanism.

## 4. Recursion, containment, cancellation

**Recursion (R2).** The default depth is **one**, and it is enforced by construction, not by a
counter: the spawner installs no spawner into the child's context and the child's tool surface omits
the delegation tool, so a child *cannot express* delegation — there is nothing to bypass. A host that
deliberately wants bounded recursion MAY inject the capability into a child under its **own** depth
and concurrency policy — tracked in its own spawner state, not on the kit's request — and the kit
MUST NOT foreclose that with a hard assertion. But the kit itself never propagates the capability —
structural depth-1 is the floor.

**Containment (R4).** A child inherits the parent's containment as an upper bound and may narrow it.
A read-only definition resolves to reads-only in-process containment, a tool surface restricted to
read-only-capability tools, and — where a launcher is installed — a kernel policy with an empty write
set. A child MUST NOT carry the **containment-rebinding tools** — those that swap the active path
policy ([ac-tools.md](ac-tools.md) §3.3) — that would let it *widen* its own reach; its binding is
inherited from the parent, never re-negotiated by a child. The two containment layers
compose for a child exactly as they compose for the parent ([ac-tools.md](ac-tools.md) I1,
[ac-sandbox.md](ac-sandbox.md)'s two-layer rule) — transitively, since a child is just another run.

**Cancellation (R6).** The child's cancellation token is *derived from* the parent's, not shared:
parent cancellation propagates down, and a child abort or bound never bubbles up. A child that hits
its iteration bound, idle timeout, or cancellation terminates the *child's* turn; the delegation tool
converts that outcome into tool-result data (§5), so it never reaches the parent's runtime as a
fault.

## 5. Streams, the parent's record, and failure

A child run has its **own** event sink, host-supplied, and its **own** session log. That sink MUST
be *consumed* — routed to a host buffer even when nothing is surfaced to a user — because the loop
reads a dropped sink as an implicit cancel at the next step boundary ([ac-loop.md](ac-loop.md) §5):
"the result is not shown to the user" must never become "the sink is not consumed", or the child is
cancelled before it works. The parent's event stream is untouched: a delegation appears in it as
exactly one tool-call event and one tool-result event, the same as any tool. The parent's log records exactly that — the delegation call
and the returned result — and nothing of the child's turns. This is precisely what makes R3 (context
hygiene) hold and what makes a fork the wrong model (§3): the child's working history is *elsewhere*,
in its own log, reachable by its session id but absent from the parent's effective history.

The kit adds **nothing** to the event vocabulary for this. Nested or child-labelled streams are the
host's to route — it owns the child sink, the child's stored log, and any drill-in surface — and the
parent's contract stays "one tool call, one result." A child-spawned marker event MAY be added later,
but it is the deferred "nested streams" item and would have to earn its place against the
every-variant round-trip and exhaustiveness guarantees of the event contract.

**Failure is data (R5).** The delegation tool authors every outcome as a tool result the parent
model reads: a completed child yields its **final text** and session id; an aborted or errored child
yields an error result naming what happened; and a host that cannot even assemble a child yields an
error result too. There is no channel by which a child failure becomes a parent-runtime fault — the
seam's result type has no error arm that escapes the tool, mirroring the tool contract's "failures
the model should see are data, not `Err`."

Two definitions the seam pins so implementations do not diverge. **Final text** is the text content
of the child's **terminal assistant message** — the message of the step that issued no further tool
calls, the same message whose emission ends the child's turn; it is not a fold of every assistant
utterance across the run. **Status** maps the loop's termination paths ([ac-loop.md](ac-loop.md) §4–§5)
onto the three-valued result: a normal stop is *completed*; cancellation, an exhausted iteration
bound, or an idle timeout is *aborted* (any assistant text the child recorded before the bound still
rides the result, so a bounded child is not silently empty); the provider's failure taxonomy is
*errored*.

## 6. Forward-compatibility for "ultra"

This seam is the substrate the harness "ultra" tier is built on ([ac-ultra.md](ac-ultra.md)): "ultra"
is not more thinking than "max" but *orchestration around* the model — proactively spawning
coordinated sub-agents. Three guarantees keep that composable later without reopening this spec:

- **Effort and model are per-child overrides that reference the agnostic tier.** The spawn request
  carries optional model and reasoning-effort overrides; when omitted, the child inherits the
  parent's. Effort MUST reference the single agnostic provider tier
  ([ac-provider.md](ac-provider.md)) — never a sub-agent-local effort enum and never an "ultra"
  value at the child level. Until effort lands as a request parameter, the per-child effort override
  is reserved and advisory; a host may approximate it today only by a per-step model swap.
- **The delegation *policy* is not owned here.** *When* to delegate — only on request, versus
  proactively whenever parallel work helps — is a standing behavioral mode, and a behavioral mode is
  a marked, recognized context fragment injected on the reactive cadence of
  [ac-context.md](ac-context.md), superseded by a later mode message. It is not a field of this
  subsystem. The delegation tool description carries only the *mechanism* and a conservative default;
  the kit MUST NOT hardcode an effort→policy mapping ("ultra ⇒ proactive") — that mapping is a host
  cadence-driver decision. "Ultra" is thus this seam **plus** a standing injected instruction
  **plus** the effort parameter, none bundled into the others.
- **Depth is a host policy, not a kit constant.** Depth-1 is the structural default (§4), but the
  seam leaves bounded recursion reachable as a host configuration, never forbidden by a kit-level
  assertion.

## 7. Invariants

- **I1 (recursion by absence).** No child context carries the spawner and no child surface carries
  the delegation tool unless a host deliberately installs them under its own bound; the kit never
  propagates them, so the default depth is one and any greater depth is host-bounded, never
  unbounded.
- **I2 (context disjointness).** A child's history is not in the parent's effective history; a
  delegation contributes exactly one tool call and one result to the parent's log. Resume or fork of
  the parent reproduces that call and result deterministically; the child session is independently
  persistable and resumable by its id.
- **I3 (containment monotonicity).** For every child the write-resolvable set is a subset of the
  parent's, and empty for a read-only child; no child act widens containment. The path-policy and
  kernel layers hold for a child as for any run.
- **I4 (failure-as-data).** Every child outcome is serialized into the one tool result the parent
  model reads. No child failure — of the run or of assembling it — faults the parent runtime.
- **I5 (one-way cancellation).** Parent cancellation implies child cancellation; child termination
  does not imply parent termination.
- **I6 (fresh root, not a fork).** A child log has no fork lineage; the delegation parentage lives in
  host metadata, never in the fork-lineage field. Forking remains peer-branch only
  ([ac-fork.md](ac-fork.md)).

## 8. Division of responsibility

| Concern | Owner |
| --- | --- |
| The spawner capability seam (request/result shapes, the context slot) | kit |
| The delegation tool — refuse-when-absent, one spawn, the result envelope | kit |
| The agent-definition *shape* (name, description, prompt, tool scope, model, read-only) | kit |
| Structural recursion guard (delegation absent from a child's assembled surface) | kit |
| What a child run *is* — assembling the provider, filtered surface, config, fresh child context, fresh root log, sink; driving the child turn | host (kit reference impl MAY exist in the runtime layer) |
| Session storage, the spawner→child link, cascade-delete, hide-from-recents | host |
| Child event routing, drill-in, any UI nesting | host |
| Agent-definition *content*; the depth/concurrency policy for any opt-in recursion | host |
| The child run's own loop, effective history, and containment | the child run |

## 9. Deferred

- **Background (fire-and-forget) delegation.** Foreground-only in v1; parallelism is concurrent
  delegation calls within one step. Fire-and-forget carries notify/inject machinery and anti-polling
  discipline — a separate, harder project, gated experimental even where it ships.
- **Resume of a child by session id.** Cheap given a persistent child session (the result envelope
  already exposes the id as the handle); reserved, may land as a fast follow.
- **Bounded recursion (depth > 1).** The seam leaves room (§4, §6); the kit ships depth-1 only.
- **Per-child effort override on the wire.** Lands with effort as a request parameter
  ([ac-provider.md](ac-provider.md), [ac-ultra.md](ac-ultra.md)); reserved here.
- **The delegation-mode standing injection ("ultra").** Lands with ac-context's reactive cadence
  driver and a concrete host consumer; this seam is its substrate, not its home.
- **Nested / child-labelled event streams.** Host territory; the kit adds no event variant, keeping
  the parent's contract at one tool call and one result.
- **Custom, user-authored agent definitions.** v1 definitions are bundled host data; a discovery
  format (frontmatter files, a config seam) is a follow-up once the built-in shape proves out.
- **Ergonomic gaps to smooth when implementing:** a registry-filter helper (a child surface is built
  by re-registration today, and wire-registered tools have no re-registration path without their
  sources); a read-only kernel-policy constructor to parallel the read-only in-process policy; and a
  final-text accessor on a completed run (the child's answer is read back from its projected log
  today). None blocks the seam; all are quality-of-implementation.

---
*Provenance: this design distills the sub-agent architectures of two production agent runtimes,
studied 2026-07-23. openai/codex (Apache-2.0) contributes the hard sub-agent subsystem — first-class
child threads spawned through an injected service, per-child model and reasoning-effort overrides,
and the effort/orchestration split of [ac-ultra.md](ac-ultra.md) (its top tier collapsing to "max"
at the wire). sst/opencode (MIT) contributes the "one agent type, a `task` tool as the surface"
synthesis and the derived-permission recursion guard, which this spec sharpens into recursion by
capability-absence. The distillation is behavioral — no code was carried over.*
