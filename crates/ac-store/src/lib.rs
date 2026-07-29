//! Durable sessions: a small SQLite store for session rows and their message
//! logs — the persistence half of reload recovery.
//!
//! The kit's division of labor is deliberate: `ac-runtime`'s `Session` owns
//! the *live* history and knows nothing about storage; this crate owns the
//! *durable* history and knows nothing about the loop. A host stitches them:
//! persist `session.messages()` after a turn, and on reload feed
//! `load_messages` into `Session::resume`. That keeps both crates consumable
//! alone and the boundary between them a plain `Vec<Message>`.
//!
//! Host-specific session state (a working directory, a mode, a UI flag) never
//! becomes a column here — that would be a consumer concept in the kit. It
//! goes in the session's `meta` JSON blob, which the kit stores verbatim and
//! never reads.
//!
//! Sync by design: rusqlite is synchronous and the store is single-user local
//! files. Calls are cheap (WAL, indexed lookups); an async host that cares
//! wraps calls in its runtime's blocking facility.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ac_types::{Message, Role};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("message (de)serialization failed: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unknown session: {0}")]
    UnknownSession(String),
    /// The caller's view of the log is stale: another writer appended since.
    /// Turns a concurrent-writer silent history fork into a detectable
    /// conflict.
    #[error("seq conflict in session {session}: expected next seq {expected}, log is at {actual}")]
    SeqConflict {
        session: String,
        expected: u64,
        actual: u64,
    },
    /// The store was stamped by a newer schema than this build understands.
    /// Old code refuses newer stores cleanly instead of mis-reading them —
    /// version skew is a crash window too: upgrades happen between runs.
    #[error(
        "store schema is from the future: found user_version {found}, supported up to {supported}"
    )]
    FutureSchema { found: u32, supported: u32 },
    /// The integrity probe at open failed. A corrupt store is reported at
    /// the door, not discovered mid-session; carries the first check line.
    #[error("store failed the integrity check: {0}")]
    Corrupt(String),
    /// The caller's view of the meta blob is stale: the stored value no
    /// longer matches the expectation. Carries the current raw value so a
    /// lock/retry loop needs no second query.
    #[error("meta conflict in session {session}: expectation does not match the stored value")]
    MetaConflict {
        session: String,
        current: Option<String>,
    },
    /// A transport retried an acknowledged submission id with different
    /// bytes. Idempotency is identity + payload, never identity alone.
    #[error(
        "managed submission {submission_id} in session {session} was retried with different payload"
    )]
    ManagedSubmissionConflict {
        session: String,
        submission_id: String,
    },
    /// A state string in the database is outside the schema understood by
    /// this build. This is loud rather than silently treating active work as
    /// pending or terminal.
    #[error("invalid managed submission state in store: {0}")]
    InvalidManagedState(String),
    #[error("invalid terminal managed submission state: {0}")]
    InvalidManagedTerminal(String),
    #[error("invalid managed pending order: {0}")]
    InvalidManagedPendingOrder(String),
    #[error("invalid managed steer settlement: {0}")]
    InvalidManagedSteerSettlement(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A session row. `meta` is host-owned JSON the kit never interprets.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    pub id: String,
    pub title: Option<String>,
    pub meta: Option<serde_json::Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable state of one backend-managed submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedSubmissionState {
    Pending,
    Claimed,
    Running,
    Steering,
    Delivered,
    Steered,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
}

impl ManagedSubmissionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Running => "running",
            Self::Steering => "steering",
            Self::Delivered => "delivered",
            Self::Steered => "steered",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "running" => Ok(Self::Running),
            "steering" => Ok(Self::Steering),
            "delivered" => Ok(Self::Delivered),
            "steered" => Ok(Self::Steered),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            other => Err(StoreError::InvalidManagedState(other.to_string())),
        }
    }
}

/// One opaque durable submission. `payload` is host-owned serialized text;
/// AC stores it verbatim and never interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedSubmissionRecord {
    pub session_id: String,
    /// Immutable per-session acceptance order.
    pub sequence: u64,
    /// Mutable pending order, retained during `steering` and `delivered` so an
    /// unacknowledged or recovered steer returns to the same place.
    pub queue_position: u64,
    pub submission_id: String,
    pub payload: String,
    pub state: ManagedSubmissionState,
    pub run_id: Option<String>,
    pub accepted_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRunRecord {
    pub session_id: String,
    pub run_id: String,
    pub submission_id: Option<String>,
    pub started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedEnqueue {
    Inserted(ManagedSubmissionRecord),
    Existing(ManagedSubmissionRecord),
}

/// Result of a compare-and-swap reorder of the complete pending queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedPendingReorder {
    Reordered,
    Unchanged,
    Conflict { current_order: Vec<String> },
}

/// Result of atomically reserving a pending submission for later delivery
/// into an active run's steer queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedSteerCommit {
    Begun(ManagedSubmissionRecord),
    AlreadySteering(ManagedSubmissionRecord),
    AlreadySteered(ManagedSubmissionRecord),
    NotPending(ManagedSubmissionRecord),
    Missing,
    RunMismatch { active_run_id: Option<String> },
}

