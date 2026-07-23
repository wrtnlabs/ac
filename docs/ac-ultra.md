# RFC: Effort and "ultra" — the reasoning dial, the orchestration dial, and their assembly

**Status:** implemented — specification of record (2026-07-23), and **live-proven**: under a host
`--ultra` switch, a real model told nothing about delegating fanned a parallelizable task out to
sub-agents on its own, at max effort. All three ingredients ship: the **effort request parameter**
(§3, in `ac-types`/`ac-provider`/the OpenRouter crate), the **delegation-mode standing injection**
over ac-context's reactive cadence driver (§4, in `ac-context`/`ac-runtime`), and the **host
assembly** (§5, the reference host's `--ultra`). The kit stays agnostic — it maps no effort tier to
a delegation policy; ultra is composed entirely host-side.
**Requires:** [ac-provider.md](ac-provider.md) (the completion contract effort extends),
[ac-context.md](ac-context.md) (the reactive-injection cadence the mode fragment rides),
[ac-hooks.md](ac-hooks.md) (effort as a step-prepare edit; the reactive phase the driver wires),
[ac-subagents.md](ac-subagents.md) (the seam ultra orchestrates).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation — two mechanisms one word conflates

Effort tiers (low … max, plus a harness "ultra" above them) are widely used but conflate two
different mechanisms — one in the model, one in the harness. The distinction is the whole design:

- **Effort is a trained control channel, not a prompt.** A reasoning model spends hidden
  reasoning tokens before its answer; effort biases how much serial test-time compute it spends
  there. It rides a **structured request field** the model was reinforcement-trained to condition
  on, so it shifts stopping behavior far more reliably than "think harder" in the prompt. The
  precise per-tier budget — and whether the top tiers fan out into parallel sampling rather than
  one longer chain — is provider-internal and unexposed. The prompt still modulates *realized*
  effort: the tier is a ceiling, the task's difficulty draws it out.
- **"Ultra" is orchestration, not a bigger number.** At the model wire, the top harness tier is
  **identical to "max"** — there is no distinct model value above it. "Ultra" spends more compute
  *around* the model: it proactively spawns coordinated sub-agents, each of which may carry its
  own model and effort. So the ladder is two regimes: **low ↔ max is a dial on the model**
  (reasoning budget); **max ↔ ultra is a dial on the harness** (how many models, coordinated how).
- **The orchestration mode is an injected instruction, not a watchdog.** Proactive orchestration
  works by injecting a **standing instruction** that changes the agent's *delegation policy* —
  default: delegate only when asked; proactive: delegate whenever parallel work materially
  improves speed or quality — effective until a later mode message supersedes it. The agent still
  **decides and delegates through its own sub-agent tool**; nothing intercepts its choices and
  auto-maps them. The only levers are the injected instruction and tool availability. This is the
  general "a behavioral mode is a standing injected fragment" pattern of
  [ac-context.md](ac-context.md), not a new mechanism.

The requirements that fall out:

- **R1 (agnostic effort).** Effort MUST be a provider-agnostic tier on the completion contract; a
  wire crate maps it to the provider's reasoning control. The kit MUST NOT model a provider's
  private budget, and MUST NOT expose "ultra" as a *model* value — the top model tier is "max".
- **R2 (effort is a turn setting, not a constant).** Effort MUST be settable per turn and
  adjustable per step (the step-prepare phase), never frozen for a session, and MUST flow to a
  child through the spawn request.
- **R3 (delegation policy is injected, not wired).** The "when to delegate" mode MUST be a marked,
  recognized [ac-context.md](ac-context.md) fragment on the reactive cadence, superseded by a
  later mode message — **not** a field of the sub-agent subsystem, and **not** a kit-level
  effort→policy mapping. The kit MUST NOT hardcode "ultra ⇒ proactive".
- **R4 (ultra is a host composition).** Ultra is assembled by a host from three kit-shipped
  ingredients (effort, the injection driver, the seam). The kit MUST NOT ship an "ultra" preset
  that bundles a delegation policy — that judgment is the host's ([ac-subagents.md](ac-subagents.md) §6).

## 2. Model

Ultra is not a primitive; it is the composition **seam + effort + delegation-mode injection**:

```
ultra(host)  ≝  enable(sub-agent seam)                 [ac-subagents.md — done]
              ∧ effort := max            (model dial)  [§3]
              ∧ inject(delegation = proactive)  (harness dial) [§4]
```

The kit ships the three operands; the host writes the ∧. §3 and §4 specify the two operands not
yet built; §5 specifies the host assembly and what it MAY NOT assume.

## 3. Component A — effort as a request parameter

**The tier.** A provider-agnostic `Effort` with the tiers **low, medium, high, max** — the model
dial of §1, no "ultra" among them (R1). It is added to the completion request as an optional
field; absent means the provider's default. Two distinct facts must not be conflated: §1's *ultra
≡ max* is a **harness-tier identity** (the top harness tier adds no model value above max); this
paragraph's *agnostic-max → provider-strongest* is a **wire mapping** (a four-tier agnostic enum
onto whatever a provider exposes). For the reference OpenRouter crate `low`/`medium`/`high` map
through and **max maps to `high`** (OpenRouter exposes three levels) — the collapse is the wire
crate's, not a flattening of the agnostic tier. A provider with no reasoning control **silently
ignores** the field: it is a **hint**, in the sense of the cache marks of
[ac-provider.md](ac-provider.md) (erasing them changes no model-visible content) — *not* a
capability-handshake surface like server tools, which are queried before use. If a host must know
whether effort took effect, a `supports_reasoning`-style predicate is the follow-up; v1 treats it
as a hint.

