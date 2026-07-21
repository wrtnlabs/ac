//! Layered skill discovery, mirroring the codex-rs skill loader's semantics:
//! each layer root is walked recursively (bounded depth) for files named
//! `SKILL.md`, every candidate is validated with a loud skip reason, duplicate
//! *paths* are deduped with the earlier layer winning, and duplicate *names*
//! are allowed — ambiguity is resolved at mention-selection time, not by
//! shadowing at discovery time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::frontmatter;

/// Maximum bytes of a skill file [`read_skill_text`] returns; longer files
/// are truncated on a char boundary with a marker.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// How deep below a layer root the walk looks for `SKILL.md` files
/// (codex-rs uses the same depth-6 bound).
const MAX_SCAN_DEPTH: usize = 6;

/// Directories scanned per layer root before the walk stops (codex-rs bounds
/// its walk the same way). Hitting the cap is reported loudly in
/// [`Listing::skipped`], never silently.
const MAX_SCAN_DIRS: usize = 2000;

/// A validated skill: one `SKILL.md` file on disk.
#[derive(Debug, Clone)]
pub struct Skill {
    /// From frontmatter `name`, falling back to the directory name when the
    /// field is absent.
    pub name: String,
    pub description: String,
    /// The skill directory (containing SKILL.md); companion `scripts/`,
    /// `references/`, `assets/` live under it.
    pub dir: PathBuf,
    /// Absolute (canonicalized) path to the SKILL.md file — the locator the
    /// model sees in the catalog and reads itself.
    pub skill_md: PathBuf,
    /// Name of the [`SkillLayer`] that supplied it.
    pub layer: String,
    /// Every frontmatter field as written, unknown keys included — hosts pick
    /// the ones they understand without re-parsing SKILL.md.
    pub fields: BTreeMap<String, String>,
}

/// A candidate that did not make the listing, and why — nothing is ever left
/// out silently.
#[derive(Debug, Clone)]
pub struct SkippedSkill {
    pub dir: PathBuf,
    pub reason: String,
}

/// One place skills live: a root whose subtree is scanned for `SKILL.md`
/// files. Hosts hand [`SkillsResolver`] layers in precedence order (e.g.
/// user, then project, then bundled); precedence decides listing order and
/// which layer wins when two roots reach the *same* file on disk.
#[derive(Debug, Clone)]
pub struct SkillLayer {
    pub name: String,
    pub root: PathBuf,
}

/// Everything a scan found: the valid skills plus every candidate that was
/// left out, with the reason.
#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub skills: Vec<Skill>,
    pub skipped: Vec<SkippedSkill>,
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid SKILL.md at {}: {reason}", path.display())]
    Invalid { path: PathBuf, reason: String },
}

/// Scans an ordered set of [`SkillLayer`]s. Disk is read fresh on every
/// call — skills added or edited between calls are picked up without cache
/// invalidation, and a missing or unreadable layer root simply contributes
/// zero skills (hosts may point at directories that don't exist yet).
pub struct SkillsResolver {
    layers: Vec<SkillLayer>,
}

impl SkillsResolver {
    /// `layers` in precedence order — earlier layers list first, and win
    /// path-dedup when two roots reach the same SKILL.md.
    pub fn new(layers: Vec<SkillLayer>) -> Self {
        Self { layers }
    }

    /// Scan every layer and report both the skills and the skipped
    /// candidates. Within a layer skills sort by path; across layers they
    /// follow layer precedence order. Duplicate names are kept (mention
    /// selection treats them as ambiguous); duplicate canonical paths are
    /// skipped with a reason.
    pub fn list(&self) -> Listing {
        let mut listing = Listing::default();
        let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();
        for layer in &self.layers {
            let (files, truncated) = discover_skill_files(&layer.root);
            if truncated {
                listing.skipped.push(SkippedSkill {
                    dir: layer.root.clone(),
                    reason: format!(
                        "skills scan reached its traversal limit ({MAX_SCAN_DIRS} directories); \
                         remaining candidates under this root were not scanned"
                    ),
                });
            }
            for skill_md in files {
                match read_skill(&skill_md, &layer.name) {
                    Ok(skill) => {
                        if seen_paths.contains(&skill.skill_md) {
                            listing.skipped.push(SkippedSkill {
                                dir: skill.dir,
                                reason: "already listed via an earlier layer".to_string(),
                            });
                        } else {
                            seen_paths.insert(skill.skill_md.clone());
                            listing.skills.push(skill);
                        }
                    }
                    Err(reason) => listing.skipped.push(SkippedSkill {
                        dir: skill_md.parent().unwrap_or(&layer.root).to_path_buf(),
                        reason,
                    }),
                }
            }
        }
        listing
    }