/// Result of durably confirming that the runtime accepted a reserved steer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedSteerDelivery {
    Delivered(ManagedSubmissionRecord),
    AlreadyDelivered(ManagedSubmissionRecord),
    AlreadySteered(ManagedSubmissionRecord),
    NotSteering(ManagedSubmissionRecord),
    Missing,
    RunMismatch { active_run_id: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedRunAcquire {
    Acquired(ManagedRunRecord),
    Held(ManagedRunRecord),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRunSettlement {
    pub settled: bool,
    /// Exact acknowledged steer children in proof order for any settled
    /// outcome. Empty only when the run did not settle or acknowledged none.
    pub steered: Vec<ManagedSubmissionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedClaim {
    Claimed {
        submission: ManagedSubmissionRecord,
        run: ManagedRunRecord,
    },
    Held(ManagedRunRecord),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedRecovery {
    Requeued {
        submission: ManagedSubmissionRecord,
        run: ManagedRunRecord,
    },
    Interrupted {
        submission: ManagedSubmissionRecord,
        run: ManagedRunRecord,
    },
    Released {
        run: ManagedRunRecord,
    },
}

/// A committed change to the store, reported to the mutation listener.
/// Fired only after the write commits — a failed or conflicted write, or a
/// no-op (delete of an absent row, truncate that removed nothing, adopt of
/// an existing id), fires nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreMutation {
    SessionCreated {
        id: String,
    },
    SessionDeleted {
        id: String,
    },
    SessionRenamed {
        id: String,
    },
    /// The row's `created_at`/`updated_at` were overwritten verbatim (see
    /// [`SqliteStore::set_session_timestamps`]). Distinct from the other
    /// setters because nothing about the session's *content* changed — only
    /// its place in an `updated_at`-ordered list.
    SessionTimestampsSet {
        id: String,
    },
    MetaSet {
        id: String,
    },
    MessagesAppended {
        id: String,
        count: u64,
        next_seq: u64,
    },
    MessagesTruncated {
        id: String,
        deleted: u64,
    },
    ManagedSubmissionChanged {
        id: String,
        submission_id: String,
        state: ManagedSubmissionState,
    },
    ManagedRunChanged {
        id: String,
        run_id: String,
        active: bool,
    },
}

pub type MutationListener = Arc<dyn Fn(&StoreMutation) + Send + Sync>;

/// SQLite-backed session + message store. One global DB file per host (or
/// `open_in_memory` for tests). Internally serialized on one connection —
/// correct and plenty for a single-user local host.
pub struct SqliteStore {
    conn: Mutex<Connection>,
    listener: Mutex<Option<MutationListener>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
  id          TEXT PRIMARY KEY,
  title       TEXT,
  meta        TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);
CREATE TABLE IF NOT EXISTS messages (
  session_id  TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq         INTEGER NOT NULL,
  role        TEXT    NOT NULL,
  content     TEXT    NOT NULL,
  cache       INTEGER NOT NULL DEFAULT 0,
  created_at  INTEGER NOT NULL,
  meta        TEXT,
  PRIMARY KEY (session_id, seq)
);
CREATE TABLE IF NOT EXISTS managed_submissions (
  session_id    TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  sequence      INTEGER NOT NULL,
  queue_position INTEGER NOT NULL,
  submission_id TEXT    NOT NULL,
  payload       TEXT    NOT NULL,
  state         TEXT    NOT NULL CHECK (
    state IN (
      'pending', 'claimed', 'running', 'steering', 'delivered', 'steered',
      'succeeded', 'failed', 'cancelled', 'interrupted'
    )
  ),
  run_id        TEXT,
  accepted_at   INTEGER NOT NULL,
  started_at    INTEGER,
  finished_at   INTEGER,
  error         TEXT,
  PRIMARY KEY (session_id, submission_id),
  UNIQUE (session_id, sequence)
);
CREATE INDEX IF NOT EXISTS idx_managed_submissions_pending
  ON managed_submissions(session_id, state, queue_position, sequence);
CREATE TABLE IF NOT EXISTS managed_runs (
  session_id    TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
  run_id        TEXT NOT NULL UNIQUE,
  submission_id TEXT,
  started_at    INTEGER NOT NULL,
  FOREIGN KEY (session_id, submission_id)
    REFERENCES managed_submissions(session_id, submission_id) ON DELETE CASCADE
);
";

/// Bumped when the on-disk schema changes shape. Opening a store stamped
/// higher fails with [`StoreError::FutureSchema`].
/// v2: `messages.meta` — an opaque per-message host annotation, the exact
/// mirror of `sessions.meta` (the kit stores it verbatim and never reads
/// it). Lets a host keep its own per-message record — an id, display
/// metadata, an alternate rendering — in the same transaction as the
/// message itself instead of a second store with its own crash window.
/// v3: durable opaque managed submissions plus the single active-run lease
/// used for atomic ordered claim and restart reconciliation.
/// v4: immutable acceptance sequence is separated from mutable pending queue
/// position, and pending submissions can enter durable steering.
/// v5: accepted runtime delivery is a distinct durable state, so an uncertain
/// reservation rollback can never be mistaken for delivered input.
const SCHEMA_VERSION: u32 = 5;

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            // Creating the parent is the store's job: the host hands us a
            // location, not a setup ritual.
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        // Another process sharing the file waits briefly instead of getting
        // an instant SQLITE_BUSY on write contention.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Self::quick_check(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL + NORMAL is durable at every commit against process death; an
        // OS-level power loss may lose the final commits but never corrupts.
        // Explicit, not a default relied on silently; the power-loss tier
        // (synchronous=FULL) is a deferred opt-in.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    /// The self-check at the door: cheap (`quick_check`, not
    /// `integrity_check`) and typed — a corrupt store fails open, not a
    /// session mid-use.
    fn quick_check(conn: &Connection) -> Result<()> {
        // A store the probe itself cannot even run over is equally corrupt.
        let line: String = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        if line != "ok" {
            return Err(StoreError::Corrupt(line));
        }
        Ok(())
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let found: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::FutureSchema {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        Self::migrate_managed_submissions_to_current_if_needed(&conn)?;
        conn.execute_batch(SCHEMA)?;
        if found < 2 {
            // v1 → v2: additive column. CREATE IF NOT EXISTS above only
            // shapes fresh tables, so a pre-existing v1 table needs the
            // ALTER; guarded by an actual column probe to stay idempotent.
            let has_meta = conn
                .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'meta'")?
                .exists([])?;
            if !has_meta {
                conn.execute_batch("ALTER TABLE messages ADD COLUMN meta TEXT;")?;
            }
        }
        if found < SCHEMA_VERSION {
            // Fresh store, or one from an older stamp (including pre-
            // versioning user_version 0) — all take the current stamp; the
            // upgrade is idempotent.
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            listener: Mutex::new(None),
        })
    }

    fn migrate_managed_submissions_to_current_if_needed(conn: &Connection) -> Result<()> {
        let table_sql: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'table' AND name = 'managed_submissions'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(table_sql) = table_sql else {
            return Ok(());
        };
        let has_queue_position = conn
            .prepare(
                "SELECT 1 FROM pragma_table_info('managed_submissions')
                 WHERE name = 'queue_position'",
            )?
            .exists([])?;
        let has_delivered_state = table_sql.contains("'delivered'");
        if has_queue_position && has_delivered_state {
            return Ok(());
        }

        // SQLite cannot widen a CHECK constraint in place. Rebuild the table
        // before the current schema is installed, preserving immutable
        // acceptance sequence as the initial pending queue position.
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        let queue_position_source = if has_queue_position {
            "queue_position"
        } else {
            "sequence"
        };
        let migration_sql = format!(
            "BEGIN IMMEDIATE;
             CREATE TABLE managed_submissions_current (
               session_id     TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
               sequence       INTEGER NOT NULL,
               queue_position INTEGER NOT NULL,
               submission_id  TEXT    NOT NULL,
               payload        TEXT    NOT NULL,
               state          TEXT    NOT NULL CHECK (
                 state IN (
                   'pending', 'claimed', 'running', 'steering', 'delivered', 'steered',
                   'succeeded', 'failed', 'cancelled', 'interrupted'
                 )
               ),
               run_id         TEXT,
               accepted_at    INTEGER NOT NULL,
               started_at     INTEGER,
               finished_at    INTEGER,
               error          TEXT,
               PRIMARY KEY (session_id, submission_id),
               UNIQUE (session_id, sequence)
             );
             INSERT INTO managed_submissions_current (
               session_id, sequence, queue_position, submission_id, payload,
               state, run_id, accepted_at, started_at, finished_at, error
             )
             SELECT
               session_id, sequence, {queue_position_source}, submission_id, payload,
               state, run_id, accepted_at, started_at, finished_at, error
             FROM managed_submissions;
             DROP TABLE managed_submissions;
             ALTER TABLE managed_submissions_current RENAME TO managed_submissions;
             COMMIT;"
        );
        let migration = conn.execute_batch(&migration_sql);
        if let Err(error) = migration {
            let _ = conn.execute_batch("ROLLBACK;");
            let _ = conn.pragma_update(None, "foreign_keys", "ON");
            return Err(error.into());
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    /// Installs (or clears) the mutation listener. One listener at a time;
    /// a host that needs fan-out multiplexes behind its own closure. The
    /// listener runs after the write commits and outside the store's
    /// internal lock, so it may reentrantly call back into the store; under
    /// concurrent writers, delivery order may differ from commit order. Its
    /// projection is advisory: a listener panic is isolated and cannot turn a
    /// committed mutation into a reported store failure.
    pub fn set_mutation_listener(&self, listener: Option<MutationListener>) {
        *self.listener.lock().expect("listener lock poisoned") = listener;
    }

    /// Precondition: the connection guard is already dropped. Clones the
    /// listener out and releases its slot before invoking — a listener that
    /// mutates the store reenters here, and holding either guard across the
    /// call would deadlock it.
    fn emit(&self, mutation: StoreMutation) {
        let listener = self
            .listener
            .lock()
            .expect("listener lock poisoned")
            .clone();
        if let Some(listener) = listener {
            let _ = catch_unwind(AssertUnwindSafe(|| listener(&mutation)));
        }
    }

    /// Mints an opaque hex id. Hosts that need their own id scheme can prefix
    /// or wrap at their layer; the kit does not care what ids look like.
    pub fn create_session(&self, title: Option<&str>) -> Result<SessionRecord> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let now = now_ms();
        let id: String =
            conn.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        conn.execute(
            "INSERT INTO sessions (id, title, meta, created_at, updated_at) VALUES (?1, ?2, NULL, ?3, ?3)",
            params![id, title, now],
        )?;
        drop(conn);
        self.emit(StoreMutation::SessionCreated { id: id.clone() });
        Ok(SessionRecord {
            id,
            title: title.map(str::to_string),
            meta: None,
            created_at_ms: now,
            updated_at_ms: now,
        })
    }

    /// Ensures a session exists under a caller-chosen id, no-op if it already
    /// does. For hosts whose client mints the session id (an AI SDK `useChat`
    /// chat id, an ACP session id): the id is theirs, the store just adopts
    /// it. Returns true if a new row was created.
    pub fn create_session_with_id(&self, id: &str, title: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let now = now_ms();
        let created = conn.execute(
            "INSERT OR IGNORE INTO sessions (id, title, meta, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?3)",
            params![id, title, now],
        )?;
        drop(conn);
        if created > 0 {
            self.emit(StoreMutation::SessionCreated { id: id.to_string() });
        }
        Ok(created > 0)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let row = conn
            .query_row(
                "SELECT id, title, meta, created_at, updated_at FROM sessions WHERE id = ?1",
                params![id],
                Self::record_from_row,
            )
            .optional()?;
        row.map(Self::parse_record).transpose()
    }

    /// Newest-first by `updated_at` — the recents list.
    pub fn list_sessions(&self, limit: usize) -> Result<Vec<SessionRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, title, meta, created_at, updated_at FROM sessions
             ORDER BY updated_at DESC, id LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], Self::record_from_row)?;
        rows.map(|r| Self::parse_record(r?)).collect()
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<()> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        drop(conn);
        self.emit(StoreMutation::SessionRenamed { id: id.to_string() });
        Ok(())
    }

    /// Overwrites the row's timestamps verbatim — the one deliberate escape
    /// hatch from the store's stamp-it-now rule. Returns false for an
    /// unknown id (no error: an importer walking a foreign log skips rows it
    /// cannot place, and that is not a failure).
    ///
    /// A host importing an existing conversation history — from an export,
    /// from a previous generation of its own storage — has to restore the
    /// original times. Every other path stamps `now_ms()`:
    /// `create_session_with_id` on insert, every setter and append on
    /// `updated_at`. Without this, imported sessions all carry today's date
    /// and a recents list (`list_sessions`, ordered by `updated_at`) loses
    /// the history it is meant to show. Call it LAST for a session, after
    /// the writes that would bump the row.
    ///
    /// Values are written as given: the store does not clamp them to the
    /// past, order them, or force the per-process monotonicity `now_ms()`
    /// guarantees. Two imported sessions sharing a millisecond tie-break on
    /// id, like any other equal pair.
    pub fn set_session_timestamps(
        &self,
        id: &str,
        created_ms: i64,
        updated_ms: i64,
    ) -> Result<bool> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET created_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, created_ms, updated_ms],
        )? > 0;
        drop(conn);
        if changed {
            self.emit(StoreMutation::SessionTimestampsSet { id: id.to_string() });
        }
        Ok(changed)
    }

    /// Replaces the session's host-owned meta blob verbatim.
    pub fn set_meta(&self, id: &str, meta: &serde_json::Value) -> Result<()> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET meta = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, serde_json::to_string(meta)?, now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        drop(conn);
        self.emit(StoreMutation::MetaSet { id: id.to_string() });
        Ok(())
    }

    /// Sets the meta blob only if the stored value still equals `expected`
    /// (NULL-safe: `None` matches only an absent blob). Both sides are raw
    /// text compared verbatim — the kit still never reads meta, it only
    /// checks equality — which makes this the compare-and-swap substrate a
    /// host builds lock/lease protocols on. On mismatch fails with
    /// [`StoreError::MetaConflict`] carrying the current value, so a retry
    /// loop needs no second query. Callers keep the text valid JSON:
    /// `get_session` parses the blob.
    pub fn set_meta_cas(&self, id: &str, expected: Option<&str>, meta: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET meta = ?2, updated_at = ?3 WHERE id = ?1 AND meta IS ?4",
            params![id, meta, now_ms(), expected],
        )?;
        if changed == 0 {
            // Zero rows is ambiguous: absent session or stale expectation.
            // Disambiguate inside the same locked section, so the reported
            // current value is exactly what the swap compared against.
            let current: Option<Option<String>> = conn
                .query_row(
                    "SELECT meta FROM sessions WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            return match current {
                None => Err(StoreError::UnknownSession(id.to_string())),
                Some(current) => Err(StoreError::MetaConflict {
                    session: id.to_string(),
                    current,
                }),
            };
        }
        drop(conn);
        self.emit(StoreMutation::MetaSet { id: id.to_string() });
        Ok(())
    }

    /// Deletes the session row and its message log — never anything outside
    /// the store (a host's files are not the store's to touch).
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let deleted = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])? > 0;
        drop(conn);
        if deleted {
            self.emit(StoreMutation::SessionDeleted { id: id.to_string() });
        }
        Ok(deleted)
    }

    /// Appends messages atomically, continuing the seq series. Returns the
    /// next unused seq. Typical host call: everything `Session::messages()`
    /// gained during the turn.
    ///
    /// `expected_next_seq` is the lost-update guard: pass the seq the caller
    /// believes comes next (its persisted count) and the append fails with
    /// [`StoreError::SeqConflict`] if another writer advanced the log —
    /// turning a silent history fork into a detectable conflict. `None` skips
    /// the check.
    pub fn append_messages(
        &self,
        id: &str,
        messages: &[Message],
        expected_next_seq: Option<u64>,
    ) -> Result<u64> {
        let entries: Vec<(&Message, Option<&str>)> = messages.iter().map(|m| (m, None)).collect();
        self.append_messages_with_meta(id, &entries, expected_next_seq)
    }

    /// Like [`append_messages`], with an opaque host annotation riding each
    /// message — the per-message mirror of `sessions.meta`, stored verbatim
    /// in the same transaction and never read by the kit.
    pub fn append_messages_with_meta(
        &self,
        id: &str,
        entries: &[(&Message, Option<&str>)],
        expected_next_seq: Option<u64>,
    ) -> Result<u64> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let next_seq = append_rows(&tx, id, entries, expected_next_seq)?;
        tx.commit()?;
        drop(conn);
        if !entries.is_empty() {
            self.emit(StoreMutation::MessagesAppended {
                id: id.to_string(),
                count: entries.len() as u64,
                next_seq,
            });
        }
        Ok(next_seq)
    }

    /// Deletes every message with `seq >= from_seq`, transactionally, and
    /// returns how many were deleted (0 is not an error). The next append
    /// continues from the table's surviving maximum — seq derives from the
    /// log, not a counter — so `expected_next_seq == from_seq` succeeds
    /// after a truncation at `from_seq`.
    pub fn truncate_messages_from(&self, id: &str, from_seq: u64) -> Result<u64> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let deleted = truncate_rows(&tx, id, from_seq)?;
        tx.commit()?;
        drop(conn);
        if deleted > 0 {
            self.emit(StoreMutation::MessagesTruncated {
                id: id.to_string(),
                deleted,
            });
        }
        Ok(deleted)
    }

    /// Atomic read-modify-write of the host meta blob, inside one
    /// transaction under the store's lock: `f` sees the current raw text
    /// (`None` when unset) and returns `Some(next)` to write (and bump
    /// `updated_at`) or `None` to leave the row untouched — a declined
    /// update is a true no-op, no bump, no event. Two concurrent updates
    /// serialize; neither is lost — the primitive lock/lease protocols
    /// build on without compare-and-swap retry loops. The kit still never
    /// reads the blob; `f` is host code.
    pub fn update_meta<T>(
        &self,
        id: &str,
        f: impl FnOnce(Option<&str>) -> (Option<Option<String>>, T),
    ) -> Result<T> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let current: Option<Option<String>> = tx
            .query_row(
                "SELECT meta FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(StoreError::UnknownSession(id.to_string()));
        };
        let (write, out) = f(current.as_deref());
        let wrote = match write {
            None => false,
            Some(next) => {
                tx.execute(
                    "UPDATE sessions SET meta = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, next, now_ms()],
                )?;
                true
            }
        };
        tx.commit()?;
        drop(conn);
        if wrote {
            self.emit(StoreMutation::MetaSet { id: id.to_string() });
        }
        Ok(out)
    }

    /// Runs `f` inside ONE transaction — the composition seam for hosts
    /// whose logical operation spans several store ops (create a branch and
    /// copy its prefix; truncate a log and clear a marker). Everything in
    /// `f` commits or rolls back together; mutation events are collected
    /// and emitted only after the commit, outside the lock. An `Err` from
    /// `f` rolls back and emits nothing.
    pub fn atomic<T>(&self, f: impl FnOnce(&TxOps<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let ops = TxOps {
            tx: &tx,
            mutations: std::cell::RefCell::new(Vec::new()),
        };
        let out = f(&ops)?;
        let mutations = ops.mutations.into_inner();
        tx.commit()?;
        drop(conn);
        for mutation in mutations {
            self.emit(mutation);
        }
        Ok(out)
    }

    // ---- managed submissions + runs ----------------------------------

    /// Durably accept one opaque submission. The pair
    /// `(session_id, submission_id)` is the idempotency key: retrying the
    /// same bytes returns [`ManagedEnqueue::Existing`], while different bytes
    /// fail with [`StoreError::ManagedSubmissionConflict`].
    pub fn enqueue_managed_submission(
        &self,
        session_id: &str,
        submission_id: &str,
        payload: &str,
    ) -> Result<ManagedEnqueue> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        if let Some(existing) = load_managed_submission(&tx, session_id, submission_id)? {
            if existing.payload != payload {
                return Err(StoreError::ManagedSubmissionConflict {
                    session: session_id.to_string(),
                    submission_id: submission_id.to_string(),
                });
            }
            tx.commit()?;
            return Ok(ManagedEnqueue::Existing(existing));
        }

        let (sequence, queue_position): (u64, u64) = tx.query_row(
            "SELECT COALESCE(MAX(sequence) + 1, 0)
                    , COALESCE(MAX(queue_position) + 1, 0)
             FROM managed_submissions WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO managed_submissions (
               session_id, sequence, queue_position, submission_id, payload,
               state, run_id, accepted_at, started_at, finished_at, error
             ) VALUES (
               ?1, ?2, ?3, ?4, ?5, 'pending', NULL, ?6, NULL, NULL, NULL
             )",
            params![
                session_id,
                sequence as i64,
                queue_position as i64,
                submission_id,
                payload,
                now
            ],
        )?;
        touch_session(&tx, session_id, now)?;
        let record = ManagedSubmissionRecord {
            session_id: session_id.to_string(),
            sequence,
            queue_position,
            submission_id: submission_id.to_string(),
            payload: payload.to_string(),
            state: ManagedSubmissionState::Pending,
            run_id: None,
            accepted_at_ms: now,
            started_at_ms: None,
            finished_at_ms: None,
            error: None,
        };
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::ManagedSubmissionChanged {
            id: session_id.to_string(),
            submission_id: submission_id.to_string(),
            state: ManagedSubmissionState::Pending,
        });
        Ok(ManagedEnqueue::Inserted(record))
    }

    pub fn get_managed_submission(
        &self,
        session_id: &str,
        submission_id: &str,
    ) -> Result<Option<ManagedSubmissionRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        ensure_session(&conn, session_id)?;
        load_managed_submission(&conn, session_id, submission_id)
    }

    /// Pending records in mutable durable queue order. Claimed/running/terminal
    /// records are deliberately absent from the editable queue projection.
    pub fn list_pending_managed_submissions(
        &self,
        session_id: &str,
    ) -> Result<Vec<ManagedSubmissionRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        ensure_session(&conn, session_id)?;
        load_managed_submissions_where(
            &conn,
            "session_id = ?1 AND state = 'pending'",
            params![session_id],
        )
    }

    /// Every session with at least one pending record, ordered by the oldest
    /// pending accept time. Used by managed recovery to wake durable work.
    pub fn pending_managed_session_ids(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT session_id
             FROM managed_submissions
             WHERE state = 'pending'
             GROUP BY session_id
             ORDER BY MIN(accepted_at), MIN(sequence)",
        )?;
        Ok(stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Cancel only an editable pending item. Claimed/running/terminal ids are
    /// a no-op so a stale client can never cancel a newer lifecycle phase.
    pub fn cancel_pending_managed_submission(
        &self,
        session_id: &str,
        submission_id: &str,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'cancelled', finished_at = ?3, error = NULL
             WHERE session_id = ?1 AND submission_id = ?2 AND state = 'pending'",
            params![session_id, submission_id, now],
        )? > 0;
        if changed {
            touch_session(&tx, session_id, now)?;
        }
        tx.commit()?;
        drop(conn);
        if changed {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id: submission_id.to_string(),
                state: ManagedSubmissionState::Cancelled,
            });
        }
        Ok(changed)
    }

    /// Compare-and-swap the complete pending order.
    ///
    /// `expected_order` protects against a stale client overwriting a
    /// concurrent acceptance, claim, cancellation, or reorder. Retrying an
    /// already-applied request is idempotent: if the durable order already
    /// equals `desired_order`, [`ManagedPendingReorder::Unchanged`] is
    /// returned even though it no longer equals `expected_order`.
    pub fn reorder_pending_managed_submissions(
        &self,
        session_id: &str,
        expected_order: &[String],
        desired_order: &[String],
    ) -> Result<ManagedPendingReorder> {
        validate_pending_reorder(expected_order, desired_order)?;
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let current = load_managed_submissions_where(
            &tx,
            "session_id = ?1 AND state = 'pending'",
            params![session_id],
        )?;
        let current_order = current
            .iter()
            .map(|record| record.submission_id.clone())
            .collect::<Vec<_>>();
        if current_order == desired_order {
            tx.commit()?;
            return Ok(ManagedPendingReorder::Unchanged);
        }
        if current_order != expected_order {
            tx.commit()?;
            return Ok(ManagedPendingReorder::Conflict { current_order });
        }

        for (position, submission_id) in desired_order.iter().enumerate() {
            let queue_position = current[position].queue_position;
            let changed = tx.execute(
                "UPDATE managed_submissions
                 SET queue_position = ?3
                 WHERE session_id = ?1 AND submission_id = ?2
                   AND state = 'pending'",
                params![session_id, submission_id, queue_position as i64],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidManagedState(
                    "pending submission changed during serialized reorder".to_string(),
                ));
            }
        }
        let now = now_ms();
        touch_session(&tx, session_id, now)?;
        tx.commit()?;
        drop(conn);
        for submission_id in desired_order {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id: submission_id.clone(),
                state: ManagedSubmissionState::Pending,
            });
        }
        Ok(ManagedPendingReorder::Reordered)
    }

    /// Durably reserve a pending submission for delivery into the named
    /// active run. This transaction is the no-loss boundary: the record is
    /// removed from ordinary claim eligibility only after its durable
    /// `steering` intent and active-run binding commit together.
    pub fn begin_pending_managed_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<ManagedSteerCommit> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let Some(mut record) = load_managed_submission(&tx, session_id, submission_id)? else {
            tx.commit()?;
            return Ok(ManagedSteerCommit::Missing);
        };
        match record.state {
            ManagedSubmissionState::Steering => {
                tx.commit()?;
                return Ok(ManagedSteerCommit::AlreadySteering(record));
            }
            ManagedSubmissionState::Delivered => {
                tx.commit()?;
                return Ok(ManagedSteerCommit::AlreadySteering(record));
            }
            ManagedSubmissionState::Steered => {
                tx.commit()?;
                return Ok(ManagedSteerCommit::AlreadySteered(record));
            }
            ManagedSubmissionState::Pending => {}
            _ => {
                tx.commit()?;
                return Ok(ManagedSteerCommit::NotPending(record));
            }
        }
        let active_run_id = load_active_managed_run(&tx, session_id)?.map(|run| run.run_id);
        if active_run_id.as_deref() != Some(run_id) {
            tx.commit()?;
            return Ok(ManagedSteerCommit::RunMismatch { active_run_id });
        }

        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'steering', run_id = ?3, started_at = ?4,
                 finished_at = NULL, error = NULL
             WHERE session_id = ?1 AND submission_id = ?2
               AND state = 'pending'",
            params![session_id, submission_id, run_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidManagedState(
                "pending submission changed during serialized steer reservation".to_string(),
            ));
        }
        touch_session(&tx, session_id, now)?;
        record.state = ManagedSubmissionState::Steering;
        record.run_id = Some(run_id.to_string());
        record.started_at_ms = Some(now);
        record.finished_at_ms = None;
        record.error = None;
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::ManagedSubmissionChanged {
            id: session_id.to_string(),
            submission_id: submission_id.to_string(),
            state: ManagedSubmissionState::Steering,
        });
        Ok(ManagedSteerCommit::Begun(record))
    }

    /// Durably confirm runtime ownership of a reserved steer. `Err` is
    /// failure-atomic: no `steering -> delivered` transition committed.
    pub fn mark_pending_managed_steer_delivered(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
    ) -> Result<ManagedSteerDelivery> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let Some(mut record) = load_managed_submission(&tx, session_id, submission_id)? else {
            tx.commit()?;
            return Ok(ManagedSteerDelivery::Missing);
        };
        match record.state {
            ManagedSubmissionState::Delivered => {
                tx.commit()?;
                return Ok(ManagedSteerDelivery::AlreadyDelivered(record));
            }
            ManagedSubmissionState::Steered => {
                tx.commit()?;
                return Ok(ManagedSteerDelivery::AlreadySteered(record));
            }
            ManagedSubmissionState::Steering => {}
            _ => {
                tx.commit()?;
                return Ok(ManagedSteerDelivery::NotSteering(record));
            }
        }
        let active_run_id = load_active_managed_run(&tx, session_id)?.map(|run| run.run_id);
        if active_run_id.as_deref() != Some(run_id) || record.run_id.as_deref() != Some(run_id) {
            tx.commit()?;
            return Ok(ManagedSteerDelivery::RunMismatch { active_run_id });
        }
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'delivered', error = NULL
             WHERE session_id = ?1 AND submission_id = ?2
               AND run_id = ?3 AND state = 'steering'",
            params![session_id, submission_id, run_id],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidManagedState(
                "steering submission changed during delivery confirmation".to_string(),
            ));
        }
        touch_session(&tx, session_id, now)?;
        record.state = ManagedSubmissionState::Delivered;
        record.error = None;
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::ManagedSubmissionChanged {
            id: session_id.to_string(),
            submission_id: submission_id.to_string(),
            state: ManagedSubmissionState::Delivered,
        });
        Ok(ManagedSteerDelivery::Delivered(record))
    }

    /// Return a failed steer reservation to its original pending order.
    /// Guarding by both state and run id makes stale delivery failures a no-op.
    pub fn rollback_pending_managed_steer(
        &self,
        session_id: &str,
        submission_id: &str,
        run_id: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'pending', run_id = NULL, started_at = NULL,
                 finished_at = NULL, error = ?4
             WHERE session_id = ?1 AND submission_id = ?2
               AND run_id = ?3 AND state = 'steering'",
            params![session_id, submission_id, run_id, error],
        )? > 0;
        if changed {
            touch_session(&tx, session_id, now)?;
        }
        tx.commit()?;
        drop(conn);
        if changed {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id: submission_id.to_string(),
                state: ManagedSubmissionState::Pending,
            });
        }
        Ok(changed)
    }

    /// Atomically claim exactly the first durably ordered pending submission
    /// if the session has no active managed run. This is the single-flight
    /// claim transaction; callers never compose a separate check + dequeue.
    pub fn claim_next_managed_submission(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<ManagedClaim> {
        self.claim_next_managed_submission_checked(session_id, run_id, |_| Ok(()))
    }

    /// [`Self::claim_next_managed_submission`] with validation of the exact
    /// oldest record inside the same transaction and before either the run
    /// lease or claimed state is written.
    ///
    /// Typed adapters use this to decode an opaque payload without a
    /// list-then-claim race. A validation error rolls back the transaction and
    /// leaves the submission pending.
    pub fn claim_next_managed_submission_checked(
        &self,
        session_id: &str,
        run_id: &str,
        validate: impl FnOnce(&ManagedSubmissionRecord) -> Result<()>,
    ) -> Result<ManagedClaim> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        if let Some(active) = load_active_managed_run(&tx, session_id)? {
            tx.commit()?;
            return Ok(ManagedClaim::Held(active));
        }
        let Some(mut submission) = load_oldest_pending_submission(&tx, session_id)? else {
            tx.commit()?;
            return Ok(ManagedClaim::Empty);
        };
        validate(&submission)?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO managed_runs (session_id, run_id, submission_id, started_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, run_id, submission.submission_id, now],
        )?;
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'claimed', run_id = ?3, started_at = ?4,
                 finished_at = NULL, error = NULL
             WHERE session_id = ?1 AND submission_id = ?2 AND state = 'pending'",
            params![session_id, submission.submission_id, run_id, now],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidManagedState(
                "oldest pending submission changed during serialized claim".to_string(),
            ));
        }
        touch_session(&tx, session_id, now)?;
        submission.state = ManagedSubmissionState::Claimed;
        submission.run_id = Some(run_id.to_string());
        submission.started_at_ms = Some(now);
        submission.finished_at_ms = None;
        submission.error = None;
        let run = ManagedRunRecord {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            submission_id: Some(submission.submission_id.clone()),
            started_at_ms: now,
        };
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::ManagedSubmissionChanged {
            id: session_id.to_string(),
            submission_id: submission.submission_id.clone(),
            state: ManagedSubmissionState::Claimed,
        });
        self.emit(StoreMutation::ManagedRunChanged {
            id: session_id.to_string(),
            run_id: run_id.to_string(),
            active: true,
        });
        Ok(ManagedClaim::Claimed { submission, run })
    }

    /// Marks the input durability point. A driver MUST call this after its
    /// idempotent input commit and before model sampling.
    pub fn mark_managed_input_committed(&self, session_id: &str, run_id: &str) -> Result<bool> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let submission_id = active_submission_id(&tx, session_id, run_id)?;
        let Some(submission_id) = submission_id else {
            tx.commit()?;
            return Ok(false);
        };
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'running'
             WHERE session_id = ?1 AND submission_id = ?2
               AND run_id = ?3 AND state = 'claimed'",
            params![session_id, submission_id, run_id],
        )? > 0;
        if changed {
            touch_session(&tx, session_id, now)?;
        }
        tx.commit()?;
        drop(conn);
        if changed {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id,
                state: ManagedSubmissionState::Running,
            });
        }
        Ok(changed)
    }

    /// Input preparation failed before sampling. Return the claimed record to
    /// queue eligibility and release its run in one transaction. The service
    /// stops that drain pass so the failure cannot become a hot retry loop.
    pub fn requeue_managed_claim(
        &self,
        session_id: &str,
        run_id: &str,
        error: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let Some(submission_id) = active_submission_id(&tx, session_id, run_id)? else {
            tx.commit()?;
            return Ok(false);
        };
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = 'pending', run_id = NULL, started_at = NULL,
                 finished_at = NULL, error = ?4
             WHERE session_id = ?1 AND submission_id = ?2
               AND run_id = ?3 AND state = 'claimed'",
            params![session_id, submission_id, run_id, error],
        )? > 0;
        let steering_mutations = if changed {
            let (_, mutations) = resolve_steering_submissions(&tx, session_id, run_id, &[], now)?;
            tx.execute(
                "DELETE FROM managed_runs WHERE session_id = ?1 AND run_id = ?2",
                params![session_id, run_id],
            )?;
            touch_session(&tx, session_id, now)?;
            mutations
        } else {
            Vec::new()
        };
        tx.commit()?;
        drop(conn);
        if changed {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id,
                state: ManagedSubmissionState::Pending,
            });
            for mutation in steering_mutations {
                self.emit(mutation);
            }
            self.emit(StoreMutation::ManagedRunChanged {
                id: session_id.to_string(),
                run_id: run_id.to_string(),
                active: false,
            });
        }
        Ok(changed)
    }

    /// Guarded terminal settlement. The matching active submission must have
    /// passed the input commit (`running`). A stale run id changes nothing.
    pub fn finish_managed_run(
        &self,
        session_id: &str,
        run_id: &str,
        terminal: ManagedSubmissionState,
        error: Option<&str>,
        committed_steer_ids: &[String],
    ) -> Result<ManagedRunSettlement> {
        if !matches!(
            terminal,
            ManagedSubmissionState::Succeeded
                | ManagedSubmissionState::Failed
                | ManagedSubmissionState::Cancelled
                | ManagedSubmissionState::Interrupted
        ) {
            return Err(StoreError::InvalidManagedTerminal(
                terminal.as_str().to_string(),
            ));
        }
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        let Some(submission_id) = active_submission_id(&tx, session_id, run_id)? else {
            tx.commit()?;
            return Ok(ManagedRunSettlement {
                settled: false,
                steered: Vec::new(),
            });
        };
        let now = now_ms();
        let changed = tx.execute(
            "UPDATE managed_submissions
             SET state = ?4, finished_at = ?5, error = ?6
             WHERE session_id = ?1 AND submission_id = ?2
               AND run_id = ?3 AND state = 'running'",
            params![
                session_id,
                submission_id,
                run_id,
                terminal.as_str(),
                now,
                error
            ],
        )? > 0;
        let (steered, steering_mutations) = if changed {
            let (records, mutations) =
                resolve_steering_submissions(&tx, session_id, run_id, committed_steer_ids, now)?;
            tx.execute(
                "DELETE FROM managed_runs WHERE session_id = ?1 AND run_id = ?2",
                params![session_id, run_id],
            )?;
            touch_session(&tx, session_id, now)?;
            (records, mutations)
        } else {
            (Vec::new(), Vec::new())
        };
        tx.commit()?;
        drop(conn);
        if changed {
            self.emit(StoreMutation::ManagedSubmissionChanged {
                id: session_id.to_string(),
                submission_id,
                state: terminal,
            });
            for mutation in steering_mutations {
                self.emit(mutation);
            }
            self.emit(StoreMutation::ManagedRunChanged {
                id: session_id.to_string(),
                run_id: run_id.to_string(),
                active: false,
            });
        }
        Ok(ManagedRunSettlement {
            settled: changed,
            steered,
        })
    }

    /// Acquire the same per-session single-flight lease without a queued
    /// submission. This supports host-initiated maintenance runs while still
    /// excluding managed submissions.
    pub fn try_acquire_managed_run(
        &self,
        session_id: &str,
        run_id: &str,
    ) -> Result<ManagedRunAcquire> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        ensure_session(&tx, session_id)?;
        if let Some(active) = load_active_managed_run(&tx, session_id)? {
            tx.commit()?;
            return Ok(ManagedRunAcquire::Held(active));
        }
        let now = now_ms();
        tx.execute(
            "INSERT INTO managed_runs (session_id, run_id, submission_id, started_at)
             VALUES (?1, ?2, NULL, ?3)",
            params![session_id, run_id, now],
        )?;
        touch_session(&tx, session_id, now)?;
        let run = ManagedRunRecord {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            submission_id: None,
            started_at_ms: now,
        };
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::ManagedRunChanged {
            id: session_id.to_string(),
            run_id: run_id.to_string(),
            active: true,
        });
        Ok(ManagedRunAcquire::Acquired(run))
    }

    /// Release a direct (non-submission) run, guarded by run id. Submission
    /// runs settle through [`Self::finish_managed_run`] instead.
    pub fn release_managed_run(
        &self,
        session_id: &str,
        run_id: &str,
        committed_steer_ids: &[String],
    ) -> Result<ManagedRunSettlement> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let active = load_active_managed_run(&tx, session_id)?;
        let releasable = active
            .as_ref()
            .is_some_and(|run| run.run_id == run_id && run.submission_id.is_none());
        let (steered, steering_mutations) = if releasable {
            let now = now_ms();
            let (records, mutations) =
                resolve_steering_submissions(&tx, session_id, run_id, committed_steer_ids, now)?;
            tx.execute(
                "DELETE FROM managed_runs WHERE session_id = ?1 AND run_id = ?2",
                params![session_id, run_id],
            )?;
            touch_session(&tx, session_id, now)?;
            (records, mutations)
        } else {
            (Vec::new(), Vec::new())
        };
        tx.commit()?;
        drop(conn);
        if releasable {
            for mutation in steering_mutations {
                self.emit(mutation);
            }
            self.emit(StoreMutation::ManagedRunChanged {
                id: session_id.to_string(),
                run_id: run_id.to_string(),
                active: false,
            });
        }
        Ok(ManagedRunSettlement {
            settled: releasable,
            steered,
        })
    }

    pub fn active_managed_run(&self, session_id: &str) -> Result<Option<ManagedRunRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        ensure_session(&conn, session_id)?;
        load_active_managed_run(&conn, session_id)
    }

    /// Submission records referenced by current managed run leases.
    ///
    /// A typed adapter uses this as a recovery preflight while it has
    /// exclusive store authority: every host-owned payload is decoded before
    /// [`Self::reconcile_managed_runs`] is allowed to mutate durable state.
    /// Direct leases have no submission and are omitted.
    pub fn active_managed_submissions(&self) -> Result<Vec<ManagedSubmissionRecord>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let runs = load_all_active_managed_runs(&conn)?;
        let mut submissions = Vec::with_capacity(runs.len());
        for run in runs {
            let Some(submission_id) = run.submission_id.as_deref() else {
                continue;
            };
            let Some(submission) = load_managed_submission(&conn, &run.session_id, submission_id)?
            else {
                return Err(StoreError::InvalidManagedState(format!(
                    "active run {} references missing submission {submission_id}",
                    run.run_id
                )));
            };
            submissions.push(submission);
        }
        Ok(submissions)
    }

    /// Reopen reconciliation. Pre-input claims return to pending; runs whose
    /// input was committed become terminal interrupted; direct leases are
    /// simply released. One transaction makes recovery a fixed point.
    pub fn reconcile_managed_runs(&self) -> Result<Vec<ManagedRecovery>> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let runs = load_all_active_managed_runs(&tx)?;
        let now = now_ms();
        let mut recovered = Vec::with_capacity(runs.len());
        let mut mutations = Vec::new();
        for run in runs {
            let (_, steering_mutations) =
                resolve_steering_submissions(&tx, &run.session_id, &run.run_id, &[], now)?;
            mutations.extend(steering_mutations);
            match run.submission_id.as_deref() {
                None => {
                    recovered.push(ManagedRecovery::Released { run: run.clone() });
                }
                Some(submission_id) => {
                    let Some(mut submission) =
                        load_managed_submission(&tx, &run.session_id, submission_id)?
                    else {
                        return Err(StoreError::InvalidManagedState(format!(
                            "active run {} references missing submission {submission_id}",
                            run.run_id
                        )));
                    };
                    match submission.state {
                        ManagedSubmissionState::Claimed => {
                            tx.execute(
                                "UPDATE managed_submissions
                                 SET state = 'pending', run_id = NULL, started_at = NULL,
                                     finished_at = NULL, error = NULL
                                 WHERE session_id = ?1 AND submission_id = ?2",
                                params![run.session_id, submission_id],
                            )?;
                            submission.state = ManagedSubmissionState::Pending;
                            submission.run_id = None;
                            submission.started_at_ms = None;
                            submission.finished_at_ms = None;
                            submission.error = None;
                            mutations.push(StoreMutation::ManagedSubmissionChanged {
                                id: run.session_id.clone(),
                                submission_id: submission_id.to_string(),
                                state: ManagedSubmissionState::Pending,
                            });
                        }
                        ManagedSubmissionState::Running => {
                            let message = "managed run interrupted by process restart";
                            tx.execute(
                                "UPDATE managed_submissions
                                 SET state = 'interrupted', finished_at = ?3, error = ?4
                                 WHERE session_id = ?1 AND submission_id = ?2",
                                params![run.session_id, submission_id, now, message],
                            )?;
                            submission.state = ManagedSubmissionState::Interrupted;
                            submission.finished_at_ms = Some(now);
                            submission.error = Some(message.to_string());
                            mutations.push(StoreMutation::ManagedSubmissionChanged {
                                id: run.session_id.clone(),
                                submission_id: submission_id.to_string(),
                                state: ManagedSubmissionState::Interrupted,
                            });
                        }
                        other => {
                            return Err(StoreError::InvalidManagedState(format!(
                                "active run {} references {} submission {submission_id}",
                                run.run_id,
                                other.as_str()
                            )));
                        }
                    }
                    touch_session(&tx, &run.session_id, now)?;
                    recovered.push(match submission.state {
                        ManagedSubmissionState::Pending => ManagedRecovery::Requeued {
                            submission,
                            run: run.clone(),
                        },
                        ManagedSubmissionState::Interrupted => ManagedRecovery::Interrupted {
                            submission,
                            run: run.clone(),
                        },
                        _ => unreachable!("recovery normalizes claimed or running only"),
                    });
                }
            }
            tx.execute(
                "DELETE FROM managed_runs WHERE session_id = ?1 AND run_id = ?2",
                params![run.session_id, run.run_id],
            )?;
            mutations.push(StoreMutation::ManagedRunChanged {
                id: run.session_id.clone(),
                run_id: run.run_id.clone(),
                active: false,
            });
        }
        tx.commit()?;
        drop(conn);
        for mutation in mutations {
            self.emit(mutation);
        }
        Ok(recovered)
    }

    /// The full message log in seq order — feed it to `Session::resume`.
    pub fn load_messages(&self, id: &str) -> Result<Vec<Message>> {
        Ok(self
            .load_messages_with_meta(id)?
            .into_iter()
            .map(|(message, _)| message)
            .collect())
    }

    /// Like [`load_messages`], carrying each message's opaque host
    /// annotation (see [`append_messages_with_meta`]).
    pub fn load_messages_with_meta(&self, id: &str) -> Result<Vec<(Message, Option<String>)>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        load_rows(&conn, id)
    }

    pub fn message_count(&self, id: &str) -> Result<u64> {
        let conn = self.conn.lock().expect("store lock poisoned");
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![id],
            |row| row.get::<_, i64>(0).map(|v| v as u64),
        )?)
    }

    fn record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRecord> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    }

    fn parse_record(
        (id, title, meta, created_at_ms, updated_at_ms): RawRecord,
    ) -> Result<SessionRecord> {
        let meta = meta.as_deref().map(serde_json::from_str).transpose()?;
        Ok(SessionRecord {
            id,
            title,
            meta,
            created_at_ms,
            updated_at_ms,
        })
    }
}

