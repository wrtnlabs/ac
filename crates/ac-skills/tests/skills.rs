//! Resolver + tool behavior over real temp directories, including a
//! `load_skill` run through a real [`ToolRegistry`].

use std::path::Path;
use std::sync::Arc;

use ac_skills::{LoadSkillTool, MAX_BODY_BYTES, SkillLayer, SkillsResolver};
use ac_tool::{Capability, SubtreePolicy, ToolCtx, ToolRegistry};

fn write_skill_md(root: &Path, dir: &str, content: &str) {
    let skill_dir = root.join(dir);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
}

fn write_skill(root: &Path, dir: &str, frontmatter_name: &str, description: &str) {
    write_skill_md(
        root,
        dir,
        &format!(
            "---\nname: {frontmatter_name}\ndescription: {description}\n---\nInstructions for {frontmatter_name}.\n"
        ),
    );
}

fn resolver_over(root: &Path) -> SkillsResolver {
    SkillsResolver::new(vec![SkillLayer {
        name: "project".into(),
        root: root.to_path_buf(),
    }])
}

#[test]
fn listing_validates_candidates_and_reports_skips() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "good", "good", "A valid skill.");
    write_skill(
        dir.path(),
        "mismatch",
        "other-name",
        "Name differs from dir.",
    );
    write_skill(dir.path(), "Bad-Case", "Bad-Case", "Uppercase name.");
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    write_skill_md(
        dir.path(),
        "multi",
        "---\nname: multi\ndescription: |\n  block\n---\nbody\n",
    );
    std::fs::write(dir.path().join("stray.md"), "a file, not a candidate").unwrap();

    let listing = resolver_over(dir.path()).list();
    assert_eq!(listing.skills.len(), 1);
    assert_eq!(listing.skills[0].name, "good");
    assert_eq!(listing.skills[0].description, "A valid skill.");
    assert_eq!(listing.skills[0].layer, "project");
    assert_eq!(listing.skills[0].dir, dir.path().join("good"));

    assert_eq!(listing.skipped.len(), 4);
    let reason_for = |d: &str| {
        listing
            .skipped
            .iter()
            .find(|s| s.dir.ends_with(d))
            .unwrap()
            .reason
            .clone()
    };
    assert!(reason_for("mismatch").contains("does not match the directory name"));
    assert!(reason_for("Bad-Case").contains("invalid skill name"));
    assert!(reason_for("empty").contains("no SKILL.md"));
    assert!(reason_for("multi").contains("block scalar"));
}

#[test]
fn overlong_description_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "wordy", "wordy", &"d".repeat(1025));
    let listing = resolver_over(dir.path()).list();
    assert!(listing.skills.is_empty());
    assert_eq!(listing.skipped.len(), 1);
    assert!(listing.skipped[0].reason.contains("1024"));
}

#[test]
fn missing_layer_root_yields_no_skills() {
    let dir = tempfile::tempdir().unwrap();
    let resolver = SkillsResolver::new(vec![SkillLayer {
        name: "user".into(),
        root: dir.path().join("does-not-exist"),
    }]);
    let listing = resolver.list();
    assert!(listing.skills.is_empty());
    assert!(listing.skipped.is_empty());
}

#[test]
fn earlier_layer_shadows_later() {
    let user = tempfile::tempdir().unwrap();
    let bundled = tempfile::tempdir().unwrap();
    write_skill(user.path(), "shared", "shared", "User copy.");
    write_skill(bundled.path(), "shared", "shared", "Bundled copy.");
    write_skill(bundled.path(), "extra", "extra", "Only bundled.");

    let resolver = SkillsResolver::new(vec![
        SkillLayer {
            name: "user".into(),
            root: user.path().to_path_buf(),
        },
        SkillLayer {
            name: "bundled".into(),
            root: bundled.path().to_path_buf(),
        },
    ]);
    let listing = resolver.list();
    assert_eq!(listing.skills.len(), 2);
    let shared = listing.skills.iter().find(|s| s.name == "shared").unwrap();
    assert_eq!(shared.layer, "user");
    assert_eq!(shared.description, "User copy.");
    assert!(listing.skills.iter().any(|s| s.name == "extra"));

    assert_eq!(listing.skipped.len(), 1);
    assert!(listing.skipped[0].reason.contains("shadowed"));
    assert!(listing.skipped[0].dir.starts_with(bundled.path()));

    assert_eq!(
        resolver.load("shared").unwrap(),
        "Instructions for shared.\n"
    );
    assert_eq!(
        resolver.resolve("shared").unwrap().dir,
        user.path().join("shared")
    );
}

