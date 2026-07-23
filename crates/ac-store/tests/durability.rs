//! The store half of the durability proof harness (docs/ac-durability.md §6):
//! the kill-and-reopen append storm, the version-skew probes, corruption
//! reported at the door, and typed append failure. These run over real
//! file-backed stores and real killed processes — the crash windows of §5,
//! not simulations of them.

use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ac_store::{SqliteStore, StoreError};
use ac_types::{Message, Role};

/// When set, this test binary is the storm child: the value is the DB path
/// to append to until killed.
const STORM_CHILD_ENV: &str = "AC_STORE_STORM_CHILD";

const STORM_SESSIONS: [&str; 3] = ["storm-a", "storm-b", "storm-c"];

/// `Result::unwrap_err` needs `T: Debug`, which `SqliteStore` does not have.
fn open_err(path: &std::path::Path) -> StoreError {
    match SqliteStore::open(path) {
        Ok(_) => panic!("open unexpectedly succeeded"),
        Err(e) => e,
    }
}

fn nano_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(1)
        | 1
}

/// The child half of the storm: append small batches forever until the
/// parent SIGKILLs the process mid-write.
fn storm_child(db_path: &str) -> ! {
    let store = SqliteStore::open(db_path).expect("storm child: open");
    for id in STORM_SESSIONS {
        store
            .create_session_with_id(id, None)
            .expect("storm child: session");
    }
    let mut n = nano_seed();
    loop {
        // LCG — cheap variety in batch size/target without a rand dep.
        n = n
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let session = STORM_SESSIONS[(n >> 33) as usize % STORM_SESSIONS.len()];
        let batch_len = 1 + (n >> 17) as usize % 3;
        let batch: Vec<Message> = (0..batch_len)
            .map(|i| Message::text(Role::User, format!("storm {n} {i}")))
            .collect();
        // One batch = one transaction: the kill can land inside the commit,
        // and the reopen must see all of the batch or none of it (I2).
        store
            .append_messages(session, &batch, None)
            .expect("storm child: append");
    }
}

/// §6 kill-and-reopen append storm, proving 5.2 (kill mid-append) and 5.10
/// (crash loop / fixed-point recovery, I4): a real child process appends in
/// a loop, the parent SIGKILLs it at a randomized instant, then reopens the
/// SAME store file and asserts it is fully intact — repeatedly, so each
/// iteration also proves recovery of the previous iteration's survivor.
///
/// The child is this very test re-invoked with [`STORM_CHILD_ENV`] set —
/// the current_exe re-entry pattern, so the storm needs no helper binary.
#[test]
fn kill_and_reopen_append_storm() {
    if let Ok(db_path) = std::env::var(STORM_CHILD_ENV) {
        storm_child(&db_path);
    }

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("storm.db");

    for iteration in 0..15u32 {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("kill_and_reopen_append_storm")
            .arg("--exact")
            .arg("--nocapture")
            .env(STORM_CHILD_ENV, &db_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn storm child");
        std::thread::sleep(Duration::from_millis(20 + nano_seed() % 101));
        // std's kill is SIGKILL on unix — no atexit, no destructors, a
        // genuine mid-write cut.
        child.kill().expect("kill storm child");
        let status = child.wait().expect("reap storm child");
        assert!(
            status.code().is_none(),
            "iteration {iteration}: storm child exited on its own ({status}) — \
             it must run until killed"
        );

        // Reopen the survivor. open() runs quick_check at the door, so a
        // plain Ok already proves the store passes its integrity probe.
        let store = SqliteStore::open(&db_path)
            .unwrap_or_else(|e| panic!("iteration {iteration}: reopen failed: {e}"));
        for record in store.list_sessions(64).unwrap() {
            let loaded = store
                .load_messages(&record.id)
                .unwrap_or_else(|e| panic!("iteration {iteration}: load {}: {e}", record.id));
            assert_eq!(
                loaded.len() as u64,
                store.message_count(&record.id).unwrap(),
                "iteration {iteration}: session {} count drift",
                record.id
            );
        }
        drop(store);

        // Seq contiguity 0..n needs a raw probe — the store orders by seq
        // but never exposes it. MIN=0 and MAX=count-1 under the (session,
        // seq) primary key imply the range is gapless.
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = raw
            .prepare(
                "SELECT session_id, COUNT(*), MIN(seq), MAX(seq)
                 FROM messages GROUP BY session_id",
            )
            .unwrap();
        let rows: Vec<(String, i64, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (session, count, min, max) in rows {
            assert_eq!(min, 0, "iteration {iteration}: session {session} seq floor");
            assert_eq!(
                max,
                count - 1,
                "iteration {iteration}: session {session} seq not contiguous"
            );
        }
    }

    // The storm must have actually landed writes, or the kill window was
    // never exercised and the test proved nothing.
    let store = SqliteStore::open(&db_path).unwrap();
    let total: u64 = store
        .list_sessions(64)
        .unwrap()
        .iter()
        .map(|r| store.message_count(&r.id).unwrap())
        .sum();
    assert!(total > 0, "storm never landed a single append");
}

/// §6 version-skew probe, store side (5.8): a store stamped by a newer
/// schema refuses to open with a typed error — old code never mis-reads
/// new data.
#[test]
fn future_schema_store_refuses_to_open_typed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future.db");
    drop(SqliteStore::open(&db_path).unwrap());

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.pragma_update(None, "user_version", 99).unwrap();
    drop(raw);

    let err = open_err(&db_path);
    assert!(
        matches!(
            err,
            StoreError::FutureSchema {
                found: 99,
                supported: 1
            }
        ),
        "got: {err}"
    );
}

/// user_version 0 with tables present is a store from before versioning
/// existed: open upgrades it in place, idempotently, losing nothing.
#[test]
fn pre_versioning_store_is_stamped_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("legacy.db");
    {
        let store = SqliteStore::open(&db_path).unwrap();
        let s = store.create_session(Some("old")).unwrap();
        store
            .append_messages(&s.id, &[Message::text(Role::User, "kept")], None)
            .unwrap();
    }
    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.pragma_update(None, "user_version", 0).unwrap();
    drop(raw);

    let store = SqliteStore::open(&db_path).unwrap();
    let sessions = store.list_sessions(10).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(store.load_messages(&sessions[0].id).unwrap().len(), 1);
    drop(store);

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    let stamped: u32 = raw
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(stamped, 1);
}

