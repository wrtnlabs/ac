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
use std::sync::Mutex;
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

/// SQLite-backed session + message store. One global DB file per host (or
/// `open_in_memory` for tests). Internally serialized on one connection —
/// correct and plenty for a single-user local host.
pub struct SqliteStore {
    conn: Mutex<Connection>,
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
        })
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
        Ok(())
    }

    /// Deletes the session row and its message log — never anything outside
    /// the store (a host's files are not the store's to touch).
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store lock poisoned");
        Ok(conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])? > 0)
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
                    message.cache,
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
        Ok(seq)
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
                row.get::<_, bool>(2)?,
            ))
        })?;
        let mut messages = Vec::new();
        for row in rows {
            let (role, content, cache) = row?;
            messages.push(Message {
                role: parse_role(&role),
                content: serde_json::from_str(&content)?,
                cache,
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
            cache: false,
        };
        let tool_result = Message {
            role: Role::User,
            content: vec![ContentPart::ToolResult(ac_types::ToolResult {
                tool_use_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            })],
            cache: false,
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
        marked.cache = true;
        store
            .append_messages(&s.id, &[marked, msg(Role::Assistant, "ok")], None)
            .unwrap();
        let loaded = store.load_messages(&s.id).unwrap();
        assert!(loaded[0].cache);
        assert!(!loaded[1].cache);
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
}
