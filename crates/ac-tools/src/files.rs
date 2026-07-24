//! Filesystem tools: `read_file`, `write_file`, `edit_file`, `list_files`.
//!
//! Every path first passes through the host [`PathPolicy`] (via `ctx.policy`);
//! these tools never touch a raw user path. `read_file` stamps the mtime it saw
//! into the per-run read-before-write ledger, and the write tools consult it.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput, WriteCheck, WriteObserver};
use futures::future::BoxFuture;
use serde::Deserialize;

/// Maximum bytes `read_file` returns; larger files are truncated with a note.
const READ_CAP: usize = 256 * 1024;

/// Render a resolved absolute path relative to the policy root for model-facing
/// output; falls back to the absolute path when it is not under the root.
fn rel(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(r) if r.as_os_str().is_empty() => ".".to_string(),
        Ok(r) => r.display().to_string(),
        Err(_) => p.display().to_string(),
    }
}

fn mtime_of(meta: &std::fs::Metadata) -> Option<SystemTime> {
    meta.modified().ok()
}

fn mtime_ms(t: SystemTime) -> Option<u64> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Snapshot seam (see [`WriteObserver`]): when a host installed an observer and
/// the target exists, hand it the prior bytes before they are replaced. An
/// observer `Err` — or failing to read the prior bytes at all — aborts the
/// write: an installed snapshot hook is a durability promise, and a write that
/// silently skipped it would lose the prior content.
async fn observe_overwrite(
    ctx: &ToolCtx,
    resolved: &Path,
    prior: Option<&[u8]>,
) -> Result<(), ToolOutput> {
    let Some(observer) = ctx.extensions.get::<Arc<dyn WriteObserver>>() else {
        return Ok(());
    };
    let read;
    let prior = match prior {
        Some(bytes) => bytes,
        None => {
            read = tokio::fs::read(resolved).await.map_err(|e| {
                ToolOutput::error(format!(
                    "cannot snapshot prior contents of {}: {e}",
                    resolved.display()
                ))
            })?;
            &read
        }
    };
    observer
        .before_overwrite(resolved, prior)
        .map_err(|reason| {
            ToolOutput::error(format!(
                "write aborted: pre-overwrite snapshot failed: {reason}"
            ))
        })
}

/// Read a UTF-8 text file within the workspace and return its contents.
///
/// The file is recorded in the read-before-write ledger, which later lets
/// `write_file` and `edit_file` overwrite it. Files larger than 256 KiB are
/// truncated (a note is appended). Reading a directory or a missing file is a
/// tool error, not a crash.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ReadFileInput {
    /// Path to the file to read, relative to the workspace root (or absolute
    /// inside it).
    pub path: String,
}

/// Reads a text file so the model can inspect it before editing.
pub struct ReadFile;

impl Tool for ReadFile {
    type Input = ReadFileInput;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> String {
        "Read a UTF-8 text file inside the workspace and return its contents. \
         Files over 256 KiB are truncated. Records the file so it can later be \
         overwritten with write_file/edit_file (read-before-write)."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let resolved = match ctx.policy.resolve_read(Path::new(&input.path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            let meta = match tokio::fs::metadata(&resolved).await {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::error(format!("file not found: {}", input.path));
                }
                Err(e) => return ToolOutput::error(format!("cannot stat {}: {e}", input.path)),
            };
            if meta.is_dir() {
                return ToolOutput::error(format!("is a directory, not a file: {}", input.path));
            }

            let bytes = match read_capped(&resolved, READ_CAP + 1).await {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", input.path)),
            };
            let truncated = bytes.len() > READ_CAP;
            let slice = if truncated {
                &bytes[..READ_CAP]
            } else {
                &bytes[..]
            };
            let mut content = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                content.push_str(&format!(
                    "\n\n[truncated: file exceeds {READ_CAP} bytes; showing the first {READ_CAP}]"
                ));
            }

            if let Some(mtime) = mtime_of(&meta) {
                ctx.file_times.stamp(resolved.clone(), mtime);
            }

            ToolOutput::ok(content)
        })
    }
}