#[test]
fn traversal_shaped_names_resolve_to_nothing() {
    let base = tempfile::tempdir().unwrap();
    let skills_root = base.path().join("skills");
    write_skill(&skills_root, "real", "real", "A skill.");
    write_skill(base.path(), "evil", "evil", "Outside the layer root.");

    let resolver = resolver_over(&skills_root);
    assert!(resolver.resolve("real").is_some());
    assert!(resolver.resolve("../evil").is_none());
    assert!(
        resolver
            .resolve(&base.path().join("evil").display().to_string())
            .is_none()
    );
    assert!(resolver.load("../evil").is_err());
}

#[test]
fn load_strips_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "demo", "D");
    let body = resolver_over(dir.path()).load("demo").unwrap();
    assert_eq!(body, "Instructions for demo.\n");
}

#[test]
fn load_caps_oversized_bodies_on_a_char_boundary() {
    let dir = tempfile::tempdir().unwrap();
    // One ASCII byte then 2-byte chars: the cap offset lands mid-char, so the
    // boundary walk must back up (String::truncate would panic otherwise).
    let body = format!("x{}", "é".repeat(140 * 1024));
    write_skill_md(
        dir.path(),
        "big",
        &format!("---\nname: big\ndescription: D\n---\n{body}"),
    );
    let loaded = resolver_over(dir.path()).load("big").unwrap();
    assert!(loaded.len() < MAX_BODY_BYTES + 100);
    assert!(loaded.ends_with("[truncated: the skill body exceeded 256 KiB]"));
}

#[test]
fn catalog_markdown_lists_skills_or_none() {
    let dir = tempfile::tempdir().unwrap();
    assert!(resolver_over(dir.path()).catalog_markdown().is_none());

    write_skill(dir.path(), "alpha", "alpha", "Does alpha things.");
    write_skill(dir.path(), "beta", "beta", "Does beta things.");
    let catalog = resolver_over(dir.path()).catalog_markdown().unwrap();
    assert!(catalog.starts_with("## Skills\n"));
    assert!(catalog.contains("load_skill"));
    assert!(catalog.contains("- alpha — Does alpha things.\n"));
    assert!(catalog.contains("- beta — Does beta things.\n"));
}

#[tokio::test]
async fn load_skill_tool_runs_through_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "demo", "A demo skill.");
    let resolver = Arc::new(resolver_over(dir.path()));

    let mut registry = ToolRegistry::new();
    registry.register(LoadSkillTool::new(resolver));
    let specs = registry.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "load_skill");
    assert_eq!(
        specs[0].input_schema["properties"]["name"]["type"],
        "string"
    );
    assert_eq!(
        registry.capability("load_skill"),
        Some(Capability::ReadOnly)
    );

    let policy = SubtreePolicy::new(dir.path()).unwrap();
    let ctx = Arc::new(ToolCtx::new(Arc::new(policy)));

    let out = registry
        .run(
            "load_skill",
            serde_json::json!({ "name": "demo" }),
            ctx.clone(),
        )
        .await;
    assert!(!out.is_error);
    assert_eq!(out.content, "Instructions for demo.\n");

    let out = registry
        .run("load_skill", serde_json::json!({ "name": "nope" }), ctx)
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("unknown skill: nope"));
    assert!(out.content.contains("demo"));
}
