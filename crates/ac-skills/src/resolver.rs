//! Layered skill discovery.
//!
//! AC supports two generic discovery modes:
//! - [`ResolverMode::Recursive`] preserves the codex-style default: bounded
//!   recursive scanning, canonical-path deduplication, and directory-name
//!   fallback when frontmatter omits `name`.
//! - [`ResolverMode::DirectChildren`] is AC's optional ordered direct-child
//!   policy: only direct child directories are candidates, layers are visited
//!   in caller order, and a successfully loaded directory shadows the same
//!   directory name in lower-priority layers.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::frontmatter;

/// Maximum bytes of a skill file [`read_skill_text`] returns; longer files
/// are truncated on a char boundary with a marker.
pub const MAX_BODY_BYTES: usize = 256 * 1024;
/// Maximum complete `SKILL.md` size accepted during discovery.
pub const MAX_SKILL_MD_BYTES: usize = 1024 * 1024;
pub const MAX_DIRECT_SKILLS: usize = 256;
/// Maximum directory entries inspected across one direct-child listing.
pub const MAX_DIRECT_ENTRIES: usize = 2048;
/// Maximum diagnostics retained by one direct-child listing.
pub const MAX_DIRECT_SKIPPED: usize = 256;

const MAX_SCAN_DEPTH: usize = 6;
const MAX_SCAN_DIRS: usize = 2000;
const MAX_SCAN_ENTRIES: usize = 4096;

/// Standard agentskills manifest fields, with nested metadata preserved.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillManifest {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub metadata: Option<Map<String, Value>>,
}

#[derive(Debug, Clone)]
pub struct ParsedSkillMd {
    pub manifest: SkillManifest,
    pub fields: Map<String, Value>,
    pub body: String,
}

/// A validated skill backed by one SKILL.md file.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// Direct child entry name. For recursively discovered skills this is the
    /// immediate parent directory of SKILL.md.
    pub dir_name: String,
    /// Canonical skill directory.
    pub dir: PathBuf,
    /// Path as encountered beneath the configured layer root.
    pub source_dir: PathBuf,
    /// Canonical SKILL.md path.
    pub skill_md: PathBuf,
    pub layer: String,
    /// Every parsed frontmatter field, including unknown nested fields.
    pub fields: Map<String, Value>,
    pub manifest: SkillManifest,
    /// Markdown after the frontmatter delimiter.
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct SkippedSkill {
    pub dir: PathBuf,
    pub reason: String,
}

/// One skill root, supplied in precedence order.
#[derive(Debug, Clone)]
pub struct SkillLayer {
    pub name: String,
    pub root: PathBuf,
}

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ResolverMode {
    /// Recursively scan each root and deduplicate canonical files.
    #[default]
    Recursive,
    /// Scan direct child directories and shadow by directory name.
    DirectChildren,
}

/// Fresh-scanning resolver over an ordered set of layers.
pub struct SkillsResolver {
    layers: Vec<SkillLayer>,
    mode: ResolverMode,
}

impl SkillsResolver {
    /// Recursive/canonical-path default.
    pub fn new(layers: Vec<SkillLayer>) -> Self {
        Self {
            layers,
            mode: ResolverMode::Recursive,
        }
    }

    /// Ordered direct-child scanning with directory-name shadowing.
    pub fn direct_children(layers: Vec<SkillLayer>) -> Self {
        Self {
            layers,
            mode: ResolverMode::DirectChildren,
        }
    }

    pub fn mode(&self) -> ResolverMode {
        self.mode
    }

    pub fn list(&self) -> Listing {
        match self.mode {
            ResolverMode::Recursive => self.list_recursive(),
            ResolverMode::DirectChildren => self.list_direct_children(),
        }
    }

