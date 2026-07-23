# RFC: Durability — the reopen contract

**Status:** specification + proof harness (2026-07-23). The contract below is normative; the
proof harness of §6 ships with it (crash-at-boundary sweeps, torn-tail sweeps, kill-and-reopen
process tests, the restart-continuity scenario). Store hardening (§4) and the acknowledge-point
correction to the AI-SDK reference host (§3) land in the same change.
**Requires:** [ac-serving.md](ac-serving.md) (the persistence split this contract governs);
[ac-fork.md](ac-fork.md) (the append-only log and `E(L)`); [ac-loop.md](ac-loop.md) (turns and
errors-as-data). **Required by:** every host that promises a user their sessions survive.
**Interacts with:** [ac-compaction.md](ac-compaction.md) §3 (compaction is a turn; its crash
behavior follows §5.3); [ac-testing.md](ac-testing.md) (the proof classes §6 instantiates).

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in RFC 2119.

## 1. Motivation

The contract a user actually holds a local agent to is mundane: *run it, close the machine,
open it tomorrow, and everything is still there.* Not "the process never dies" — processes die
constantly (quit, crash, kill, power loss, sleep-wake, upgrade) — but that **dying is never
lossy beyond the instant of death, and never corrupting**. Five requirements:

- **R1 (durable once acknowledged).** Work the system has acknowledged — a completed turn, an
  accepted user message — MUST survive any subsequent process death.
- **R2 (bounded loss).** A crash at the worst possible instant loses at most the work in
  flight at that instant — never prior history, never sibling sessions, never the store.
- **R3 (always reopenable).** Whatever bytes a crash leaves behind, the next open MUST
  succeed: loadable store, healed logs, typed surfacing of anything skipped. There is no state
  from which the system refuses to start.
- **R4 (idempotent recovery).** Crash → reopen → crash → reopen, arbitrarily many times,
  accumulates no damage: recovery is a fixed point.
- **R5 (honest failure).** Every recovery action is observable — skipped lines counted,
  interrupted turns marked, version mismatches typed. Silent healing that hides data loss is
  as forbidden as losing the data loudly.

## 2. Model — acknowledge points and crash windows

Let a turn's lifecycle be: input accepted → steps run → turn settles → history appended. A
**durability point** is the instant a piece of state becomes crash-proof. The contract names
three tiers:

- **A0 — accepted.** In process memory only. Crash loses it. Everything is A0 until persisted.
- **A1 — turn-durable** *(the REQUIRED floor)*: the user's input reaches the store **before**
  the turn samples, and the turn's output reaches the store at turn settle, atomically. A crash
  mid-turn therefore loses *at most the in-flight turn's output* — never the input that
  provoked it (a user should never have to retype what they already said), never prior turns.
- **A2 — step-durable** *(SHOULD, where the wire allows)*: completed steps append as they
  settle, so a mid-turn crash preserves the finished prefix of the turn. The ACP reference
  host already operates at A2 (per-message incremental append); a batching host operates at A1.

A **crash window** is any instant between two durability points. §5 enumerates the windows and
the required post-crash observation for each. The store's unit of atomicity is the
transaction: an append either fully lands (all messages, one sequence range, one commit) or
leaves no trace — there is no partially-appended turn (I2).

## 3. Host obligations (the reopen contract)

A host that persists sessions MUST:

1. **Persist input first.** Append the user's message (A1) before the turn's first sample. The
   reference AI-SDK host is corrected to do this; batching output is permitted, batching the
   *input* into the post-turn append is not.
2. **Reopen from the store, not from memory.** On restart, session state derives from
   `load → resume`; no in-memory artifact (run registry, ring buffer, handle) is assumed to
   have survived. Resume equivalence is the kit's obligation ([ac-hooks.md](ac-hooks.md) I3).
3. **Reconcile stuck liveness marks.** Any host-maintained "running" mark (a run-status column,
   a lock file) MUST be reconciled at open: a mark with no living process behind it flips to a
   terminal failed state, once, idempotently (R4).
4. **Treat persist failure as loud.** An append that fails (disk full, conflict) is surfaced —
   to the log at minimum, to the user where a surface exists. It MUST NOT abort an otherwise
   completed turn retroactively, and MUST NOT be silently swallowed.
5. **Never hold the only copy in a doomed process.** Client disconnect MAY cancel the turn
   (a stateless host) or MAY let it finish detached (a daemon host) — but in both designs
   whatever completed still reaches the store (R1).

## 4. Store obligations (the kit's half)

