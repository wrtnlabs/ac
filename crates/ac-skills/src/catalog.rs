//! The model-facing skills catalog, mirroring codex-rs's rendering: a
//! `## Skills` block listing `- {name}: {description} (file: {path})` per
//! skill plus a "How to use skills" section. The usage prose is adapted from
//! codex-rs core-skills/src/render.rs (Apache-2.0), trimmed to the one
//! locator kind AC v1 has — files on the host filesystem.

use crate::resolver::Listing;

const CATALOG_INTRO: &str = "A skill is a set of instructions provided through a `SKILL.md` file. \
Below is the list of skills that can be used. Each entry includes a name, a description, and the \
absolute path to its `SKILL.md`.";

const HOW_TO_USE: &str = "\
- Trigger rules: If the user names a skill (with `$SkillName` or plain text) OR the task clearly \
matches a skill's description shown above, you must use that skill for that turn. Multiple \
mentions mean use them all. Do not carry skills across turns unless re-mentioned.
- Missing/blocked: If a named skill isn't in the list or its file can't be read, say so briefly \
and continue with the best fallback.
- How to use a skill (progressive disclosure):
  1) After deciding to use a skill, read its `SKILL.md` completely before taking task actions. \
If a read is truncated, continue until EOF.
  2) When `SKILL.md` references relative paths (e.g., `scripts/foo.py`), resolve them relative \
to the directory containing that `SKILL.md`.
  3) If `SKILL.md` points to extra folders such as `references/`, use its routing instructions \
to identify the files required for the task, and read each required file before acting on it.
  4) If `scripts/` exist, prefer running or patching them instead of retyping large code blocks.
  5) If `assets/` or templates exist, reuse them instead of recreating from scratch.
- Coordination: If multiple skills apply, choose the minimal set that covers the request and \
announce which skill(s) you're using and why (one short line).
- Context hygiene: Progressive disclosure applies to selecting relevant files, not partially \
reading a selected instruction file. Do not load unrelated references, scripts, or assets.
- Safety and fallback: If a skill can't be applied cleanly (missing files, unclear \
instructions), state the issue, pick the next-best approach, and continue.";

/// Render the catalog block for a listing, or `None` when it holds no skills
/// (hosts then omit the section entirely). Hosts append this to their system
/// prompt (or equivalent once-per-context instructions).
pub fn catalog_markdown(listing: &Listing) -> Option<String> {
    if listing.skills.is_empty() {
        return None;
    }
    let mut out = format!("## Skills\n\n{CATALOG_INTRO}\n\n### Available skills\n");
    for skill in &listing.skills {
        out.push_str(&format!(
            "- {}: {} (file: {})\n",
            skill.name,
            skill.description,
            skill.skill_md.display()
        ));
    }
    out.push_str(&format!("\n### How to use skills\n{HOW_TO_USE}\n"));
    Some(out)
}