    /// First listed skill with this exact name, against a fresh scan. Never
    /// joins `name` into a filesystem path — a traversal-shaped name cannot
    /// resolve to anything.
    pub fn resolve(&self, name: &str) -> Option<Skill> {
        self.list().skills.into_iter().find(|s| s.name == name)
    }
}

/// Read a skill's SKILL.md verbatim (frontmatter included — the injected
/// text is the file as the author wrote it), capped at [`MAX_BODY_BYTES`].
pub fn read_skill_text(skill: &Skill) -> Result<String, LoadError> {
    let bytes = std::fs::read(&skill.skill_md).map_err(|source| LoadError::Io {
        path: skill.skill_md.clone(),
        source,
    })?;
    let mut text = String::from_utf8(bytes).map_err(|_| LoadError::Invalid {
        path: skill.skill_md.clone(),
        reason: "not valid UTF-8".to_string(),
    })?;
    if text.len() > MAX_BODY_BYTES {
        let mut end = MAX_BODY_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[truncated: the skill file exceeded 256 KiB]");
    }
    Ok(text)
}

/// Every `SKILL.md` under `root` within [`MAX_SCAN_DEPTH`], sorted for
/// deterministic listings, plus whether the walk hit [`MAX_SCAN_DIRS`].
/// Dot-prefixed directories are not descended (VCS/editor dirs would
/// otherwise surface as candidates). Directory symlinks are followed, but
/// each *physical* directory is scanned once (canonical-path dedup — a
/// symlink cycle terminates and an aliased dir cannot list a skill twice).
/// A SKILL.md that is itself a file symlink is not a candidate (codex-rs
/// parity — the walk reports real files only). Missing or unreadable
/// directories contribute nothing.
fn discover_skill_files(root: &Path) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut truncated = false;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(physical) = dir.canonicalize() else {
            continue;
        };
        if !visited.insert(physical) {
            continue;
        }
        if visited.len() > MAX_SCAN_DIRS {
            truncated = true;
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let path = entry.path();
            if path.is_dir() {
                if depth < MAX_SCAN_DEPTH && !name.to_string_lossy().starts_with('.') {
                    stack.push((path, depth + 1));
                }
            } else if name == "SKILL.md"
                && !entry.file_type().map(|t| t.is_symlink()).unwrap_or(true)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    (found, truncated)
}

fn read_skill(skill_md: &Path, layer: &str) -> Result<Skill, String> {
    let bytes = std::fs::read(skill_md).map_err(|e| format!("cannot read SKILL.md: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| "SKILL.md is not valid UTF-8".to_string())?;
    let fm = frontmatter::parse(&text).map_err(|e| e.to_string())?;

    let dir = skill_md
        .parent()
        .ok_or_else(|| "SKILL.md has no parent directory".to_string())?;
    // Frontmatter `name` wins; absent, the directory name is the name
    // (codex-rs behavior). Either way the result must satisfy the name
    // contract — a directory like "My Skills" is not silently mangled into
    // an identity, it is skipped with a reason.
    let name = match fm.fields.get("name") {
        Some(name) => name.clone(),
        None => dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if !valid_name(&name) {
        return Err(format!(
            "invalid skill name {name:?}: must be 1-64 characters of [a-z0-9-], not starting or ending with '-'"
        ));
    }
    let description = fm
        .fields
        .get("description")
        .ok_or_else(|| "frontmatter is missing the required 'description' field".to_string())?;
    if description.is_empty() {
        return Err("description must not be empty".to_string());
    }
    if description.chars().count() > 1024 {
        return Err("description exceeds 1024 characters".to_string());
    }

    let canonical = skill_md
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize SKILL.md path: {e}"))?;
    let dir = canonical
        .parent()
        .ok_or_else(|| "SKILL.md has no parent directory".to_string())?
        .to_path_buf();
    Ok(Skill {
        name,
        description: description.clone(),
        dir,
        skill_md: canonical,
        layer: layer.to_string(),
        fields: fm.fields,
    })
}

/// `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most 64 chars — the skill-name
/// contract (the agentskills.io charset; also exactly the shape the `$name`
/// mention syntax can express).
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_contract() {
        assert!(valid_name("a"));
        assert!(valid_name("skill-2"));
        assert!(valid_name(&"a".repeat(64)));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(65)));
        assert!(!valid_name("-leading"));
        assert!(!valid_name("trailing-"));
        assert!(!valid_name("Upper"));
        assert!(!valid_name("under_score"));
        assert!(!valid_name("../evil"));
    }
}
