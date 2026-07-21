# Design: fork / rewind — branching a session from an earlier point

Status: **studied, design of record, not yet implemented** (2026-07-21). Grounded in a full read
of openai/codex `codex-rs` at commit `1836ae0612` (Apache-2.0): `rollout/src/recorder.rs`,
`core/src/thread_manager.rs`, `core/src/thread_rollout_truncation.rs`,
`core/src/session/mod.rs` (fork persistence), `core/src/session/handlers.rs` (in-place
rollback), `app-server/src/request_processors/thread_processor.rs` (`thread/fork`), and the TUI
backtrack surface. File:line references are into that checkout. Depends on AC adopting an
event-log session substrate (the codex "rollout"); this document specifies both together.

## The problem

Hosts want transcript editing: go back to an earlier user message, change it, and continue from
there — without destroying the original session. They also want cheap "try something on the
side" branches. A row-of-messages store can *append*; it cannot express "this thread is that
thread up to point X, then diverges" without a substrate designed for it. Codex's answer is an
append-only event log per thread plus fork-by-copying-a-truncated-prefix, and it is the design
to mirror.

## What codex does

### The rollout: one append-only JSONL event log per thread

Every thread is a file of timestamped lines (`RolloutLine`), each a tagged `RolloutItem`:
session meta, response items, compaction markers (with their replacement history), per-turn
context, world-state baselines, and protocol events (protocol.rs:3209-3224). The first line is
the session-meta head carrying identity and lineage: `id`, `forked_from_id`,
`parent_thread_id`, and (in the newer paginated mode) `history_base` — an exclusive
byte-offset/ordinal cutoff into a *parent* file (protocol.rs:3082-3139).

Resume is replay: read the lines, tolerate individually-broken ones, rebuild in-memory history
(applying compaction replacements and context baselines), seed token usage
(recorder.rs:960-1023, session/mod.rs:1284+). Two rules make the substrate fork-safe:

- **The file is never rewritten.** Even in-place rollback is an *appended marker*:
  `ThreadRolledBack { num_turns }` goes on the end of the same file, and every scanner applies
  it logically when computing positions (handlers.rs:446-548, thread_rollout_truncation.rs:36-38).
- **First session-meta wins on load.** A fork's file *contains the source's meta line* mid-file
  (copied with the prefix); the loader takes the first meta as canonical and treats later ones
  as ordinary items (recorder.rs:986-1010). Lineage survives; identity is unambiguous.

### Fork = copy a truncated prefix into a new log

`thread/fork` (thread_processor.rs:3961-4290) reads the source thread's raw items — the source
is **only ever read, never mutated** — truncates the in-memory copy at a boundary, and spawns a
new thread whose first persisted write is the new meta head (fresh UUIDv7 id,
`forked_from_id = source`) plus the entire copied prefix **in one atomic append** so a cold
resume can never observe a half-copied fork (session/mod.rs:1324-1359).

Truncation boundaries are *persisted canonical turn starts*, not free positions
(thread_rollout_truncation.rs):

- `before_turn_id` (exclusive) and `last_turn_id` (inclusive-through) cut at recorded
  turn-started boundaries; ids that were synthesized while projecting old logs are rejected
  because they don't name a stable raw boundary; an in-progress turn cannot be `last_turn_id`.
- `TruncateBeforeNthUserMessage(n)` cuts before the nth user message — with positions computed
  against the *post-rollback* effective history, since rolled-back lines physically remain.
- A fork of a thread that ends **mid-turn** is allowed but honest: the copied prefix gets a
  synthesized interrupt boundary — the same "the user interrupted the previous turn on purpose"
  history marker plus aborted-turn event a live interrupt would record
  (thread_manager.rs:1951-1990) — so the fork's model sees a deliberate cut, not a silent one.

Ephemeral forks skip the file entirely (pathless session, used for side-conversations), and
subagent spawns are forks too — full history or last-N-turns — with both `parent_thread_id`
and `forked_from_id` set (spawn.rs:700-735).

### Compaction rides along

No special fork/compaction guard exists and none is needed: compaction markers are ordinary
items in the prefix, replayed on load via their embedded replacement history; a compaction gets
its own addressable turn boundary; forking *before* a compaction naturally yields the
pre-compaction context. This only works because compaction is itself an event in the log — see
[ac-compaction.md](ac-compaction.md).

### The client surface

The TUI's Esc-Esc backtrack is fork, never in-place truncation: pick an earlier user message →
`thread/fork` with `before_turn_id` → new thread attached with the old prompt restored to the
composer for editing; the original thread is untouched. Guardrails worth keeping verbatim: a
*steered* (non-first) user message inside a turn cannot be branched independently — forking
happens at turn boundaries only; an in-progress turn's prompt cannot be branched; selecting the
very first turn starts a brand-new thread instead of forking. The in-place alternative
(`thread/rollback`, the appended marker) refuses while a turn is running.

### The v2 substrate codex is moving toward

Paginated threads chain physical files by cutoff instead of copying: `history_base` points into
the parent file at an ordinal/byte offset, and a lineage resolver walks the chain with cycle
detection and bounds validation (thread-store rollout_lineage.rs:31-132). Copy-free forks are
the destination; the copy path is the shipped v1. Codex's own `thread/fork` currently *rejects*
paginated sources — evidence the copy design is the right first phase.

## What AC adopts

Two pieces, in dependency order — both app-agnostic:

1. **`ac-rollout`: the event-log substrate.** One append-only JSONL log per session of
   timestamped, type-tagged items: session meta head (id, `forked_from_id`, timestamps),
   messages/response items, compaction markers with replacement history, lifecycle events.
   Load = replay with per-line fault tolerance and first-meta-wins. In-place rewind = an
   appended rollback marker that all position math honors. This subsumes what `ac-store`'s
   message table does for history (the SQLite side remains the *index* — sessions list, titles,
   metadata — while the log becomes the source of truth for content; codex uses exactly this
   split).
2. **Fork as a kit operation** on that substrate: read source items → truncate at a canonical
   turn boundary (`before`/`through`/`nth-user-message`) → new log with fresh id + lineage +
   atomically-appended copied prefix; synthesize the interrupt boundary when the cut lands
   mid-turn; never touch the source. Ephemeral (in-memory) forks fall out for free by skipping
   persistence.

Rules inherited unchanged: forks only at turn boundaries (a steered message mid-turn is not a
boundary — see [ac-queue-steer.md](ac-queue-steer.md)); no fork of an in-progress boundary;
append-only always; lineage as data (`forked_from_id`) so hosts can render ancestry.

## Deferred

- Copy-free forking (cutoff chains into parent logs à la `history_base`) — v2, after the copy
  path proves the API. The copy path's contract is deliberately compatible: a cutoff is just a
  prefix that didn't need copying.
- Zstd cold-compression of old logs and paginated loading — storage optimizations, not
  semantics.
- Cross-session dedup/GC of forked prefixes — explicitly a non-goal; disk is cheap, correctness
  of an append-only log is not.
