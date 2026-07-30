//! Skill body injection, mirroring codex-rs's `SkillInstructions` fragment:
//! a selected skill's SKILL.md is read verbatim host-side and wrapped in a
//! `<skill>` block that the host places into that turn's user input. There
//! is no tool in this path — injection is text, and further companion files
//! are read by the model itself at the paths the body references.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::mention::{extract_skill_mentions, select_skills_for_mentions_with_path};
use crate::resolver::{Skill, read_skill_text};

/// One skill body ready to inject into a turn's input.
#[derive(Debug, Clone)]
pub struct SkillInjection {
    pub name: String,
    pub path: PathBuf,
    pub contents: String,
}

impl SkillInjection {
    /// The exact fragment format codex-rs uses (skill_instructions.rs):
    /// `<skill>` / `<name>` / `<path>` / contents / `</skill>`.
    pub fn render(&self) -> String {
        format!(
            "<skill>\n<name>{}</name>\n<path>{}</path>\n{}\n</skill>",
            self.name,
            self.path.display(),
            self.contents
        )
    }
}

/// Read each selected skill's SKILL.md for injection. A skill whose file
/// cannot be read becomes a warning (codex's wording), never a hard failure —
/// the turn proceeds with the skills that loaded.
pub fn build_skill_injections(skills: &[Skill]) -> (Vec<SkillInjection>, Vec<String>) {
    build_skill_injections_with_path(skills, |skill| skill.skill_md.clone())
}

/// Build injections while letting a host project the model-facing path.
///
/// The body is always read from AC's resolved source path; only the `<path>`
/// advertised inside the injection changes. This supports hosts that expose
/// skills through a contained virtual or materialized route.
pub fn build_skill_injections_with_path(
    skills: &[Skill],
    path_for: impl Fn(&Skill) -> PathBuf,
) -> (Vec<SkillInjection>, Vec<String>) {
    let mut injections = Vec::new();
    let mut warnings = Vec::new();
    for skill in skills {
        match read_skill_text(skill) {
            Ok(contents) => injections.push(SkillInjection {
                name: skill.name.clone(),
                path: path_for(skill),
                contents,
            }),
            Err(e) => warnings.push(format!(
                "Failed to load skill {} at {}: {e}",
                skill.name,
                skill.skill_md.display()
            )),
        }
    }
    (injections, warnings)
}

/// Compose one user turn with AC's standard skill-selection semantics.
///
/// Plain `$name` and linked mentions select from `available`; `preselected`
/// carries any host choice made outside the prompt. Selected paths dedupe,
/// unreadable bodies become warnings, and loaded bodies append as `<skill>`
/// blocks after the user's text.
pub fn compose_skill_input(
    available: &[Skill],
    preselected: &[Skill],
    prompt: &str,
) -> (String, Vec<String>) {
    compose_skill_input_with_path(available, preselected, prompt, |skill| {
        skill.skill_md.clone()
    })
}

/// Compose a skill-selected turn with a host-projected model-facing path.
pub fn compose_skill_input_with_path(
    available: &[Skill],
    preselected: &[Skill],
    prompt: &str,
    path_for: impl Fn(&Skill) -> PathBuf,
) -> (String, Vec<String>) {
    let mentions = extract_skill_mentions(prompt);
    let mentioned = select_skills_for_mentions_with_path(available, &mentions, &path_for);
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for skill in preselected.iter().cloned().chain(mentioned) {
        if seen.insert(skill.skill_md.clone()) {
            selected.push(skill);
        }
    }
    let (injections, warnings) = build_skill_injections_with_path(&selected, path_for);
    let mut input = prompt.to_string();
    for injection in injections {
        input.push_str("\n\n");
        input.push_str(&injection.render());
    }
    (input, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SkillLayer, SkillsResolver};
    use std::fs;
    use std::path::Path;

    fn write_skill(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nInstructions for {name}."),
        )
        .unwrap();
    }

    fn skills(root: &Path) -> Vec<crate::Skill> {
        SkillsResolver::direct_children(vec![SkillLayer {
            name: "test".to_string(),
            root: root.to_path_buf(),
        }])
        .list()
        .skills
    }

    #[test]
    fn mentions_append_injections_with_projected_paths() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "alpha", "Alpha.");
        let available = skills(root.path());
        let (input, warnings) =
            compose_skill_input_with_path(&available, &[], "Please use $alpha.", |skill| {
                PathBuf::from("virtual")
                    .join(&skill.dir_name)
                    .join("SKILL.md")
            });
        assert!(warnings.is_empty());
        assert!(input.starts_with("Please use $alpha."));
        assert!(input.contains("<name>alpha</name>"));
        assert!(input.contains("<path>virtual/alpha/SKILL.md</path>"));
        assert!(input.contains("Instructions for alpha."));
    }

    #[test]
    fn linked_mentions_and_duplicate_preselection_inject_once() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "alpha", "Alpha.");
        let available = skills(root.path());
        let alpha = available[0].clone();
        let linked = format!("Use [$alpha]({}).", alpha.skill_md.to_string_lossy());
        let (input, warnings) = compose_skill_input(&available, &[alpha.clone(), alpha], &linked);
        assert!(warnings.is_empty());
        assert_eq!(input.matches("<name>alpha</name>").count(), 1);
    }

    #[test]
    fn linked_projected_locator_selects_the_advertised_skill() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "alpha", "Alpha.");
        let available = skills(root.path());
        let projected = |skill: &crate::Skill| {
            PathBuf::from("virtual")
                .join(&skill.dir_name)
                .join("SKILL.md")
        };
        let prompt = "Use [$alpha](virtual/alpha/SKILL.md).";
        let (input, warnings) = compose_skill_input_with_path(&available, &[], prompt, projected);
        assert!(warnings.is_empty());
        assert_eq!(input.matches("<name>alpha</name>").count(), 1);
        assert!(input.contains("<path>virtual/alpha/SKILL.md</path>"));
    }

    #[test]
    fn unreadable_selected_skill_warns_without_blocking_the_turn() {
        let root = tempfile::tempdir().unwrap();
        write_skill(root.path(), "alpha", "Alpha.");
        let available = skills(root.path());
        fs::remove_file(&available[0].skill_md).unwrap();
        let (input, warnings) = compose_skill_input(&available, &[], "$alpha");
        assert_eq!(input, "$alpha");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Failed to load skill alpha"));
    }
}
