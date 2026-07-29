# RFC: Managed submissions and sequential runs

**Status:** specification of record (2026-07-29).
**Requires:** [ac-durability.md](ac-durability.md) (acknowledge points and
recovery), [ac-queue-steer.md](ac-queue-steer.md) (the active-turn boundary),
and [ac-serving.md](ac-serving.md) (thin clients over backend authority).
**Implemented by:** `ac-managed` over `ac-store`.

The key words MUST, MUST NOT, SHOULD, and MAY are to be interpreted as in
RFC 2119.

## 1. Motivation

A client should submit intent once. It should not have to inspect a locally
cached run status, acquire a lock, append input, choose between start and
enqueue, drain a stream, release the lock, and remember to start the next
item. That sequence is one backend operation, and splitting it across clients
creates three avoidable failures:

- two clients can both decide a session is idle;
- input can become durable before a later start fails as busy;
- an acknowledged queued item can disappear with the process that held it;
- a direct lease can start after durable settlement but before the prior
  terminal event is published.

Managed mode therefore owns the core submission operation:

> **submit durable input, start it if the session is available, otherwise keep
> it in a durable pending order and start it at the next safe boundary.**

The payload is opaque host data. The mechanism does not know prompts, models,
credentials, transports, event envelopes, or application vocabulary.

Some hosts also need a non-queued, already-prepared run. `ac-managed` owns that
direct lease too: direct and submission-backed runs share one store lease, one
per-session gate, and one in-memory publication fence. Direct describes how
the run was admitted, not an escape hatch around the scheduler.

## 2. Boundary with steering

There are two queues with different owners and semantics:

- The **steer queue** belongs to an already-active runtime turn. It is
  in-memory, drains only at step boundaries, and is specified by
  [ac-queue-steer.md](ac-queue-steer.md).
- The **submission queue** belongs to the managed backend. It is durable,
  cancellable and reorderable while pending, and ordinarily starts a distinct
  sequential run after the active run settles.

`steer_pending` is the bridge, and AC owns it. It first binds an existing
pending submission durably to the exact active run as `steering`, asks the
host runner to deliver it to that run's runtime steer queue, and records the
runner's acceptance as `delivered`. Run settlement then acknowledges the exact
ordered delivered ids that the host durably committed. A client or host MUST
NOT approximate this with cancel/delete followed by runtime `steer`; that
composition has a loss window. The runtime still does not inspect or persist
next-turn submissions.

## 3. Record

For each session, the managed store holds submission records:

```text
submission = {
  session_id,
  sequence,       // immutable acceptance order
  queue_position, // mutable pending order
  submission_id,
  payload,
  state,
  run_id?,
  accepted_at,
  started_at?,
  finished_at?,
  error?
}
```

`sequence` is assigned once at acceptance and never changes. `queue_position`
selects among pending records and MAY change through a guarded reorder. It is
retained while `steering` or `delivered`, so an unacknowledged delivery or
interrupted active run can restore the record to its prior place.

`payload` is serialized and stored verbatim. The kit MUST NOT inspect it.
Credentials, bearer tokens, and other live secrets MUST NOT be placed in the
payload; a driver resolves them at execution time from a host-owned credential
source.

The payload record can outlive the process and the host version that accepted
it. A host therefore MUST choose a stable, backward-decodable payload schema
and SHOULD carry an explicit schema version when the representation can
evolve. For the stock `SqliteManagedStore<P>`, idempotency compares serialized
payload bytes. `claim_next` decodes the exact oldest pending payload inside the
same transaction that proves the session idle and, only after validation,
creates the lease and changes the record to `claimed`. An incompatible or
corrupt head row therefore fails while it is still `pending`, with no active
lease created. It can block that session's queue until the host migrates,
repairs, cancels, or reorders it; a corrupt later row does not block valid
records ahead of it.

Recovery is invoked under exclusive startup authority. The stock adapter
decodes every payload referenced by an active lease before allowing durable
reconciliation to mutate any of them. A schema failure consequently leaves
the recovery action intact and replayable after repair instead of consuming a
transition whose report could not be decoded.

`state` is one of:

```text
pending | claimed | running | steering | delivered | steered |
succeeded | failed | cancelled | interrupted
```

Allowed transitions are:

