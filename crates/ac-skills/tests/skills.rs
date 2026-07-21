//! Resolver, catalog, mention, and injection behavior over real temp
//! directories — the codex-mirror surface: skills are discovered by walking
//! layer roots for SKILL.md files, advertised as a text catalog carrying
//! file paths, selected by `$mention`, and injected as `<skill>` text blocks.

use std::path::{Path, PathBuf};

use ac_skills::{
    MAX_BODY_BYTES, SkillLayer, SkillMention, SkillsResolver, build_skill_injections,
    catalog_markdown, extract_skill_mentions, read_skill_text, select_skills_for_mentions,
};

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
    // A frontmatter name differing from the directory name is fine (codex
    // semantics): the frontmatter name is the identity.
    write_skill(dir.path(), "some-dir", "other-name", "Renamed skill.");
    // No frontmatter name: the directory name is the fallback identity.
    write_skill_md(
        dir.path(),
        "dir-named",
        "---\ndescription: Named by its directory.\n---\nBody.\n",
    );
    // Invalid fallback name (uppercase) and a rejected-dialect frontmatter.
    write_skill_md(
        dir.path(),
        "Bad-Case",
        "---\ndescription: Uppercase dir fallback.\n---\nBody.\n",
    );
    write_skill_md(
        dir.path(),
        "multi",
        "---\nname: multi\ndescription: |\n  block\n---\nbody\n",
    );
    // Not candidates at all: an empty directory, a stray non-SKILL.md file.
    std::fs::create_dir(dir.path().join("empty")).unwrap();
    std::fs::write(dir.path().join("stray.md"), "not a candidate").unwrap();

    let listing = resolver_over(dir.path()).list();
    let names: Vec<&str> = listing.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["dir-named", "good", "other-name"]);

    let renamed = listing
        .skills
        .iter()
        .find(|s| s.name == "other-name")
        .unwrap();
    assert!(renamed.dir.ends_with("some-dir"));
    assert!(renamed.skill_md.ends_with("some-dir/SKILL.md"));
    assert_eq!(renamed.layer, "project");

    assert_eq!(listing.skipped.len(), 2);
    let reason_for = |d: &str| {
        listing
            .skipped
            .iter()
            .find(|s| s.dir.ends_with(d))
            .unwrap()
            .reason
            .clone()
    };
    assert!(reason_for("Bad-Case").contains("invalid skill name"));
    assert!(reason_for("multi").contains("block scalar"));
}

#[test]
fn discovery_is_recursive_with_a_depth_bound_and_skips_dot_dirs() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "top", "top", "At the root.");
    write_skill(dir.path(), "group/nested", "nested", "One level down.");
    write_skill(
        dir.path(),
        "d1/d2/d3/d4/d5/deep",
        "deep",
        "At the depth bound.",
    );
    write_skill(
        dir.path(),
        "d1/d2/d3/d4/d5/d6/toodeep",
        "toodeep",
        "Beyond the depth bound.",
    );
    write_skill(dir.path(), ".hidden/secret", "secret", "Under a dot dir.");

    let listing = resolver_over(dir.path()).list();
    let names: Vec<&str> = listing.skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["deep", "nested", "top"],
        "depth-6 nested skills load; depth-7 and dot-dir skills do not"
    );
    assert!(listing.skipped.is_empty());
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
fn duplicate_names_are_kept_and_duplicate_paths_dedupe_to_the_earlier_layer() {
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
    // Duplicate NAMES both survive (codex semantics — ambiguity is resolved
    // at mention time); resolve() returns the earlier layer's copy.
    assert_eq!(listing.skills.len(), 3);
    assert_eq!(
        listing.skills.iter().filter(|s| s.name == "shared").count(),
        2
    );
    assert_eq!(resolver.resolve("shared").unwrap().layer, "user");
    assert!(listing.skipped.is_empty());

    // The same PHYSICAL path reachable via two layers dedupes with a reason.
    let resolver = SkillsResolver::new(vec![
        SkillLayer {
            name: "a".into(),
            root: user.path().to_path_buf(),
        },
        SkillLayer {
            name: "b".into(),
            root: user.path().to_path_buf(),
        },
    ]);
    let listing = resolver.list();
    assert_eq!(listing.skills.len(), 1);
    assert_eq!(listing.skipped.len(), 1);
    assert!(listing.skipped[0].reason.contains("already listed"));
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
fn skill_fields_carry_unknown_frontmatter_keys() {
    let dir = tempfile::tempdir().unwrap();
    write_skill_md(
        dir.path(),
        "annotated",
        "---\nname: annotated\ndescription: D\nlicense: Apache-2.0\n---\nB\n",
    );
    let skill = resolver_over(dir.path()).resolve("annotated").unwrap();
    assert_eq!(
        skill.fields.get("license").map(String::as_str),
        Some("Apache-2.0")
    );
}

#[test]
fn read_skill_text_is_the_verbatim_file_with_a_size_cap() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "demo", "D");
    let resolver = resolver_over(dir.path());
    let skill = resolver.resolve("demo").unwrap();
    // Verbatim: frontmatter included — the injected text is the file as the
    // author wrote it.
    let text = read_skill_text(&skill).unwrap();
    assert!(text.starts_with("---\nname: demo\n"));
    assert!(text.ends_with("Instructions for demo.\n"));

    // One ASCII byte then 2-byte chars: the cap offset lands mid-char, so the
    // boundary walk must back up (String::truncate would panic otherwise).
    let body = format!("x{}", "é".repeat(140 * 1024));
    write_skill_md(
        dir.path(),
        "big",
        &format!("---\nname: big\ndescription: D\n---\n{body}"),
    );
    let skill = resolver.resolve("big").unwrap();
    let text = read_skill_text(&skill).unwrap();
    assert!(text.len() < MAX_BODY_BYTES + 100);
    assert!(text.ends_with("[truncated: the skill file exceeded 256 KiB]"));
}