**Where it is set (R2).** Effort is a **turn setting, not a session constant** — a default that a
per-step edit refines, never a freeze. Three seams, most-specific wins:

- **Session default** — an optional effort on the agent configuration, applied to every request
  the loop builds (a *default*, exactly as the model is a default — not a freeze; the seams below
  refine it).
- **Per-turn / per-step override** — the **step-prepare** hook ([ac-hooks.md](ac-hooks.md)) MAY
  edit the request's effort, alongside model swap and forced choice. Per-turn effort is just the
  step-0 edit; there is no separate turn-scoped handle. Edits expire by construction (the request
  rebuilds each step), so a turn MAY open at high and drop to low once the hard reasoning is done.
- **Per-child override** — the spawn request's reserved effort field
  ([ac-subagents.md](ac-subagents.md)) becomes live, and the precedence is **spawn-request
  override → the agent definition's default → the parent run's effort**, exactly parallel to how
  `model` resolves. This requires an optional **default effort on `AgentDefinition`** (it has a
  default `model` but no default effort today): without it, under `--ultra` (parent at max) a child
  that omits effort would inherit max, and a cheap `explore` agent could not declare itself cheap.
  With it, `explore` sets `effort = low` in its definition and a spawn need not repeat it. (The
  reserved `SpawnRequest.effort` — a string today — becomes the agnostic `Effort` enum, and the
  `task` tool's string input maps onto it.)

**This amends** [ac-provider.md](ac-provider.md) (the request field + the wire-mapping obligation),
[ac-hooks.md](ac-hooks.md) (effort named among the step-prepare edits), and
[ac-subagents.md](ac-subagents.md) / `AgentDefinition` (the default-effort field + the live
per-child override). It is independently useful — it is the outstanding provider-parity crux — and
has no dependency on §4.

## 4. Component B — the delegation-mode standing injection

The "when to delegate" mode is a **reactive state section** (the ℛ cadence of
[ac-context.md](ac-context.md) §4–§5): a contributor whose **desired snapshot** `σ` is the current
mode (`on-request` | `proactive`) and whose renderer is the standing-instruction fragment the model
reads. It is emitted as a **marked, recognized** fragment (§2–§3 of [ac-context.md](ac-context.md)):
identifiable in history from its text alone, filtered from "what the user said", and — the load-
bearing property — its **prior** is read from that history, not from a process-local snapshot.

**Prior is derived from `E(L)`, always.** The single correction that makes this sound across
compaction, resume, and fork: the driver computes what the model was *last told* as **the last
recognized mode fragment in the effective history `E(L)`**, never a retained in-memory value. This
broadens the recovery rule of [ac-context.md](ac-context.md) §5 — which today scopes log-derived
prior to *resume or fork replay* — to **every window re-establishment**, and is the amendment this
component makes to §5. With prior thus grounded, `emit(σ) ⟺ σ ≠ prior(E(L))` behaves correctly at
each firing point:

- **window open** (session start; after each compaction). A compaction **strips** the recognized
  mode fragment along with the old window ([ac-compaction.md](ac-compaction.md)), so the re-derived
  `prior(E(L))` is now **absent** — and `σ ≠ absent` emits, giving the new window the current mode.
  The freshly-opened window is therefore never mode-blind. (Note this is *not* the old §5
  resume-only "conservative re-emit": a live compaction loses no in-memory snapshot; correctness
  comes entirely from prior being re-read from the post-strip `E(L)`.)
- **turn boundary**. A **flip** makes `σ ≠ prior` → one superseding fragment is appended (R3 of §1,
  "effective until a later mode message supersedes it"). **No change** → `σ = prior` → nothing is
  appended: the meaningful-silence property, so an unchanged mode costs zero tokens and the prompt
  cache holds.

**Flipping, resume, and fork.** The host sets and flips `σ` through a small runtime handle; a flip
is picked up at the next turn boundary. Because *both* halves of the decision derive from `E(L)` —
`prior` directly, and `σ` **re-hydrated from the last recognized mode fragment in `E(L)` when a
fresh process resumes or forks** ([ac-context.md](ac-context.md) §5, "snapshots SHOULD ride the
session log") — a resumed or forked session continues at the *logged* mode, not a default, and a
host flip after resume legitimately overrides it. This is what earns resume/fork correctness; it is
**not** the free ride of a stateless hook, because the desired mode is live host intent — the
correction is that the handle must be seeded from the log, never left at a process default (the
"flag as shadow history" [ac-hooks.md](ac-hooks.md) §1 condemns). There is no watchdog: flipping
the mode changes only the standing instruction; the model still chooses whether to delegate.

**Registration.** Recognition (the compaction strip, the "not what the user said" filter) is kit
machinery keyed on **registered** fragment classes (the runtime registers the handoff and
interruption classes today). The delegation-mode class — its open/close markers, satisfying the
non-empty, no-edge-whitespace assertions of [ac-context.md](ac-context.md) — MUST be registered
into the session's fragment registry, or the fragment would survive as if the user typed it (an R1
violation). The reactive **driver** and this registration are kit machinery; the mode's **rendered
prose** and the flip are host content.

**Where it lands, honestly.** [ac-hooks.md](ac-hooks.md) has **no "reactive" phase** — its wired
phases are step-prepare and observation, and its §6 anticipates this exact behavior as a *deferred*
"change-detected re-rendering, an optimization of session-context." This component **lands the
ac-context ℛ cadence as a wired driver** and reconciles the two docs: the driver fires at window
open (the session-context lifetime) and at turn boundaries (the ℛ per-turn evaluation of
[ac-context.md](ac-context.md) §5), which no single existing hook phase spans. The spec MUST name
it the reactive cadence driver, not a pre-existing phase.

**This amends** [ac-context.md](ac-context.md) (promoting the §4–§5 reactive cadence driver from
deferred to specified, and broadening the §5 recovery rule to every window re-establishment) and
[ac-hooks.md](ac-hooks.md) (landing the ℛ driver its §6 anticipated). The kit provides the driver,
the registration, and the fragment machinery; the kit MUST NOT map an effort tier to a mode (R3/R4).

## 5. Component C — ultra assembly (host-only)

Ultra is a **host mode**, not a kit primitive (R4). In the reference host it is a single switch
that composes the three ingredients:

1. **enable the sub-agent seam** — register the delegation tool, install the reference spawner and
   the agent definitions ([ac-subagents.md](ac-subagents.md), already wired behind a host flag);
2. **set effort to max** — the session-default effort of §3 (the model dial at its top; "ultra =
   max at the wire");
3. **inject `delegation = proactive`** — set the delegation-mode section of §4 to proactive, so the
   standing instruction telling the model to delegate whenever parallel work helps is injected on
   window open and held until flipped.

The kit ships **no** bundle of these — no `ultra()` constructor that presumes a delegation policy —
because "delegate proactively" is a shipped judgment the kit's agnosticism forbids (R4,
[ac-subagents.md](ac-subagents.md) §6). Turning ultra *off* mid-session is the same switch in
reverse: flip the mode back to on-request (a superseding fragment, §4) and, if desired, lower the
effort default; the seam stays available but unforced.

**This amends** this document's own status to *implemented (host composition)* when done, adds an
`--ultra` mode to the reference host, and amends [architecture.md](architecture.md) — moving this
document from the "reference / explainer, not commitments" list to the design set, and updating its
§7 non-goal (proactive orchestration is now *specified over the seam*, not "unspecced").

## 6. Invariants

- **I1 (ultra is composed, never a kit primitive).** No kit type or function bundles effort +
  delegation-mode + seam under an "ultra" name, and nothing in the kit maps an effort tier to a
  delegation policy. Removing the host's ultra switch removes ultra; the three ingredients remain
  independently usable.
- **I2 (effort is agnostic and per-turn).** The effort tier names no provider budget and carries no
  "ultra" value; it is set per request (session default, step-prepare override, or per-child) and a
  provider without reasoning control ignores it.
- **I3 (the mode fragment is marked, idempotent, and silent when unchanged).** The delegation-mode
  fragment is recognized in history from its text alone (registered class); re-evaluating against a
  history that already reflects the current mode appends nothing. A window rebuild re-emits it not
  by a retained snapshot but because the compaction strip leaves `prior(E(L))` absent — the driver
  derives prior from the effective history at every firing point.
- **I4 (flip supersedes; resume/fork continue the logged mode).** A mode flip appends exactly one
  new fragment; the model's effective policy is the last mode fragment in `E(L)`. Resume and fork
  are correct *because both* the driver's prior *and* the host's desired snapshot derive from `E(L)`
  (the snapshot re-hydrated from the last logged mode fragment on process start) — never a process
  default. This is not the free ride of a stateless hook; the correction is the log-seeding.
- **I5 (children resolve effort by precedence).** A child runs at the effort of its spawn request,
  else its agent definition's default, else the parent run's — parallel to `model`. Ultra's
  cheap-child / expensive-parent split falls out: a low-effort `explore` definition stays cheap
  under a max-effort parent without every spawn site repeating it.
- **I6 (the seam is unchanged).** Ultra adds no delegation depth and no recursion: it only flips a
  standing instruction and sets effort. Depth-1-by-absence ([ac-subagents.md](ac-subagents.md) I1)
  holds under ultra exactly as without it.

## 7. Division of responsibility

| Concern | Owner |
| --- | --- |
| The agnostic effort tier + the request field | kit ([ac-provider.md](ac-provider.md)) |
| Mapping effort to a provider's reasoning control (+ the max collapse) | the wire crate |
| Effort seams: session default, step-prepare edit, `AgentDefinition` default, per-child override | kit; host sets the values |
| The reactive cadence driver, the mode-class registration, the marked-fragment machinery | kit ([ac-context.md](ac-context.md) + [ac-hooks.md](ac-hooks.md)) |
| The delegation-mode section's prose and the runtime flip handle | host |
| The `--ultra` composition (enable seam + max effort + proactive mode) | host |
| Whether/when to run ultra, and the delegation policy it injects | host |

## 8. Deferred

- **Provider-internal top-tier behavior** — whether "max" fans a single request into parallel
  samples is the provider's, unobservable and unmodeled here.
- **Measuring proactive-delegation quality** — a live test can assert that a real model under the
  proactive mode delegates on a parallelizable task, but "did it delegate *well*" is an evaluation
  concern, not a contract.
- **Non-mode reactive sections** — the cadence driver of §4 is general (any change-detected
  section), but the delegation mode is its only consumer in this landing; other ambient-state
  sections (a token-budget banner, a workspace summary) adopt it on evidence.
- **A finer effort ladder or per-provider tier tables** — the four-tier agnostic enum is the
  contract until a provider demands more.
- **An effort→cost accounting surface** — a host concern, not the kit's.

---
*Provenance: the effort/orchestration split, the wire-collapse of the top tier to "max", and the
injected-instruction delegation mode are distilled from a production agent runtime (openai/codex,
Apache-2.0), studied 2026-07-22: its `ReasoningEffort` (with an `Ultra` that collapses to `"max"`
at the client), per-child model and reasoning-effort overrides, and its standing "orchestration
mode" injected as a context fragment. The distillation is behavioral — no code was carried over.*