/// §4 self-check: mid-file damage surfaces as a typed error at open — at
/// the door, never as silent data loss discovered mid-session (I5, R5).
#[test]
fn corrupt_store_reports_at_the_door() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("corrupt.db");
    {
        let store = SqliteStore::open(&db_path).unwrap();
        let s = store.create_session(None).unwrap();
        // A few KB of content so the damage below lands in used b-tree
        // pages, not unallocated slack quick_check would never visit.
        let filler = "x".repeat(200);
        for i in 0..64 {
            store
                .append_messages(
                    &s.id,
                    &[Message::text(Role::User, format!("padding {i} {filler}"))],
                    None,
                )
                .unwrap();
        }
        // Dropping the last connection checkpoints the WAL into the main
        // file, so the bytes we are about to damage are the real pages.
    }

    let len = std::fs::metadata(&db_path).unwrap().len();
    assert!(
        len > 8192,
        "store too small to corrupt past page 1 ({len}B)"
    );
    use std::io::{Seek, SeekFrom, Write};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&db_path)
        .unwrap();
    // Mid-file, past page 1: the 16-byte magic and the header stay intact,
    // making this a corruption, not a not-a-database.
    file.seek(SeekFrom::Start((len / 2).max(4096))).unwrap();
    file.write_all(&[0xAA; 2048]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let err = open_err(&db_path);
    assert!(matches!(err, StoreError::Corrupt(_)), "got: {err}");
}

/// §4 content-versioning probe (5.8): a message row written by a newer kit
/// (an unknown content variant) fails THAT session's load with a typed
/// error — never silently skipped — and sibling sessions are untouched (I5).
#[test]
fn unknown_content_variant_fails_that_session_only() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("skew.db");
    {
        let store = SqliteStore::open(&db_path).unwrap();
        store.create_session_with_id("tainted", None).unwrap();
        store.create_session_with_id("clean", None).unwrap();
        store
            .append_messages("tainted", &[Message::text(Role::User, "before")], None)
            .unwrap();
        store
            .append_messages("clean", &[Message::text(Role::User, "fine")], None)
            .unwrap();
    }

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.execute(
        "INSERT INTO messages (session_id, seq, role, content, cache, created_at)
         VALUES ('tainted', 1, 'assistant', ?1, 0, 0)",
        [r#"[{"type":"from_the_future","data":"unrepresentable here"}]"#],
    )
    .unwrap();
    drop(raw);

    let store = SqliteStore::open(&db_path).unwrap();
    let err = store.load_messages("tainted").unwrap_err();
    assert!(matches!(err, StoreError::Serde(_)), "got: {err}");
    let clean = store.load_messages("clean").unwrap();
    assert_eq!(clean.len(), 1);
    // The store itself remains listable: unreadability is per-session.
    assert_eq!(store.list_sessions(10).unwrap().len(), 2);
}

/// §5.6-class probe: an append that cannot reach the disk surfaces as a
/// typed error — not a panic, not a silent Ok. The failure is induced by
/// dropping the messages table via a second connection: deterministic on
/// every platform, unlike chmod tricks (root ignores permission bits, and
/// directory-permission behavior varies by filesystem).
#[test]
fn append_failure_is_typed_not_silent() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("failing.db");
    let store = SqliteStore::open(&db_path).unwrap();
    let s = store.create_session(None).unwrap();
    store
        .append_messages(&s.id, &[Message::text(Role::User, "ok")], None)
        .unwrap();

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.execute_batch("DROP TABLE messages").unwrap();
    drop(raw);

    let err = store
        .append_messages(&s.id, &[Message::text(Role::User, "later")], None)
        .unwrap_err();
    assert!(matches!(err, StoreError::Sqlite(_)), "got: {err}");
}