```text
pending --claim--> claimed --input commit--> running
   |                 |                          |
   | cancel          | recovery                 | settle/recovery
   v                 v                          v
cancelled          pending       succeeded | failed | cancelled | interrupted

pending --reserve for active run--> steering
   steering --runtime accepts--> delivered
   steering --runtime rejects--> pending (same queue_position)
   delivered --settlement ACKs this id--> steered
   steering | delivered --settlement omits this id / recovery-->
     pending (same queue_position)
```

Terminal records remain addressable. Repeating an acknowledged
`submission_id` with the same payload returns its existing record; repeating it
with different payload is a typed conflict. This makes transport retry
idempotent without making execution retry implicit.

The per-session run lease carries an optional `submission_id`. A populated id
means a claimed submission; `null` means a direct run. Both forms are mutually
exclusive in the same durable row and both are represented by the scheduler's
in-memory active fence while owned by this process.

## 4. Operations

### `submit(session, submission)`

1. Persist the record as `pending` in one transaction. This commit is the
   acknowledge point.
2. Publish the pending snapshot on a best-effort observer path.
3. Attempt to claim the oldest pending record for the session.
4. If another run is active, return `queued`.
5. If a record is claimed, register its active fence and launch the driver.
6. Reload the exact submitted record and derive the receipt from its current
   durable state.

Submitting MUST always schedule a claim attempt. “Enqueue while accidentally
idle and wait forever” is prohibited.

The service starts this entire accept-to-launch handshake in a backend-owned
task before its first await. Once the call is polled, cancellation or
disconnect of that caller MUST NOT interrupt a durable accept or leave a
committed claim between lease creation and active-run registration. Process
death is handled by ordinary recovery.

The result is:

```text
accept_receipt = {
  submission_id,
  inserted,
  sequence,
  disposition,
  scheduling_fault // null | ManagedFault
}
```

`sequence` is the durable per-session sequence assigned at first acceptance.
It is present for every disposition, including `Started` and `Existing`, and
is unchanged by an idempotent retry. `inserted = false` means the idempotency
key already existed with the same serialized payload; it does not allocate a
new `queue_position`. A queued disposition reports current position separately
from the immutable receipt sequence.

The accept transaction is an irreversible boundary for the call. Once it
commits, a subsequent claim or launch failure MUST NOT turn `submit` into an
unacknowledged error. The service returns an `AcceptReceipt` with the durable
record's disposition (normally `Queued`) and `scheduling_fault` describing the
post-accept failure. The record remains `pending` and eligible for a later
`wake` or recovery.

The disposition is projected from an exact-record read after the scheduling
attempt, not from the acceptance snapshot or from which concurrent submit
caller acquired the drain gate. If a sibling call claimed this submission in
the meantime, the receipt reports `Started` with that durable `run_id`. Failure
of this projection is itself a `scheduling_fault`; it cannot revoke the
acceptance, sequence, or idempotency key.

`scheduling_fault` is not permission to submit again: the input is already
acknowledged. A client MUST NOT mint a replacement submission or blindly retry
transport from that field. If the transport loses the entire receipt, it MAY
repeat the same `submission_id` and byte-equivalent payload; ordinary
idempotency then returns the existing durable record.

### `claim_next(session, run_id)`

The store MUST atomically prove that the session has no active run and
transition exactly the first `pending` record by `(queue_position, sequence)`
to `claimed`. Concurrent claims have at most one winner. A loser changes
nothing. After the driver's
idempotent input commit succeeds, the store transitions the matching
`claimed` record to `running` before model sampling begins.

A persistence adapter that decodes an opaque payload MUST validate the exact
candidate under the same atomic authority as that claim. A separate
list/decode/claim sequence is prohibited because the decoded row can differ
from the one ultimately claimed.

The persistence seam is failure-atomic at acceptance and claim: `Err` means no
new acceptance, lease, or claimed transition committed. An implementation with
a fallible post-commit transport or callback MUST resolve the proposed
`run_id` and return the committed claim instead of an uncertain error. The
stock SQLite adapter enforces this with one transaction and panic-isolated
post-commit listeners.

### `try_acquire_direct_run(session, run_id)`

A direct acquire enters the same per-session gate and lifecycle gate as
`claim_next`. It returns one of:

```text
Acquired(DirectRunLease) | Held { run_id } | Quiescing
```

On acquisition, AC failure-atomically creates the durable direct lease,
registers an in-memory direct fence, publishes `DirectRunStarted`, and only
then hands the lease to the caller. The handoff is cancellation-safe: if the
caller disappears before receiving it, AC releases the lease as `cancelled`
and wakes pending work.

