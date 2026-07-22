# AC: system architecture

**Status:** living overview (2026-07-21). This is the entry point to the specification set in
this directory; it defines the system's shape and the reading order. Each subsystem's contract
lives in its own document — this one contains no mechanics.

## 1. What AC is

AC is an **application-agnostic agent runtime**, built as a kit of Rust crates: model
providers, the agent loop, contained tools, skills, third-party tool integration, an OS-level
sandbox, and protocol adapters for serving — consumable as a library by any host (a desktop
application, an editor integration, a headless CLI, a server) without the kit knowing which.

The design position it occupies: runtime cores welded to their product cannot be reused;
frameworks that own your server dictate your architecture. AC is the third thing — a library
of composable parts with hard boundaries, where everything application-shaped enters through
declared seams.

## 2. The one rule

**No part of the kit may name a consumer concept.** No host domain vocabulary, no host-specific
tools, no host-shaped configuration. If a change requires the kit to know who is hosting it,
the change is wrong — a seam is extended instead. A minimal generic host is maintained
permanently as the proof: the kit must always be assemblable into a working agent with no
application attached.

## 3. The five seams

Hosts inject what varies; the kit owns what doesn't:

1. **Path policy** — tools are compiled in, but *where they may act* is a host-supplied
   judgment, composable from a small algebra of containment combinators.
   → [ac-tools.md](ac-tools.md)
2. **Lifecycle hooks** — per-step (and, designed, per-phase) interception: edit the outgoing
   request, force a tool, filter the toolset, contribute context.
   → [ac-loop.md](ac-loop.md), [ac-hooks.md](ac-hooks.md)
3. **Typed context extensions** — host state rides the run context in a type-keyed slot, so
   host tools carry what they need without the kit freezing a struct for them.
   → [ac-tools.md](ac-tools.md)
4. **Tool registration** — three paths into one registry: compiled-in typed tools, host tools,
   and wire-discovered third-party tools; uniform capability classification across all three.
   → [ac-tools.md](ac-tools.md), [ac-mcp.md](ac-mcp.md)
5. **Sandbox and store injection** — the kit ships enforcement and persistence *mechanisms*;
   the host supplies policy and location.
   → [ac-sandbox.md](ac-sandbox.md), [ac-serving.md](ac-serving.md)

## 4. The system in one paragraph

A **provider** ([ac-provider.md](ac-provider.md)) turns a completion request into a normalized
event stream. The **loop** ([ac-loop.md](ac-loop.md)) drives turns as sequences of atomic
steps — sample, execute tools, feed results back — under hooks, cancellation, and
errors-as-data. **Tools** ([ac-tools.md](ac-tools.md)) act only within host-supplied
containment, reinforced at the kernel by the **sandbox** ([ac-sandbox.md](ac-sandbox.md)).
**Skills** ([ac-skills.md](ac-skills.md)) extend the agent with injected instructions, not
code. Everything the agent does is emitted as one typed **event stream**
([ac-events.md](ac-events.md)), off which every **serving adapter**
([ac-serving.md](ac-serving.md)) is a thin, logic-free map — clients speak a wire protocol and
never link the runtime. Sessions are backed in the runtime by an
append-only **log** ([ac-fork.md](ac-fork.md)) that makes branching, rewind, and
**compaction** ([ac-compaction.md](ac-compaction.md)) pure projections of it, and persist
through a store (the flat view) or the log itself; mid-turn input
**steers** the running turn ([ac-queue-steer.md](ac-queue-steer.md)).

## 5. Reading order

**Foundations — implemented, specifications of record:**

| Document | Subject |
| --- | --- |
| [ac-events.md](ac-events.md) | the event stream: taxonomy, ordering, adapter obligations |
| [ac-loop.md](ac-loop.md) | session/turn/step, cancellation, errors-as-data, hooks |
| [ac-tools.md](ac-tools.md) | tool forms, capability classes, the containment algebra |
| [ac-provider.md](ac-provider.md) | the wire contract, server tools, usage truth, retries |

**Capabilities — implemented, specifications of record:**

| Document | Subject |
| --- | --- |
| [ac-skills.md](ac-skills.md) | instruction packs as injected text; discovery, mentions, trust |
| [ac-mcp.md](ac-mcp.md) | third-party tools; claims-not-grants trust model |
| [ac-sandbox.md](ac-sandbox.md) | kernel containment; strict/degraded/off honesty |
| [ac-serving.md](ac-serving.md) | protocol adapters; the persistence split |

**Accepted designs — not yet implemented:**

| Document | Subject |
| --- | --- |
| [ac-fork.md](ac-fork.md) | the append-only session log; forking and rewind *(implemented: `ac-rollout`)* |
| [ac-compaction.md](ac-compaction.md) | context compaction as an agent-to-agent handoff *(implemented)* |
| [ac-queue-steer.md](ac-queue-steer.md) | mid-turn input steering *(implemented)* |
| [ac-approvals.md](ac-approvals.md) | pre-flight intent classification and approval routing |
| [ac-context.md](ac-context.md) | marked fragments, injection cadence, budgeted rendering *(machinery implemented: `ac-context`)* |
| [ac-hooks.md](ac-hooks.md) | the lifecycle-phase taxonomy for extension seams |

**Doctrine — in force:**

| Document | Subject |
| --- | --- |
| [ac-security.md](ac-security.md) | the threat model and the boundary register |
| [ac-testing.md](ac-testing.md) | the proof classes and their obligations |

**Reference — explainers, not commitments:**

| Document | Subject |
| --- | --- |
| [ac-ultra.md](ac-ultra.md) | effort vs. "ultra": reasoning budget (a model parameter) vs. orchestration (a harness mode) |

## 6. Standing doctrine (the short list)

- **Errors are data.** A tool's failure is a result the model reads and recovers from;
  infrastructure failure ends the turn. The dividing line is precise and specified.
- **Truth is server-side or on disk.** Token counts come from the provider; session content
  from the record; never from client-side reconstruction.
- **The kit ships no prompts** — the host owns the system prompt — with one scoped exception:
  model-facing text that *is* a mechanism (the skills catalog, the compaction handoff) ships
  with the mechanism it operates.
- **Serving is layered, not chosen.** Editor-ecosystem and web-ecosystem protocols are sibling
  adapters over the same events; adding a wire never touches the runtime.
- **Adopt, build, or study — deliberately.** Protocols with ecosystems are adopted and pinned;
  thin wires and small engines are built; mature systems are studied and their designs
  distilled, never their code copied blind. Decisions and their rejected alternatives are
  recorded, and reopening one requires new evidence.
- **Never pretend.** The rule that names the whole posture: a capability that is not real —
  an unenforced sandbox, an unverified proof, an unimplemented claim — is stated as absent.

## 7. Non-goals

- A UI, a product, or opinions about either.
- A plugin marketplace or dynamic code loading; skills are text, extensions are compiled.
- Multi-agent orchestration (sub-agent spawn/delegate) — deliberately unspecced; see [ac-ultra.md](ac-ultra.md). It earns its own document if and when adopted, not before.
- Multi-tenant service duty: the kit assumes it acts *for one user on their machine*; hosts
  that serve many users own that isolation.
