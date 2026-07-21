# RFC: Serving — protocol adapters over one event stream

**Status:** implemented — specification of record (2026-07-21).
**Requires:** [ac-events.md](ac-events.md) (the event vocabulary), [ac-loop.md](ac-loop.md)
(turns, sessions, resume). **Required by:** [architecture.md](architecture.md).
**Interacts with:** [ac-fork.md](ac-fork.md) §2 (the coming re-scoping of persistence),
[ac-queue-steer.md](ac-queue-steer.md) (mid-turn input reaches clients through these wires),
[ac-security.md](ac-security.md) (transport-trust obligations on host binaries).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

An agent runtime dies in one of two ways at its serving boundary: welded to its first UI, so no
second client can ever exist; or run *underneath* another framework's orchestration, demoted to a
completion provider. Both locate agency in the wrong layer. The design goal is a boundary that is
a **protocol, not an API**: clients speak a wire; the runtime never learns which one.

- **R1 (wire, not linkage).** A client MUST reach the agent through a protocol. Anything linking
  the runtime directly is a *host*, with a host's obligations (§5).
- **R2 (one stream, many wires).** Every wire MUST be derivable from the one canonical event
  stream ([ac-events.md](ac-events.md)) alone; adding a wire MUST NOT touch the loop.
- **R3 (no relocation of agency).** Adapters and host binaries MUST NOT contain agent logic — no
  sampling, no tool dispatch, no step control, no history editing.
- **R4 (continuity without ownership).** The kit persists sessions durably without ever
  interpreting what a session *means* to its host.
- **R5 (loud concurrency).** Two writers on one session MUST surface as a detectable conflict,
  never as a silently forked history.

## 2. Model

A turn emits a typed event sequence `E = ⟨e₁, …, eₙ⟩` — text and reasoning deltas, tool call and
result records, citations, usage reports, a terminal outcome. `E` is the loop's *only* export;
everything a client ever renders is a function of it. A **wire** `W` is a client-facing protocol
with frame set `F_W`; an **adapter** for `W` is three pure translations:

- **ingress** `in_W` : protocol requests → the runtime's session operations — *create*, *load*,
  *prompt*, *cancel*. This set is exhaustive; nothing else crosses inward.
- **egress** `out_W` : `E → F_W*` — a per-event fold whose only state is protocol-local framing (which
  part is open, a local id counter); it never inspects history and never influences the computation.
- **hydration** `hyd_W` : `H → F_W*` — renders a stored history `H` in the wire's own vocabulary,
  for resumed clients. Both shipped wires define it (§3).

> **The layering theorem.** For a fixed history and input, the runtime's computation — sampling requests
> issued, tools executed, history produced — is identical under every wire; wires differ only in `out_W`.
> Serving is `out_W ∘ loop`; a new ecosystem costs one adapter, never a change to the loop.

## 3. The two shipped wires

**ACP** — the standardized agent↔client RPC, for editors and any out-of-process client. Ingress
answers *initialize* in the adapter (capability advertisement — load is advertised only when a
store is configured) and maps new/load/prompt/cancel onto the session operations; egress maps each
event to one session-update notification — deltas to message and thought chunks, tool calls to
status-carrying tool records, usage to a context-occupancy report — while the turn's outcome rides
the prompt response and failures ride protocol errors. Hydration is **replay**: on load, stored
history is re-sent as ordinary update notifications, so a resumed client repaints through the same
path as a live turn. The wire is transport-agnostic: one connectable agent serves newline-delimited
stdio for editors and frame-per-message WebSocket for a browser — the shipped web harness bridges
socket frames into the identical agent and contains zero agent logic (R3).

**The AI SDK UI message-stream wire** — the v5 UI Message Stream Protocol, for the *web/React*
ecosystem: a stock chat client renders an AC agent with zero custom client code. The protocol
models an assistant message as explicitly bracketed parts, so egress carries exactly that framing
state — the open text or reasoning part and a part counter — emitting start/delta/end brackets,
the tool input/output lifecycle, and source and metadata chunks over server-sent events. Hydration
renders stored history as the client's message objects, pairing each tool call with its result
into a completed tool part; system messages are dropped (the host owns the system prompt — it is
not conversation). The two wires are **siblings, not stacked**: each a thin adapter off `E`,
neither importing the other; a host picks the wire its ecosystem already speaks.

## 4. The anti-pattern

A full-stack AI framework has two halves: a *server* half (an orchestration loop — sampling, tool
dispatch, step control) and a *client* half (a UI protocol plus rendering components). Only the server
half overlaps an agent kit. Running the kit **under** that half is the force-fit this RFC prohibits:
the framework's loop becomes the de facto runtime, and every kit capability — step hooks,
read-before-write, cancellation, compaction — must be re-expressed in its vocabulary or lost. The
correct composition inverts it: the kit **replaces** the framework's server half and **feeds** its
client half over the framework's own wire — the second shipped wire (§3) is exactly this composition.

## 5. Host binaries

A host binary is transport glue. It MAY hold: transport framing, origin discipline, the session factory
the runtime's seams require (provider, tool registry, path policy, system prompt — host territory by
design), storage placement, and UI conveniences like a session-list or configuration endpoint; *the
conversation is all protocol*. It MUST NOT contain agent logic (R3): a binary that inspects or edits
the event stream, reorders history, or implements a tool inline has relocated agency into the glue layer.