Direct acquisition is not a third queue: it may acquire an otherwise-idle
session even when durable submissions are pending. A host that wants queued
admission MUST use `submit`; direct runs are for work the host has already
admitted outside the submission record.

A host using `ManagedRuns` MUST NOT call the lower-level `ac-store`
`try_acquire_managed_run` or `release_managed_run` methods for the same
sessions. Those methods implement the adapter transaction; only
`ManagedRuns` owns cross-mode publication order and quiescence.

### `release_direct_run(lease, settlement)`

Direct release is guarded by the lease's `run_id` and uses the same
per-session gate. After durable release, AC publishes `DirectRunSettled` while
retaining the in-memory fence, then clears that fence and wakes pending
submission work. A stale/non-owned lease returns `false`. An uncertain owned
release retains the fence and faults rather than exposing a false idle state.
Caller cancellation cannot interrupt this release sequence.

`settlement` contains the outer `outcome` and an ordered
`committed_steer_ids` list. The same release transaction validates that every
listed id is a unique `delivered` child bound to this exact run, changes those
children to `steered`, and restores every unlisted `steering` or `delivered`
child to `pending`. The outer outcome does not decide steer commitment: a run
that ultimately fails or is cancelled can already have durably persisted
some steers. `DirectRunSettled.steered` contains exactly the acknowledged
records in acknowledgement order.

### `cancel_pending(session, submission_id)`

Only `pending → cancelled` is legal. Cancelling a running submission requires
the active-run cancellation mechanism; cancelling one queued item MUST NOT
cancel its siblings. A successful pending cancellation schedules a claim
attempt after publishing the new queue snapshot, so removing a blocked queue
head cannot leave an eligible successor idle.

### `reorder_pending(session, expected_order, desired_order)`

Reorder is a compare-and-swap over the complete pending queue. Both orders
MUST contain the same unique submission ids. AC applies `desired_order` only
when the durable current order equals `expected_order`; otherwise it returns
`Conflict { current_order }` and changes nothing. If the current order already
equals `desired_order`, the operation returns `Unchanged`, making transport
retry idempotent.

Acceptance `sequence` never changes. Only `queue_position` changes. Concurrent
accept, claim, cancel, steer reservation, or reorder cannot be overwritten by
a stale client. Every result publishes an authoritative complete queue
snapshot; a successful reorder also wakes scheduling.

### `steer_pending(session, submission_id)`

This lossless handoff is a backend-owned, cancellation-safe sequence:

1. Under the session gate, verify the record is pending and identify the exact
   active run.
2. Atomically change `pending → steering` and bind its `run_id`. An error from
   this store transition MUST mean no reservation committed.
3. Ask `ManagedRunner::steer` to deliver the opaque payload into that run's
   runtime steer queue.
4. If the runtime did not take ownership, atomically roll the record back to
   `pending` at its retained `queue_position`.
5. If it did take ownership, atomically change `steering → delivered`. A
   transient failure of this failure-atomic confirmation is retried while AC
   retains the session gate and active fence; AC surfaces the first fault and
   backs off to a five-second retry cap without observer spam. It neither
   reports an ambiguous result nor lets settlement overtake the confirmation.
6. Leave the `delivered` record bound to the run until settlement either
   acknowledges it or restores it.

The public result distinguishes `Steered`, idempotent `AlreadySteered`,
`NoActiveRun`, `NotFound`, `NotPending`, and `Unavailable`. Repeating the call
while the record is `steering`, `delivered`, or `steered` never enqueues it
again. The entire reservation-to-delivery-confirmation-or-rollback handshake
begins in a backend-owned task before its first await, so caller cancellation
cannot strand a reserved but undelivered item.

Active settlement carries an ordered `committed_steer_ids` proof from the
host's own durable input persistence. AC accepts only unique ids whose records
are `delivered` and bound to that exact run. It atomically changes exactly
those records to `steered`, returns them in proof order, and restores every
other bound `steering` or `delivered` record to `pending`. This rule is
independent of `Completed`, `Failed`, or `Cancelled`.

### `cancel_active(session)`

Active cancellation targets only the submission-backed run currently owned
for the session. A direct lease is host-executed and `cancel_active` returns
`false` for it. For submission work, AC creates its cancellation token and
registers the active entry before publishing `RunStarted` and before the driver
can enter `commit_input`. There is therefore no “started but not yet
cancellable” gap.