async fn read_capped(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    file.take(limit as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Create or overwrite a file inside the workspace.
///
/// An existing file may only be overwritten if it was read this run (via
/// `read_file`) and has not changed on disk since — otherwise the write is
/// refused and you must read it first. Missing parent directories are created.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteFileInput {
    /// Destination path, relative to the workspace root (or absolute inside it).
    pub path: String,
    /// Full new contents as UTF-8 text. Set exactly one of this or
    /// `content_base64`.
    pub content: Option<String>,
    /// Full new contents as base64-encoded bytes, for binary files. Set
    /// exactly one of this or `content`.
    pub content_base64: Option<String>,
    /// Optional optimistic-concurrency guard: when set and the target's
    /// current modification time in milliseconds differs (or the target no
    /// longer exists), the write is refused before anything is written and a
    /// structured conflict (`kind: "conflict"`, carrying both mtimes) is
    /// returned so the caller can re-read and retry.
    pub expected_mtime_ms: Option<u64>,
}

/// Writes a file, enforcing read-before-write on existing files.
pub struct WriteFile;

impl Tool for WriteFile {
    type Input = WriteFileInput;

    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> String {
        "Create a new file or overwrite an existing one inside the workspace. \
         Provide exactly one of 'content' (UTF-8 text) or 'content_base64' \
         (base64-encoded bytes, for binary files). An existing file must have \
         been read this run (read_file) and be unchanged on disk, or the write \
         is refused. Optionally pass 'expected_mtime_ms': if the target's \
         current mtime (ms) differs, the write is refused with a structured \
         conflict ({\"kind\":\"conflict\", ...} carrying both mtimes) so you \
         can re-read and retry. Parent directories are created as needed."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            use base64::Engine as _;
            let bytes: Vec<u8> = match (input.content, input.content_base64) {
                (Some(text), None) => text.into_bytes(),
                (None, Some(b64)) => {
                    match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
                        Ok(b) => b,
                        Err(e) => return ToolOutput::error(format!("invalid content_base64: {e}")),
                    }
                }
                (Some(_), Some(_)) => {
                    return ToolOutput::error(
                        "set exactly one of content or content_base64, not both",
                    );
                }
                (None, None) => {
                    return ToolOutput::error("set exactly one of content or content_base64");
                }
            };

            let resolved = match ctx.policy.resolve_write(Path::new(&input.path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            // Serialize the check→write against any concurrent writer of the
            // same path so a batched pair of edits can't lose an update.
            let _guard = ctx.locks.lock(&resolved).await;

            let current = tokio::fs::metadata(&resolved)
                .await
                .ok()
                .and_then(|m| m.modified().ok());

            // Explicit optimistic-concurrency check, before anything is
            // written. The conflict is STRUCTURED data — the caller reads both
            // mtimes, re-reads the file, and retries. Composes with (never
            // replaces) the read-before-write ledger below.
            if let Some(expected) = input.expected_mtime_ms {
                let actual = current.and_then(mtime_ms);
                if actual != Some(expected) {
                    return ToolOutput::error(
                        serde_json::json!({
                            "kind": "conflict",
                            "expected_mtime_ms": expected,
                            "actual_mtime_ms": actual,
                        })
                        .to_string(),
                    );
                }
            }

            match ctx.file_times.check_write(&resolved, current) {
                WriteCheck::NeverRead => {
                    return ToolOutput::error("must read_file before overwriting an existing file");
                }
                WriteCheck::Stale => {
                    return ToolOutput::error(
                        "file changed on disk since it was read; read it again",
                    );
                }
                WriteCheck::New | WriteCheck::Fresh => {}
            }

            if current.is_some()
                && let Err(abort) = observe_overwrite(&ctx, &resolved, None).await
            {
                return abort;
            }

            if let Some(parent) = resolved.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return ToolOutput::error(format!("cannot create parent dirs: {e}"));
            }

            let n = bytes.len();
            if let Err(e) = tokio::fs::write(&resolved, &bytes).await {
                return ToolOutput::error(format!("cannot write {}: {e}", input.path));
            }

            if let Ok(meta) = tokio::fs::metadata(&resolved).await
                && let Some(mtime) = mtime_of(&meta)
            {
                ctx.file_times.stamp(resolved.clone(), mtime);
            }

            ToolOutput::ok(format!(
                "wrote {n} bytes to {}",
                rel(&ctx.policy.root(), &resolved)
            ))
        })
    }
}

/// Replace one exact, unique occurrence of a string in an existing file.
///
/// The file must already have been read this run. `old_string` must occur
/// exactly once — zero matches or multiple matches are refused so the edit is
/// never ambiguous.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditFileInput {
    /// Path to the file to edit, relative to the workspace root.
    pub path: String,
    /// The exact text to find; it must occur exactly once in the file.
    pub old_string: String,
    /// The text to replace it with.
    pub new_string: String,
}

/// Makes a precise single-occurrence replacement in a file.
pub struct EditFile;

