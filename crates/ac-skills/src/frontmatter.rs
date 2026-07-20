//! The SKILL.md frontmatter dialect: a `---`-fenced block of single-line
//! `key: value` scalars ahead of the markdown body.
//!
//! Deliberately not a YAML parser. Supporting exactly the scalar subset means
//! anything richer — block scalars, flow collections, nested mappings,
//! anchors/aliases/tags, multiline quoted strings — is rejected with a reason
//! instead of being silently read as a different value than a YAML parser
//! would produce. The one YAML behavior mirrored inside bare scalars: an
//! inline ` #` comment ends the value, exactly as YAML reads it.

use std::collections::BTreeMap;

/// A parsed SKILL.md: frontmatter fields plus the markdown body that follows
/// the closing `---` (borrowed from the input, frontmatter stripped).
#[derive(Debug)]
pub struct Frontmatter<'a> {
    /// All frontmatter fields, unknown keys included — callers pick the ones
    /// they understand and tolerate the rest.
    pub fields: BTreeMap<String, String>,
    pub body: &'a str,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrontmatterError {
    #[error("missing frontmatter: the file must open with '---' on its first line")]
    Missing,
    #[error("unterminated frontmatter: no closing '---' line")]
    Unterminated,
    #[error("frontmatter line {line} is not a single-line 'key: value' scalar: {text:?}")]
    NotScalar { line: usize, text: String },
    #[error(
        "frontmatter line {line} is indented: multiline and nested YAML values are not supported"
    )]
    Nested { line: usize },
    #[error(
        "frontmatter key {key:?} uses a block scalar ('|' or '>'): multiline values are not supported"
    )]
    BlockScalar { key: String },
    #[error(
        "frontmatter key {key:?} uses a flow collection ('[' or '{{'): nested values are not supported; quote the value if it is meant literally"
    )]
    FlowCollection { key: String },
    #[error(
        "frontmatter key {key:?} uses a YAML anchor, alias, or tag ('&', '*', '!'): not supported; quote the value if it is meant literally"
    )]
    YamlIndicator { key: String },
    #[error("frontmatter key {key:?} has a malformed quoted value")]
    BadQuote { key: String },
    #[error("duplicate frontmatter key {key:?}")]
    Duplicate { key: String },
}

/// Parse `text` as SKILL.md: optional UTF-8 BOM, `---`, `key: value` lines,
/// closing `---`, markdown body. CRLF endings are tolerated; blank lines and
/// full-line `#` comments inside the frontmatter are ignored. Values may be
/// bare or single/double-quoted.
pub fn parse(text: &str) -> Result<Frontmatter<'_>, FrontmatterError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let mut lines = Lines { rest: Some(text) };
    if lines.next_line() != Some("---") {
        return Err(FrontmatterError::Missing);
    }

    let mut fields = BTreeMap::new();
    let mut line_no = 1usize;
    loop {
        let Some(line) = lines.next_line() else {
            return Err(FrontmatterError::Unterminated);
        };
        line_no += 1;
        if line == "---" {
            break;
        }
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(FrontmatterError::Nested { line: line_no });
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(FrontmatterError::NotScalar {
                line: line_no,
                text: line.to_string(),
            });
        };
        let key = key.trim_end();
        if key.is_empty() {
            return Err(FrontmatterError::NotScalar {
                line: line_no,
                text: line.to_string(),
            });
        }
        let value = parse_scalar(key, value.trim())?;
        if fields.insert(key.to_string(), value).is_some() {
            return Err(FrontmatterError::Duplicate {
                key: key.to_string(),
            });
        }
    }

    Ok(Frontmatter {
        fields,
        body: lines.remainder(),
    })
}

fn parse_scalar(key: &str, value: &str) -> Result<String, FrontmatterError> {
    if let Some(rest) = value.strip_prefix('"') {
        return parse_double_quoted(key, rest);
    }
    if let Some(rest) = value.strip_prefix('\'') {
        return parse_single_quoted(key, rest);
    }
    if is_block_scalar_header(value) {
        return Err(FrontmatterError::BlockScalar {
            key: key.to_string(),
        });
    }
    if value.starts_with(['[', '{']) {
        return Err(FrontmatterError::FlowCollection {
            key: key.to_string(),
        });
    }
    // Anchors/aliases/tags are YAML constructs this dialect can't represent
    // faithfully — reject rather than read a different value than a YAML
    // parser would produce.
    if value.starts_with(['&', '*', '!']) {
        return Err(FrontmatterError::YamlIndicator {
            key: key.to_string(),
        });
    }
    // A bare scalar ends where a YAML inline comment starts: a `#` preceded by
    // whitespace (or a value that IS a comment, which YAML reads as empty).
    // Keeping the comment text would silently diverge from every YAML parser.
    let value = match bare_comment_start(value) {
        Some(i) => value[..i].trim_end(),
        None => value,
    };
    Ok(value.to_string())
}

