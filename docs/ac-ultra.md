# Reference: effort and "ultra" — reasoning budget vs. orchestration

**Status:** reference — an explainer anchor, not a design of record (2026-07-22). AC implements
none of this yet. It records the mechanism and where each half would land, so the distinction is
anchored before either is built.
**Relates to:** [ac-provider.md](ac-provider.md), [ac-hooks.md](ac-hooks.md), [ac-context.md](ac-context.md).

## Why this exists

Effort tiers (low … max, plus a harness "ultra" above them) are widely used but **conflate two
different mechanisms** — one in the model, one in the harness. This anchor names them so the
distinction survives into whatever AC builds.

## 1. Effort is a trained control channel, not a prompt

A reasoning model emits hidden reasoning tokens before its visible answer. Effort biases how much
serial test-time compute it spends there — roughly the reasoning-token budget the policy targets
before committing.

- It rides in a **structured request field**, and the model was reinforcement-trained to condition
  on it, so it shifts stopping behavior far more reliably than "think harder" in the prompt. The
  precise per-tier budget — and whether the top tiers also fan out into parallel sampling rather
  than one longer chain — is provider-internal and not exposed.
- The prompt still modulates *realized* effort: the tier is a ceiling/bias, the problem's
  difficulty draws it out. A trivial task won't burn the top budget even at the top tier; a hard,
  underspecified one will pull more at any tier.

## 2. "Ultra" is orchestration, not a bigger number

- At the model wire, the top harness tier is **identical to "max"** — there is no distinct model
  value above it. So "ultra" is not more thinking than max.
- Its extra is at the **harness** layer: the tier flips an orchestration mode that spends more
  compute *around* the model — proactively spawning coordinated sub-agents, each of which can
  carry its own model and effort.
- So the ladder is two regimes: **low ↔ max is a dial on the model** (reasoning budget);
  **max ↔ ultra is a dial on the harness** (how many models, coordinated how).

## 3. The orchestration mode is an injected instruction, not a watchdog

- "Proactive orchestration" works by injecting a **persistent standing instruction** that changes
  the agent's *delegation policy* — default: delegate only when the user asks; proactive: delegate
  whenever parallel work materially improves speed or quality — effective until a later mode
  message supersedes it.
- The agent still **decides and delegates through its own sub-agent tool.** Nothing intercepts its
  choices and auto-maps them to sub-agents; there is no watchdog. The only levers are the injected
  instruction and tool availability — the model chooses, under different standing orders. This is
  the general "a behavioral mode is a standing injected fragment" pattern of the context
  architecture, not a new mechanism.

## Where each half lands in AC, when adopted

| Piece | Home |
| --- | --- |
| Effort as a request parameter | the completion-request contract ([ac-provider.md](ac-provider.md)) — an agnostic tier the wire crate maps to a provider's tiers or its thinking-token budget |
| Effort as a per-step setting | the step-prepare phase ([ac-hooks.md](ac-hooks.md)), alongside model swap and forced choice |
| "A mode is a standing injected instruction" | already the reactive-injection cadence of [ac-context.md](ac-context.md) — no new mechanism |
| The sub-agent seam itself (spawn / wait / delegate) | a distinct subsystem with no spec yet; earns its own document if and when AC commits to building it — not before, per the "specs are contracts, not speculation" rule |

The takeaway for AC: adopting "effort" is a **request-parameter** change (two small edits). Adopting
"ultra" is an **orchestration** change (a subsystem decision) — and the two must not be bundled, or
a request field gets mistaken for a system mode.

---
*Provenance: the effort/orchestration split, the wire-collapse of the top tier to "max", and the
injected-instruction delegation mode are distilled from studying a production agent runtime
(openai/codex, Apache-2.0), 2026-07-22. Behavioral distillation — no code was carried over.*