Cancellation does not undo the durable input phase. If requested while
`commit_input` is in progress, the driver's `run` phase receives an
already-cancelled token and MUST honor it before sampling or starting new
side effects. It then returns `cancelled`, which uses ordinary guarded
settlement and allows the next pending submission to drain.

### `settle(session, run_id, RunSettlement)`

Settlement is guarded by `run_id`. A stale completion cannot release or mutate
a newer run. After a real settlement, the service claims the next pending
submission regardless of success, failure, or deliberate cancellation of the
active run. `RunSettlement` carries `{ outcome, committed_steer_ids }`. The
settlement transaction validates the ordered proof and resolves every bound
`steering` or `delivered` child atomically with the run: acknowledged
`delivered` ids become `steered`; every unacknowledged child returns to
pending. `RunSettled.steered` carries the exact acknowledged records in proof
order for any terminal outcome.

### `wake(session)`

`wake` schedules a claim attempt for one session. Ordinary submission,
successful pending cancellation, managed settlement, and direct release wake
automatically. A host uses this operation after restoring a transient execution
prerequisite.

### `recover()`

When invoked after open, recovery returns an orphaned `claimed` record to
`pending`; its input-commit phase is safe to replay because that phase is
idempotent. Every orphaned `running` record becomes `interrupted`, exactly
once. It is not automatically rerun: tools may already have produced side
effects. Other pending records remain eligible to drain. Recovery is a fixed
point. An orphaned direct lease has no submission to classify and is simply
released. Every `steering` or `delivered` child of an orphaned managed or
direct lease returns to `pending` in the same reconciliation transaction;
recovery has no host persistence proof and therefore cannot commit a steer.
`recover()` returns a
report with `requeued`, `interrupted`,
`released`, and `pending_sessions` counts, and schedules a wake for every
session that still has pending work. It does not wait for those runs to finish.

Recovery MUST run while the host has exclusive store authority, before normal
submission or claim scheduling begins. This gives payload preflight and the
following reconciliation one recovery boundary without racing a new lease.

### `recover_deferred()`

`recover_deferred()` performs the same reconciliation and returns the same
report shape, but does not claim or wake pending work. A host whose credentials,
provider connections, tool registry, or other transient prerequisites are not
ready at store-open time MUST use deferred recovery, restore those
prerequisites, and then call `wake` for the pending sessions known to the host.

### `begin_quiesce()` / `wait_quiesced()` / `quiesce()`

`begin_quiesce()` permanently closes the claim gate for that scheduler
instance and requests cancellation of every active submission-backed run it
owns. Direct runs are not cancelled by AC, but their fences remain in the
active set. After `begin_quiesce()` returns, no new claim or direct acquisition
can begin; pending submissions remain durable for a later instance.
`wait_quiesced()` waits for both submission and direct fences to become empty
through successfully proven requeue, settlement, or direct release. It does
not wait for pending work. `quiesce()` is the convenience composition of both.

A host SHOULD bound `wait_quiesced()` with its own shutdown deadline. If a
driver does not honor cancellation, an observer blocks, or AC cannot prove a
store transition committed, waiting can intentionally remain incomplete
rather than report a false clean shutdown.

## 5. Driver and observer seams

The host injects a driver with three responsibilities:

1. **prepare input** — make the accepted input visible to the durable session
   record before model sampling. This operation MUST be idempotent by
   `submission_id`, because a process can die after the input commit but before
   the caller observes it.
2. **run to terminal** — execute one run and return `RunSettlement {
   outcome, committed_steer_ids }`. The phase receives the active cancellation
   token and MUST check or propagate it before sampling and through
   cancellable work. It MUST list only delivered steer ids already committed
   by the host's own durable input persistence, in that persistence order.
3. **deliver a steer reservation** — map the opaque pending payload into the
   active runtime's steer input. `Accepted` means the runtime took ownership.
   `Unavailable` or `Err` MUST mean it did not; returning either after partial
   acceptance allows AC's rollback to duplicate input. After `Accepted`, AC
   retains the session fence and retries its failure-atomic durable delivery
   confirmation until it converges.

The host also injects an observer. It maps managed lifecycle changes to its
own protocol events, logs, metrics, or UI projections. Observer output is not
the source of truth; snapshots are rebuilt from the managed store. AC catches
an observer panic so it cannot unwind through a committed store transition or
strand the scheduler. This isolates panics, not hangs: `observe` MUST return
promptly and MUST hand slow or fallible delivery to another task. In
particular, the per-session queue-publication gate is held across snapshot
load plus observation, and the launch barrier does not admit `commit_input`
until `RunStarted` observation returns.