impl Tool for EditFile {
    type Input = EditFileInput;

    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> String {
        "Replace one exact occurrence of old_string with new_string in an \
         existing file (which must have been read this run). old_string must \
         match exactly once — zero or multiple matches are refused."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            if input.old_string.is_empty() {
                return ToolOutput::error("old_string must not be empty");
            }

            let resolved = match ctx.policy.resolve_write(Path::new(&input.path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            // Hold the path lock across the read→replace→write so a concurrent
            // editor of the same file cannot interleave and clobber this change.
            let _guard = ctx.locks.lock(&resolved).await;

            let current = tokio::fs::metadata(&resolved)
                .await
                .ok()
                .and_then(|m| m.modified().ok());
            match ctx.file_times.check_write(&resolved, current) {
                WriteCheck::NeverRead => {
                    return ToolOutput::error("must read_file before editing an existing file");
                }
                WriteCheck::Stale => {
                    return ToolOutput::error(
                        "file changed on disk since it was read; read it again",
                    );
                }
                WriteCheck::New | WriteCheck::Fresh => {}
            }

            let content = match tokio::fs::read_to_string(&resolved).await {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::error(format!("file not found: {}", input.path));
                }
                Err(e) => return ToolOutput::error(format!("cannot read {}: {e}", input.path)),
            };

            let count = content.matches(&input.old_string).count();
            if count == 0 {
                return ToolOutput::error("old_string not found in file");
            }
            if count > 1 {
                return ToolOutput::error(format!(
                    "{count} matches for old_string, must be unique"
                ));
            }

            if let Err(abort) = observe_overwrite(&ctx, &resolved, Some(content.as_bytes())).await {
                return abort;
            }

            let updated = content.replacen(&input.old_string, &input.new_string, 1);
            if let Err(e) = tokio::fs::write(&resolved, updated.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", input.path));
            }

            if let Ok(meta) = tokio::fs::metadata(&resolved).await
                && let Some(mtime) = mtime_of(&meta)
            {
                ctx.file_times.stamp(resolved.clone(), mtime);
            }

            ToolOutput::ok(format!("edited {}", rel(&ctx.policy.root(), &resolved)))
        })
    }
}

/// List the immediate entries of a directory inside the workspace.
///
/// Non-recursive. Directories are suffixed with `/`. Results are sorted.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListFilesInput {
    /// Directory to list, relative to the workspace root. Defaults to the root.
    pub path: Option<String>,
}

/// Lists the direct children of a directory.
pub struct ListFiles;

impl Tool for ListFiles {
    type Input = ListFilesInput;