    fn list_recursive(&self) -> Listing {
        let mut listing = Listing::default();
        let mut seen_paths: BTreeSet<PathBuf> = BTreeSet::new();
        for layer in &self.layers {
            let (files, truncated) = discover_skill_files(&layer.root);
            if let Some(limit) = truncated {
                listing.skipped.push(SkippedSkill {
                    dir: layer.root.clone(),
                    reason: limit.diagnostic(),
                });
            }
            for skill_md in files {
                match read_skill(&skill_md, &layer.name, ManifestPolicy::recursive()) {
                    Ok(skill) if seen_paths.insert(skill.skill_md.clone()) => {
                        listing.skills.push(skill);
                    }
                    Ok(skill) => listing.skipped.push(SkippedSkill {
                        dir: skill.source_dir,
                        reason: "already listed via an earlier layer's canonical path".to_string(),
                    }),
                    Err(reason) => listing.skipped.push(SkippedSkill {
                        dir: skill_md.parent().unwrap_or(&layer.root).to_path_buf(),
                        reason,
                    }),
                }
            }
        }
        listing
    }

    fn list_direct_children(&self) -> Listing {
        let mut listing = Listing::default();
        let mut seen_directories: BTreeSet<String> = BTreeSet::new();
        let mut inspected_entries = 0_usize;
        let mut skipped_saturated = false;

        'layers: for layer in &self.layers {
            let Ok(entries) = std::fs::read_dir(&layer.root) else {
                continue;
            };
            let mut bounded_entries = Vec::new();
            let mut scan_truncated = false;
            for result in entries {
                if inspected_entries >= MAX_DIRECT_ENTRIES {
                    scan_truncated = true;
                    break;
                }
                inspected_entries += 1;
                match result {
                    Ok(entry) => bounded_entries.push(entry),
                    Err(error) => push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: layer.root.clone(),
                            reason: format!("cannot read directory entry: {error}"),
                        },
                    ),
                }
            }
            bounded_entries.sort_by_key(|entry| entry.file_name());

            for entry in bounded_entries {
                if listing.skills.len() >= MAX_DIRECT_SKILLS {
                    push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: layer.root.clone(),
                            reason: format!(
                                "direct-child skill listing reached its limit \
                                 ({MAX_DIRECT_SKILLS}); remaining candidates were not scanned"
                            ),
                        },
                    );
                    break 'layers;
                }
                let Ok(file_type) = entry.file_type() else {
                    push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: entry.path(),
                            reason: "cannot determine directory entry type".to_string(),
                        },
                    );
                    continue;
                };
                // Symlinked directories are not direct-child candidates.
                if !file_type.is_dir() {
                    continue;
                }
                let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
                    push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: entry.path(),
                            reason: "skill directory name is not valid UTF-8".to_string(),
                        },
                    );
                    continue;
                };
                if !is_valid_direct_skill_name(&dir_name) {
                    push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: entry.path(),
                            reason: format!(
                                "invalid direct-child skill directory {dir_name:?}: \
                                 expected ^[a-z][a-z0-9-]*$"
                            ),
                        },
                    );
                    continue;
                }
                let skill_md = entry.path().join("SKILL.md");
                if !skill_md.is_file() {
                    continue;
                }
                if seen_directories.contains(&dir_name) {
                    push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: entry.path(),
                            reason: format!(
                                "directory {dir_name:?} is shadowed by an earlier layer"
                            ),
                        },
                    );
                    continue;
                }

                match read_direct_skill(&layer.root, &skill_md, &layer.name) {
                    Ok(skill) => {
                        seen_directories.insert(dir_name);
                        listing.skills.push(skill);
                    }
                    Err(reason) => push_direct_skip(
                        &mut listing,
                        &mut skipped_saturated,
                        SkippedSkill {
                            dir: entry.path(),
                            reason,
                        },
                    ),
                }
            }
            if scan_truncated {
                push_direct_skip(
                    &mut listing,
                    &mut skipped_saturated,
                    SkippedSkill {
                        dir: layer.root.clone(),
                        reason: format!(
                            "direct-child skill scan reached its entry limit \
                             ({MAX_DIRECT_ENTRIES}); remaining entries and layers were not scanned"
                        ),
                    },
                );
                break;
            }
        }

        listing.skills.sort_by(|a, b| a.name.cmp(&b.name));
        listing
    }

    /// First listed skill with this manifest name.
    pub fn resolve(&self, name: &str) -> Option<Skill> {
        self.list()
            .skills
            .into_iter()
            .find(|skill| skill.name == name)
    }

    /// First listed skill whose directory entry has this exact name.
    pub fn resolve_directory(&self, dir_name: &str) -> Option<Skill> {
        self.list()
            .skills
            .into_iter()
            .find(|skill| skill.dir_name == dir_name)
    }
}

