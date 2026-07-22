//! `ac-tools` — the hard built-in tools every host can hand the agent.
//!
//! Eight compiled-in [`ac_tool::Tool`] implementations covering the baseline an
//! agent needs to work over a workspace:
//!
//! - [`ReadFile`], [`WriteFile`], [`EditFile`], [`ListFiles`] — file I/O with a
//!   per-run read-before-write ledger.
//! - [`Glob`], [`Grep`] — filename and content search.
//! - [`Shell`] — run commands (cwd-contained; no OS sandbox in this phase).
//! - [`Fetch`] — HTTP(S) GET (the one network tool).
//!
//! Every path a tool touches is first run through the host-supplied
//! [`ac_tool::PathPolicy`] on the [`ac_tool::ToolCtx`]; these tools never act on
//! a raw user path and never assume where the workspace lives. There are no
//! host- or app-domain concepts here — the crate is usable by any host.
//!
//! Register the full set with [`register_builtins`], or register individual
//! structs to expose only a subset (e.g. drop [`Shell`]/[`Fetch`] for a
//! read-only, offline host).

mod fetch;
mod files;
mod search;
mod shell;
mod task;

pub use fetch::{Fetch, FetchInput};
pub use files::{
    EditFile, EditFileInput, ListFiles, ListFilesInput, ReadFile, ReadFileInput, WriteFile,
    WriteFileInput,
};
pub use search::{Glob, GlobInput, Grep, GrepInput};
pub use shell::{Shell, ShellInput};
pub use task::{Task, TaskInput};

use ac_tool::ToolRegistry;