Opening a listening socket creates obligations ([ac-security.md](ac-security.md)). WebSocket
upgrades are exempt from the browser same-origin policy, so origin MUST be verified — else any
web page the user visits could drive a shell-capable agent on loopback — and side-effecting
endpoints MUST refuse cross-origin browser requests. The session store MUST live outside every
subtree the agent's tools can reach, or an injected prompt could destroy every session's history.

## 6. Persistence — the store as index

The relational store holds a **sessions index** and, per session, a **seq-ordered message log**
`Λ = ⟨m₀, …, m_{k-1}⟩` where seq is position. The index row carries identity, an optional title,
recency timestamps, and a **host-owned metadata blob** the kit stores verbatim and never reads
(R4) — a working directory or a mode flag is a consumer concept and MUST NOT become a column.
Deleting a session deletes the row and its log, never anything outside the store.

The append is guarded by a **seq-CAS**: a writer passes the position it believes comes next, and
the append succeeds — atomically, all messages or none — iff that expectation matches `|Λ|`;
otherwise it fails with a typed conflict carrying both positions. Of two concurrent guarded
appends, at most one succeeds; the loser re-synchronizes by reloading and MUST NOT retry blindly
into a fork (R5). Same-session concurrency across connections is thereby *detected, not prevented*
(§10). Live and durable history meet in one direction each: after a turn, the host persists the messages
the session gained; on resume, the loaded log *is* the session's history — rebuild, not repair.

**Re-scoping ahead** ([ac-fork.md](ac-fork.md)): the append-only per-session event log becomes the
record of truth; this store remains what it already is — an **index**, derived data reconstructible
from logs. The wire contracts are invariant under that migration: hydration and replay become folds
over the log's projection, and the seq-CAS discipline transfers to the log's append point unchanged.

## 7. The two session models

Both shipped wires sit on the same substrate and hold sessions differently; both shapes are valid
over it — the choice belongs to the protocol, never to the runtime.

**Slot-holding** (ACP). A connection holds live sessions; a turn serializes on its session's slot.
Cancellation reaches the live turn's token; because a cancelled execution context cannot be reused,
recovery is *resume from own history* — the same path as load. A second prompt while a turn runs is
refused as a client error, not queued (mid-turn input is a runtime capability,
[ac-queue-steer.md](ac-queue-steer.md), not an adapter's to improvise); a load over a running turn is
refused rather than orphaning an uncancellable computation. The user's prompt is persisted, seq-guarded,
*before* the turn samples — a connection that dies mid-turn loses nothing the user typed. A seq conflict
on that append refuses the prompt outright; a transient store failure does not block the turn — the
post-turn persist retries the full delta, trading the mid-turn-death guarantee for availability.

**Rebuild-per-request** (the AI SDK wire). The protocol's model is server-owns-history keyed by a
client-minted chat id, which the store adopts idempotently as the session id. Every request is
load → resume → run → persist; there are no slots and no cancel request — *disconnection is
cancellation*, while persistence runs detached from the response so an aborted turn still records
its completed work. The store is the continuity; the request is stateless.

## 8. Invariants

- **I1 (adapter neutrality).** For a fixed history and input, the persisted history a turn
  produces is identical across wires; deleting every adapter leaves the runtime fully defined.
- **I2 (frame determinism).** `out_W` and `hyd_W` are deterministic: same events (resp. same
  history) yield the same frames, up to freshly minted local identifiers.
- **I3 (same seq from same history).** Load inverts persist: two consumers loading one log derive
  identical histories and identical next-seq expectations.
- **I4 (conflicts surface).** A guarded append extends the log at exactly the expected position or
  fails whole with a typed conflict — no partial write, no silent fork. An adapter with an open
  response channel MUST surface it as a client-visible error instructing re-synchronization
  (slot-holding does, on both its appends); where persistence is detached from the response by
  design (§7), the conflict is recorded host-side and the client sees the divergence on next load.
- **I5 (index opacity).** Host metadata round-trips value-equivalent as JSON (structurally
  unchanged) and is never interpreted; no kit decision depends on its contents.
- **I6 (persistence scoped by the session model).** Rebuild-per-request runs turn and persistence
  detached from the response (§7): a client killed mid-turn still gets its completed items persisted.
  Slot-holding's turn rides its connection: client death mid-turn preserves the pre-persisted prompt,
  no more; a uniform completed-items guarantee needs that turn detached too — deferred, not a present
  invariant. Cancellation is uniform: an in-band cancel persists completed items on both wires.

## 9. Division of responsibility

| Concern | Owner |
| --- | --- |
| Turn computation, event emission, cancellation semantics | kit |
| Event → frame encoding, hydration/replay, protocol errors | adapter |
| Transport, origin discipline, storage placement, session-list conveniences | host binary |
| Provider, tool registry, path policy, system prompt | host, via the session seams |
| Sessions index, seq-guarded log, atomic append | store |
| Meaning of session metadata | host, exclusively |

## 10. Deferred

- **Preventing same-session concurrency** — the seq conflict makes it loud today; prevention
  needs process-shared session state, a seam to add when a real host needs it.
- **Steering through the wires** — the ACP adapter refuses mid-turn prompts today; on the
  rebuild-per-request wire a concurrent request is a concurrent turn, detected by the seq guard (§6).
  When [ac-queue-steer.md](ac-queue-steer.md) lands, ingress gains *steer* with no new framing.
- **Refusal truncation** — the ACP contract excludes a refused prompt from the next turn; the kit
  currently persists and replays it. Deferred until a provider emits refusals in practice.
- **Richer prompt content** — image and audio blocks are legitimately unsupported and
  unadvertised; text and resource links are the baseline.
- **Further wires** — one adapter each, by the theorem; evidence first.
