//! Filesystem tools: `read_file`, `write_file`, `edit_file`, `list_files`.
//!
//! Every path first passes through the host [`PathPolicy`] (via `ctx.policy`);
//! these tools never touch a raw user path. `read_file` stamps the mtime it saw
//! into the per-run read-before-write ledger, and the write tools consult it.

use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput, WriteCheck};
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

/// Create or overwrite a text file inside the workspace.
///
/// An existing file may only be overwritten if it was read this run (via
/// `read_file`) and has not changed on disk since — otherwise the write is
/// refused and you must read it first. Missing parent directories are created.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteFileInput {
    /// Destination path, relative to the workspace root (or absolute inside it).
    pub path: String,
    /// Full new contents of the file.
    pub content: String,
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
         An existing file must have been read this run (read_file) and be \
         unchanged on disk, or the write is refused. Parent directories are \
         created as needed."
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

            if let Some(parent) = resolved.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return ToolOutput::error(format!("cannot create parent dirs: {e}"));
            }

            let n = input.content.len();
            if let Err(e) = tokio::fs::write(&resolved, input.content.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", input.path));
            }

            if let Ok(meta) = tokio::fs::metadata(&resolved).await
                && let Some(mtime) = mtime_of(&meta)
            {
                ctx.file_times.stamp(resolved.clone(), mtime);
            }

            ToolOutput::ok(format!(
                "wrote {n} bytes to {}",
                rel(ctx.policy.root(), &resolved)
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

            let updated = content.replacen(&input.old_string, &input.new_string, 1);
            if let Err(e) = tokio::fs::write(&resolved, updated.as_bytes()).await {
                return ToolOutput::error(format!("cannot write {}: {e}", input.path));
            }

            if let Ok(meta) = tokio::fs::metadata(&resolved).await
                && let Some(mtime) = mtime_of(&meta)
            {
                ctx.file_times.stamp(resolved.clone(), mtime);
            }

            ToolOutput::ok(format!("edited {}", rel(ctx.policy.root(), &resolved)))
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
