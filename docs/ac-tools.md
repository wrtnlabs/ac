# RFC: The tool system — typed and raw tools, capability, and the path-policy algebra

**Status:** implemented — specification of record (2026-07-21). **Requires:** nothing. **Required by:** [ac-mcp.md](ac-mcp.md) (wire tools enter through the raw form), [ac-sandbox.md](ac-sandbox.md)
(implements the launcher seam carried here). **Interacts with:** [ac-skills.md](ac-skills.md) (hosts admit skill roots as read grants), [ac-loop.md](ac-loop.md) (every
call dispatches through the registry), [ac-approvals.md](ac-approvals.md) (capability is the hook a
read-only permission mode gates on).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

A tool call is where the model touches the world, so the tool layer is where three drifts
concentrate: **schema drift** (the model shown one shape, the decoder expecting another),
**containment drift** (a tool deciding where it may act), and **trust drift** (a wire tool's
self-description believed). Each is eliminated structurally:

- **R1 (schema fidelity).** What the model is shown MUST be derived from the same declaration that
  parses the input — or, where no compile-time declaration exists, pass through verbatim with
  validation owned by whoever wrote the schema. No third state can disagree with either.
- **R2 (host-decided containment).** A tool MUST NOT decide where it may act. Every path it touches
  is first judged by a host-supplied policy; the tool sees only the verdict.
- **R3 (failures are data).** Any failure the model caused or can repair — bad input, a policy
  refusal, an unknown tool name — MUST return as error *output* the model reads, never a runtime
  fault; that channel is reserved for infrastructure.
- **R4 (total classification, untrusted claims).** Every tool MUST carry a capability class — an
  unclassified tool cannot exist — and a class claimed over a wire MUST NOT be believed by default.
- **R5 (symlink honesty).** Containment MUST be judged against what a name actually reaches on disk,
  not its lexical spelling — a link pointing outside the permitted tree is outside.

## 2. Model

### 2.1 Two registration forms, and a third by reference

A **tool** is a quadruple ⟨name, description, input schema, capability⟩ plus a run function `run :
Input × Ctx → Output`. Tools reach the registry by exactly two forms:

- **Typed** (compile-time). The schema the model sees is generated mechanically from the typed input
  declaration, and the dispatcher decodes arguments into that same declaration before the tool runs
  — schema and decoder cannot drift because they are one artifact (R1).
- **Raw** (runtime). Name, description, and schema are supplied as data; arguments reach the tool as
  the model's raw JSON, verbatim. Validation is the tool's own job — whoever advertised the schema
  validates against it — and invalid input MUST come back as error data, never a fault.

The third path — tools discovered from an MCP server — is the raw form at a protocol boundary: spec
verbatim, server-side validation, namespacing as the collision guard. Specified in
[ac-mcp.md](ac-mcp.md); it adds nothing to this model. The **registry** is a name-keyed map holding
both forms behind one erased interface. Its order is deterministic, so the tool list the model
samples over is stable across runs. Registration under an existing name replaces; dispatch of an
unknown name is error data (R3).

### 2.2 Capability

Capability is a two-valued classification — **read-only** (cannot alter state the host answers for)
or **mutating** (may) — total by construction: the class is part of the tool's definition, so an
unclassified tool is unrepresentable (R4). Its one intended consumer is the permission mode of
[ac-approvals.md](ac-approvals.md) — design of record, not yet implemented — which lets read-only
tools run freely and gates the rest; the class is carried and exposed, gating is that layer's job.

The **untrusted-claims rule** (its wire-side formalization lives in [ac-mcp.md](ac-mcp.md) §2): a compiled-in declaration is the host's own code — trusted. Wire
annotations are self-claimed hints the MCP specification itself forbids trust decisions on, so every
wire tool defaults to **mutating**; a host MAY opt in to honoring a read-only claim, per server it
trusts. A lying server gains nothing: it is already in the gated class.

## 3. The path-policy algebra

### 3.1 Policies as resolution functions

A **path policy** is a triple `P = (base, resolve_read, resolve_write)`: a canonical absolute base
directory and two partial functions from model-supplied names to canonical absolute paths. A name
resolves to the real path a tool may then touch or is refused with a typed verdict: *outside*
(escapes containment), *denied* (the operation class is forbidden), or *invalid*. Tools resolve for
the operation they intend and act only on success; refusal text is model-facing data (R3).

### 3.2 The base policy and the resolution discipline

The leaf of the algebra is **subtree(r)**: reads and writes confined to one directory tree, resolved
by the discipline:

```
resolve(p):
  1. join       — relative names join against base; absolute names stand
  2. normalize  — fold `.` and `..` lexically; escaping the filesystem root is invalid
  3. realize    — canonicalize the deepest EXISTING ancestor (resolving its symlinks),
                  then re-append the not-yet-existing tail
  4. judge      — the realized path is contained in r, or the verdict is outside
```

Step 3 is R5: a symlink planted inside the tree but pointing out of it is refused however contained
its spelling looks — even for targets that do not exist yet, since every existing component is
resolved before judgment.

### 3.3 Combinators