type RawRecord = (String, Option<String>, Option<String>, i64, i64);

/// Wall-clock ms, forced strictly monotonic per process: rapid successive
/// store ops land in the same wall millisecond, and `updated_at` ordering —
/// the recents list — must still be deterministic.
fn now_ms() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST: AtomicI64 = AtomicI64::new(0);
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = wall.max(last + 1);
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

/// The transactional view handed to [`SqliteStore::atomic`]'s closure. Each
/// method mirrors its `SqliteStore` namesake, but runs on the enclosing
/// transaction and queues its mutation event for post-commit emission.
pub struct TxOps<'a> {
    tx: &'a rusqlite::Transaction<'a>,
    mutations: std::cell::RefCell<Vec<StoreMutation>>,
}

impl TxOps<'_> {
    pub fn create_session_with_id(&self, id: &str, title: Option<&str>) -> Result<bool> {
        let now = now_ms();
        let created = self.tx.execute(
            "INSERT OR IGNORE INTO sessions (id, title, meta, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?3)",
            params![id, title, now],
        )? > 0;
        if created {
            self.mutations
                .borrow_mut()
                .push(StoreMutation::SessionCreated { id: id.to_string() });
        }
        Ok(created)
    }

    /// Unconditional meta replace — atomicity comes from the enclosing
    /// transaction, so no compare-and-swap is needed inside it.
    pub fn set_meta_raw(&self, id: &str, meta: Option<&str>) -> Result<()> {
        let changed = self.tx.execute(
            "UPDATE sessions SET meta = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, meta, now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        self.mutations
            .borrow_mut()
            .push(StoreMutation::MetaSet { id: id.to_string() });
        Ok(())
    }

    /// See [`SqliteStore::set_session_timestamps`]. In a transaction an
    /// import lands as one unit: create the row, append its log, restore its
    /// times — all or nothing.
    pub fn set_session_timestamps(
        &self,
        id: &str,
        created_ms: i64,
        updated_ms: i64,
    ) -> Result<bool> {
        let changed = self.tx.execute(
            "UPDATE sessions SET created_at = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, created_ms, updated_ms],
        )? > 0;
        if changed {
            self.mutations
                .borrow_mut()
                .push(StoreMutation::SessionTimestampsSet { id: id.to_string() });
        }
        Ok(changed)
    }

    /// The current raw meta text (`None` = unset), read inside the
    /// transaction — consistent with every other op in the closure.
    pub fn meta_raw(&self, id: &str) -> Result<Option<String>> {
        let current: Option<Option<String>> = self
            .tx
            .query_row(
                "SELECT meta FROM sessions WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        current.ok_or_else(|| StoreError::UnknownSession(id.to_string()))
    }

    pub fn append_messages_with_meta(
        &self,
        id: &str,
        entries: &[(&Message, Option<&str>)],
        expected_next_seq: Option<u64>,
    ) -> Result<u64> {
        let next_seq = append_rows(self.tx, id, entries, expected_next_seq)?;
        if !entries.is_empty() {
            self.mutations
                .borrow_mut()
                .push(StoreMutation::MessagesAppended {
                    id: id.to_string(),
                    count: entries.len() as u64,
                    next_seq,
                });
        }
        Ok(next_seq)
    }

    pub fn truncate_messages_from(&self, id: &str, from_seq: u64) -> Result<u64> {
        let deleted = truncate_rows(self.tx, id, from_seq)?;
        if deleted > 0 {
            self.mutations
                .borrow_mut()
                .push(StoreMutation::MessagesTruncated {
                    id: id.to_string(),
                    deleted,
                });
        }
        Ok(deleted)
    }

    pub fn load_messages_with_meta(&self, id: &str) -> Result<Vec<(Message, Option<String>)>> {
        load_rows(self.tx, id)
    }
}

#[derive(Debug)]
struct RawManagedSubmission {
    session_id: String,
    sequence: i64,
    queue_position: i64,
    submission_id: String,
    payload: String,
    state: String,
    run_id: Option<String>,
    accepted_at_ms: i64,
    started_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    error: Option<String>,
}

fn raw_managed_submission(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawManagedSubmission> {
    Ok(RawManagedSubmission {
        session_id: row.get(0)?,
        sequence: row.get(1)?,
        queue_position: row.get(2)?,
        submission_id: row.get(3)?,
        payload: row.get(4)?,
        state: row.get(5)?,
        run_id: row.get(6)?,
        accepted_at_ms: row.get(7)?,
        started_at_ms: row.get(8)?,
        finished_at_ms: row.get(9)?,
        error: row.get(10)?,
    })
}

fn managed_submission_record(raw: RawManagedSubmission) -> Result<ManagedSubmissionRecord> {
    Ok(ManagedSubmissionRecord {
        session_id: raw.session_id,
        sequence: raw.sequence as u64,
        queue_position: raw.queue_position as u64,
        submission_id: raw.submission_id,
        payload: raw.payload,
        state: ManagedSubmissionState::parse(&raw.state)?,
        run_id: raw.run_id,
        accepted_at_ms: raw.accepted_at_ms,
        started_at_ms: raw.started_at_ms,
        finished_at_ms: raw.finished_at_ms,
        error: raw.error,
    })
}

fn validate_pending_reorder(expected: &[String], desired: &[String]) -> Result<()> {
    if expected.len() != desired.len() {
        return Err(StoreError::InvalidManagedPendingOrder(
            "expected and desired orders have different lengths".to_string(),
        ));
    }
    let expected_set = expected.iter().collect::<HashSet<_>>();
    let desired_set = desired.iter().collect::<HashSet<_>>();
    if expected_set.len() != expected.len() || desired_set.len() != desired.len() {
        return Err(StoreError::InvalidManagedPendingOrder(
            "orders must not contain duplicate submission ids".to_string(),
        ));
    }
    if expected_set != desired_set {
        return Err(StoreError::InvalidManagedPendingOrder(
            "desired order must be a permutation of expected order".to_string(),
        ));
    }
    Ok(())
}

const MANAGED_SUBMISSION_COLUMNS: &str = "
  session_id, sequence, queue_position, submission_id, payload, state, run_id,
  accepted_at, started_at, finished_at, error
";

fn load_managed_submission(
    conn: &Connection,
    session_id: &str,
    submission_id: &str,
) -> Result<Option<ManagedSubmissionRecord>> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {MANAGED_SUBMISSION_COLUMNS}
                 FROM managed_submissions
                 WHERE session_id = ?1 AND submission_id = ?2"
            ),
            params![session_id, submission_id],
            raw_managed_submission,
        )
        .optional()?;
    raw.map(managed_submission_record).transpose()
}