pub fn read_skill_text(skill: &Skill) -> Result<String, LoadError> {
    let metadata = std::fs::metadata(&skill.skill_md).map_err(|source| LoadError::Io {
        path: skill.skill_md.clone(),
        source,
    })?;
    let mut file = std::fs::File::open(&skill.skill_md).map_err(|source| LoadError::Io {
        path: skill.skill_md.clone(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(MAX_BODY_BYTES.saturating_add(4));
    file.by_ref()
        .take(MAX_BODY_BYTES as u64 + 4)
        .read_to_end(&mut bytes)
        .map_err(|source| LoadError::Io {
            path: skill.skill_md.clone(),
            source,
        })?;
    let truncated = metadata.len() > MAX_BODY_BYTES as u64 || bytes.len() > MAX_BODY_BYTES;
    let prefix = &bytes[..bytes.len().min(MAX_BODY_BYTES)];
    let valid_prefix = match std::str::from_utf8(prefix) {
        Ok(text) => text,
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&prefix[..error.valid_up_to()]).map_err(|_| LoadError::Invalid {
                path: skill.skill_md.clone(),
                reason: "not valid UTF-8".to_string(),
            })?
        }
        Err(_) => {
            return Err(LoadError::Invalid {
                path: skill.skill_md.clone(),
                reason: "not valid UTF-8".to_string(),
            });
        }
    };
    let mut text = valid_prefix.to_string();
    if truncated {
        text.push_str("\n[truncated: the skill file exceeded 256 KiB]");
    }
    Ok(text)
}

fn push_direct_skip(listing: &mut Listing, saturated: &mut bool, skipped: SkippedSkill) {
    if *saturated {
        return;
    }
    if listing.skipped.len() < MAX_DIRECT_SKIPPED.saturating_sub(1) {
        listing.skipped.push(skipped);
        return;
    }
    listing.skipped.push(SkippedSkill {
        dir: skipped.dir,
        reason: format!(
            "direct-child skill diagnostics reached their limit \
             ({MAX_DIRECT_SKIPPED}); additional diagnostics were omitted"
        ),
    });
    *saturated = true;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecursiveScanLimit {
    Directories,
    Entries,
}

impl RecursiveScanLimit {
    fn diagnostic(self) -> String {
        match self {
            Self::Directories => format!(
                "skills scan reached its traversal limit ({MAX_SCAN_DIRS} queued or visited \
                 directories); remaining candidates under this root were not scanned"
            ),
            Self::Entries => format!(
                "skills scan reached its entry inspection limit ({MAX_SCAN_ENTRIES}); \
                 remaining candidates under this root were not scanned"
            ),
        }
    }
}

fn discover_skill_files(root: &Path) -> (Vec<PathBuf>, Option<RecursiveScanLimit>) {
    let mut found = Vec::new();
    let mut visited: BTreeSet<PathBuf> = BTreeSet::new();
    let mut inspected_entries = 0_usize;
    let mut truncated = None;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    'walk: while let Some((dir, depth)) = stack.pop() {
        let Ok(physical) = dir.canonicalize() else {
            continue;
        };
        if !visited.insert(physical) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if inspected_entries >= MAX_SCAN_ENTRIES {
                truncated = Some(RecursiveScanLimit::Entries);
                break 'walk;
            }
            inspected_entries += 1;
            let name = entry.file_name();
            let path = entry.path();
            if path.is_dir() {
                if depth < MAX_SCAN_DEPTH && !name.to_string_lossy().starts_with('.') {
                    if visited.len() + stack.len() >= MAX_SCAN_DIRS {
                        truncated = Some(RecursiveScanLimit::Directories);
                        break 'walk;
                    }
                    stack.push((path, depth + 1));
                }
            } else if name == "SKILL.md"
                && !entry
                    .file_type()
                    .map(|kind| kind.is_symlink())
                    .unwrap_or(true)
            {
                found.push(path);
            }
        }
    }
    found.sort();
    (found, truncated)
}

