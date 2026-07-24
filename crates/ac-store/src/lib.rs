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
  PRIMARY KEY (session_id, seq)
);
";

/// Bumped when the on-disk schema changes shape. Opening a store stamped
/// higher fails with [`StoreError::FutureSchema`].
const SCHEMA_VERSION: u32 = 1;

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
        conn.execute_batch(SCHEMA)?;
        if found < SCHEMA_VERSION {
            // Fresh store, or one from before versioning existed (tables
            // present, user_version 0) — both take the current stamp; the
            // upgrade is idempotent.
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            listener: Mutex::new(None),
        })
    }

    /// Installs (or clears) the mutation listener. One listener at a time;
    /// a host that needs fan-out multiplexes behind its own closure. The
    /// listener runs after the write commits and outside the store's
    /// internal lock, so it may reentrantly call back into the store; under
    /// concurrent writers, delivery order may differ from commit order.
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
            listener(&mutation);
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
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        let mut seq: u64 = tx.query_row(
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
        if messages.is_empty() {
            return Ok(seq);
        }
        let now = now_ms();
        for message in messages {
            tx.execute(
                "INSERT INTO messages (session_id, seq, role, content, cache, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    id,
                    seq as i64,
                    role_str(message.role),
                    serde_json::to_string(&message.content)?,
                    cache_column(&message.cache),
                    now
                ],
            )?;
            seq += 1;
        }
        tx.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        tx.commit()?;
        drop(conn);
        self.emit(StoreMutation::MessagesAppended {
            id: id.to_string(),
            count: messages.len() as u64,
            next_seq: seq,
        });
        Ok(seq)
    }

    /// Deletes every message with `seq >= from_seq`, transactionally, and
    /// returns how many were deleted (0 is not an error). The next append
    /// continues from the table's surviving maximum — seq derives from the
    /// log, not a counter — so `expected_next_seq == from_seq` succeeds
    /// after a truncation at `from_seq`.
    pub fn truncate_messages_from(&self, id: &str, from_seq: u64) -> Result<u64> {
        let mut conn = self.conn.lock().expect("store lock poisoned");
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        let deleted = tx.execute(
            "DELETE FROM messages WHERE session_id = ?1 AND seq >= ?2",
            params![id, from_seq as i64],
        )? as u64;
        if deleted > 0 {
            tx.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![id, now_ms()],
            )?;
        }
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

    /// The full message log in seq order — feed it to `Session::resume`.
    pub fn load_messages(&self, id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().expect("store lock poisoned");
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
            params![id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(StoreError::UnknownSession(id.to_string()));
        }
        let mut stmt = conn.prepare(
            "SELECT role, content, cache FROM messages WHERE session_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, rusqlite::types::Value>(2)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (role, content, cache) = row?;
            messages.push(Message {
                role: parse_role(&role),
                content: serde_json::from_str(&content)?,
                cache: parse_cache_column(cache)?,
            });
        }
        Ok(messages)
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
