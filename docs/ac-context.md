# RFC: The context architecture

**Status:** machinery implemented — specification of record (2026-07-22). The kit-owned
machinery of §8 ships as the `ac-context` crate: fragment classes, the recognition registry
and `injected(t)` (§2–§3, I1), the reactive change-detection primitive `emit(s)` (§5), and the
budgeted-catalog renderer with the D0–D3 ladder, reports, and host warnings (§6, I3–I4).
Recognition is wired into compaction — the handoff and the interruption marker are registered
fragment classes, filtered from the verbatim user set `U` (§3.1), replacing the earlier
one-off. The live **cadence drivers** (§4: window-establishment injection, per-turn mention
injection, per-turn reactive evaluation against real sections) are the integration layer a host
wires over this machinery (§8) and land with a concrete host consumer; **dominance** (§6) and
snapshot persistence (§9) remain deferred.
**Requires:** [ac-compaction.md](ac-compaction.md) §3 (`context′`; its R2 verbatim-user rule
depends on the recognition predicate below). **Interacts with:** [ac-skills.md](ac-skills.md)
(catalog and body injections instantiate 𝒲 and 𝒯; deferred catalog budgeting is §6 here);
[ac-fork.md](ac-fork.md) (fragments and snapshots ride the session log and must survive replay).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A runtime does not only relay what the user typed; it *injects* — a skills catalog, standing
instructions, mention-selected documents, ambient state. Naive injection fails four ways:

- **R1 (identifiability).** Injected text, once in history, is indistinguishable from user
  input: compaction preserves it verbatim as if the user said it, transcripts display it as
  the user's words, re-injection duplicates it. Machine-injected context MUST remain
  machine-identifiable in history *forever after* — from the text alone, no side table.
- **R2 (window economy).** Material valid for a whole window — the catalog, standing
  instructions — MUST NOT be re-paid every turn. It is the `context′` of
  [ac-compaction.md](ac-compaction.md): injected at window establishment, alive until replaced.
- **R3 (currency without churn).** Mutable ambient state must track reality in the model's
  view, yet unchanged state MUST cost zero marginal tokens. Emissions are append-only — an
  in-place edit would invalidate the provider's prompt cache for the entire suffix — so
  redundant re-emission is pure waste. Silence must be *meaningful*: no emission ⟺ no change.
- **R4 (bounded catalogs).** A catalog scales with what the user has installed, not with the
  task at hand; rendered unbounded, it crowds working context out of the window. Catalogs
  MUST fit a budget proportional to the window, degrade under pressure in a fixed lawful
  order, and MUST NOT degrade silently.

## 2. Model

Let `H = E(L)` be the effective history of [ac-fork.md](ac-fork.md) §3. A **fragment** is a history item
the runtime writes rather than relays. A **fragment class** is a triple

> `φ = (role, (o, c), γ)`

— the message role it is emitted under, a pair of non-empty **open/close markers** unique to
the class, and a **cadence class** `γ ∈ {𝒲, 𝒯, ℛ}` (§4). A fragment renders as `o ⧺ body ⧺ c`:
markers are in-band text, which is exactly why they survive persistence, replay, and forking.

**Recognition predicate.** For class `φ` with markers `(o, c)` and any item text `t`:

```
marked_φ(t)  ⟺  prefix≈(trimₗ(t), o)  ∧  suffix≈(trim(t), c)
```

where `trim` strips surrounding whitespace and `≈` compares ASCII-case-insensitively. The
runtime's registry of classes makes `injected(t) ⟺ ∃φ. marked_φ(t)` decidable for any history
item — a pure function of the text (R1). Every class MUST declare a **body bound**: oversized
bodies are truncated (middle-truncation MAY preserve head and tail) and reported per I4.

## 3. Recognition and filtering

Three consumers of `injected(t)`, all REQUIRED:

1. **Filter-from-user-input.** Wherever the runtime computes "what the user said" — the
   verbatim-preserved set `U` of [ac-compaction.md](ac-compaction.md) R2, transcript
   projection, mention scanning — items satisfying `injected` are excluded despite sharing
   the user role: promoting machinery to instructions corrupts them as surely as paraphrase.
2. **Strip-on-compaction.** Window-class fragments in the pre-compaction history are
   recognized and dropped; the new window receives fresh renderings inside `context′`, never
   stale copies summarized or carried across.
3. **Dedupe.** An injection driver consults history before emitting (§4); recognition is how
   it sees its own prior work after a restart or fork replay, where no in-memory record survives.

Spoofing resolves in one direction: text matching a marker vocabulary is treated as context
even if a user typed it — demoting input to context is a bounded loss; promoting context to
input is not. Marker vocabularies SHOULD be improbable in organic text.

## 4. Cadence classes

**𝒲 — per-window.** Catalogs and standing instructions: injected at window establishment —
session start, and after each compaction inside `context′` — and never re-emitted within a
window (R2). Stripped and re-rendered on compaction, so a new window sees current content.

**𝒯 — per-turn.** Mention-selected material: a document the user named this turn, injected
in full alongside that turn's input. Valid for that turn only — the standing instructions
tell the model not to carry it forward unless re-mentioned — and never re-injected by the
runtime. One turn, one injection per body, however many routes selected it; a body that
cannot be read degrades to a warning, never a failed turn.

**ℛ — reactive.** State sections, specified in §5: emitted only when what the model would be
told differs from what it was last told.

## 5. Change-detected state

A **state section** is a contributor `s = (id, σ_s, ρ_s)`: a stable identifier, a **snapshot
function** `σ_s` mapping world state to a comparison value (only the data needed to decide
what the model must be told next), and a renderer `ρ_s`. At each turn boundary the runtime
evaluates each section against three-valued prior knowledge `k ∈ {absent, unknown, known(v)}`
— no prior emission on record; prior emission, snapshot unrecoverable; exact prior snapshot:

