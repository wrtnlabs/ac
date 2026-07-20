//! Search tools: `glob` (filename patterns) and `grep` (regex line search).
//!
//! Both walk the policy root on a blocking thread (walkdir is synchronous),
//! skip dot-directories, and cap their output so a huge tree never floods the
//! model's context.

use std::path::Path;
use std::sync::Arc;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;
use serde::Deserialize;

/// Maximum number of paths `glob` returns.
const GLOB_CAP: usize = 500;
/// Maximum number of matching lines `grep` returns.
const GREP_CAP: usize = 200;
/// Files larger than this are skipped by `grep`.
const GREP_MAX_FILE: u64 = 1024 * 1024;

/// True if any path component (below the root) starts with a dot.
fn has_dot_component(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .map(|s| s.starts_with('.'))
            .unwrap_or(false)
}

/// Find files whose path matches a glob pattern (e.g. `**/*.rs`, `src/*.txt`).
///
/// Patterns match paths relative to the workspace root. Dot-directories are
/// skipped. Results are sorted and capped.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct GlobInput {
    /// A glob pattern matched against workspace-relative paths, e.g.
    /// `**/*.rs` or `assets/*.png`.
    pub pattern: String,
}

/// Lists files matching a glob pattern.
pub struct Glob;

impl Tool for Glob {
    type Input = GlobInput;

    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> String {
        "Find files whose workspace-relative path matches a glob pattern (e.g. \
         '**/*.rs'). Dot-directories are skipped; results are sorted and capped \
         at 500."
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
            let matcher = match globset::Glob::new(&input.pattern) {
                Ok(g) => g.compile_matcher(),
                Err(e) => return ToolOutput::error(format!("invalid glob pattern: {e}")),
            };
            let root = ctx.policy.root().to_path_buf();

            let result = tokio::task::spawn_blocking(move || {
                // Cap collection during the walk (not just the output) so an
                // enormous tree can't balloon memory before truncation runs.
                let mut hits: Vec<String> = Vec::new();
                let mut capped = false;
                let walker = walkdir::WalkDir::new(&root).into_iter();
                for entry in walker.filter_entry(|e| !has_dot_component(e)) {
                    let Ok(entry) = entry else { continue };
                    if entry.depth() == 0 || !entry.file_type().is_file() {
                        continue;
                    }
                    let Ok(relative) = entry.path().strip_prefix(&root) else {
                        continue;
                    };
                    if matcher.is_match(relative) {
                        hits.push(relative.display().to_string());
                        if hits.len() >= GLOB_CAP {
                            capped = true;
                            break;
                        }
                    }
                }
                hits.sort();
                (hits, capped)
            })
            .await;

            match result {
                Ok((hits, capped)) => {
                    if hits.is_empty() {
                        return ToolOutput::ok("(no matches)");
                    }
                    let mut out = hits.join("\n");
                    if capped {
                        out.push_str(&format!(
                            "\n[capped: stopped after {GLOB_CAP} matches; narrow the pattern]"
                        ));
                    }
                    ToolOutput::ok(out)
                }
                Err(e) => ToolOutput::error(format!("glob walk failed: {e}")),
            }
        })
    }
}

/// Search file contents for a regular expression, line by line.
///
/// Walks under a starting path (default the workspace root), skipping
/// dot-directories, binary files, and files over 1 MiB. Each match is reported
/// as `relative/path:lineno: <trimmed line>`.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct GrepInput {
    /// A Rust `regex`-crate regular expression to match against each line.
    pub pattern: String,
    /// Directory or file to search under, relative to the workspace root.
    /// Defaults to the workspace root.
    pub path: Option<String>,
    /// Optional glob (matched against workspace-relative paths) restricting
    /// which files are searched, e.g. `**/*.rs`.
    pub glob: Option<String>,
}

/// Searches file contents with a regular expression.
pub struct Grep;

impl Tool for Grep {
    type Input = GrepInput;

    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> String {
        "Search file contents for a regex, line by line. Reports matches as \
         'path:lineno: line'. Skips dot-directories, binary files, and files \
         over 1 MiB. Optional glob narrows which files are searched. Capped at \
         200 lines."
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
            let regex = match regex::Regex::new(&input.pattern) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("invalid regex: {e}")),
            };

            let glob_matcher = match input.glob.as_deref() {
                Some(g) => match globset::Glob::new(g) {
                    Ok(g) => Some(g.compile_matcher()),
                    Err(e) => return ToolOutput::error(format!("invalid glob filter: {e}")),
                },
                None => None,
            };

            let path = input.path.unwrap_or_else(|| ".".to_string());
            let resolved = match ctx.policy.resolve_read(Path::new(&path)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let root = ctx.policy.root().to_path_buf();

            let result = tokio::task::spawn_blocking(move || {
                grep_walk(&resolved, &root, &regex, glob_matcher.as_ref())
            })
            .await;

            match result {
                Ok(lines) => {
                    if lines.is_empty() {
                        return ToolOutput::ok("(no matches)");
                    }
                    let total = lines.len();
                    let capped = total > GREP_CAP;
                    let mut shown: Vec<String> = lines.into_iter().take(GREP_CAP).collect();
                    if capped {
                        shown.push(format!(
                            "[truncated: {total}+ matching lines, showing first {GREP_CAP}]"
                        ));
                    }
                    ToolOutput::ok(shown.join("\n"))
                }
                Err(e) => ToolOutput::error(format!("grep walk failed: {e}")),
            }
        })
    }
}

fn grep_walk(
    start: &Path,
    root: &Path,
    regex: &regex::Regex,
    glob: Option<&globset::GlobMatcher>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let walker = walkdir::WalkDir::new(start).into_iter();
    for entry in walker.filter_entry(|e| !has_dot_component(e)) {
        if out.len() >= GREP_CAP {
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if let Some(g) = glob
            && !g.is_match(relative)
        {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && meta.len() > GREP_MAX_FILE
        {
            continue;
        }
        // Read as bytes; skip anything that is not valid UTF-8 (a cheap,
        // reliable binary filter) or contains a NUL byte.
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        if bytes.contains(&0) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let rel_display = relative.display().to_string();
        for (idx, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                out.push(format!("{rel_display}:{}: {}", idx + 1, line.trim()));
                if out.len() >= GREP_CAP {
                    break;
                }
            }
        }
    }
    out
}