The lower-level store mutation listener is advisory as well. AC isolates its
panic after commit, so a projection hook cannot convert a committed acceptance
or state transition into an apparent store failure.

The service owns the per-session drain gate, pending-order CAS, claim/settle
order, durable steer reservation and resolution, direct acquire/release order,
recovery, and recursion. A driver or observer that reimplements those rules is
a second scheduler and is prohibited.

## 6. Ordering

For one session:

```text
persist pending
→ publish pending snapshot
→ atomic claim
→ create cancellation token and register active run
→ publish pending snapshot without the claimed item
→ publish run started
→ prepare input durably
→ publish input committed
→ execute run
→ validate ordered steer ACK and settle run + bound steer children by run_id
→ publish run settled with exact acknowledged records in ACK order
→ claim next
```

For a direct run:

```text
atomic direct lease
→ register direct fence
→ publish direct run started
→ host executes
→ guarded release + ordered steer-ACK validation and child resolution
→ publish direct run settled with exact acknowledged records in ACK order
→ clear direct fence
→ claim next
```

Different sessions MAY run concurrently. One session MUST have at most one
run of either kind. `QueueChanged.pending` is a complete durable pending
snapshot, not a delta. Snapshot load and observer publication are serialized
behind a separate per-session gate, so observers cannot see those snapshots in
reverse durable order even when submission, cancellation, and settlement
race. Different sessions do not share that gate.

The in-memory active entry also fences lifecycle publication. AC retains it
after a confirmed durable settlement or requeue until the corresponding
`RunSettled` or `InputCommitFailed` observer call returns, so a successor
cannot publish `RunStarted` or `DirectRunStarted` first. Direct release
likewise retains its fence until `DirectRunSettled` returns, so managed work
cannot start first.

Receipt projection follows the claim attempt and reloads the exact accepted
record. The detached accept-to-launch handshake and the per-session drain gate
are separate guarantees: the former survives caller cancellation; the latter
serializes the scheduling decision.

## 7. Failure semantics

- **Accept/persist failure:** no receipt is produced and the new submission is
  not acknowledged.
- **Post-accept scheduling failure:** `submit` still returns the durable
  acknowledgement with `scheduling_fault`. A claim-phase store error is
  failure-atomic, so that record remains pending and a later wake or reopen can
  retry scheduling. A later projection fault leaves whatever authoritative
  durable state already committed. In neither case does the client retry the
  submission.
- **Caller cancellation after submit begins:** the backend-owned handshake
  continues through active registration or a surfaced scheduling fault.
  Cancellation cannot strand a committed claim.
- **Caller cancellation during direct handoff/release:** an unreceived direct
  acquisition is released as cancelled; an accepted direct release continues
  detached through terminal publication and fence clearing.
- **Caller cancellation during `steer_pending`:** the backend-owned
  reservation/delivery-confirmation/rollback handshake continues. A caller
  cannot strand a `steering` record merely by disconnecting.
- **Steer reservation failure:** `ManagedStore::begin_steer` is
  failure-atomic. `Err` means no reservation committed; a custom store that
  observes a post-commit fault MUST read back and return the committed
  reservation.
- **Steer delivery rejection:** `Unavailable` or runner error means the
  runtime took no ownership. AC restores the record to pending at its retained
  queue position. A runner MUST NOT report rejection after partial acceptance.
- **Delivery-confirmation persistence failure:** after the runner reports
  `Accepted`, AC knows runtime ownership transferred. It retains the session
  gate and active fence and retries the failure-atomic `steering → delivered`
  confirmation with bounded backoff. It does not roll back, release, or return
  an ambiguous public result. A permanent store failure intentionally requires
  process recovery.
- **Payload decode/schema failure:** the stock adapter validates the exact FIFO
  head inside the claim transaction, so the incompatible row remains pending
  and no active lease is created. During exclusive startup recovery, all
  active payloads validate before reconciliation mutates durable state. The
  host repairs, migrates, or explicitly cancels the blocking row.
- **Input preparation failure:** sampling MUST NOT begin. The claim is
  released back to `pending`, the drain pass stops to avoid a hot retry loop,
  and the failure is surfaced, provided AC can prove the guarded requeue
  committed. If the requeue result is uncertain, AC retains the active fence
  and MUST NOT publish a fabricated `pending` lifecycle event.