```
emit(s) = ∅               if k = known(v) ∧ v = σ_s(now)
        = ∅               if k = absent ∧ σ_s(now) is empty
        = ρ_s(k, σ_s(now)) otherwise         (a fragment of class ℛ, appended)
```

The contract, both directions of R3: **snapshot equality suppresses emission; inequality
forces it.** The model's view of section `s` is the *last* `s`-fragment in `H`; emissions
being append-only, everything before it is byte-stable and the prompt cache holds across
turns whose state did not change. Two consequences the rule forces:

- **Becoming-empty is a change.** From `known(non-empty)` or `unknown`, a transition to
  "nothing" MUST emit a fragment saying so — silence means *unchanged*; vanishing is said aloud.
- **Recovery is conservative.** On resume or fork replay: snapshot lost but the section's
  fragment recognized in `H` (via `marked`) ⇒ `unknown`, and the section SHOULD re-emit in a
  form correct regardless of prior content; fragment gone from the effective history ⇒
  `absent`, full render. Snapshots SHOULD ride the session log so `known` is the common case.

## 6. Budgeted catalogs

A catalog rendering lists entries `e₁ ≺ … ≺ eₙ` under a deterministic total rank (provenance
tier, then name, then locator); each entry line has a **minimum** part (name + locator) and an
optional description. The budget `B` SHOULD default to **2% of the model's context window**
(approximate tokens; fixed character fallback when unknown). Every description is first
normalized under a hard per-entry cap (ellipsis-terminated), so one pathological entry cannot
monopolize `B`. Degradation is then a total order; the renderer takes the *first* level that fits:

- **D0 — full**: every entry, full (capped) descriptions.
- **D1 — redistributed caps**: all entries kept; description space above the sum of minimums
  is allocated character-fairly — short descriptions donate their unused share to long ones.
- **D2 — descriptions dropped**: minimum lines only.
- **D3 — entries omitted**: entries walked in rank order, each included iff its minimum line
  fits the budget remaining; the walk continues past a miss, so one oversized minimum line
  cannot blank every entry ranked after it.

Every rendering yields a **report** ⟨total, included, omitted, truncated-chars⟩ and, whenever
*any* content was lost — a level below D0 **or** a per-entry-cap truncation at D0 — a
host-visible warning naming what was lost (R4, I4): the actionable remedy, disabling unused
entries, belongs to a user who cannot act on what was hidden.

**Rank priority vs. monotonicity at the D3 boundary.** The D3 walk honors strict rank priority.
As the budget crosses the point where a large high-ranked entry first fits, that entry is
included — and MAY displace several smaller lower-ranked entries a tighter budget had shown. At
that single boundary the rank-order rule (the explicit construction above) takes precedence over
I3's monotonicity SHOULD: the *level* still improves monotonically with budget, and the omission
rule still holds, but the *entry count* is not monotone. Preserving both would require abandoning
rank priority in the degraded regime; the catalog's whole point is that rank encodes what matters
most, so priority wins.

**Dominance.** Alternative complete renderings of one catalog MAY exist (e.g. a compact
locator encoding via an alias table versus absolute locators): considered only when the
primary rendering degraded, selected iff lexicographically dominant — more entries included,
else fewer truncated description characters, else lower rendered cost.

## 7. Invariants

- **I1 (identifiability).** `marked_φ(render(f))` holds for every emitted fragment;
  `injected` is decidable from item text alone under persistence, replay, and forking.
- **I2 (idempotent re-injection).** Every driver checks before emitting — recognition for 𝒲,
  turn dedupe for 𝒯, snapshot equality for ℛ; re-run against a history that already reflects
  it, any driver emits nothing.
- **I3 (lawful omission).** For `b ≤ b′`, the rendering at `b` degrades that at `b′` in the
  D-order; an entry is omitted only if its minimum line did not fit the budget remaining
  after all higher-ranked included entries. Budget growth SHOULD never lose information.
- **I4 (loud truncation).** Every departure from a full rendering — catalog degradation,
  body truncation, an unreadable 𝒯 body — appears in a report and a host-visible warning.
  There is no silent step anywhere in this design.
- **I5 (meaningful silence).** For a section with a prior emission in `H`, silence at a turn
  boundary implies snapshot equality with that last emission — its snapshot is still current.

## 8. Division of responsibility

| Concern | Owner |
| --- | --- |
| Fragment classes, marker registry, recognition, filtering | kit |
| Cadence enforcement (window / turn / reactive drivers) | kit |
| Budget renderer, degradation ladder, reports, dominance | kit |
| Section snapshots, renderers, transition bodies | content module (contributor) |
| Catalog entries, ranks, descriptions | content module |
| Window size, budget fraction override, warning surfacing | host |

## 9. Deferred

- **Snapshot persistence as diffs** (merge patches) — storage compression, no semantic
  content; record full snapshots first.
- **Retrieval-gated catalogs** — rank entries by task relevance to shrink 𝒲 injections; run
  any ranker in shadow against the model's observed choices before it gates anything.
- **Fragment fingerprinting** (role + text hash) — an optimization over `marked`; not needed
  for correctness.
- **Additional locator encodings** beyond one alternative per catalog — the dominance rule
  already generalizes; add encodings on evidence.

---
*Provenance: this design distills the context-fragment, world-state, and catalog-budgeting
machinery of a production agent runtime (openai/codex, Apache-2.0), studied 2026-07-21. The
distillation is behavioral — no code was carried over.*