fn load_oldest_pending_submission(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ManagedSubmissionRecord>> {
    let raw = conn
        .query_row(
            &format!(
                "SELECT {MANAGED_SUBMISSION_COLUMNS}
                 FROM managed_submissions
                 WHERE session_id = ?1 AND state = 'pending'
                 ORDER BY queue_position, sequence
                 LIMIT 1"
            ),
            params![session_id],
            raw_managed_submission,
        )
        .optional()?;
    raw.map(managed_submission_record).transpose()
}

fn load_managed_submissions_where<P: rusqlite::Params>(
    conn: &Connection,
    predicate: &str,
    params: P,
) -> Result<Vec<ManagedSubmissionRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MANAGED_SUBMISSION_COLUMNS}
         FROM managed_submissions
         WHERE {predicate}
         ORDER BY queue_position, sequence"
    ))?;
    let raw: Vec<RawManagedSubmission> = stmt
        .query_map(params, raw_managed_submission)?
        .collect::<rusqlite::Result<_>>()?;
    raw.into_iter().map(managed_submission_record).collect()
}

fn load_active_managed_run(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<ManagedRunRecord>> {
    Ok(conn
        .query_row(
            "SELECT session_id, run_id, submission_id, started_at
             FROM managed_runs WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(ManagedRunRecord {
                    session_id: row.get(0)?,
                    run_id: row.get(1)?,
                    submission_id: row.get(2)?,
                    started_at_ms: row.get(3)?,
                })
            },
        )
        .optional()?)
}

