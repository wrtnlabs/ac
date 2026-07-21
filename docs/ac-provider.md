# RFC: The provider wire — streaming completion contract

**Status:** implemented — specification of record (2026-07-21).
**Requires:** nothing (the wire is the bottom of the stack). **Required by:** [ac-loop.md](ac-loop.md) (the loop samples through this seam), [ac-events.md](ac-events.md) (agent events project completion events), [ac-testing.md](ac-testing.md) (the scripted provider replays this contract offline).
**Interacts with:** [ac-tools.md](ac-tools.md), [ac-hooks.md](ac-hooks.md) (hooks mutate the request pre-flight; forced tool choice is their mechanism), [ac-compaction.md](ac-compaction.md) (usage is the occupancy signal its triggers read).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

Model providers differ in everything incidental — wire framing, tool-call fragmentation, usage
field names, error shapes — and agree in everything essential: a request goes up; a stream of
text, reasoning, tool calls, and accounting comes down. Letting provider detail leak upward
couples every layer above the seam to every backend below it, and the generic-SDK alternative
gets exactly the money-and-correctness parts wrong (cache breakpoint placement, usage
normalization, error classes). So the kit pins those parts down in one narrow contract and
pushes everything provider-shaped into per-provider **wire crates**.

Five requirements shape the contract:

- **R1 (one seam).** A provider is one required streaming entry point over a closed request
  and event vocabulary. Adding a backend MUST NOT touch any layer above the seam.
- **R2 (normalization).** No consumer above the seam ever observes a native wire format.
- **R3 (capability honesty).** A provider MUST NOT silently ignore a requested server tool it
  advertised. Silent dropping is reserved for capabilities it never claimed — safe only
  because hosts ask before requesting (§5).
- **R4 (server truth).** Token accounting derives exclusively from server-reported usage.
- **R5 (offline closure).** The contract is small enough to impersonate: a fully scripted
  provider exercises every consumer path with no network (§8).

## 2. Model

A **provider** is a triple `P = (id, complete, caps)`:

- `id` — a stable name for attribution and diagnostics;
- `complete : R → A ⊎ S` — the one required method, mapping a completion request to either an
  **acceptance failure** in the error taxonomy `A` (§7) or an event stream `S`;
- `caps` — the capability predicate over server tools, **false everywhere by default** (§5).

`complete` resolves when the request is *accepted* — acceptance failures (bad credentials,
rate limits, malformed requests, transport refusal) are therefore distinguishable from
mid-stream failures without inspecting events. A well-formed stream obeys the grammar:

```
S        ::=  body* terminal
body     ::=  Text | Thinking | ToolUse | Citation | UsageUpdate
terminal ::=  Stop(r) | Error(ε)          r ∈ {end_turn, max_tokens, tool_use, refusal}
```

Exactly one terminal, in final position. Cancellation is not a provider concern: a consumer
abandons a stream by dropping it, and wire crates need no cancel surface.

## 3. The request

The request is the full contract of one sampling step; wire crates encode it, never extend it.

- **Model** — the provider-scoped identifier of what to sample; it rides the request precisely
  so a pre-flight hook can swap it per step ([ac-hooks.md](ac-hooks.md)).
- **System prompt** — carried separately from the message history, so the wire crate owns its
  provider-specific placement; an adjacent flag marks a cache breakpoint after it.