- **Run failure or cancellation:** the active record becomes terminal.
  Delivered steer ids named by the ordered persistence proof become
  `steered`; every unacknowledged `steering` or `delivered` child returns to
  pending. Later pending records still drain.
- **Process death before claim:** the record remains `pending`.
- **Process death while claimed:** the record returns to `pending` on open.
- **Process death while running:** the record becomes `interrupted` on open;
  every `steering` or `delivered` child returns to pending, and later pending
  records drain.
- **Process death with a direct lease:** recovery releases the orphaned lease
  and reports it; its `steering` or `delivered` children return to pending and
  AC does not invent a persistence proof.
- **Invalid settlement proof:** duplicate ids, ids not bound to the exact run,
  and ids not durably `delivered` are rejected. The whole settlement
  transaction rolls back and AC retains its active fence; no partial child or
  outer-outcome transition is published.
- **Hard-process-death steer gap:** normal settlement is exact with respect to
  the host's ordered durable proof, regardless of outer outcome. Across
  process death it remains at-least-once: the host can persist a delivered
  steer and die before returning the proof and before AC commits settlement.
  Recovery has no proof, so it conservatively restores that record to pending
  and it may be observed again. A host MUST NOT erase an attributed consumed
  user record while retaining assistant/tool consequences derived from it.
  Exactly-once across this gap requires the host to atomically compose its
  input persistence with AC settlement, or to deduplicate replay durably by
  submission id.
- **Observer or mutation-listener panic:** cannot roll back a committed state
  transition and is contained by AC; authoritative store snapshots remain
  available to repair presentation. A blocking observer is a host contract
  violation and can stall ordered publication or launch.
- **Uncertain active transition:** if requeue, input-committed marking,
  settlement, or direct release errors or reports that the guarded row did not
  change, AC emits a fault and MUST NOT clear its active entry or claim a
  successor. The process no longer has proof that releasing the session is
  safe. The host MUST terminate that scheduler instance and recover from the
  durable store; it MUST NOT translate the fault into an idle or released
  state.
- **Shutdown deadline:** forced process exit can leave `claimed` or `running`
  work. The next instance uses normal recovery; quiescence never lies merely
  to make shutdown complete.

## 8. Invariants

- **I1 (accepted means durable).** The acceptance represented by an
  `AcceptReceipt` survives process death, including when the receipt carries
  `scheduling_fault`.
- **I2 (one scheduler).** The backend, not a client, decides start versus
  queue.
- **I3 (single flight).** At most one submission-backed or direct run exists
  per session.
- **I4 (durable pending order).** Claims select the lowest
  `(queue_position, sequence)`. Reorder mutates only `queue_position`.
- **I5 (no stale release).** Only the matching `run_id` settles or releases an
  active run.
- **I6 (honest recovery).** An orphaned pre-commit claim is requeued; orphaned
  post-commit work is marked `interrupted` and never silently retried.
- **I7 (secret-free record).** Durable payloads contain no live credentials.
- **I8 (stable receipt order).** Every successful acceptance returns the
  immutable acceptance sequence, and an idempotent retry returns the same
  sequence regardless of later pending reorder.
- **I9 (continuous cancellation).** An owned run is cancellable before
  `RunStarted` is published and before input commit can begin. This guarantee
  applies to submission-backed runs; direct execution remains host-owned.
- **I10 (ordered queue projection).** Complete pending snapshots for one
  session are observed in durable order.
- **I11 (no false release).** An uncertain guarded store transition retains
  the active fence until process recovery.
- **I12 (closed means closed).** Once quiescing begins, that scheduler instance
  claims or directly acquires no additional work.
- **I13 (acknowledgement is monotonic).** A post-accept scheduling fault cannot
  revoke acceptance or be reported as an unacknowledged submission failure.
- **I14 (authoritative receipt projection).** Absent a surfaced projection
  fault, disposition comes from the exact durable submission after scheduling,
  including a claim won by a concurrent sibling submit.
- **I15 (cancellation-safe handoff).** Once `submit` begins, caller
  cancellation cannot interrupt the accept-to-launch handshake between a
  durable claim and active registration.
- **I16 (validated transition).** The exact queue-head candidate validates under the
  same atomic claim authority; recovery validates active payloads before
  mutation under exclusive startup authority.
