# RFC: Skills — instruction packs as injected text

**Status:** implemented — specification of record (2026-07-21).
**Requires:** nothing (skills ride the host's ordinary tool surface, [ac-tools.md](ac-tools.md)).
**Interacts with:** [ac-context.md](ac-context.md) (catalog budgeting is deferred there),
[ac-security.md](ac-security.md) and [ac-sandbox.md](ac-sandbox.md) (read grants are fixed at
assembly; skill use never changes either policy).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A model that can already read files needs no new machinery to follow domain instructions — it
needs to *find* them when relevant, without paying for them the rest of the time. Keeping
every pack permanently in context scales linearly with the library; a retrieval tool adds a
round-trip, a schema, and an execution surface to what is, in the end, text. Four requirements:

- **R1 (progressive disclosure).** A skill's steady-state cost MUST be one catalog line —
  name, description, locator. Full instructions enter context only on selection; deeper
  material the model fetches itself, by path, with the tools it already has.
- **R2 (text, not capability).** A skill only ever becomes text in context. Selecting one MUST
  NOT register a tool, widen any policy, or alter approvals — whatever a skill instructs, the
  model acts through its ordinary, already-contained tools.
- **R3 (loud rejection).** Discovery MUST account for every candidate: one that fails
  validation is reported with a human-readable reason, never dropped silently.
- **R4 (never guess).** Explicit selection resolves a name only when the answer is unique.
  Unknown and ambiguous names select nothing; structured selection surfaces the refusal (§4.2).

## 2. Model

A **skill** is a directory bearing an instruction file named `SKILL.md`. A validated skill is
a tuple `s = (n, d, p, φ)`: name, description, canonical (symlink-resolved) instruction-file
path, and the map of all frontmatter fields as written. Companion material — `scripts/`,
`references/`, `assets/` — is convention for the model, opaque to discovery. A skill's **text**
is the file as authored, frontmatter included, capped at 256 KiB with marked truncation.

The **name contract**: `n ∈ [a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most 64 characters — a
sublanguage of what the mention syntax (§4.2) consumes, so every valid name is mentionable.
The description: required, non-empty, at most 1024 characters.

A **layer** is a root directory where skills live; hosts hand discovery an ordered sequence
`Λ = ⟨λ₁, …, λₖ⟩` in precedence order (e.g. user, project, bundled). Discovery is a total
function `list(Λ) = (S, X)`: the listed skills plus every skipped candidate with its reason.

**Frontmatter** is a `---`-fenced block of **single-line `key: value` scalars**, bare or quoted,
ahead of the markdown body — deliberately not YAML. Supporting exactly the scalar subset means
anything richer (block scalars, flow collections, nested mappings, anchors, aliases, tags) is
rejected with a reason rather than silently read as a different value than a YAML parser would
produce; the one YAML behavior mirrored inside bare values is the inline ` #` comment ending
the value. Quoted values mirror YAML narrowly — double quotes recognize only the `\"` and `\\`
escapes, single quotes double (`''`) to embed one — and a malformed quote (bad escape, missing
close, trailing content) rejects the skill, as a duplicated key does; BOM, CRLF, blank and
comment lines are tolerated. `name` is optional, falling back to the directory name; either
way the result MUST satisfy the name contract — a directory named `My Skills` is skipped, not
mangled into an identity. Unknown keys are preserved verbatim in `φ` for host-level conventions.

## 3. Discovery

Each layer root is walked recursively for files named `SKILL.md`, under two bounds: directories
deeper than 6 below the root are not descended (a bound, not a report), and at most 2000
directories per root — hitting that cap emits a skip entry saying the remainder was not scanned.
Dot-prefixed directories are not descended; unreadable subdirectories, like a missing or
unreadable root, contribute nothing (hosts MAY point layers at directories that do not exist yet).

- **Symlinks.** Directory symlinks are followed, but each *physical* directory is scanned
  once — a cycle terminates, an aliased directory cannot list a skill twice. An instruction
  file that is itself a file symlink is not a candidate: the walk reports real files only.
- **Duplicates.** Duplicate *names* are legal — both list; ambiguity is resolved at mention
  time (R4), not by shadowing at discovery. Duplicate *canonical paths* (two layers reaching
  one file) dedupe to the earlier layer, with a skip entry for the later.
- **Freshness.** Every listing is a fresh scan of disk — no cache to invalidate. A name is
  never joined into a filesystem path, so a traversal-shaped name resolves to nothing.

## 4. The three channels

Skills reach the model through three text channels. There is no skill tool.

### 4.1 The catalog

A markdown block — `## Skills` — listing every discovered skill as

```
- {name}: {description} (file: {absolute path to SKILL.md})
```

followed by usage prose: trigger rules (use a skill when the user names it or the task
clearly matches its description; use all mentioned; do not carry across turns unless
re-mentioned), the progressive-disclosure procedure (read the listed file completely before
acting; resolve relative paths against the skill's directory; follow its routing into
`references/`; prefer running `scripts/` over retyping; reuse `assets/`), coordination,
context hygiene, and fallback. An empty listing renders no block at all.

The catalog is injected **once per context window** — hosts place it in the system prompt or
equivalent. It is the library's entire steady-state cost (R1): the model reads the listed file
itself, with ordinary read tools, when a skill matches — disclosure by path, not by round-trip.

### 4.2 Explicit mention

`$name` in user text selects a skill explicitly; the linked form `[$name](path)` selects by
exact canonical instruction-file path. The mention token accepts a superset of the name
contract (uppercase, underscore, `:`), so lookalikes are consumed whole; common
environment-variable names (`$PATH`, `$HOME`, …) never match, and a consumed link span is not
rescanned — a `$` inside its path cannot surface as a phantom mention. Selection follows R4:
a plain name matches only when exactly one listed skill carries it; unknown and ambiguous
mentions select nothing. Selections dedupe by path, in mention order. A host MAY select a
skill up front through structured input (the equivalent of writing the mention); an unknown
name MUST be refused naming the available skills, an ambiguous one the colliding paths.

### 4.3 Body injection

Each selected skill's text is read host-side and wrapped in a marked block appended to that
turn's input, after the user's prompt:

```
<skill>
<name>{name}</name>
<path>{path}</path>
{file contents, verbatim}
</skill>
```

Injection is **per-turn**: a body persists only by re-selection. An unreadable file becomes a
warning on the host's diagnostic channel, never a hard failure; the turn proceeds with what loaded.

## 5. Deliberately absent

- **No skill tool.** No list/load round-trip: the catalog is the list, the filesystem is the
  loader, and the model's ordinary read tools are the access path.
- **No per-skill permissions, no allowed-tools.** A skill declares no capability requirements
  and its use enforces none (R2). A host that contains reads grants its skill directories
  read access **up front, at assembly** — in-process policy and OS sandbox both — including
  canonical directories a symlink resolves outside the root. Writes never widen for a skill.
- **No execution surface.** A skill's `scripts/` run through the host's ordinary,
  already-sandboxed tools, indistinguishable from any other command the model issues.

## 6. Trust boundary

Layer roots are trusted **as prompts**. Descriptions flow verbatim into the catalog —
typically the system prompt — and bodies verbatim into turn input; nothing sanitizes either. A
hostile skill is a prompt injection with a catalog entry: hosts MUST point layers only at
directories they trust as much as their own prompts, never at roots from untrusted content.

## 7. Invariants

- **I1 (totality).** Every candidate instruction file the walk encounters either appears in
  the listing or has a skip entry with a reason; a truncated walk itself produces one.
- **I2 (physical uniqueness).** Listed skills have pairwise distinct canonical paths; when
  two layers reach one file, the earlier layer's entry survives.
- **I3 (verbatim body).** Injected contents are byte-identical to the authored file up to the
  256 KiB cap; truncation is marked and cuts on a character boundary.
- **I4 (no guessing).** A plain mention selects iff exactly one listed skill carries the
  name; resolution never touches the filesystem with the name.
- **I5 (per-turn scope).** A body injection is part of exactly one turn's input; an
  unselected skill's steady-state context cost is its catalog line.
- **I6 (policy constancy).** Containment after any skill is selected or used is identical to
  the containment assembled before the first turn.
- **I7 (freshness).** Every listing reflects disk at call time.

## 8. Division of responsibility

| Concern | Owner |
| --- | --- |
| Walk, bounds, symlink discipline, validation, skip reasons | kit |
| Catalog text, mention extraction, selection rule, injection block | kit |
| Catalog placement, rescan cadence, turn-input composition, warning surface | host |
| Read grants for skill directories (in-process policy + OS sandbox), at assembly | host |
| Reading the instruction file and companions, following them | model, via ordinary tools |
| Skill content | author — trusted as prompts (§6) |

## 9. Deferred

- **Catalog budgeting** under context pressure (shortened descriptions, omission ladders) —
  deferred to [ac-context.md](ac-context.md); today the catalog renders in full.
- **Non-filesystem locators** — v1's only locator kind is files on the host filesystem.
- **Per-skill enable/disable configuration** — a host-side filter over the listing.
- **Implicit-invocation attribution** — telemetry that a shell command ran a skill's script.

---
*Provenance: this design distills the skill system of a production agent runtime (openai/codex,
Apache-2.0), studied 2026-07-21. The distillation is behavioral — no code was carried over.*
