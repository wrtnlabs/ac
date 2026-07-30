# RFC: The tool system — typed and raw tools, capability, and the path-policy algebra

**Status:** implemented — specification of record (2026-07-21). **Amended 2026-07-24:** §3.3
gains the *prefix-remap* and *deny* combinators and single-file grants; the composition laws are
restated to cover them. **Amended 2026-07-28:** §4.3 adds the host-injected URL policy for
`fetch`; §4.2 and §4.4 make file and shell execution reusable by host adapters with
different schemas or presentation; §4.2 also specifies the stock list/write bounds; §2.3
specifies durable and transient tool output; §2.4 adds an optional dispatch gate for atomic
host-state transitions. **Requires:** nothing. **Required by:** [ac-mcp.md](ac-mcp.md) (wire tools enter through the raw form), [ac-sandbox.md](ac-sandbox.md)
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
- **R6 (atomic host transitions).** When one tool changes authority or shared host state that
  sibling tools consume, the host MUST be able to exclude sibling dispatch for the complete
  transition without serializing unrelated tools by default.

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

Capability is a three-valued classification — **read-only** (cannot alter host-owned state),
**guarded** (may mutate normally, but a host policy can collapse its effects to a read-only-safe
surface), or **mutating** — total by construction: the class is part of the tool's definition, so an
unclassified tool is unrepresentable (R4). `ToolRegistry::retain_for_read_only` keeps read-only and
guarded tools and drops mutating ones for both typed and raw registrations. A guarded tool is safe
there only because the host installs the matching policy; `shell` is the canonical case, with the
kernel write set collapsed to scratch space.

The **untrusted-claims rule** (its wire-side formalization lives in [ac-mcp.md](ac-mcp.md) §2): a compiled-in declaration is the host's own code — trusted. Wire
annotations are self-claimed hints the MCP specification itself forbids trust decisions on, so every
wire tool defaults to **mutating**; a host MAY opt in to honoring a read-only claim, per server it
trusts. Wire hints never produce **guarded** — that classification asserts host enforcement, not a
server claim. A lying server gains nothing: it is already in the gated class.

### 2.3 Output lifetime

`ToolOutput` separates three things that must not share a persistence lifetime:

- `content` is the current result string. The runtime emits it in the live `ToolResult` event and
  projects it onto the next request in the producing turn.
- `durable_content`, when present, is the truthful fallback recorded in the rollout instead.
  When absent, `content` is itself durable.
- `transient_parts` are live-only typed parts. The implemented part is an image carrying
  `(media_type, Arc<str> base64)`; `Arc` lets a host share the allocation with a preview cache.

The runtime retains transient outputs in a turn-local, call-id-keyed FIFO with a configurable byte
budget (`AgentConfig::transient_tool_output_bytes`, 128 MiB by default). It overlays only entries
whose durable `ToolResult` is present, placing their image parts in a user message after the whole
tool-result message; this preserves `assistant(tool_calls) -> tool* -> user(image)` when calls ran
concurrently. Oldest entries that cross the budget are evicted as a unit and naturally fall back to
their recorded result, so no orphan image remains. An individually oversized entry is evicted too.

Projection happens **before** step-prepare hooks. Hooks therefore see the exact live request and
retain their established final authority to redact or remove it. `TailCacheHook` skips image-only
rows and marks the last cacheable text/tool-result messages instead. The rollout, `Session::messages`,
a later turn, resume, and fork never contain transient parts or base64; they see only the durable
fallback. This boundary is structural, not a convention delegated to hosts.

One semantic invariant spans both views: if history-derived control logic reads a tool result
(forced-chain release, conditional tool reveal, or any similar predicate), `content` and its
durable fallback MUST preserve the same control facts. Otherwise the producing turn and a
resume/fork make different policy decisions from the same call. Ordinary control-plane results
such as `tool_search` should normally omit the override entirely; durable divergence is for
stripping transient representation, not changing the result's meaning.

### 2.4 Optional exclusive dispatch

Tool calls are concurrent by default. A host whose tool publishes shared state MAY install a
`ToolDispatchGate` in `ToolCtx` and declare the names that require exclusive dispatch. Ordinary
tools take a shared lease; a declared tool takes an exclusive lease for its complete future. This
preserves ordinary parallelism while ensuring that no sibling tool enters during the transition
(R6).

The gate is intentionally name- and host-defined. AC does not know what state changes, which tools
perform them, or how the host commits it. The host remains responsible for validating all fallible
work before publication and for making the final authoritative update cancellation-safe. The gate
only establishes dispatch ordering; direct reads outside the registry are outside its contract.

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
  grants; writes go to `P` alone. A grant is a subtree policy, canonicalized when granted
  — the target MUST exist then, else a symlink planted later could redirect it — and resolved
  symlink-safely on use; or a **single file**, denoting exactly one directory entry (parent
  canonicalized at grant, and the entry itself resolved on use — a symlinked entry that leaves
  the granted directory, or lands on an ungranted sibling, is refused). Only absolute names
  reach the grants.
