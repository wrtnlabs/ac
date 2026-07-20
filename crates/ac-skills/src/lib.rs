//! SKILL.md (agentskills.io) parser, layered skill resolver, and the
//! `load_skill` built-in tool.
//!
//! A skill is a directory whose `SKILL.md` opens with `---`-fenced
//! frontmatter (`name`, `description`; unknown keys tolerated) over a
//! markdown body of instructions. Hosts describe where skills live as
//! [`SkillLayer`]s in precedence order (e.g. user over project over
//! bundled); [`SkillsResolver`] scans them fresh on every call. The
//! directory name is the skill's identity, an earlier layer shadows a
//! later one, and every candidate that doesn't make the listing is
//! reported in [`Listing::skipped`] with a reason — never dropped
//! silently. [`SkillsResolver::catalog_markdown`] renders the
//! system-prompt block advertising the skills, and [`LoadSkillTool`] is
//! the read-only tool the model calls to pull one skill's body into
//! context.
//!
//! The frontmatter dialect is deliberately tiny: single-line
//! `key: value` scalars, bare or quoted. Anything richer — block
//! scalars, flow collections, nested mappings — rejects the skill with
//! a reason instead of risking a value a real YAML parser would read
//! differently.
//!
//! **Layer roots are a trust boundary.** A skill's description flows
//! verbatim into [`SkillsResolver::catalog_markdown`] — typically the
//! system prompt — and its body enters model context verbatim on load.
//! Nothing here sanitizes that content: point layers only at
//! directories the host trusts as much as its own prompts, never at
//! e.g. a fetched repository's tree.

mod frontmatter;
mod resolver;
mod tool;

pub use frontmatter::{Frontmatter, FrontmatterError, parse as parse_frontmatter};
pub use resolver::{
    Listing, LoadError, MAX_BODY_BYTES, Skill, SkillLayer, SkillsResolver, SkippedSkill,
};
pub use tool::{LoadSkillInput, LoadSkillTool};