- **Messages** — the effective history: roles (system/user/assistant) over typed content
  parts: text, reasoning with optional signature, redacted reasoning (an opaque provider
  blob), images, tool calls, tool results. Each message carries an optional **cache mark**: a
  host-signaled hint that a prompt-cache breakpoint belongs after it. The wire crate owns the
  encoding (e.g. an ephemeral cache-control attribute on the message's last text part); a
  wire for a provider without prompt caching drops the marks (I7).
- **Tools** — specifications from the tool registry: name, description, JSON Schema input.
- **Tool choice** — *auto*, *none*, *required*, or *force(name)*. Force names a single tool
  the model must call — the mechanism a step hook uses to pin a forced step
  ([ac-hooks.md](ac-hooks.md)). Tool choice is meaningful only alongside declared tools: with
  an empty tool list the wire MUST omit both keys.
- **Server tools** — provider-executed capabilities requested for this step (§5).
- **Sampling parameters** — token ceiling and temperature, both optional, omitted when unset.

## 4. The event stream

The wire crate maps its native stream into the closed vocabulary, with these normalizations:

- **Text and reasoning** stream as deltas, in arrival order. Reasoning carries an optional
  provider signature slot; empty deltas are dropped.
- **Tool calls are emitted whole.** Native streams fragment a call across frames; reassembly
  is the wire crate's job. A tool-call event MUST carry a complete id, name, and *parsed*
  input — absent arguments normalize to the empty object, unparseable arguments are a stream
  failure, not a truncated event. Calls MAY be emitted late but MUST precede the terminal.
- **Stop reasons are normalized**: tool-call finishes map to *tool_use*, length cutoffs to
  *max_tokens*, content filtering to *refusal*, everything else to *end_turn*. The wire MUST
  emit a terminal even when the native stream ends without a finish signal (defaulting
  *end_turn*); consumers SHOULD tolerate bare end-of-stream as *end_turn* — both sides defend.
- **In-band failure** terminates the stream: an unparseable frame or mid-stream transport
  error surfaces as an error item in the taxonomy (§7), and nothing follows it.

## 5. Server tools

A **server tool** is a capability the provider executes on its own infrastructure during
sampling — web search today. It is not a local tool: it has no run semantics, never enters the
tool registry, and produces nothing to feed back. Three rules keep the seam honest:

- **Intent is provider-agnostic.** The kit names the capability (web search, with an optional
  result cap); the wire crate maps it to whatever its provider calls it. Encodings for
  multiple requested server tools accumulate — encoding one MUST NOT clobber another.
- **The handshake precedes the request.** Hosts query the capability predicate before
  requesting. A wire crate silently ignores requests for capabilities it never advertised —
  never an error — and per **R3** MUST encode every requested capability it did advertise.
  Honesty lives in the pairing: silent-ignore without the handshake would be capability theater.
- **Results surface as citations.** Provider-side execution announces itself inline as
  citation events (URL plus optional title), never as tool results. Citations are decorative
  metadata: one without a URL MUST be skipped, never allowed to fail a load-bearing turn.

## 6. Usage accounting

Every usage update reproduces **server-reported** counts, normalized to one field contract:
the input count is the *total* prompt-side figure, and the cache-read / cache-write figures
are *subsets* of it, never additional tokens; occupancy is `input + output`, and adding the
cache fields on top double-counts. Updates are cumulative snapshots, not deltas: the last
update before the terminal is the step's accounting record. A wire crate MUST NOT synthesize
counts by tokenizing locally (R4); absent server figures are honestly zero.

## 7. Failure and retry

One taxonomy covers both failure phases (acceptance and mid-stream), and its classes are
**semantic** — what happened, never how we learned it: authentication failure; rate-limited
(with the server's stated retry-after delay when given); overloaded; prompt too large; bad
request; transport error; parse error; other. An HTTP-shaped wire classifies status codes into
these classes (auth for 401/403, rate-limited for 429 honoring a Retry-After header, bad
request for 400, overloaded for the 5xx range) and MUST NOT require consumers to read a status
code — the class is the whole classification surface; raw statuses MAY survive only inside
diagnostic text.

**The wire crate performs no retries.** Classification is its entire contribution to
resilience; the retry decision belongs to the consumer above the seam, keyed on the class:
rate-limited and overloaded are transient; authentication, bad-request, and prompt-too-large
are deterministic and MUST NOT be retried unchanged (prompt-too-large is a context signal,
[ac-compaction.md](ac-compaction.md) §4). A stream that fails after yielding events is a failed *step*:
partial output is not resumable, and retry means resampling from the request.

## 8. The scripted provider

R5 made checkable: because the seam is one method over a closed vocabulary, a provider can be
a *script* — an ordered list of turns, each the exact event sequence a model would stream, one
popped per call, every received request recorded. Proofs assert both directions of the loop:
emitted tool calls execute, and the next recorded request contains their results. A dry script
MUST end the turn cleanly (*end_turn*) rather than hang — runtime over-eagerness then surfaces
as a call-count assertion, not a deadlock. [ac-loop.md](ac-loop.md)'s guarantees are proved
against this impersonation.

## 9. Invariants

- **I1 (closed vocabulary).** No event above the seam carries provider wire format; the event
  set of §2 is exhaustive, extended only by amending this contract.
- **I2 (single terminal).** A well-formed stream has exactly one terminal event, in final position.
- **I3 (whole calls).** Every tool-call event carries a complete id, name, and parsed input;
  no fragment ever crosses the seam.
- **I4 (advertised ⇒ encoded).** For every requested server tool `t`: `caps(t)` implies the
  wire encodes `t`; `¬caps(t)` implies silent omission. No third behavior exists.
- **I5 (server-truth usage).** Every usage figure originates from the provider's server, under
  §6's total-plus-subsets field contract.
- **I6 (citation non-fatality).** No malformed citation terminates a stream.
- **I7 (marks are hints).** Erasing all cache marks leaves model-visible content unchanged.
- **I8 (choice coherence).** Tool choice is encoded iff tools are declared.

## 10. Division of responsibility

| Concern | Owner |
| --- | --- |
| Request assembly (model, history, tools, hooks, cache marks) | runtime / host |
| System-prompt placement, cache-mark encoding | wire crate |
| Fragment reassembly, stop normalization, error classification | wire crate |
| Capability advertising; server-tool encoding | wire crate |
| Whether to request a server tool (after asking) | host |
| Retry and backoff policy | consumer above the seam |
| Usage counts | provider's server |
| Occupancy arithmetic, compaction triggers | runtime / host |

## 11. Deferred

- **Reasoning replay** — the vocabulary carries signed and redacted reasoning, but re-encoding
  either form into subsequent requests is deferred; wires currently drop both on replay.
- **Further server tools** — each addition is a vocabulary amendment, not a new mechanism.
- **A shared retry executor** — the taxonomy is the contract; a reusable backoff helper above
  the seam is convenience, not doctrine. Evidence first.