/// Register all eight built-in tools into `registry`.
///
/// Hosts that want a narrower surface can skip this and register individual
/// tools (each is a `pub` struct) instead. [`Task`] is deliberately **not** here:
/// delegation is opt-in — a host registers it only on a parent run and leaves it
/// out of a child's surface ([docs/ac-subagents.md] §4, the recursion guard).
pub fn register_builtins(registry: &mut ToolRegistry) {
    registry.register(ReadFile);
    registry.register(WriteFile);
    registry.register(EditFile);
    registry.register(ListFiles);
    registry.register(Glob);
    registry.register(Grep);
    registry.register(Shell);
    registry.register(Fetch);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{SubtreePolicy, ToolCtx, ToolRegistry};
    use serde_json::json;
    use std::sync::Arc;

    fn ctx_in(dir: &std::path::Path) -> Arc<ToolCtx> {
        let policy = SubtreePolicy::new(dir).unwrap();
        Arc::new(ToolCtx::new(Arc::new(policy)))
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        register_builtins(&mut r);
        r
    }

    #[test]
    fn register_builtins_registers_all_eight() {
        let r = registry();
        for name in [
            "read_file",
            "write_file",
            "edit_file",
            "list_files",
            "glob",
            "grep",
            "shell",
            "fetch",
        ] {
            assert!(r.contains(name), "missing tool {name}");
        }
        assert_eq!(r.specs().len(), 8);
    }

    #[tokio::test]
    async fn read_then_write_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let read = r
            .run("read_file", json!({ "path": "a.txt" }), ctx.clone())
            .await;
        assert!(!read.is_error);
        assert_eq!(read.content, "hello");

        let write = r
            .run(
                "write_file",
                json!({ "path": "a.txt", "content": "world" }),
                ctx.clone(),
            )
            .await;
        assert!(!write.is_error, "{}", write.content);
        assert!(write.content.contains("wrote 5 bytes"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "world"
        );
    }

    #[tokio::test]
    async fn write_to_existing_unread_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let write = r
            .run(
                "write_file",
                json!({ "path": "a.txt", "content": "x" }),
                ctx,
            )
            .await;
        assert!(write.is_error);
        assert!(write.content.contains("must read_file"));
    }

    #[tokio::test]
    async fn write_new_file_needs_no_read() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let write = r
            .run(
                "write_file",
                json!({ "path": "sub/new.txt", "content": "fresh" }),
                ctx,
            )
            .await;
        assert!(!write.is_error, "{}", write.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/new.txt")).unwrap(),
            "fresh"
        );
    }

    #[tokio::test]
    async fn stale_write_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "hello").unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let read = r
            .run("read_file", json!({ "path": "a.txt" }), ctx.clone())
            .await;
        assert!(!read.is_error);

        // Change the file on disk after the read, forcing a distinct mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::write(&path, "changed").unwrap();
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(later).unwrap();
        drop(f);

        let write = r
            .run(
                "write_file",
                json!({ "path": "a.txt", "content": "y" }),
                ctx,
            )
            .await;
        assert!(write.is_error, "{}", write.content);
        assert!(write.content.contains("changed on disk"));
    }

    #[tokio::test]
    async fn edit_unique_replaces_and_nonunique_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one two one").unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        // Must read first.
        r.run("read_file", json!({ "path": "a.txt" }), ctx.clone())
            .await;

        // "two" is unique -> replaced.
        let ok = r
            .run(
                "edit_file",
                json!({ "path": "a.txt", "old_string": "two", "new_string": "2" }),
                ctx.clone(),
            )
            .await;
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "one 2 one"
        );

        // "one" occurs twice -> error.
        let dup = r
            .run(
                "edit_file",
                json!({ "path": "a.txt", "old_string": "one", "new_string": "1" }),
                ctx.clone(),
            )
            .await;
        assert!(dup.is_error);
        assert!(dup.content.contains("must be unique"));

        // "zzz" absent -> error.
        let none = r
            .run(
                "edit_file",
                json!({ "path": "a.txt", "old_string": "zzz", "new_string": "!" }),
                ctx,
            )
            .await;
        assert!(none.is_error);
        assert!(none.content.contains("not found"));
    }

    #[tokio::test]
    async fn list_glob_grep_find_seeded_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() { needle(); }").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "// nothing here").unwrap();
        std::fs::write(dir.path().join("README.md"), "docs").unwrap();
        std::fs::create_dir_all(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join(".hidden/secret.rs"), "needle").unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let list = r.run("list_files", json!({}), ctx.clone()).await;
        assert!(!list.is_error);
        assert!(list.content.contains("src/"));
        assert!(list.content.contains("README.md"));

        let glob = r
            .run("glob", json!({ "pattern": "**/*.rs" }), ctx.clone())
            .await;
        assert!(!glob.is_error, "{}", glob.content);
        assert!(glob.content.contains("src/main.rs"));
        assert!(glob.content.contains("src/lib.rs"));
        // dot-directory is skipped.
        assert!(!glob.content.contains(".hidden"));

        let grep = r
            .run("grep", json!({ "pattern": "needle" }), ctx.clone())
            .await;
        assert!(!grep.is_error, "{}", grep.content);
        assert!(grep.content.contains("src/main.rs:1:"));
        // dot-directory match is skipped.
        assert!(!grep.content.contains(".hidden"));
    }

    #[tokio::test]
    async fn shell_runs_echo() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let out = r.run("shell", json!({ "command": "echo hi" }), ctx).await;
        assert!(!out.is_error, "{}", out.content);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(v["exit_code"], 0);
        assert!(v["stdout_tail"].as_str().unwrap().contains("hi"));
    }

    #[tokio::test]
    async fn path_escaping_root_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let read = r
            .run("read_file", json!({ "path": "../escape.txt" }), ctx.clone())
            .await;
        assert!(read.is_error);

        let write = r
            .run(
                "write_file",
                json!({ "path": "../escape.txt", "content": "x" }),
                ctx,
            )
            .await;
        assert!(write.is_error);
    }

    #[tokio::test]
    async fn fetch_rejects_non_http_scheme() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        let out = r
            .run("fetch", json!({ "url": "file:///etc/hosts" }), ctx.clone())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("scheme"));

        let bad = r.run("fetch", json!({ "url": "not a url" }), ctx).await;
        assert!(bad.is_error);
    }

    /// Two edits to the SAME file, launched concurrently, must both land. The
    /// per-path lock serializes the read→modify→write so neither clobbers the
    /// other; without it, one replacement is lost.
    #[tokio::test]
    async fn concurrent_same_file_edits_do_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "AAA BBB").unwrap();
        let ctx = ctx_in(dir.path());
        let r = Arc::new(registry());

        // read-before-write satisfied once; both edits then race.
        r.run("read_file", json!({ "path": "a.txt" }), ctx.clone())
            .await;

        let e1 = {
            let (r, ctx) = (r.clone(), ctx.clone());
            tokio::spawn(async move {
                r.run(
                    "edit_file",
                    json!({ "path": "a.txt", "old_string": "AAA", "new_string": "aaa" }),
                    ctx,
                )
                .await
            })
        };
        let e2 = {
            let (r, ctx) = (r.clone(), ctx.clone());
            tokio::spawn(async move {
                r.run(
                    "edit_file",
                    json!({ "path": "a.txt", "old_string": "BBB", "new_string": "bbb" }),
                    ctx,
                )
                .await
            })
        };
        let (o1, o2) = (e1.await.unwrap(), e2.await.unwrap());
        assert!(!o1.is_error, "{}", o1.content);
        assert!(!o2.is_error, "{}", o2.content);

        // Both replacements survived — no lost update.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "aaa bbb"
        );
    }

    /// A command that backgrounds a long-lived child must not outlive the call:
    /// the whole process group is swept when the tool returns.
    #[cfg(unix)]
    #[tokio::test]
    async fn shell_reaps_backgrounded_children() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let r = registry();

        // Background a sleep and print its PID; sh exits immediately.
        let out = r
            .run(
                "shell",
                json!({ "command": "sleep 30 & echo $!" }),
                ctx.clone(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let v: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let pid: i32 = v["stdout_tail"]
            .as_str()
            .unwrap()
            .trim()
            .parse()
            .expect("a pid");

        // Give the group-kill a beat to take effect, then the pid must be gone
        // (kill -0 fails). Without the process-group sweep the sleep survives.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "backgrounded child {pid} should have been reaped");
    }
}