fn resolve_steering_submissions(
    tx: &Connection,
    session_id: &str,
    run_id: &str,
    committed_steer_ids: &[String],
    now: i64,
) -> Result<(Vec<ManagedSubmissionRecord>, Vec<StoreMutation>)> {
    let mut records = load_managed_submissions_where(
        tx,
        "session_id = ?1 AND run_id = ?2 AND state IN ('steering', 'delivered')",
        params![session_id, run_id],
    )?;
    let committed = committed_steer_ids.iter().collect::<HashSet<_>>();
    if committed.len() != committed_steer_ids.len() {
        return Err(StoreError::InvalidManagedSteerSettlement(
            "committed steer ids must be unique".to_string(),
        ));
    }
    for submission_id in &committed {
        if !records.iter().any(|record| {
            &record.submission_id == *submission_id
                && record.state == ManagedSubmissionState::Delivered
        }) {
            return Err(StoreError::InvalidManagedSteerSettlement(format!(
                "committed steer {submission_id} is not delivered and bound to run {run_id}"
            )));
        }
    }

    let mut steered_by_id = HashMap::with_capacity(committed.len());
    let mut mutations = Vec::with_capacity(records.len());
    for record in &mut records {
        let commit = committed.contains(&record.submission_id);
        let state = if commit {
            let changed = tx.execute(
                "UPDATE managed_submissions
                 SET state = 'steered', finished_at = ?4, error = NULL
                 WHERE session_id = ?1 AND submission_id = ?2
                   AND run_id = ?3 AND state = 'delivered'",
                params![session_id, record.submission_id, run_id, now],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidManagedState(
                    "delivered steer changed during serialized settlement".to_string(),
                ));
            }
            ManagedSubmissionState::Steered
        } else {
            let changed = tx.execute(
                "UPDATE managed_submissions
                 SET state = 'pending', run_id = NULL, started_at = NULL,
                     finished_at = NULL, error = NULL
                 WHERE session_id = ?1 AND submission_id = ?2
                   AND run_id = ?3 AND state IN ('steering', 'delivered')",
                params![session_id, record.submission_id, run_id],
            )?;
            if changed != 1 {
                return Err(StoreError::InvalidManagedState(
                    "uncommitted steer changed during serialized settlement".to_string(),
                ));
            }
            ManagedSubmissionState::Pending
        };
        record.state = state;
        if commit {
            record.finished_at_ms = Some(now);
        } else {
            record.run_id = None;
            record.started_at_ms = None;
            record.finished_at_ms = None;
        }
        record.error = None;
        if commit {
            steered_by_id.insert(record.submission_id.clone(), record.clone());
        }
        mutations.push(StoreMutation::ManagedSubmissionChanged {
            id: session_id.to_string(),
            submission_id: record.submission_id.clone(),
            state,
        });
    }
    let steered = committed_steer_ids
        .iter()
        .map(|submission_id| {
            steered_by_id.remove(submission_id).ok_or_else(|| {
                StoreError::InvalidManagedSteerSettlement(format!(
                    "committed steer {submission_id} disappeared during settlement"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((steered, mutations))
}

fn load_all_active_managed_runs(conn: &Connection) -> Result<Vec<ManagedRunRecord>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, run_id, submission_id, started_at
         FROM managed_runs ORDER BY started_at, session_id",
    )?;
    Ok(stmt
        .query_map([], |row| {
            Ok(ManagedRunRecord {
                session_id: row.get(0)?,
                run_id: row.get(1)?,
                submission_id: row.get(2)?,
                started_at_ms: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

fn active_submission_id(
    conn: &Connection,
    session_id: &str,
    run_id: &str,
) -> Result<Option<String>> {
    let row: Option<Option<String>> = conn
        .query_row(
            "SELECT submission_id FROM managed_runs
             WHERE session_id = ?1 AND run_id = ?2",
            params![session_id, run_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.flatten())
}

fn ensure_session(conn: &Connection, session_id: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        params![session_id],
        |row| row.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(StoreError::UnknownSession(session_id.to_string()))
    }
}

fn touch_session(conn: &Connection, session_id: &str, now: i64) -> Result<()> {
    let changed = conn.execute(
        "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
        params![session_id, now],
    )?;
    if changed == 0 {
        Err(StoreError::UnknownSession(session_id.to_string()))
    } else {
        Ok(())
    }
}

/// Shared row logic for the append paths ([`SqliteStore::append_messages_with_meta`]
/// and [`TxOps::append_messages_with_meta`]) — one implementation, two
/// transaction owners.
fn append_rows(
    conn: &rusqlite::Connection,
    id: &str,
    entries: &[(&Message, Option<&str>)],
    expected_next_seq: Option<u64>,
) -> Result<u64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::UnknownSession(id.to_string()));
    }
    let mut seq: u64 = conn.query_row(
        "SELECT COALESCE(MAX(seq) + 1, 0) FROM messages WHERE session_id = ?1",
        params![id],
        |row| row.get::<_, i64>(0).map(|v| v as u64),
    )?;
    if let Some(expected) = expected_next_seq
        && expected != seq
    {
        return Err(StoreError::SeqConflict {
            session: id.to_string(),
            expected,
            actual: seq,
        });
    }
    // An empty append is a no-op: it must not touch `updated_at` (recents
    // ordering) and must not emit a mutation (the listener doctrine —
    // "a no-op fires nothing").
    if entries.is_empty() {
        return Ok(seq);
    }
    let now = now_ms();
    for (message, meta) in entries {
        conn.execute(
            "INSERT INTO messages (session_id, seq, role, content, cache, created_at, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                seq as i64,
                role_str(message.role),
                serde_json::to_string(&message.content)?,
                cache_column(&message.cache),
                now,
                meta
            ],
        )?;
        seq += 1;
    }
    conn.execute(
        "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(seq)
}

fn truncate_rows(conn: &rusqlite::Connection, id: &str, from_seq: u64) -> Result<u64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::UnknownSession(id.to_string()));
    }
    let deleted = conn.execute(
        "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2",
        params![id, from_seq as i64],
    )? as u64;
    if deleted > 0 {
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
    }
    Ok(deleted)
}

fn load_rows(conn: &rusqlite::Connection, id: &str) -> Result<Vec<(Message, Option<String>)>> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
        params![id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(StoreError::UnknownSession(id.to_string()));
    }
    let mut stmt = conn.prepare(
        "SELECT role, content, cache, meta FROM messages WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, rusqlite::types::Value>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut messages = Vec::new();
    for row in rows {
        let (role, content, cache, meta) = row?;
        messages.push((
            Message {
                role: parse_role(&role),
                content: serde_json::from_str(&content)?,
                cache: parse_cache_column(cache)?,
            },
            meta,
        ));
    }
    Ok(messages)
}

/// The `cache` column is dynamically typed for wire-compatible widening:
/// legacy rows hold the historical bool (0/1); a TTL-pinned mark stores the
/// TTL's wire string (e.g. "1h") so a pinned breakpoint survives a store
/// round-trip instead of degrading to a plain mark.
fn cache_column(mark: &ac_types::CacheMark) -> rusqlite::types::Value {
    match mark {
        ac_types::CacheMark::Off => rusqlite::types::Value::Integer(0),
        ac_types::CacheMark::On => rusqlite::types::Value::Integer(1),
        ac_types::CacheMark::WithTtl(ttl) => rusqlite::types::Value::Text(ttl.as_str().to_string()),
    }
}

/// Inverse of [`cache_column`]. A text value routes through `CacheMark`'s own
/// wire deserialization, so a TTL string written by a newer kit that this
/// build does not model fails the load as a typed per-session error — never a
/// silent mis-read.
fn parse_cache_column(value: rusqlite::types::Value) -> Result<ac_types::CacheMark> {
    match value {
        rusqlite::types::Value::Integer(i) => Ok(ac_types::CacheMark::from(i != 0)),
        rusqlite::types::Value::Text(s) => {
            Ok(serde_json::from_value(serde_json::Value::String(s))?)
        }
        // Null/Real/Blob were never written by any version of this store: fail
        // the load typed rather than guess (from_value(Null) is a Serde error).
        _ => Ok(serde_json::from_value(serde_json::Value::Null)?),
    }
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn parse_role(role: &str) -> Role {
    match role {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        _ => Role::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_types::ContentPart;

    fn msg(role: Role, text: &str) -> Message {
        Message::text(role, text)
    }

    #[test]
    fn session_crud_and_recents_order() {
        let store = SqliteStore::open_in_memory().unwrap();
        let a = store.create_session(Some("first")).unwrap();
        let b = store.create_session(None).unwrap();
        assert_ne!(a.id, b.id);

        store.rename_session(&b.id, "second").unwrap();
        let listed = store.list_sessions(10).unwrap();
        assert_eq!(listed.len(), 2);
        // b was touched last → first in recents.
        assert_eq!(listed[0].id, b.id);
        assert_eq!(listed[0].title.as_deref(), Some("second"));

        assert!(store.get_session(&a.id).unwrap().is_some());
        assert!(store.get_session("nope").unwrap().is_none());
        assert!(store.delete_session(&a.id).unwrap());
        assert!(!store.delete_session(&a.id).unwrap());
        assert_eq!(store.list_sessions(10).unwrap().len(), 1);
    }

    #[test]
    fn meta_is_stored_verbatim_and_never_interpreted() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let meta = serde_json::json!({ "host": { "workdir": "/x", "mode": "design" } });
        store.set_meta(&s.id, &meta).unwrap();
        let got = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(got.meta.unwrap(), meta);
    }

    #[test]
    fn message_log_round_trips_in_seq_order() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();

        let next = store
            .append_messages(
                &s.id,
                &[msg(Role::User, "hi"), msg(Role::Assistant, "hello")],
                None,
            )
            .unwrap();
        assert_eq!(next, 2);
        let next = store
            .append_messages(&s.id, &[msg(Role::User, "again")], None)
            .unwrap();
        assert_eq!(next, 3);

        let loaded = store.load_messages(&s.id).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].role, Role::User);
        assert_eq!(loaded[1].role, Role::Assistant);
        assert!(matches!(&loaded[2].content[0], ContentPart::Text { text } if text == "again"));
        assert_eq!(store.message_count(&s.id).unwrap(), 3);
    }

    #[test]
    fn structured_content_survives_the_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();

        let assistant = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Text {
                    text: "on it".into(),
                },
                ContentPart::ToolUse(ac_types::ToolUse {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({ "path": "a.txt" }),
                }),
            ],
            cache: ac_types::CacheMark::Off,
        };
        let tool_result = Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ac_types::ToolResult {
                tool_use_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            })],
            cache: ac_types::CacheMark::Off,
        };
        store
            .append_messages(&s.id, &[assistant, tool_result], None)
            .unwrap();

        let loaded = store.load_messages(&s.id).unwrap();
        assert!(
            matches!(&loaded[0].content[1], ContentPart::ToolUse(tu) if tu.id == "c1" && tu.input["path"] == "a.txt")
        );
        assert!(
            matches!(&loaded[1].content[0], ContentPart::ToolResult(tr) if tr.tool_use_id == "c1" && !tr.is_error)
        );
    }

    #[test]
    fn unknown_session_is_an_error_not_a_silent_noop() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(matches!(
            store.append_messages("nope", &[msg(Role::User, "x")], None),
            Err(StoreError::UnknownSession(_))
        ));
        assert!(matches!(
            store.load_messages("nope"),
            Err(StoreError::UnknownSession(_))
        ));
        assert!(matches!(
            store.rename_session("nope", "t"),
            Err(StoreError::UnknownSession(_))
        ));
    }

    #[test]
    fn stale_writer_gets_a_seq_conflict_not_a_silent_fork() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "a")], Some(0))
            .unwrap();
        // Another writer advanced the log…
        store
            .append_messages(&s.id, &[msg(Role::Assistant, "b")], None)
            .unwrap();
        // …so the stale writer's expectation (1) no longer holds.
        let err = store
            .append_messages(&s.id, &[msg(Role::User, "c")], Some(1))
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::SeqConflict {
                    expected: 1,
                    actual: 2,
                    ..
                }
            ),
            "got: {err}"
        );
        // Nothing was written by the failed append.
        assert_eq!(store.message_count(&s.id).unwrap(), 2);
    }

    #[test]
    fn create_with_id_is_idempotent_and_adopts_the_caller_id() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(
            store
                .create_session_with_id("chat-abc", Some("hi"))
                .unwrap()
        );
        // Second call is a no-op — the row is kept, not replaced.
        assert!(
            !store
                .create_session_with_id("chat-abc", Some("other"))
                .unwrap()
        );
        let record = store.get_session("chat-abc").unwrap().unwrap();
        assert_eq!(record.id, "chat-abc");
        assert_eq!(record.title.as_deref(), Some("hi"));
        // The adopted id works as a normal session for the message log.
        store
            .append_messages("chat-abc", &[msg(Role::User, "yo")], Some(0))
            .unwrap();
        assert_eq!(store.message_count("chat-abc").unwrap(), 1);
    }

    #[test]
    fn cache_marks_survive_the_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let mut marked = msg(Role::User, "pinned");
        marked.cache = ac_types::CacheMark::On;
        store
            .append_messages(&s.id, &[marked, msg(Role::Assistant, "ok")], None)
            .unwrap();
        let loaded = store.load_messages(&s.id).unwrap();
        assert!(loaded[0].cache.is_on());
        assert!(loaded[1].cache.is_off());
    }

    #[test]
    fn a_ttl_pinned_mark_survives_the_round_trip() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let mut pinned = msg(Role::User, "pinned long");
        pinned.cache = ac_types::CacheMark::WithTtl(ac_types::CacheTtl::OneHour);
        store.append_messages(&s.id, &[pinned], None).unwrap();
        // The pin must not degrade to a plain mark across the store: a host
        // resuming a session would silently lose its long-TTL breakpoint.
        let loaded = store.load_messages(&s.id).unwrap();
        assert_eq!(
            loaded[0].cache,
            ac_types::CacheMark::WithTtl(ac_types::CacheTtl::OneHour)
        );
    }

    #[test]
    fn an_empty_append_is_a_true_no_op() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "hi")], None)
            .unwrap();
        let before = store.get_session(&s.id).unwrap().unwrap().updated_at_ms;

        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));
        // Nothing written: no mutation event, no recents bump, seq unchanged.
        let seq = store.append_messages(&s.id, &[], None).unwrap();
        assert_eq!(seq, 1);
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().updated_at_ms,
            before
        );
    }

    #[test]
    fn per_message_meta_rides_the_same_transaction() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let m1 = msg(Role::User, "hi");
        let m2 = msg(Role::Assistant, "ok");
        store
            .append_messages_with_meta(&s.id, &[(&m1, Some(r#"{"id":"m_1"}"#)), (&m2, None)], None)
            .unwrap();
        let loaded = store.load_messages_with_meta(&s.id).unwrap();
        assert_eq!(loaded[0].1.as_deref(), Some(r#"{"id":"m_1"}"#));
        assert_eq!(loaded[1].1, None);
        // The meta-blind loaders keep working over the same rows.
        assert_eq!(store.load_messages(&s.id).unwrap().len(), 2);
    }

    #[test]
    fn update_meta_serializes_concurrent_writers() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let s = store.create_session(None).unwrap();
        store
            .update_meta(&s.id, |_| (Some(Some(r#"{"n":0}"#.to_string())), ()))
            .unwrap();
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let store = store.clone();
                let id = s.id.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        store
                            .update_meta(&id, |current| {
                                let mut v: serde_json::Value =
                                    serde_json::from_str(current.unwrap()).unwrap();
                                v["n"] = (v["n"].as_i64().unwrap() + 1).into();
                                (Some(Some(v.to_string())), ())
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        // Two racing writers, one hundred increments, zero lost updates and
        // zero conflicts — the property the CAS-retry shape cannot give.
        let meta = store.get_session(&s.id).unwrap().unwrap().meta.unwrap();
        assert_eq!(meta["n"], 100);
    }

    #[test]
    fn a_declined_meta_update_is_a_true_no_op() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let before = store.get_session(&s.id).unwrap().unwrap().updated_at_ms;
        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));
        let seen = store
            .update_meta(&s.id, |current| (None, current.is_none()))
            .unwrap();
        assert!(seen);
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().updated_at_ms,
            before
        );
    }

    #[test]
    fn restored_timestamps_are_written_verbatim_and_reorder_recents() {
        let store = SqliteStore::open_in_memory().unwrap();
        let old = store.create_session(Some("imported")).unwrap();
        let fresh = store.create_session(Some("today")).unwrap();
        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));

        assert!(
            store
                .set_session_timestamps(&old.id, 1_600_000_000_000, 1_600_000_500_000)
                .unwrap()
        );
        let got = store.get_session(&old.id).unwrap().unwrap();
        assert_eq!(got.created_at_ms, 1_600_000_000_000);
        assert_eq!(got.updated_at_ms, 1_600_000_500_000);
        // The point of the escape hatch: history sorts as history.
        assert_eq!(store.list_sessions(10).unwrap()[0].id, fresh.id);
        assert_eq!(
            *events.lock().unwrap(),
            [StoreMutation::SessionTimestampsSet { id: old.id.clone() }]
        );
    }

    #[test]
    fn restoring_an_unknown_session_is_false_not_an_error() {
        let store = SqliteStore::open_in_memory().unwrap();
        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));
        assert!(!store.set_session_timestamps("nope", 1, 2).unwrap());
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn a_later_append_still_bumps_a_restored_updated_at() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .set_session_timestamps(&s.id, 1_600_000_000_000, 1_600_000_500_000)
            .unwrap();

        store
            .append_messages(&s.id, &[msg(Role::User, "resumed")], None)
            .unwrap();

        // A restore, not a freeze: the row rejoins the live clock on the
        // next write, and only `updated_at` moves.
        let got = store.get_session(&s.id).unwrap().unwrap();
        assert_eq!(got.created_at_ms, 1_600_000_000_000);
        assert!(got.updated_at_ms > 1_600_000_500_000);
    }

    #[test]
    fn an_imported_session_lands_in_one_transaction() {
        let store = SqliteStore::open_in_memory().unwrap();
        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));

        let m = msg(Role::User, "from the old log");
        store
            .atomic(|tx| {
                assert!(tx.create_session_with_id("old", Some("t"))?);
                tx.append_messages_with_meta("old", &[(&m, None)], Some(0))?;
                assert!(tx.set_session_timestamps("old", 1_600_000_000_000, 1_600_000_500_000)?);
                assert!(!tx.set_session_timestamps("absent", 1, 2)?);
                Ok(())
            })
            .unwrap();

        let got = store.get_session("old").unwrap().unwrap();
        assert_eq!(got.created_at_ms, 1_600_000_000_000);
        // Restored last, so the append's bump does not survive it.
        assert_eq!(got.updated_at_ms, 1_600_000_500_000);
        let kinds: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                format!("{m:?}")
                    .split('{')
                    .next()
                    .unwrap()
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            ["SessionCreated", "MessagesAppended", "SessionTimestampsSet"]
        );
    }

    #[test]
    fn atomic_composes_all_or_nothing_with_post_commit_events() {
        let store = SqliteStore::open_in_memory().unwrap();
        let events: Arc<Mutex<Vec<StoreMutation>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));

        // Failure: everything in the closure rolls back, nothing is emitted.
        let m = msg(Role::User, "orphan");
        let err: Result<()> = store.atomic(|tx| {
            tx.create_session_with_id("branch", Some("b"))?;
            tx.append_messages_with_meta("branch", &[(&m, None)], None)?;
            Err(StoreError::UnknownSession("boom".into()))
        });
        assert!(err.is_err());
        assert!(store.get_session("branch").unwrap().is_none());
        assert!(events.lock().unwrap().is_empty());

        // Success: one commit, events after it, in op order.
        let m2 = msg(Role::Assistant, "copied");
        store
            .atomic(|tx| {
                tx.create_session_with_id("branch", Some("b"))?;
                tx.set_meta_raw("branch", Some(r#"{"lineage":"src"}"#))?;
                tx.append_messages_with_meta("branch", &[(&m2, Some("host"))], Some(0))?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            store
                .load_messages_with_meta("branch")
                .unwrap()
                .first()
                .unwrap()
                .1
                .as_deref(),
            Some("host")
        );
        let kinds: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                format!("{m:?}")
                    .split('{')
                    .next()
                    .unwrap()
                    .trim()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, ["SessionCreated", "MetaSet", "MessagesAppended"]);
    }

    #[test]
    fn a_v1_store_gains_the_meta_column_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            // A genuine v1 store: the old table shape, stamped 1, with data.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT, meta TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE TABLE messages (session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, seq INTEGER NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, cache INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL, PRIMARY KEY (session_id, seq));
                 INSERT INTO sessions VALUES ('s1', NULL, NULL, 1, 1);
                 INSERT INTO messages VALUES ('s1', 0, 'user', '[{\"type\":\"text\",\"text\":\"old\"}]', 1, 1);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let loaded = store.load_messages_with_meta("s1").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, None);
        let m = msg(Role::Assistant, "new");
        store
            .append_messages_with_meta("s1", &[(&m, Some("host"))], Some(1))
            .unwrap();
        assert_eq!(
            store.load_messages_with_meta("s1").unwrap()[1].1.as_deref(),
            Some("host")
        );
    }

    #[test]
    fn delete_cascades_to_messages() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "hi")], None)
            .unwrap();
        assert!(store.delete_session(&s.id).unwrap());
        // The log is gone with the session (foreign_keys=ON cascade).
        assert!(matches!(
            store.load_messages(&s.id),
            Err(StoreError::UnknownSession(_))
        ));
    }

    #[test]
    fn meta_cas_swaps_from_absent_and_from_a_value() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .set_meta_cas(&s.id, None, Some(r#"{"lock":"a"}"#))
            .unwrap();
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().meta.unwrap(),
            serde_json::json!({ "lock": "a" })
        );
        store
            .set_meta_cas(&s.id, Some(r#"{"lock":"a"}"#), Some(r#"{"lock":"b"}"#))
            .unwrap();
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().meta.unwrap(),
            serde_json::json!({ "lock": "b" })
        );
        // Swap back to absent — the release half of a lock protocol.
        store
            .set_meta_cas(&s.id, Some(r#"{"lock":"b"}"#), None)
            .unwrap();
        assert!(store.get_session(&s.id).unwrap().unwrap().meta.is_none());
    }

    #[test]
    fn meta_cas_conflict_carries_the_current_value() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .set_meta_cas(&s.id, None, Some(r#"{"lock":"winner"}"#))
            .unwrap();
        match store
            .set_meta_cas(&s.id, None, Some(r#"{"lock":"loser"}"#))
            .unwrap_err()
        {
            StoreError::MetaConflict { session, current } => {
                assert_eq!(session, s.id);
                assert_eq!(current.as_deref(), Some(r#"{"lock":"winner"}"#));
            }
            other => panic!("got: {other}"),
        }
        // A stale expectation of a value conflicts too — the NULL-safe
        // compare covers both directions.
        assert!(matches!(
            store.set_meta_cas(&s.id, Some(r#"{"lock":"stale"}"#), None),
            Err(StoreError::MetaConflict { .. })
        ));
    }

    #[test]
    fn meta_cas_unknown_session_is_typed_not_a_conflict() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(matches!(
            store.set_meta_cas("nope", None, Some("{}")),
            Err(StoreError::UnknownSession(_))
        ));
    }

    #[test]
    fn meta_cas_race_has_exactly_one_winner() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        let barrier = std::sync::Barrier::new(2);
        let contenders = ["a", "b"];
        let outcomes: Vec<Result<()>> = std::thread::scope(|scope| {
            let handles: Vec<_> = contenders
                .iter()
                .map(|who| {
                    let (store, id, barrier) = (&store, &s.id, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        store.set_meta_cas(id, None, Some(&format!(r#"{{"lock":"{who}"}}"#)))
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            outcomes.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one CAS must win: {outcomes:?}"
        );
        // The loser's conflict carries the winner's committed value — enough
        // to drive a lock/retry loop without a second query.
        let winner = contenders[outcomes.iter().position(|r| r.is_ok()).unwrap()];
        match outcomes
            .into_iter()
            .find(Result::is_err)
            .unwrap()
            .unwrap_err()
        {
            StoreError::MetaConflict { current, .. } => {
                assert_eq!(current, Some(format!(r#"{{"lock":"{winner}"}}"#)));
            }
            other => panic!("got: {other}"),
        }
    }

    #[test]
    fn truncate_rewinds_the_log_and_next_seq_follows_the_table() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .append_messages(
                &s.id,
                &[
                    msg(Role::User, "keep"),
                    msg(Role::Assistant, "cut"),
                    msg(Role::User, "cut too"),
                ],
                Some(0),
            )
            .unwrap();
        assert_eq!(store.truncate_messages_from(&s.id, 1).unwrap(), 2);
        assert_eq!(store.message_count(&s.id).unwrap(), 1);
        // next_seq derives from the surviving table, not a counter: the
        // freed positions are immediately reusable under the seq-CAS.
        let next = store
            .append_messages(&s.id, &[msg(Role::Assistant, "rewritten")], Some(1))
            .unwrap();
        assert_eq!(next, 2);
        let loaded = store.load_messages(&s.id).unwrap();
        assert!(matches!(&loaded[0].content[0], ContentPart::Text { text } if text == "keep"));
        assert!(matches!(&loaded[1].content[0], ContentPart::Text { text } if text == "rewritten"));
        // Truncating from 0 empties the log; the series restarts at 0.
        assert_eq!(store.truncate_messages_from(&s.id, 0).unwrap(), 2);
        assert_eq!(
            store
                .append_messages(&s.id, &[msg(Role::User, "fresh")], Some(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn truncate_past_the_end_is_ok_zero_and_unknown_session_is_typed() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "a")], None)
            .unwrap();
        assert_eq!(store.truncate_messages_from(&s.id, 5).unwrap(), 0);
        assert_eq!(store.message_count(&s.id).unwrap(), 1);
        assert!(matches!(
            store.truncate_messages_from("nope", 0),
            Err(StoreError::UnknownSession(_))
        ));
    }

    #[test]
    fn every_mutation_kind_reaches_the_listener() {
        let store = SqliteStore::open_in_memory().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));

        let s = store.create_session(None).unwrap();
        store.create_session_with_id("adopted", None).unwrap();
        store.rename_session(&s.id, "titled").unwrap();
        store
            .set_meta(&s.id, &serde_json::json!({ "k": 1 }))
            .unwrap();
        store.set_meta_cas(&s.id, Some(r#"{"k":1}"#), None).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "a"), msg(Role::User, "b")], None)
            .unwrap();
        store.truncate_messages_from(&s.id, 1).unwrap();
        store.delete_session(&s.id).unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![
                StoreMutation::SessionCreated { id: s.id.clone() },
                StoreMutation::SessionCreated {
                    id: "adopted".into()
                },
                StoreMutation::SessionRenamed { id: s.id.clone() },
                StoreMutation::MetaSet { id: s.id.clone() },
                StoreMutation::MetaSet { id: s.id.clone() },
                StoreMutation::MessagesAppended {
                    id: s.id.clone(),
                    count: 2,
                    next_seq: 2
                },
                StoreMutation::MessagesTruncated {
                    id: s.id.clone(),
                    deleted: 1
                },
                StoreMutation::SessionDeleted { id: s.id.clone() },
            ]
        );
    }

    #[test]
    fn failed_conflicted_and_no_op_writes_fire_nothing() {
        let store = SqliteStore::open_in_memory().unwrap();
        let s = store.create_session(None).unwrap();
        store.set_meta_cas(&s.id, None, Some(r#""held""#)).unwrap();
        store
            .append_messages(&s.id, &[msg(Role::User, "a")], None)
            .unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            sink.lock().unwrap().push(m.clone());
        })));

        assert!(store.set_meta_cas(&s.id, None, Some("{}")).is_err());
        assert!(
            store
                .append_messages(&s.id, &[msg(Role::User, "x")], Some(0))
                .is_err()
        );
        assert!(store.rename_session("nope", "t").is_err());
        assert!(store.truncate_messages_from("nope", 0).is_err());
        assert!(!store.delete_session("nope").unwrap());
        assert!(!store.create_session_with_id(&s.id, None).unwrap());
        assert_eq!(store.truncate_messages_from(&s.id, 9).unwrap(), 0);

        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_reentrant_listener_does_not_deadlock() {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let s = store.create_session(None).unwrap();
        let weak = Arc::downgrade(&store);
        let observed = Arc::new(Mutex::new(None));
        let obs = observed.clone();
        store.set_mutation_listener(Some(Arc::new(move |m: &StoreMutation| {
            if let StoreMutation::MessagesAppended { id, .. } = m {
                let store = weak.upgrade().unwrap();
                // A read (store lock) and a mutation (which re-enters emit,
                // relocking the listener slot) — both from inside the
                // listener. Neither may deadlock.
                *obs.lock().unwrap() = Some(store.message_count(id).unwrap());
                store
                    .rename_session(id, "renamed from the listener")
                    .unwrap();
            }
        })));

        store
            .append_messages(&s.id, &[msg(Role::User, "hi")], None)
            .unwrap();
        assert_eq!(*observed.lock().unwrap(), Some(1));
        assert_eq!(
            store.get_session(&s.id).unwrap().unwrap().title.as_deref(),
            Some("renamed from the listener")
        );
    }
}
