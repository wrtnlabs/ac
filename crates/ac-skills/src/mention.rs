//! `$skill-name` mention extraction and selection, mirroring codex-rs's
//! mention syntax (core-skills/src/injection.rs): a `$` sigil followed by a
//! name, or the linked form `[$name](path)` whose path is matched exactly.
//! Common environment-variable names are excluded so shell snippets in a
//! prompt don't read as mentions, and a plain name only selects a skill when
//! exactly one listed skill carries it — ambiguity skips, it never guesses.

use std::path::{Path, PathBuf};

use crate::resolver::Skill;

/// Env-var lookalikes that are never treated as skill mentions.
const EXCLUDED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "PWD",
    "TMPDIR",
    "TEMP",
    "TMP",
    "LANG",
    "TERM",
    "XDG_CONFIG_HOME",
];

/// One mention found in user text. `path` is present only for the linked
/// form and selects by exact SKILL.md path instead of by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMention {
    pub name: String,
    pub path: Option<PathBuf>,
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-')
}

/// Extract skill mentions from free text: plain `$name` and linked
/// `[$name](path)`. Deduped, order preserved.
pub fn extract_skill_mentions(text: &str) -> Vec<SkillMention> {
    let mut mentions: Vec<SkillMention> = Vec::new();
    let mut push = |m: SkillMention| {
        if !mentions.contains(&m) {
            mentions.push(m);
        }
    };

    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => {
                if let Some((mention, next)) = parse_linked(text, i) {
                    push(mention);
                    i = next;
                    continue;
                }
                i += 1;
            }
            b'$' => {
                if let Some((name, next)) = parse_name(text, i + 1) {
                    if !EXCLUDED_NAMES.contains(&name.as_str()) {
                        push(SkillMention { name, path: None });
                    }
                    i = next;
                    continue;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    mentions
}

/// Parse a name starting at byte offset `at`; returns (name, next offset).
fn parse_name(text: &str, at: usize) -> Option<(String, usize)> {
    let rest = &text[at..];
    let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some((rest[..end].to_string(), at + end))
}

/// Parse `[$name](path)` starting at the `[` byte offset; returns the
/// mention and the absolute byte offset just past the closing `)` — the
/// caller resumes scanning there, so nothing inside the consumed span (e.g.
/// a `$` in the path) can be re-read as a mention.
fn parse_linked(text: &str, at: usize) -> Option<(SkillMention, usize)> {
    let rest = &text[at..];
    let rest = rest.strip_prefix("[$")?;
    let name_end = rest.find(|c: char| !is_name_char(c))?;
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let after_name = &rest[name_end..];
    let after_bracket = after_name.strip_prefix(']')?;
    let after_ws = after_bracket.trim_start();
    let inner = after_ws.strip_prefix('(')?;
    let close = inner.find(')')?;
    let path = inner[..close].trim();
    if path.is_empty() {
        return None;
    }
    let next = text.len() - (inner.len() - close - 1);
    Some((
        SkillMention {
            name: name.to_string(),
            path: Some(PathBuf::from(path)),
        },
        next,
    ))
}

/// Resolve mentions against a listing. Linked mentions match by exact
/// SKILL.md path (canonicalized when possible); plain mentions match by name
/// only when exactly one listed skill carries that name. Unknown and
/// ambiguous mentions are skipped. Deduped by path, listing order is not
/// imposed — selection order follows mention order.
pub fn select_skills_for_mentions(skills: &[Skill], mentions: &[SkillMention]) -> Vec<Skill> {
    let mut selected: Vec<Skill> = Vec::new();
    let mut push = |s: &Skill| {
        if !selected.iter().any(|p| p.skill_md == s.skill_md) {
            selected.push(s.clone());
        }
    };
    for mention in mentions {
        match &mention.path {
            Some(path) => {
                let target = canonical_or_owned(path);
                if let Some(skill) = skills.iter().find(|s| s.skill_md == target) {
                    push(skill);
                }
            }
            None => {
                let mut matches = skills.iter().filter(|s| s.name == mention.name);
                if let (Some(skill), None) = (matches.next(), matches.next()) {
                    push(skill);
                }
            }
        }
    }
    selected
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_linked_and_excluded_mentions() {
        let mentions = extract_skill_mentions(
            "use $deck-builder and [$notes](/tmp/notes/SKILL.md); ignore $PATH and $ alone",
        );
        assert_eq!(
            mentions,
            vec![
                SkillMention {
                    name: "deck-builder".to_string(),
                    path: None
                },
                SkillMention {
                    name: "notes".to_string(),
                    path: Some(PathBuf::from("/tmp/notes/SKILL.md"))
                },
            ]
        );
    }

    #[test]
    fn duplicate_mentions_dedupe_and_namespaced_names_parse() {
        let mentions = extract_skill_mentions("$a $a $ns:tool");
        assert_eq!(mentions.len(), 2);
        assert_eq!(mentions[1].name, "ns:tool");
    }

    /// Regression: parse_linked once returned the match LENGTH where the
    /// caller expected the next absolute offset — a linked mention whose
    /// start offset exceeded its length looped forever, and a `$` inside a
    /// consumed link path leaked out as a phantom plain mention.
    #[test]
    fn a_linked_mention_after_a_long_prefix_terminates_and_consumes_its_path() {
        let mentions = extract_skill_mentions(
            "please summarize the report and then apply [$notes](/skills/notes/SKILL.md) here",
        );
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].name, "notes");

        // A `$` inside the consumed path must not surface as a mention.
        let mentions = extract_skill_mentions("aaaaaaaa[$x](/ppp/$beta) and $gamma");
        assert_eq!(
            mentions.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["x", "gamma"]
        );
    }
}