- **prefix-remap(P, {name ↦ Mᵢ})** — mounts other policies under virtual leading segments. A
  relative name whose first segment (after folding `.` — spelling must not change what a name
  denotes) equals a mount name strips it and is judged wholly by that mount's policy; every other
  name — absolute names included — delegates to `P` untouched, so a mount never shadows a real
  path. Lets a host expose side trees under stable virtual names without them living inside the
  primary root.
- **deny(P, D_read, D_write)** — a restricting post-check: after `P` fully resolves, the realized
  path is refused if it lies under a denied entry (per access kind). Judging the *realized* path
  means a symlink resolving into a denied subtree is caught though its lexical name looks clean.
  A deny entry that cannot itself be resolved fails **closed** — an unevaluable deny refuses the
  access rather than silently leaving the deny set.

Three laws follow (checkable as §5's invariants), in their post-amendment form: every combinator's
write resolver factors through exactly one inner write resolver — remap picks exactly one mount or
the inner policy — or refuses outright; realization happens only at leaves, and a non-leaf may add
only *restricting* judgment on an already-realized path (deny), never admit what a leaf refused; and
relative names are anchored deterministically — remap partitions the relative namespace by leading
segment, and within each partition exactly one directory anchors.

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
time and size it observed, plus the time of that read; the file-writing tools consult it before
overwriting. Observation via search or
listing deliberately confers no overwrite right — only a content read does. The check yields one of
four verdicts — *new* (target absent; free to create), *fresh* (read this run, unchanged since),
*never-read*, *stale* (read, but changed on disk since) — and a write proceeds only on *new* or
*fresh*; the other two return as error data telling the model to read first (or again), and a
successful write re-stamps, so a writer retains freshness. The ledger and its app-actionable
never-read/stale errors are one `ToolCtx` field; a host MUST NOT install a second freshness tracker
in extensions. The context also carries per-path
**locks**: a file-writing tool holds its resolved path's lock across check→modify→write, so
concurrent edits of one file serialize instead of losing an update; distinct paths never contend.
Those locks coordinate calls inside one run; they are not a defense against another process changing
the directory tree.

`FileMutation` is the reusable transaction behind the compiled write and edit tools. A host that
needs different schema/copy or result envelopes begins one mutation on a model-supplied or already
policy-resolved path, optionally reads/transforms under its lock, and commits full replacement
bytes. The mutation authorizes the path itself. AC owns the
metadata checks, optimistic-mtime guard, shared ledger assertion, `WriteObserver` call, parent
creation, write, post-write stat, and re-stamp. `WriteObserver` is the host seam for an undo/history
ring and is called with prior bytes only for a content-changing overwrite. `read_text_slice` and
`list_directory` similarly own resolved-path read/enumeration mechanics while leaving routing and
presentation to the host. The pure `fuzzy_replace` cascade is shared by the compiled edit tool and
host adapters; it tolerates common textual drift but refuses ambiguity and disproportionate spans.
The stock `write_file.expected_mtime_ms` accepts the exact fractional-millisecond JSON number a
reader returns and compares it at full precision; hosts do not need a rounding adapter.

The compiled tools are bounded without requiring a host adapter. `write_file` accepts at most
10 MiB of UTF-8 or decoded binary data by default. Text is measured before conversion; base64 is
rejected from its encoded length when it cannot possibly fit, then checked again after decoding.
`list_files` retains and renders at most 500 sorted entries by default, while still counting the
complete filtered directory so it can visibly report truncation. Exact
names for common metadata, dependency, and generated-output directories are filtered before the
ceiling; ordinary dotfiles remain visible. A host may replace either policy for one run by inserting
`WriteFileConfig` or `ListFilesConfig` into `ToolCtx::extensions`. The lower-level
`FileMutation` and `list_directory` APIs remain policy-neutral: callers of those primitives supply
their own payload checks, filters, ceilings, sorting, and presentation.

`list_files` also honors an optional `ReadPathRecoveryConfig` extension after exact read
authorization fails. Its callback may leave the policy rejection untouched, return model-facing
diagnostic text, or propose another path identity. A proposed identity is always passed through the
same `PathPolicy` before directory I/O; the callback can recover a platform or Unicode spelling but
cannot create read authority. Exact authorized paths never invoke the callback. This keeps
containment in the policy while giving a host with explicit user-referenced identities one narrow
diagnostic/recovery seam instead of forcing it to fork the listing tool.

A policy's `authorize_read` / `authorize_write` verdict retains both the resolved path and the
specific policy root that contains it. On Unix, every stock file operation opens that root as a
directory descriptor, traverses each later component with descriptor-relative `O_NOFOLLOW`, and
creates missing parents with `mkdirat` followed by the same no-follow open. Reads inspect and consume
one opened descriptor; mutation commits read prior bytes and truncate/write that same leaf
descriptor. Therefore a concurrent absent-parent or existing-parent symlink swap can make the
operation fail, but cannot redirect it outside the authorized root. This is intentionally stronger
than re-canonicalizing immediately before a path-based open, which merely moves the race window.

The non-Unix implementation is an explicit compatibility fallback over standard path APIs. It
preserves policy authorization and tool behavior, but is not yet hardened against directory-symlink
TOCTOU; its cfg-gated test fixes that claim in place until a Windows handle-relative traversal
replaces it.

### 4.3 Network fetch policy

The compiled `fetch` tool accepts a host-supplied URL policy. The tool parses the initial URL,
admits only HTTP(S), and asks that policy before opening the request. Redirects are followed
manually so every resolved redirect target passes the same scheme and host-policy checks before its
socket is opened. A refusal is model-facing tool-error data. AC supplies an exact-origin policy
(scheme + host + effective port); hosts may inject another policy through the same trait, register
unrestricted `fetch`, or omit the tool entirely. This policy governs in-process `fetch` only and
does not widen the sandboxed `shell` tool's independent network mode. The run's cancellation signal
and one wall-clock deadline bound the complete redirect/request/body sequence; the default is 30
seconds and a host may override it with `Fetch::with_timeout`. The deadline is total, not renewed at
each redirect, and both cancellation and expiry return model-facing error data.

### 4.4 Configurable stock shell

`Shell` is the one compiled tool declaration and `execute_shell` is its one command-execution
mechanism. A host installs `ShellConfig` to select the command interpreter, timeout/cleanup policy,
transcript capture, an optional cwd-root restriction, and sandbox-compatible cwd fallback;
`ShellEnvironmentProvider` supplies run-sensitive environment values. The model-selected cwd is
always re-authorized through the active `PathPolicy`. A host may replace the model-facing description with
`Shell::with_description`, but does not rebuild the schema, dispatcher, result envelope, or
execution path.

The stock path owns sandbox preparation, approval classification, stdout/stderr bounding,
transcript capture, cancel/timeout handling, process-group termination, result shaping, and
reaping. The process group is swept even after a successful leader exit, so background children
cannot survive a tool call. `execute_shell` remains public for lower-level process consumers, not
as an invitation to declare a second shell tool.

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
- **I8 (redirects cannot widen fetch authority).** Every URL the compiled `fetch` tool requests,
  including each redirect target, was admitted by the same host policy before that request opened.
- **I9 (fetch cannot hold a cancelled turn).** The complete redirect/request/body operation races
  the run's cancellation signal and one total host-configurable deadline.
- **I10 (one mutation and process plane).** Host-specific file presentation may wrap AC's
  execution primitives; shell customization configures AC's stock declaration. Neither replaces
  ledgers, locks, observer ordering, sandbox, approval, cancellation, or cleanup mechanics.
- **I11 (exclusive dispatch is complete).** When a host installs a dispatch gate, an exclusive
  tool holds its lease for the complete tool future; ordinary sibling tools cannot enter until it
  returns.

## 6. Division of responsibility

| Concern | Owner |
| --- | --- |
| Schema derivation (typed), erasure, deterministic order, dispatch | registry |
| Raw-input validation | the raw tool itself (MCP: the server) |
| Policy construction, composition, grant issuance, swap timing | host |
| Resolution and the containment verdict | the policy (kit combinators or the host's own) |
| Calling the resolver before touching any path | every tool |
| Capability truth for wire tools | host (per-server trust opt-in) |
| URL authority exposed through compiled `fetch` | host policy; AC enforces it on every hop |
| Fetch cancellation and total timeout | AC; host may configure the deadline |
| File read/write transaction, freshness, locking, fuzzy replacement | AC; host supplies path policy, copy/result mapping, and optional observer |
| Command execution, approval, capture, cancellation, process cleanup | AC `execute_shell`; host supplies command/cwd/env/transcript policy |
| Optional shared/exclusive tool dispatch ordering | AC gate; host declares exclusive tool names and owns the transition |
| Kernel containment of spawned processes | the launcher ([ac-sandbox.md](ac-sandbox.md)) |

## 7. Deferred

- **A finer capability lattice** (e.g. network-reading vs local-reading) — binary is the contract
  until a permission model demands more; evidence first.
- **Per-tool policy views** — all tools in a run share one policy today; evidence first.
- **Deriving the OS sandbox policy from the path policy** — today the host builds both; a mechanical
  translation would remove a divergence risk but freezes both shapes prematurely.
- **Write grants** — a non-goal, not a gap: reads widen by grant, writes only by a new policy (I1).