#[test]
fn catalog_lists_skills_with_file_locators_and_usage_instructions() {
    let dir = tempfile::tempdir().unwrap();
    assert!(catalog_markdown(&resolver_over(dir.path()).list()).is_none());

    write_skill(dir.path(), "alpha", "alpha", "Does alpha things.");
    write_skill(dir.path(), "beta", "beta", "Does beta things.");
    let listing = resolver_over(dir.path()).list();
    let catalog = catalog_markdown(&listing).unwrap();
    assert!(catalog.starts_with("## Skills\n"));
    assert!(catalog.contains("### Available skills\n"));
    let alpha = listing.skills.iter().find(|s| s.name == "alpha").unwrap();
    assert!(catalog.contains(&format!(
        "- alpha: Does alpha things. (file: {})\n",
        alpha.skill_md.display()
    )));
    assert!(catalog.contains("### How to use skills\n"));
    assert!(catalog.contains("read its `SKILL.md` completely"));
    assert!(catalog.contains("Do not carry skills across turns unless re-mentioned."));
}

#[test]
fn mention_selection_requires_unambiguous_names_and_matches_paths_exactly() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    write_skill(a.path(), "unique", "unique", "Only one.");
    write_skill(a.path(), "twin", "twin", "First twin.");
    write_skill(b.path(), "twin", "twin", "Second twin.");

    let resolver = SkillsResolver::new(vec![
        SkillLayer {
            name: "a".into(),
            root: a.path().to_path_buf(),
        },
        SkillLayer {
            name: "b".into(),
            root: b.path().to_path_buf(),
        },
    ]);
    let listing = resolver.list();

    // Unique plain name selects; ambiguous and unknown plain names skip.
    let mentions = extract_skill_mentions("use $unique, $twin and $nonexistent");
    let selected = select_skills_for_mentions(&listing.skills, &mentions);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "unique");

    // The linked form disambiguates the twin by exact path.
    let twin_b = listing
        .skills
        .iter()
        .find(|s| s.name == "twin" && s.layer == "b")
        .unwrap();
    let mentions = vec![SkillMention {
        name: "twin".to_string(),
        path: Some(twin_b.skill_md.clone()),
    }];
    let selected = select_skills_for_mentions(&listing.skills, &mentions);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].skill_md, twin_b.skill_md);
}

#[test]
fn injections_render_the_codex_skill_block_and_warn_on_unreadable_files() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "demo", "D");
    let resolver = resolver_over(dir.path());
    let skill = resolver.resolve("demo").unwrap();

    let mut gone = skill.clone();
    gone.name = "gone".to_string();
    gone.skill_md = PathBuf::from("/nonexistent/SKILL.md");

    let (injections, warnings) = build_skill_injections(&[skill.clone(), gone]);
    assert_eq!(injections.len(), 1);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].starts_with("Failed to load skill gone at /nonexistent/SKILL.md:"));

    let rendered = injections[0].render();
    assert!(rendered.starts_with("<skill>\n<name>demo</name>\n<path>"));
    assert!(rendered.contains(&format!("<path>{}</path>\n---\n", skill.skill_md.display())));
    assert!(rendered.ends_with("Instructions for demo.\n\n</skill>"));
}

#[cfg(unix)]
#[test]
fn a_symlink_cycle_terminates_and_lists_each_skill_once() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "solo", "solo", "The only skill.");
    // A cycle back to the root, and an alias of the skill dir.
    std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("solo"), dir.path().join("alias")).unwrap();

    let listing = resolver_over(dir.path()).list();
    assert_eq!(listing.skills.len(), 1, "one physical skill, listed once");
    assert!(
        listing.skipped.is_empty(),
        "aliases and cycles must not surface as skips: {:?}",
        listing.skipped
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_skill_md_file_is_not_a_candidate() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "real", "real", "A real skill.");
    let fake = dir.path().join("fake");
    std::fs::create_dir_all(&fake).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real/SKILL.md"), fake.join("SKILL.md")).unwrap();

    let listing = resolver_over(dir.path()).list();
    assert_eq!(listing.skills.len(), 1);
    assert!(listing.skills[0].dir.ends_with("real"));
}