/// Byte offset where a YAML inline comment begins inside a bare scalar.
fn bare_comment_start(value: &str) -> Option<usize> {
    if value.starts_with('#') {
        return Some(0);
    }
    let mut prev_is_space = false;
    for (i, c) in value.char_indices() {
        if c == '#' && prev_is_space {
            return Some(i);
        }
        prev_is_space = c.is_whitespace();
    }
    None
}

/// `|` or `>` optionally followed by YAML chomping/indent indicators — the
/// headers that introduce a multiline block scalar.
fn is_block_scalar_header(value: &str) -> bool {
    let Some(rest) = value.strip_prefix(['|', '>']) else {
        return false;
    };
    rest.chars().all(|c| matches!(c, '+' | '-' | '0'..='9'))
}

/// `rest` is everything after the opening `"`. Only `\"` and `\\` escapes are
/// recognized; any other escape, a missing closing quote, or content after it
/// is malformed — better to skip the skill than to guess.
fn parse_double_quoted(key: &str, rest: &str) -> Result<String, FrontmatterError> {
    let malformed = || FrontmatterError::BadQuote {
        key: key.to_string(),
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next() {
            Some('"') => {
                return if chars.as_str().trim().is_empty() {
                    Ok(out)
                } else {
                    Err(malformed())
                };
            }
            Some('\\') => match chars.next() {
                Some(c @ ('"' | '\\')) => out.push(c),
                _ => return Err(malformed()),
            },
            Some(c) => out.push(c),
            None => return Err(malformed()),
        }
    }
}

/// `rest` is everything after the opening `'`. YAML single-quote rules: `''`
/// is a literal `'`; the first lone `'` closes the value.
fn parse_single_quoted(key: &str, rest: &str) -> Result<String, FrontmatterError> {
    let malformed = || FrontmatterError::BadQuote {
        key: key.to_string(),
    };
    let mut out = String::new();
    let mut chars = rest.chars();
    loop {
        match chars.next() {
            Some('\'') if chars.as_str().starts_with('\'') => {
                out.push('\'');
                chars.next();
            }
            Some('\'') => {
                return if chars.as_str().trim().is_empty() {
                    Ok(out)
                } else {
                    Err(malformed())
                };
            }
            Some(c) => out.push(c),
            None => return Err(malformed()),
        }
    }
}

/// Line scanner that keeps a handle on the unconsumed remainder, so the body
/// after the closing `---` comes back as one verbatim slice.
struct Lines<'a> {
    rest: Option<&'a str>,
}

