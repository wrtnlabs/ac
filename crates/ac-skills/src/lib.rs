//! SKILL.md skills for AC, mirroring the codex-rs skill system's
//! architecture (studied at openai/codex `codex-rs/{core-skills,skills}`,
//! Apache-2.0): skills are **injected text, not a tool**.
//!
//! Three pieces, all host-driven:
//!
//! - **Catalog** ([`catalog_markdown`]): a `## Skills` block listing every
//!   skill as `- name: description (file: /abs/path/SKILL.md)` plus usage
//!   instructions, appended once per context window (AC hosts put it in the
//!   system prompt). The model is told to *read the listed file itself* with
//!   its normal tools when a skill matches the task — progressive disclosure
//!   by path, no load_skill round-trip.
//! - **Mentions** ([`extract_skill_mentions`], [`select_skills_for_mentions`]):
//!   `$skill-name` in user text (or the linked `[$name](path)` form) selects
//!   skills explicitly. A plain name only matches when unambiguous; env-var
//!   lookalikes (`$PATH`, `$HOME`, …) never match.
//! - **Injection** ([`build_skill_injections`], [`SkillInjection::render`]):
//!   each selected skill's SKILL.md is read host-side and wrapped in a
//!   `<skill><name>…</name><path>…</path>…</skill>` block the host adds to
//!   that turn's input. Per-turn only — skills don't persist across turns
//!   unless re-mentioned.
//!
//! Discovery ([`SkillsResolver`]) walks layer roots recursively (bounded
//! depth) for `SKILL.md` files. The frontmatter dialect stays deliberately
//! tiny — single-line `key: value` scalars, bare or quoted; anything richer
//! rejects the skill with a reason instead of risking a value a real YAML
//! parser would read differently. `name` falls back to the directory name;
//! `description` is required. Duplicate names are allowed (they are only
//! unreachable by *plain* mention); duplicate paths dedupe to the earlier
//! layer.
//!
//! What this deliberately does not do, matching codex: no per-skill
//! permission or sandbox widening (hosts that contain reads grant their
//! skills roots read access up front — skill use must never widen a policy),
//! no allowed-tools enforcement, no execution surface — a skill only ever
//! becomes text in context, and its `scripts/` run through the host's
//! ordinary, already-sandboxed tools.
//!
//! **Layer roots are a trust boundary.** Descriptions flow verbatim into the
//! catalog — typically the system prompt — and bodies into turn input.
//! Nothing here sanitizes that content: point layers only at directories the
//! host trusts as much as its own prompts.

mod catalog;
mod frontmatter;
mod inject;
mod mention;
mod resolver;

pub use catalog::catalog_markdown;
pub use frontmatter::{Frontmatter, FrontmatterError, parse as parse_frontmatter};
pub use inject::{SkillInjection, build_skill_injections};
pub use mention::{SkillMention, extract_skill_mentions, select_skills_for_mentions};
pub use resolver::{
    Listing, LoadError, MAX_BODY_BYTES, Skill, SkillLayer, SkillsResolver, SkippedSkill,
    read_skill_text,
};
