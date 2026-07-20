//! Layered skill discovery: scan layer roots in precedence order, validate
//! each candidate directory, let earlier layers shadow later ones, and load a
//! skill's body on demand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::frontmatter;

/// Maximum bytes of skill body [`SkillsResolver::load`] returns; longer
/// bodies are truncated on a char boundary with a marker.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// A validated skill. `name` is the identity — it always equals the
/// directory name — and `layer` names the [`SkillLayer`] that supplied it.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The skill directory (containing SKILL.md).
    pub dir: PathBuf,
    pub layer: String,
}

/// A candidate directory that did not make the listing, and why — nothing is
/// ever left out silently.
#[derive(Debug, Clone)]
pub struct SkippedSkill {
    pub dir: PathBuf,
    pub reason: String,
}

/// One place skills live: a root whose immediate subdirectories are skill
/// candidates. Hosts hand [`SkillsResolver`] layers in precedence order
/// (e.g. user, then project, then bundled); when two layers hold a skill
/// with the same name the earlier layer wins — shadowing is the override
/// mechanism, not an error.
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
    #[error("unknown skill: {0}")]
    UnknownSkill(String),
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
    /// `layers` in precedence order — the first layer holding a name wins.
    pub fn new(layers: Vec<SkillLayer>) -> Self {
        Self { layers }
    }

    /// Scan every layer and report both the skills and the skipped
    /// candidates. Within a layer skills are sorted by name; across layers
    /// they follow layer precedence order.
    pub fn list(&self) -> Listing {
        let mut listing = Listing::default();
        let mut winners: BTreeMap<String, String> = BTreeMap::new();
        for layer in &self.layers {
            for dir in subdirs(&layer.root) {
                match read_skill(&dir, &layer.name) {
                    Ok(skill) => {
                        if let Some(winning_layer) = winners.get(&skill.name) {
                            listing.skipped.push(SkippedSkill {
                                dir,
                                reason: format!(
                                    "shadowed by the {:?} skill in the earlier {winning_layer:?} layer",
                                    skill.name
                                ),
                            });
                        } else {
                            winners.insert(skill.name.clone(), layer.name.clone());
                            listing.skills.push(skill);
                        }
                    }
                    Err(reason) => listing.skipped.push(SkippedSkill { dir, reason }),
                }
            }
        }
        listing
    }

    /// Exact-match lookup against a fresh scan. Deliberately never joins
    /// `name` into a filesystem path — a traversal-shaped name (`../x`, an
    /// absolute path) cannot resolve to anything.
    pub fn resolve(&self, name: &str) -> Option<Skill> {
        self.list().skills.into_iter().find(|s| s.name == name)
    }

    /// Load a skill's markdown body: SKILL.md with the frontmatter block
    /// stripped, capped at [`MAX_BODY_BYTES`].
    pub fn load(&self, name: &str) -> Result<String, LoadError> {
        let skill = self
            .resolve(name)
            .ok_or_else(|| LoadError::UnknownSkill(name.to_string()))?;
        let path = skill.dir.join("SKILL.md");
        let bytes = std::fs::read(&path).map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        let text = String::from_utf8(bytes).map_err(|_| LoadError::Invalid {
            path: path.clone(),
            reason: "not valid UTF-8".to_string(),
        })?;
        let fm = frontmatter::parse(&text).map_err(|e| LoadError::Invalid {
            path: path.clone(),
            reason: e.to_string(),
        })?;
        let mut body = fm.body.to_string();
        if body.len() > MAX_BODY_BYTES {
            let mut end = MAX_BODY_BYTES;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str("\n[truncated: the skill body exceeded 256 KiB]");
        }
        Ok(body)
    }

    /// The system-prompt block advertising available skills, or `None` when
    /// there are none (hosts then omit the section entirely).
    pub fn catalog_markdown(&self) -> Option<String> {
        let skills = self.list().skills;
        if skills.is_empty() {
            return None;
        }
        let mut out = String::from(
            "## Skills\n\nSkills are named packs of instructions you can load with the \
             load_skill tool; when a skill's description matches the task at hand, load it \
             before proceeding.\n\n",
        );
        for skill in &skills {
            out.push_str(&format!("- {} — {}\n", skill.name, skill.description));
        }
        Some(out)
    }
}

/// Immediate subdirectories of `root`, sorted so listings are deterministic
/// across platforms. Dot-prefixed directories are not candidates (the name
/// charset can never validate them, and VCS/editor dirs would otherwise show
/// up as perpetual skips). Missing or unreadable root yields nothing.
fn subdirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn read_skill(dir: &Path, layer: &str) -> Result<Skill, String> {
    let path = dir.join("SKILL.md");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("no SKILL.md file".to_string());
        }
        Err(e) => return Err(format!("cannot read SKILL.md: {e}")),
    };
    let text = String::from_utf8(bytes).map_err(|_| "SKILL.md is not valid UTF-8".to_string())?;
    let fm = frontmatter::parse(&text).map_err(|e| e.to_string())?;

    let name = fm
        .fields
        .get("name")
        .ok_or_else(|| "frontmatter is missing the required 'name' field".to_string())?;
    if !valid_name(name) {
        return Err(format!(
            "invalid skill name {name:?}: must be 1-64 characters of [a-z0-9-], not starting or ending with '-'"
        ));
    }
    let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name != dir_name {
        return Err(format!(
            "frontmatter name {name:?} does not match the directory name {dir_name:?}"
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
    Ok(Skill {
        name: name.clone(),
        description: description.clone(),
        dir: dir.to_path_buf(),
        layer: layer.to_string(),
    })
}

/// `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most 64 chars — the skill-name
/// contract. Because names are this narrow and lookups are exact matches,
/// no caller input ever reaches the filesystem as a path.
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