impl<'a> Lines<'a> {
    fn next_line(&mut self) -> Option<&'a str> {
        let rest = self.rest?;
        match rest.find('\n') {
            Some(i) => {
                self.rest = Some(&rest[i + 1..]);
                Some(rest[..i].strip_suffix('\r').unwrap_or(&rest[..i]))
            }
            None => {
                self.rest = None;
                Some(rest)
            }
        }
    }

    fn remainder(&self) -> &'a str {
        self.rest.unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_scalars_and_body() {
        let fm = parse("---\nname: demo\ndescription: A demo.\n---\nBody line.\n").unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.fields["description"], "A demo.");
        assert_eq!(fm.body, "Body line.\n");
    }

    #[test]
    fn no_frontmatter_is_missing() {
        assert_eq!(
            parse("# Just markdown\n").unwrap_err(),
            FrontmatterError::Missing
        );
        assert_eq!(parse("").unwrap_err(), FrontmatterError::Missing);
    }

    #[test]
    fn unterminated_frontmatter() {
        assert_eq!(
            parse("---\nname: x\n").unwrap_err(),
            FrontmatterError::Unterminated
        );
    }

    #[test]
    fn bom_and_crlf_tolerated() {
        let text = "\u{FEFF}---\r\nname: demo\r\ndescription: D\r\n---\r\nBody\r\n";
        let fm = parse(text).unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.fields["description"], "D");
        assert_eq!(fm.body, "Body\r\n");
    }

    #[test]
    fn quoted_values() {
        let text = "---\nname: \"demo\"\ndescription: 'it''s quoted'\n---\n";
        let fm = parse(text).unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.fields["description"], "it's quoted");
    }

    #[test]
    fn double_quote_escapes() {
        let text = "---\nname: \"a \\\"b\\\" \\\\ c\"\n---\n";
        assert_eq!(parse(text).unwrap().fields["name"], "a \"b\" \\ c");
    }

    #[test]
    fn unknown_keys_preserved() {
        let text = "---\nname: demo\ndescription: D\nlicense: MIT\nallowed-tools: all\n---\n";
        let fm = parse(text).unwrap();
        assert_eq!(fm.fields["license"], "MIT");
        assert_eq!(fm.fields["allowed-tools"], "all");
    }

    #[test]
    fn bare_inline_comment_ends_the_value_like_yaml() {
        let text = "---\nname: demo\ndescription: Use for X reports # not for Y\n---\n";
        assert_eq!(
            parse(text).unwrap().fields["description"],
            "Use for X reports"
        );
        // A value that IS a comment reads as empty, as YAML would.
        let text = "---\nname: demo\ndescription: #tbd\n---\n";
        assert_eq!(parse(text).unwrap().fields["description"], "");
        // A '#' NOT preceded by whitespace is part of the value, as in YAML.
        let text = "---\nname: demo\ndescription: item#7 of the list\n---\n";
        assert_eq!(
            parse(text).unwrap().fields["description"],
            "item#7 of the list"
        );
        // Quoting keeps a literal '#' even after a space.
        let text = "---\nname: demo\ndescription: \"Use for X # literally\"\n---\n";
        assert_eq!(
            parse(text).unwrap().fields["description"],
            "Use for X # literally"
        );
    }

    #[test]
    fn yaml_anchors_aliases_and_tags_are_rejected() {
        for value in ["&anchor v", "*alias", "!!str v", "!tag v"] {
            let text = format!("---\nname: demo\ndescription: {value}\n---\n");
            assert_eq!(
                parse(&text).unwrap_err(),
                FrontmatterError::YamlIndicator {
                    key: "description".to_string()
                },
                "value {value:?} must be rejected"
            );
        }
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let text = "---\n# a comment\n\nname: demo\n  # indented comment\ndescription: D\n---\n";
        let fm = parse(text).unwrap();
        assert_eq!(fm.fields.len(), 2);
    }

    #[test]
    fn multiline_and_nested_values_rejected() {
        assert!(matches!(
            parse("---\ndescription: |\n  line\n---\n").unwrap_err(),
            FrontmatterError::BlockScalar { .. }
        ));
        assert!(matches!(
            parse("---\ndescription: >-\n  line\n---\n").unwrap_err(),
            FrontmatterError::BlockScalar { .. }
        ));
        assert!(matches!(
            parse("---\nmeta:\n  nested: x\n---\n").unwrap_err(),
            FrontmatterError::Nested { .. }
        ));
        assert!(matches!(
            parse("---\ntags: [a, b]\n---\n").unwrap_err(),
            FrontmatterError::FlowCollection { .. }
        ));
    }

    #[test]
    fn malformed_quotes_rejected() {
        assert!(matches!(
            parse("---\nname: \"unterminated\n---\n").unwrap_err(),
            FrontmatterError::BadQuote { .. }
        ));
        assert!(matches!(
            parse("---\nname: \"x\" trailing\n---\n").unwrap_err(),
            FrontmatterError::BadQuote { .. }
        ));
        assert!(matches!(
            parse("---\nname: \"bad \\n escape\"\n---\n").unwrap_err(),
            FrontmatterError::BadQuote { .. }
        ));
    }

    #[test]
    fn non_scalar_and_duplicate_lines_rejected() {
        assert!(matches!(
            parse("---\n- item\n---\n").unwrap_err(),
            FrontmatterError::NotScalar { .. }
        ));
        assert!(matches!(
            parse("---\nname: a\nname: b\n---\n").unwrap_err(),
            FrontmatterError::Duplicate { .. }
        ));
    }

    #[test]
    fn value_starting_with_pipe_text_is_a_plain_scalar() {
        let fm = parse("---\nname: |pipe-ish\n---\n").unwrap();
        assert_eq!(fm.fields["name"], "|pipe-ish");
    }
}