- **Journal discipline.** The SQLite store runs WAL with `synchronous=NORMAL` — durable
  against process death at every commit; an OS-level power loss MAY lose the final commits but
  MUST NOT corrupt (SQLite's WAL guarantee). Hosts with stricter power-loss needs MAY request
  `FULL`. The choice is explicit in code, not a default relied upon silently.
- **Self-check on open.** Opening runs a cheap integrity probe (`quick_check`) and fails with
  a typed error on corruption — a corrupt store is reported at the door, not discovered
  mid-session (R5).
- **Schema versioning.** The store stamps `user_version` at creation and refuses, with a typed
  error, to open a store stamped by a *newer* schema — old code MUST fail cleanly on new data,
  never mis-read it (version skew is a crash window too: upgrades happen between runs).
- **Append atomicity.** `append_messages` is transactional with an optional sequence CAS;
  concurrent writers are detected (`SeqConflict`), and a detected conflict loses the loser's
  append only — never the winner's, never the table (I2, I5).
- **Content is opaque but versioned by construction.** Message content is serialized enum
  data; loading content written by a newer kit fails as a typed per-session error. It MUST NOT
  be silently skipped — a session with unreadable messages is unusable *as itself*, and saying
  so beats corrupting its context.
- **Log healing.** The append-only session log tolerates a torn tail at any byte offset: the
  damaged line is skipped and counted, an open turn at the cut is closed with the interruption
  marker, and the loaded view is a consistent prefix (R3, R5;
  [ac-fork.md](ac-fork.md) I6). Every full-file write is temp-then-rename atomic.

## 5. Failure-mode inventory

Each row names the window, the worst legal outcome, and the reopen observation the harness asserts.

| # | Window | Worst legal outcome | On reopen |
| --- | --- | --- | --- |
| 5.1 | Kill mid-turn (between input append and settle) | in-flight turn output lost | input present; no partial assistant message; next turn works |
| 5.2 | Kill mid-append (inside the store transaction) | that append absent | store passes quick_check; sequence contiguous; prior turns intact |
| 5.3 | Kill mid-compaction | compaction lost; pre-compaction history stands | session loads pre-compaction; compaction retryable |
| 5.4 | Kill mid-fork (during the fork file write) | no fork born | source untouched; no torn fork file visible (temp-rename) |
| 5.5 | Torn log tail (partial final line) | that line lost | load heals: line skipped + counted, open turn closed with marker |
| 5.6 | Disk full on append | that append fails, typed | session intact; append succeeds once space returns |
| 5.7 | Concurrent writer on one session | loser's append rejected | `SeqConflict` surfaced; winner's history unbroken |
| 5.8 | Version skew (newer schema or newer content) | refuse that store / that session, typed | no mis-read, no partial load, other sessions unaffected |
| 5.9 | Provider stream death mid-turn (network, sleep-wake) | turn ends as a typed error | errors-as-data; session resumable immediately |
| 5.10 | Crash loop (repeated kill during recovery) | nothing beyond the first crash's loss | reopen is a fixed point: state identical after N recoveries |

## 6. The proof harness

Durability claims are worthless untested — these are implemented, not aspirational:

- **Torn-tail byte sweep** (log): serialize a representative multi-turn log (turns, a
  compaction, an interruption); for *every* prefix length in bytes, load the prefix — MUST
  yield a healed, prefix-consistent view without panic, with `skipped_lines` accounting for the
  cut (5.5, R3, R4).
- **Kill-and-reopen append storm** (store): a child process appends batches in a loop; the
  parent SIGKILLs it at randomized instants, reopens the store, asserts `quick_check` passes,
  every session's sequence is contiguous from 0, and every stored message parses. Repeated —
  each iteration reuses the survivor store, proving recovery is a fixed point (5.2, 5.10).
- **Restart continuity** (the reopen scenario, end-to-end): drive a real serving host binary
  against a scripted provider; complete a turn; **SIGKILL the host mid-second-turn** (the
  provider stub stalls mid-stream to pin the window); restart the binary on the same store;
  assert the first turn and the second turn's *input* are present, no partial output leaked,
  and a fresh turn completes. This is "close the desktop, open it the next day," simulated
  honestly (5.1, R1, R2).
- **Version-skew probes** (store): a store stamped with a future `user_version` refuses to
  open, typed; a message row carrying an unknown content variant fails that session's load,
  typed, without touching sibling sessions (5.8).
- **Conflict and disk probes**: concurrent CAS appends (5.7) and append-failure surfacing (5.6)
  at the unit level.

The harness is the regression fence: any change to persistence, the log format, or a host's
acknowledge points MUST keep it green.

## 7. Invariants

- **I1 (acknowledged ⇒ durable).** Any state a durability point has passed survives every
  subsequent crash (R1).
- **I2 (atomic append).** No crash instant exists at which an append is partially visible.
- **I3 (universal reopen).** For every byte-level state a permitted crash can leave, open
  succeeds and yields a consistent view (R3).
- **I4 (fixed-point recovery).** recover(recover(s)) = recover(s) for every crash state s (R4).
- **I5 (isolation).** A crash or corruption affecting one session never damages another, and
  a store-level failure is reported at open, not discovered as silent data loss (R2, R5).
- **I6 (loud healing).** Every deviation from a clean load — skipped line, closed-open turn,
  refused version, failed append — is observable as a count, marker, or typed error (R5).

## 8. Division of responsibility

| Concern | Owner |
| --- | --- |
| Store atomicity, journal discipline, self-check, versioning | kit |
| Log healing (torn tails, interruption closure), atomic file writes | kit |
| Resume equivalence from the record | kit |
| Acknowledge points (input-first, output at settle/step) | host, per §3 |
| Liveness-mark reconciliation at open | host |
| Surfacing persist failures and healing reports to the user | host |
| The proof harness | kit (store/log sweeps) + reference hosts (restart continuity) |

## 9. Deferred

- **Step-durable everywhere (A2 as floor)** — a live per-event log appender in the runtime,
  making every host step-durable by construction. The log substrate exists; the appender lands
  with evidence that A1 loses more than users tolerate.
- **Power-loss tier (`synchronous=FULL`, fsync-on-log-append)** — the OS-crash tier beyond
  process death; opt-in when a host wants it.
- **Store backup/export hooks** — periodic snapshot or export-on-close; recovery-from-backup
  is a different contract than crash recovery.
- **In-place log compaction** (rewriting a session log to drop healed damage) — reopen already
  heals logically; physical rewrite is an optimization.

---
*Provenance: the acknowledge-point taxonomy and reconcile-at-open discipline distill common
practice in production local-first agent runtimes; the torn-tail and kill-storm proof shapes
follow SQLite's own crash-testing doctrine (test what the bytes allow, not what the happy path
produces). Written against this kit's store, log, and reference hosts as they exist.*