    fn name(&self) -> &'static str {
        "list_files"
    }

    fn description(&self) -> String {
        "List the immediate entries of a directory inside the workspace \
         (non-recursive). Directories end with '/'. Defaults to the workspace \
         root."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let path = input.path.unwrap_or_else(|| ".".to_string());
            let resolved = match ctx.policy.resolve_read(Path::new(&path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };

            let mut entries = match tokio::fs::read_dir(&resolved).await {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return ToolOutput::error(format!("directory not found: {path}"));
                }
                Err(e) => return ToolOutput::error(format!("cannot list {path}: {e}")),
            };

            let mut names: Vec<String> = Vec::new();
            loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                        names.push(if is_dir { format!("{name}/") } else { name });
                    }
                    Ok(None) => break,
                    Err(e) => return ToolOutput::error(format!("cannot list {path}: {e}")),
                }
            }
            names.sort();

            if names.is_empty() {
                ToolOutput::ok("(empty)")
            } else {
                ToolOutput::ok(names.join("\n"))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::SubtreePolicy;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn ctx_in(dir: &Path) -> Arc<ToolCtx> {
        Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(dir).unwrap())))
    }

    async fn read(ctx: &Arc<ToolCtx>, path: &str) -> ToolOutput {
        Arc::new(ReadFile)
            .run(ReadFileInput { path: path.into() }, ctx.clone())
            .await
    }

    async fn write(ctx: &Arc<ToolCtx>, input: WriteFileInput) -> ToolOutput {
        Arc::new(WriteFile).run(input, ctx.clone()).await
    }

    async fn edit(ctx: &Arc<ToolCtx>, path: &str, old: &str, new: &str) -> ToolOutput {
        Arc::new(EditFile)
            .run(
                EditFileInput {
                    path: path.into(),
                    old_string: old.into(),
                    new_string: new.into(),
                },
                ctx.clone(),
            )
            .await
    }

    fn text_input(path: &str, content: &str) -> WriteFileInput {
        WriteFileInput {
            path: path.into(),
            content: Some(content.into()),
            content_base64: None,
            expected_mtime_ms: None,
        }
    }

    fn disk_mtime_ms(path: &Path) -> u64 {
        mtime_ms(std::fs::metadata(path).unwrap().modified().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn expected_mtime_conflict_is_structured_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        read(&ctx, "a.txt").await;
        let expected = disk_mtime_ms(&path);

        // Move the on-disk mtime forward, as an outside writer would.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(later).unwrap();
        drop(f);
        let actual = disk_mtime_ms(&path);
        assert_ne!(expected, actual);

        let out = write(
            &ctx,
            WriteFileInput {
                expected_mtime_ms: Some(expected),
                ..text_input("a.txt", "v2")
            },
        )
        .await;
        assert!(out.is_error);
        let v: serde_json::Value = serde_json::from_str(&out.content).expect("structured conflict");
        assert_eq!(v["kind"], "conflict");
        assert_eq!(v["expected_mtime_ms"], expected);
        assert_eq!(v["actual_mtime_ms"], actual);
        // Refused BEFORE writing: the prior contents are intact.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");
    }

    #[tokio::test]
    async fn matching_expected_mtime_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        read(&ctx, "a.txt").await;

        let out = write(
            &ctx,
            WriteFileInput {
                expected_mtime_ms: Some(disk_mtime_ms(&path)),
                ..text_input("a.txt", "v2")
            },
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v2");
    }

    #[tokio::test]
    async fn content_base64_round_trips_bytes() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let bytes: Vec<u8> = vec![0, 255, 128, 10, 13, 0, 7];

        let out = write(
            &ctx,
            WriteFileInput {
                path: "bin.dat".into(),
                content: None,
                content_base64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("wrote 7 bytes"));
        assert_eq!(std::fs::read(dir.path().join("bin.dat")).unwrap(), bytes);

        let bad = write(
            &ctx,
            WriteFileInput {
                path: "bad.dat".into(),
                content: None,
                content_base64: Some("not base64!!".into()),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(bad.is_error);
        assert!(bad.content.contains("content_base64"));
    }

    #[tokio::test]
    async fn exactly_one_content_form_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());

        let both = write(
            &ctx,
            WriteFileInput {
                path: "x.txt".into(),
                content: Some("a".into()),
                content_base64: Some("YQ==".into()),
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(both.is_error);
        assert!(both.content.contains("exactly one"));

        let neither = write(
            &ctx,
            WriteFileInput {
                path: "x.txt".into(),
                content: None,
                content_base64: None,
                expected_mtime_ms: None,
            },
        )
        .await;
        assert!(neither.is_error);
        assert!(neither.content.contains("exactly one"));

        assert!(!dir.path().join("x.txt").exists());
    }

    #[derive(Default)]
    struct Recording(Mutex<Vec<(PathBuf, Vec<u8>)>>);

    impl WriteObserver for Recording {
        fn before_overwrite(&self, path: &Path, prior: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push((path.to_path_buf(), prior.to_vec()));
            Ok(())
        }
    }

    struct Failing;

    impl WriteObserver for Failing {
        fn before_overwrite(&self, _path: &Path, _prior: &[u8]) -> Result<(), String> {
            Err("history store unavailable".into())
        }
    }

    #[tokio::test]
    async fn observer_sees_prior_bytes_on_overwrite_and_edit_but_not_creation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "v1").unwrap();
        let ctx = ctx_in(dir.path());
        let recording = Arc::new(Recording::default());
        ctx.extensions
            .insert(recording.clone() as Arc<dyn WriteObserver>);

        read(&ctx, "a.txt").await;
        let overwrote = write(&ctx, text_input("a.txt", "v2")).await;
        assert!(!overwrote.is_error, "{}", overwrote.content);
        let edited = edit(&ctx, "a.txt", "v2", "v3").await;
        assert!(!edited.is_error, "{}", edited.content);
        // New-file creation must not invoke the observer.
        let created = write(&ctx, text_input("new.txt", "fresh")).await;
        assert!(!created.is_error, "{}", created.content);

        let seen = recording.0.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.ends_with("a.txt"));
        assert_eq!(seen[0].1, b"v1");
        assert_eq!(seen[1].1, b"v2");
    }

    #[tokio::test]
    async fn failing_observer_aborts_write_and_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "v1").unwrap();
        let ctx = ctx_in(dir.path());
        ctx.extensions
            .insert(Arc::new(Failing) as Arc<dyn WriteObserver>);
        read(&ctx, "a.txt").await;

        let out = write(&ctx, text_input("a.txt", "v2")).await;
        assert!(out.is_error);
        assert!(out.content.contains("history store unavailable"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");

        let out = edit(&ctx, "a.txt", "v1", "v2").await;
        assert!(out.is_error);
        assert!(out.content.contains("history store unavailable"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1");

        // A failing observer never blocks creating a NEW file.
        let created = write(&ctx, text_input("new.txt", "fresh")).await;
        assert!(!created.is_error, "{}", created.content);
    }
}