- **I17 (failure-atomic handoff).** `ManagedStore::accept`,
  `ManagedStore::claim_next`, `ManagedStore::begin_steer`,
  `ManagedStore::mark_steer_delivered`, and
  `ManagedStore::try_acquire_direct_run` never return `Err` after committing
  the transition the scheduler must acknowledge or drive.
- **I18 (cross-mode publication order).** A managed terminal event precedes
  the next direct start, and a direct terminal event precedes the next managed
  start, even though the durable lease is released before terminal
  publication.
- **I19 (direct fence ownership).** Direct acquire and release pass only
  through `ManagedRuns`; quiescence waits for direct fences while
  `cancel_active` remains submission-only.
- **I20 (guarded queue editing).** Reorder is a complete-order compare-and-swap;
  a stale client cannot discard, duplicate, or overwrite a concurrent queue
  mutation.
- **I21 (lossless steering).** A pending item is removed from claim eligibility
  only by a durable active-run reservation. Runtime rejection restores it;
  runtime acceptance becomes durably `delivered` before AC reports success.
  Settlement restores every unacknowledged reservation, and recovery restores
  all reservations because it has no persistence proof.
- **I22 (authoritative steer settlement).** For every outer terminal outcome,
  the ordered unique `committed_steer_ids` proof commits exactly the matching
  delivered children. Terminal events carry those exact records in proof
  order; outcome alone neither commits nor rejects a steer.

## 9. Proof obligations

The implementation ships:

- P1 store proofs for idempotent enqueue with a stable acceptance sequence,
  compare-and-swap pending reorder with separate queue position, payload
  conflict, one-winner concurrent claim, stale settlement, pending-only
  cancellation, steering reservation/rollback, runtime-delivery confirmation,
  ordered exact-ACK validation and failure atomicity, outcome-independent
  partial commit, unacknowledged/recovery restoration, hidden queue-position
  preservation, cascade deletion, shape-driven schema migration,
  checked-claim rollback, and post-commit mutation-listener panic isolation
  across acceptance, claim, and direct acquisition;
- P2 service proofs for same-session serialization, cross-session
  concurrency, post-accept claim failure returning an acknowledgement with a
  scheduling fault, exact concurrent receipt projection, caller cancellation
  after durable claim, cancellation during input commit, observer-panic
  isolation, quiescence, automatic and deferred recovery, guarded reorder and
  conflict repair snapshots, cancellation-safe steer handoff, idempotent
  repeated steer, no-active/terminal behavior, retry convergence after a
  failure-atomic delivery-confirmation fault, exact ordered steer settlement
  projection on successful and failed outcomes, unacknowledged
  failed/cancelled/direct restoration, every terminal outcome continuing the
  drain, uncertain-transition fencing without false
  pending publication, durably ordered queue snapshots under races, direct
  handoff and quiescence, both cross-mode terminal-before-start orderings, loud
  driver/store failure, and restart over the same file-backed store;
- P3 stock-adapter proofs that corrupt queue-head payloads fail inside the
  checked claim before lease creation and corrupt active payloads fail before
  recovery mutation;
- an application-free example client that submits multiple opaque jobs,
  cancels and reorders pending work, promotes one pending item into an active
  run, proves the resulting sequential drain, and acquires/releases a direct
  run through the real managed service.

## 10. Division of responsibility

| Concern | Owner |
| --- | --- |
| Durable submission record and atomic state transitions | `ac-store` |
| Start-or-queue decision, guarded pending reorder, durable steer promotion, direct lease admission/release, drain and publication gates, submission cancellation, recovery, quiescence | `ac-managed` |
| Active-turn steer queue and step-boundary drain | `ac-runtime` |
| Payload meaning and compatible schema, input projection, runtime steer mapping, credentials, cancellation-aware submission execution, direct run body and outcome | host driver |
| Startup prerequisite ordering, shutdown deadline, wire names, UI state, queue editing presentation | host adapter/client |

## 11. Deferred

- Priorities and coalescing.
- Exactly-once steer consumption across the host-persisted-but-AC-unsettled
  process-death window. It requires an atomic composition or durable replay
  deduplication spanning the host input record and AC settlement.
- Fair scheduling between non-queued direct admission and pending submissions.
- Automatic retry of interrupted runs. It requires a side-effect/idempotency
  contract stronger than agents generally provide.
- Multi-tenant scheduling and quotas.