Policies compose. Each combinator wraps inner policies and **delegates** the normalize/realize/judge
steps of §3.2 — a combinator may re-anchor the join (split does), but every realized-path verdict is
rendered by a leaf — so a property proven at the leaves survives composition:

- **read-only(P)** — reads delegate to `P`; writes are *denied*, the denial telling the model writes
  are not permitted *yet*: the shape a host wants while a precondition of its choosing is unmet.
- **split(R, W)** — reads contained by `R`, writes by `W`: read a whole parent tree, write one
  subtree of it. `base = base(W)`, and **every** relative name — read or write — joins against it;
  the wider read tree is reached only by `..` or absolute paths, which `R` then judges.
- **swap(P₀)** — a policy whose target can be replaced mid-run. The host installs the swap cell once
  as *the* policy of a run; a host tool may later rebind it — say, from read-only over a parent tree
  to a split policy writing one chosen subtree — and every tool observes the new policy on its next
  resolution, with zero runtime changes.
- **granted(P, G)** — reads that `P` refuses fall back to `G`, a shared grow-only set of read
  grants; writes go to `P` alone. Each grant is itself a subtree policy, canonicalized when granted
  — the target MUST exist then, else a symlink planted later could redirect it — and resolved
  symlink-safely on use. Only absolute names reach the grants.

Three laws follow (checkable as §5's invariants): every combinator's write resolver factors through
exactly one inner write resolver or refuses outright; only leaves realize and judge, so R5 proven
for subtree holds everywhere; exactly one directory anchors relative names under any composition.

## 4. Mechanics

### 4.1 The run context

One **run context** is created per run and shared by every call in it. It is the seam carrier: the
policy (§3); an optional **sandbox launcher** — tools that spawn external processes prepare their
command through it and report the achieved isolation mode in their result envelope, and its absence
means unsandboxed *and said so*, never silently ([ac-sandbox.md](ac-sandbox.md) — kernel defense in
depth beneath the in-process policy judgment); a cancellation token every long-running tool MUST
honor; **typed extensions**, by which host tools carry host state through the kit's context keyed by
type, the kit never knowing the types; and the two ledgers of §4.2.

### 4.2 Read-before-write

The context carries a per-run **file-times ledger**: the file-reading tool stamps the modification
time it observed; the file-writing tools consult it before overwriting. Observation via search or
listing deliberately confers no overwrite right — only a content read does. The check yields one of
four verdicts — *new* (target absent; free to create), *fresh* (read this run, unchanged since),
*never-read*, *stale* (read, but changed on disk since) — and a write proceeds only on *new* or
*fresh*; the other two return as error data telling the model to read first (or again), and a
successful write re-stamps, so a writer retains freshness. The context also carries per-path
**locks**: a file-writing tool holds its resolved path's lock across check→modify→write, so
concurrent edits of one file serialize instead of losing an update; distinct paths never contend.

## 5. Invariants

- **I1 (writes never widen).** No composition enlarges the write-resolvable set; a mid-run rebind
  widens writes only by *installing* a policy that permits them — an explicit host act.
- **I2 (resolution is symlink-safe at every layer).** A successful resolution's existing components
  are canonical and the verdict rendered on that form — leaves by §3.2, compositions by delegation.
- **I3 (one relative name, one file).** Checkable per call: resolving one relative name for read and
  for write yields one path or a refusal — never two files.
- **I4 (swap preserves in-flight safety).** A resolution is judged entirely by the policy current
  when it began; a rebind affects subsequent resolutions only. The swap cell's guard is never held
  across a delegated resolution, so resolving and rebinding cannot block each other.
- **I5 (classification is total; claims are untrusted).** Every registered tool has a capability; no
  wire tool is read-only without an explicit per-server host opt-in.
- **I6 (no blind overwrite).** A built-in write tool overwrites an existing file only under a
  *fresh* verdict from the ledger, serialized by the path lock.
- **I7 (model-attributable failure is data).** Unknown name, undecodable input, policy refusal, and
  tool-level failure all return as error output; the registry and dispatcher do not fault.

## 6. Division of responsibility

| Concern | Owner |
| --- | --- |
| Schema derivation (typed), erasure, deterministic order, dispatch | registry |
| Raw-input validation | the raw tool itself (MCP: the server) |
| Policy construction, composition, grant issuance, swap timing | host |
| Resolution and the containment verdict | the policy (kit combinators or the host's own) |
| Calling the resolver before touching any path | every tool |
| Capability truth for wire tools | host (per-server trust opt-in) |
| Kernel containment of spawned processes | the launcher ([ac-sandbox.md](ac-sandbox.md)) |

## 7. Deferred

- **A finer capability lattice** (e.g. network-reading vs local-reading) — binary is the contract
  until a permission model demands more; evidence first.
- **Per-tool policy views** — all tools in a run share one policy today; evidence first.
- **Deriving the OS sandbox policy from the path policy** — today the host builds both; a mechanical
  translation would remove a divergence risk but freezes both shapes prematurely.
- **Write grants** — a non-goal, not a gap: reads widen by grant, writes only by a new policy (I1).
