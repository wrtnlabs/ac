# RFC: The agent event stream

**Status:** implemented — specification of record (2026-07-21).
**Requires:** [ac-loop.md](ac-loop.md) (the run loop is the sole producer),
[ac-provider.md](ac-provider.md) (the completion stream this one is distilled from).
**Required by:** [ac-serving.md](ac-serving.md) (every adapter consumes exactly this stream).
**Interacts with:** [ac-queue-steer.md](ac-queue-steer.md) (turn boundaries are observable
here), [ac-testing.md](ac-testing.md) (the P1 hermetic-unit proof class the wire guard belongs to).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The kit serves many wires — editors speak the Agent Client Protocol, web clients speak the AI
SDK's UI message stream, a terminal renders plain text, a log wants JSON lines — and
providers speak as many beneath. Without a fixed middle, every serving surface couples to
every provider format: an N×M of ad-hoc translations, each with its own ordering bugs. The
**agent event stream** is that fixed middle — per turn, the run loop emits one typed,
ordered, serializable event sequence, and every consumer adapts from it and only it. It is
the kit's central serving contract. Five requirements shape it:

- **R1 (canonicality).** All observation of a turn MUST flow through this stream. No adapter
  reaches into the loop's internals or a provider's native wire.
- **R2 (completeness).** An adapter MUST make an explicit decision for every event variant.
  Deliberate absorption into the protocol's own envelope is a decision; silent dropping is not.
- **R3 (ordering).** An adapter MUST preserve emission order. Expanding one event into several
  wire items, in place, is permitted; reordering across events is not.
- **R4 (terminality).** Every served stream MUST end with an unambiguous terminal signal, on
  success and failure alike — never "turn over" vs "stream stalled" guessed by timeout.
- **R5 (wire-safety).** Events MUST serialize losslessly — onto a socket or into a log
  verbatim — and MUST round-trip back to the identical variant.

## 2. Model

A turn's emission is a finite sequence `E = ⟨e₁, …, eₙ⟩` over the alphabet
`Σ = { text, thinking, citation, usage, tool_call, tool_result, turn_complete, error }`.
Let `Δ = {text, thinking, citation, usage}` — the **delta events**, produced while a sampling
response streams. A **step** is as defined in [ac-loop.md](ac-loop.md) §2: one
sampling request, the model's response, and the complete execution of every tool call that
response issued. The producer's grammar on the success path:

```
turn      ::= tool_step* final
tool_step ::= Δ*  tool_call⁺  tool_result⁺    (equal counts, same ids, same order)
final     ::= Δ*  turn_complete               (itself a step — one that issued no calls)
```

A failed turn's emission is a proper prefix of this grammar with no terminal symbol: the
stream closes and the failure is the turn's returned **outcome**, not an event the loop emits.
`error` never appears in the producer's grammar — it exists for hosts that must frame that
outcome in-band (§6).

Emission is single-writer and non-blocking: the loop alone sends, sends never wait on the
consumer, and a departed consumer is detected at the next step boundary and converted into
cancellation — a turn nobody watches stops spending tokens and running tools.

## 3. The taxonomy

| Tag | Payload | Meaning |
| --- | --- | --- |
| `text` | string delta | Assistant-visible prose, incremental. |
| `thinking` | string delta | Reasoning, for display. Provider thinking signatures are stripped here — they belong to the provider layer's replay path (itself deferred, per [ac-provider.md](ac-provider.md)), never to observation. |
| `tool_call` | id, name, input | The model issued a tool call, input complete. No partial-input streaming: a call is announced once, whole. |
| `tool_result` | id, name, output, is_error | The resolution of exactly one announced id. A tool failure — including a panic inside the tool — is a result with `is_error`, never a stream failure. |
| `citation` | url, optional title | A source surfaced by a provider-executed server tool (e.g. web search). Citations are annotations — no local execution, nothing to feed back — so they ride their own variant, not the tool path. |
| `usage` | token counts | Server-reported accounting, a complete snapshot per sampling request — never a delta. Semantics (totals, cache subsets, occupancy, replace-never-sum) are specified in [ac-provider.md](ac-provider.md) §6. |
| `turn_complete` | stop reason | The success terminal: the final sampling's stop reason (end_turn, max_tokens, tool_use, refusal). Exactly once, last, success path only. |
| `error` | message | In-band failure framing, reserved for the serving layer (§6). The loop never emits it. |

## 4. Ordering

Within a step, ordering is layered, not interleaved:

- **Deltas first, in provider order.** All `Δ` events of a step precede its `tool_call`s:
  calls are announced only after the sampling response has fully streamed, so the issued set
  is complete and ordered before execution begins.
- **Announce all, then resolve all.** Every `tool_call` of the step is emitted before any
  `tool_result`. Execution is concurrent; **emission is ordered**: results are emitted in
  issue order regardless of completion order, so calls pair to results positionally, not
  only by id.
- **Exactly-one-result.** Each id appears exactly once as a call and once as a result, even
  when the tool crashed — the crash becomes an `is_error` result. This mirrors the loop's
  history invariant ([ac-loop.md](ac-loop.md)): the stream never shows a dangling call.