fn read_direct_skill(root: &Path, skill_md: &Path, layer: &str) -> Result<Skill, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize layer root: {error}"))?;
    let canonical_md = skill_md
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize SKILL.md: {error}"))?;
    if canonical_md == canonical_root || !canonical_md.starts_with(&canonical_root) {
        return Err("SKILL.md resolves outside its layer root".to_string());
    }
    read_skill(skill_md, layer, ManifestPolicy::direct())
}

#[derive(Clone, Copy)]
struct ManifestPolicy {
    require_name: bool,
    direct_name_rule: bool,
    max_description_chars: Option<usize>,
}

impl ManifestPolicy {
    fn recursive() -> Self {
        Self {
            require_name: false,
            direct_name_rule: false,
            max_description_chars: Some(1024),
        }
    }

    fn direct() -> Self {
        Self {
            require_name: true,
            direct_name_rule: true,
            max_description_chars: None,
        }
    }
}

fn read_skill(skill_md: &Path, layer: &str, policy: ManifestPolicy) -> Result<Skill, String> {
    let metadata =
        std::fs::metadata(skill_md).map_err(|error| format!("cannot inspect SKILL.md: {error}"))?;
    if metadata.len() > MAX_SKILL_MD_BYTES as u64 {
        return Err(format!(
            "SKILL.md exceeds byte limit of {MAX_SKILL_MD_BYTES}"
        ));
    }
    let file =
        std::fs::File::open(skill_md).map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_SKILL_MD_BYTES)
            .min(MAX_SKILL_MD_BYTES),
    );
    file.take(MAX_SKILL_MD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    if bytes.len() > MAX_SKILL_MD_BYTES {
        return Err(format!(
            "SKILL.md exceeds byte limit of {MAX_SKILL_MD_BYTES}"
        ));
    }
    let text = String::from_utf8(bytes).map_err(|_| "SKILL.md is not valid UTF-8".to_string())?;
    let source_dir = skill_md
        .parent()
        .ok_or_else(|| "SKILL.md has no parent directory".to_string())?
        .to_path_buf();
    let dir_name = source_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "skill directory name is not valid UTF-8".to_string())?
        .to_string();
    let parsed = parse_skill_md_with_policy(&text, &dir_name, policy)?;

    let canonical = skill_md
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize SKILL.md path: {error}"))?;
    let dir = canonical
        .parent()
        .ok_or_else(|| "SKILL.md has no parent directory".to_string())?
        .to_path_buf();
    Ok(Skill {
        name: parsed.manifest.name.clone(),
        description: parsed.manifest.description.clone(),
        dir_name,
        dir,
        source_dir,
        skill_md: canonical,
        layer: layer.to_string(),
        fields: parsed.fields,
        manifest: parsed.manifest,
        body: parsed.body,
    })
}

/// Parse and validate a standalone agentskills SKILL.md using strict
/// direct-child manifest semantics (`name` is required).
pub fn parse_skill_md(text: &str) -> Result<ParsedSkillMd, String> {
    parse_skill_md_with_policy(text, "", ManifestPolicy::direct())
}

