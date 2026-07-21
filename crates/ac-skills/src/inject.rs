//! Skill body injection, mirroring codex-rs's `SkillInstructions` fragment:
//! a selected skill's SKILL.md is read verbatim host-side and wrapped in a
//! `<skill>` block that the host places into that turn's user input. There
//! is no tool in this path — injection is text, and further companion files
//! are read by the model itself at the paths the body references.

use std::path::PathBuf;

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
    let mut injections = Vec::new();
    let mut warnings = Vec::new();
    for skill in skills {
        match read_skill_text(skill) {
            Ok(contents) => injections.push(SkillInjection {
                name: skill.name.clone(),
                path: skill.skill_md.clone(),
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