- **The terminal is last.** No event follows `turn_complete`.

## 5. Wire form

Events serialize **adjacently tagged**: a uniform discriminant field (`type`) naming the
variant in snake_case, and a `data` field carrying the payload — `{"type": "text", "data":
"…"}`, `{"type": "tool_call", "data": {"id": …}}`.

The form is forced, not stylistic. A uniform discriminant field is required so wire consumers
switch on one key (R5 implies self-description). Internal tagging — folding the discriminant
into the payload object — cannot represent variants whose payload is a bare primitive: a
plain-string payload has no object to fold the tag into, and the failure surfaces at
serialization time, at runtime, not at compile time. Adjacent tagging is the one form giving
both the uniform field and representability of every payload shape; the same decision, for
the same reason, governs the completion stream — this document specifies the tag layout of both.

The tag layout is public surface and MUST change only deliberately. It is guarded by an
every-variant round-trip test — serialize, deserialize, compare variant identity — on both
streams; that test is what makes the runtime-only failure class loud — a hermetic unit
proof in the P1 class of [ac-testing.md](ac-testing.md).

## 6. Failure: three tiers, one rule each

- **Tool failure is progress.** A failed tool is a `tool_result` with `is_error`, fed back to
  the model like any other result. It MUST NOT terminate or poison the stream.
- **Turn failure is an outcome.** Provider errors, idle timeout, iteration exhaustion, and
  cancellation resolve the turn's returned outcome; the loop emits no terminal event and the
  stream simply closes. Cancellation in particular is never an event: the user's own client
  initiated it.
- **The wire still owes a terminal (R4).** The serving layer maps the outcome onto its wire.
  A protocol with a response envelope carries it there — the ACP adapter answers the prompt
  request with a stop reason or a typed error, and cancellation is a normal response. A bare
  one-way stream frames it in-band, appending `error` as the final event. `error` is the
  escape hatch for envelope-less wires — in the taxonomy, absent from the producer's grammar.

## 7. Adapter obligations

Obligations every adapter owes, as the two live wires (ACP session updates, AI SDK UI
message chunks) demonstrate:

- **Total, explicit mapping (R2).** Every variant maps to zero or more wire items; zero is
  legal only as deliberate absorption — the ACP adapter maps `turn_complete` and `error` to
  no session update *because* the same information rides the response envelope.
- **In-order, in-place expansion (R3).** One event may become several wire items — the AI SDK
  encoder brackets text and reasoning into start/delta/end part sequences, closing an open
  part before a tool call or error — but items of different events never interleave.
- **Stateful rendering is the adapter's private affair.** Part ids, bracket state, tool-kind
  classification, occupancy denominators: all adapter- or host-supplied, none on the stream.
- **The stream is a feed, not a record.** Resume and reload repaint from persisted history
  ([ac-loop.md](ac-loop.md)), which both adapters render into their wire's message shape; no
  consumer may reconstruct durable state by replaying stored events. Corollary: `citation`
  and `usage` are ephemeral observability, absent from history — record them if you want them.

## 8. Invariants

> **I1 (total order).** A turn's events form one sequence; every consumer observes them in
> emission order. There is no concurrent emission — tool concurrency ends before the sink.
>
> **I2 (call/result discipline).** Per step: calls follow all deltas, results follow all
> calls, result order equals call order, ids biject between the two sets.
>
> **I3 (terminality).** `turn_complete` appears at most once, only as the final event, and
> iff the turn resolved successfully. The loop never emits `error`.
>
> **I4 (round-trip).** Every variant serializes under the adjacent tag layout and
> deserializes back to the same variant; checked exhaustively by test.
>
> **I5 (compile-time completeness).** Every in-tree consumer matches the event type
> exhaustively, with no wildcard arm: adding a variant fails compilation everywhere a mapping
> decision is owed — R2 enforced by the type system.
>
> **I6 (no zombie turns).** A closed consumer is detected at the next step boundary and the
> turn resolves as cancelled; emission never blocks on a slow or absent reader.

## 9. Division of responsibility

| Concern | Owner |
| --- | --- |
| Taxonomy, per-turn emission, ordering, terminality | run loop |
| Normalizing provider-native wires into completion events | provider layer |
| Tag layout and the round-trip guarantee | the kit's type surface + tests |
| Per-variant wire mapping, part bracketing, tool-kind classification | adapter |
| Terminal signal on the wire; error framing (envelope vs in-band) | adapter |
| Context-window denominator for occupancy display | serving host |
| Fan-out, buffering, multi-attach, cursors | serving host |
| Durable record, resume, hydration | store + host |

## 10. Deferred

- **Partial tool-input streaming.** Calls are announced whole; the AI SDK lifecycle leaves
  room for input deltas, deliberately unused until a consumer needs progressive rendering.
- **Structured tool outputs.** Output is a string plus `is_error`; richer payloads await evidence.
- **Sequence numbers and resumable cursors on the event itself** — host territory (ring
  buffers, replay windows) until a second host proves a shared need.
- **Nested streams for sub-agents** — out of scope until multi-agent sessions exist.
- **Durable citations and usage** — promotion into history is a store question, not a stream
  question.