fn parse_skill_md_with_policy(
    text: &str,
    directory_name: &str,
    policy: ManifestPolicy,
) -> Result<ParsedSkillMd, String> {
    let frontmatter = frontmatter::parse(text).map_err(|error| error.to_string())?;
    let manifest = manifest_from(&frontmatter.fields, directory_name, policy)?;
    Ok(ParsedSkillMd {
        manifest,
        fields: frontmatter.fields,
        body: frontmatter.body.to_string(),
    })
}

fn manifest_from(
    fields: &Map<String, Value>,
    directory_name: &str,
    policy: ManifestPolicy,
) -> Result<SkillManifest, String> {
    let name = match fields.get("name") {
        Some(Value::String(name)) => name.clone(),
        Some(_) => return Err("frontmatter 'name' must be a string".to_string()),
        None if policy.require_name => {
            return Err("frontmatter is missing the required 'name' field".to_string());
        }
        None => directory_name.to_string(),
    };
    let valid = if policy.direct_name_rule {
        is_valid_direct_skill_name(&name)
    } else {
        valid_recursive_name(&name)
    };
    if !valid {
        let expected = if policy.direct_name_rule {
            "must match ^[a-z][a-z0-9-]*$"
        } else {
            "must be 1-64 characters of [a-z0-9-], not starting or ending with '-'"
        };
        return Err(format!("invalid skill name {name:?}: {expected}"));
    }

    let description = fields
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            "frontmatter is missing the required string 'description' field".to_string()
        })?
        .trim()
        .to_string();
    if description.is_empty() {
        return Err("description must not be empty".to_string());
    }
    if let Some(max) = policy.max_description_chars
        && description.chars().count() > max
    {
        return Err(format!("description exceeds {max} characters"));
    }

    let license = optional_string(fields, "license")?;
    let compatibility = optional_string(fields, "compatibility")?;
    let allowed_tools = match fields.get("allowed-tools") {
        None => None,
        Some(Value::Array(values)) => Some(
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "'allowed-tools' entries must all be strings".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err("'allowed-tools' must be an array of strings".to_string()),
    };
    let metadata = match fields.get("metadata") {
        None => None,
        Some(Value::Object(metadata)) => Some(metadata.clone()),
        Some(_) => return Err("'metadata' must be a mapping".to_string()),
    };

    Ok(SkillManifest {
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        metadata,
    })
}

fn optional_string(fields: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    match fields.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("'{key}' must be a string")),
    }
}

fn valid_recursive_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        && !name.starts_with('-')
        && !name.ends_with('-')
}

pub fn is_valid_direct_skill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_contracts() {
        assert!(valid_recursive_name("a"));
        assert!(valid_recursive_name("skill-2"));
        assert!(!valid_recursive_name("trailing-"));
        assert!(valid_recursive_name("2-start"));
        assert!(is_valid_direct_skill_name("skill-"));
        assert!(!is_valid_direct_skill_name("2-start"));
        assert!(!is_valid_direct_skill_name("Upper"));
        assert!(!is_valid_direct_skill_name("../evil"));
    }

    #[test]
    fn recursive_scan_bounds_a_single_wide_directory_before_enqueuing_every_child() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_SCAN_DIRS {
            std::fs::create_dir(root.path().join(format!("wide-{index:04}"))).unwrap();
        }

        let (found, truncated) = discover_skill_files(root.path());
        assert!(found.is_empty());
        assert_eq!(truncated, Some(RecursiveScanLimit::Directories));
        assert_eq!(
            truncated.unwrap().diagnostic(),
            format!(
                "skills scan reached its traversal limit ({MAX_SCAN_DIRS} queued or visited \
                 directories); remaining candidates under this root were not scanned"
            )
        );
    }

    #[test]
    fn recursive_scan_bounds_non_directory_entries_too() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_SCAN_ENTRIES {
            std::fs::write(root.path().join(format!("wide-{index:04}.txt")), "").unwrap();
        }

        let (found, truncated) = discover_skill_files(root.path());
        assert!(found.is_empty());
        assert_eq!(truncated, Some(RecursiveScanLimit::Entries));
    }
}
