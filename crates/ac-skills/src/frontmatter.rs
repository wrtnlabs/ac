//! YAML frontmatter parsing for agentskills `SKILL.md` files.
//!
//! The parser accepts the shapes used by real agentskills manifests: quoted
//! and bare scalars, block scalars, block and flow sequences, and nested
//! mappings. Values are retained as JSON-shaped data so hosts can consume
//! standard fields such as `allowed-tools` and `metadata` without reparsing
//! the file. YAML anchors, aliases, and tags are rejected: a skill manifest
//! should be self-contained data, not an executable YAML graph.

use serde_json::{Map, Value};
use serde_yaml::Value as YamlValue;

/// A parsed SKILL.md: frontmatter fields plus the markdown body that follows
/// the closing `---` (borrowed verbatim from the input).
#[derive(Debug)]
pub struct Frontmatter<'a> {
    /// All frontmatter fields, unknown keys included.
    pub fields: Map<String, Value>,
    pub body: &'a str,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrontmatterError {
    #[error("missing frontmatter: the file must open with '---' on its first line")]
    Missing,
    #[error("unterminated frontmatter: no closing '---' delimiter")]
    Unterminated,
    #[error("invalid YAML frontmatter: {message}")]
    InvalidYaml { message: String },
    #[error("frontmatter must be a YAML mapping")]
    NotMapping,
    #[error(
        "frontmatter line {line} uses a YAML anchor, alias, or tag; manifests must be self-contained"
    )]
    YamlReference { line: usize },
    #[error("frontmatter mapping keys must be strings")]
    NonStringKey,
}

/// Parse a SKILL.md document into rich, JSON-shaped YAML fields and a
/// verbatim markdown body. An optional UTF-8 BOM and CRLF endings are
/// tolerated.
pub fn parse(text: &str) -> Result<Frontmatter<'_>, FrontmatterError> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    let (yaml_src, body) = split_frontmatter(text)?;
    reject_yaml_references(yaml_src)?;
    let yaml: YamlValue =
        serde_yaml::from_str(yaml_src).map_err(|error| FrontmatterError::InvalidYaml {
            message: error.to_string(),
        })?;
    let value = yaml_to_json(yaml)?;
    let fields = value
        .as_object()
        .cloned()
        .ok_or(FrontmatterError::NotMapping)?;
    Ok(Frontmatter { fields, body })
}

/// Match the Agent Skills markdown convention: the opening delimiter must be
/// the first line and the first `\n---` terminates YAML. The body is returned
/// without normalizing line endings.
fn split_frontmatter(md: &str) -> Result<(&str, &str), FrontmatterError> {
    let rest = md
        .strip_prefix("---\r\n")
        .or_else(|| md.strip_prefix("---\n"))
        .ok_or(FrontmatterError::Missing)?;
    let idx = rest.find("\n---").ok_or(FrontmatterError::Unterminated)?;
    let yaml_end = if idx > 0 && rest.as_bytes()[idx - 1] == b'\r' {
        idx - 1
    } else {
        idx
    };
    let after = idx + "\n---".len();
    let body_start = if rest[after..].starts_with("\r\n") {
        after + 2
    } else if rest[after..].starts_with('\n') {
        after + 1
    } else {
        after
    };
    Ok((&rest[..yaml_end], &rest[body_start..]))
}

/// serde_yaml resolves anchors and aliases, so reject those tokens before
/// deserialization. Quoted text, comments, and block-scalar content are not
/// scanned as YAML syntax.
fn reject_yaml_references(src: &str) -> Result<(), FrontmatterError> {
    let mut block_parent_indent: Option<usize> = None;
    for (index, raw) in src.lines().enumerate() {
        let content = raw.trim_start_matches(' ');
        let indent = raw.len() - content.len();

        if let Some(parent) = block_parent_indent {
            if content.is_empty() || indent > parent {
                continue;
            }
            block_parent_indent = None;
        }

        if line_starts_block_scalar(content) {
            block_parent_indent = Some(indent);
        }
        if contains_yaml_reference_token(content) {
            return Err(FrontmatterError::YamlReference { line: index + 2 });
        }
    }
    Ok(())
}

fn line_starts_block_scalar(line: &str) -> bool {
    let Some((_, value)) = line.split_once(':') else {
        return false;
    };
    let value = value.trim_start();
    let Some(rest) = value.strip_prefix(['|', '>']) else {
        return false;
    };
    rest.split_once('#')
        .map(|(head, _)| head)
        .unwrap_or(rest)
        .trim()
        .chars()
        .all(|c| matches!(c, '+' | '-' | '0'..='9'))
}

fn contains_yaml_reference_token(line: &str) -> bool {
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut escaped = false;
    let mut previous: Option<char> = None;
    let chars: Vec<char> = line.chars().collect();

    for (index, &current) in chars.iter().enumerate() {
        if double_quoted {
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == '"' {
                double_quoted = false;
            }
            previous = Some(current);
            continue;
        }
        if single_quoted {
            if current == '\'' {
                if chars.get(index + 1) == Some(&'\'') {
                    previous = Some(current);
                    continue;
                }
                single_quoted = false;
            }
            previous = Some(current);
            continue;
        }
        match current {
            '"' => double_quoted = true,
            '\'' => single_quoted = true,
            '#' if previous.is_none_or(char::is_whitespace) => break,
            '&' | '*' | '!'
                if previous
                    .is_none_or(|c| c.is_whitespace() || matches!(c, '[' | '{' | ',' | ':')) =>
            {
                return true;
            }
            _ => {}
        }
        previous = Some(current);
    }
    false
}

fn yaml_to_json(value: YamlValue) -> Result<Value, FrontmatterError> {
    match value {
        YamlValue::Null => Ok(Value::Null),
        YamlValue::Bool(value) => Ok(Value::Bool(value)),
        YamlValue::Number(value) => {
            serde_json::to_value(value).map_err(|error| FrontmatterError::InvalidYaml {
                message: error.to_string(),
            })
        }
        YamlValue::String(value) => Ok(Value::String(value)),
        YamlValue::Sequence(values) => values
            .into_iter()
            .map(yaml_to_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        YamlValue::Mapping(values) => {
            let mut out = Map::new();
            for (key, value) in values {
                let YamlValue::String(key) = key else {
                    return Err(FrontmatterError::NonStringKey);
                };
                out.insert(key, yaml_to_json(value)?);
            }
            Ok(Value::Object(out))
        }
        YamlValue::Tagged(_) => Err(FrontmatterError::InvalidYaml {
            message: "YAML tags are not supported".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_plain_scalars_and_body() {
        let fm = parse("---\nname: demo\ndescription: A demo.\n---\nBody line.\n").unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.fields["description"], "A demo.");
        assert_eq!(fm.body, "Body line.\n");
    }

    #[test]
    fn parses_real_agentskills_yaml_shapes() {
        let fm = parse(
            "---\nname: reports\ndescription: >-\n  A folded\n  description\nallowed-tools:\n- read_file\n- shell\nmetadata:\n  tags: [\"alpha\", 'beta']\n  nested: { enabled: true, weight: 3 }\n---\nBody\n",
        )
        .unwrap();
        assert_eq!(fm.fields["description"], "A folded description");
        assert_eq!(fm.fields["allowed-tools"], json!(["read_file", "shell"]));
        assert_eq!(
            fm.fields["metadata"],
            json!({
                "tags": ["alpha", "beta"],
                "nested": { "enabled": true, "weight": 3 }
            })
        );
        assert_eq!(fm.body, "Body\n");
    }

    #[test]
    fn quoted_values_and_comments_are_yaml_correct() {
        let fm = parse(
            "---\nname: \"demo\"\ndescription: 'it''s # literal' # ignored\nlicense: MIT\n---\n",
        )
        .unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.fields["description"], "it's # literal");
        assert_eq!(fm.fields["license"], "MIT");
    }

    #[test]
    fn bom_and_crlf_are_tolerated() {
        let text = "\u{FEFF}---\r\nname: demo\r\ndescription: D\r\n---\r\nBody\r\n";
        let fm = parse(text).unwrap();
        assert_eq!(fm.fields["name"], "demo");
        assert_eq!(fm.body, "Body\r\n");
    }

    #[test]
    fn missing_unterminated_and_non_mapping_are_detailed() {
        assert_eq!(parse("# markdown").unwrap_err(), FrontmatterError::Missing);
        assert_eq!(
            parse("---\nname: demo").unwrap_err(),
            FrontmatterError::Unterminated
        );
        assert_eq!(
            parse("---\n- item\n---\n").unwrap_err(),
            FrontmatterError::NotMapping
        );
    }

    #[test]
    fn malformed_yaml_and_non_string_keys_are_rejected() {
        assert!(matches!(
            parse("---\nname: [unterminated\n---\n").unwrap_err(),
            FrontmatterError::InvalidYaml { .. }
        ));
        assert_eq!(
            parse("---\n1: value\n---\n").unwrap_err(),
            FrontmatterError::NonStringKey
        );
    }

    #[test]
    fn anchors_aliases_and_tags_are_rejected_but_block_text_is_not() {
        for value in ["&anchor value", "*alias", "!!str value", "!tag value"] {
            let text = format!("---\nname: demo\ndescription: {value}\n---\n");
            assert!(matches!(
                parse(&text).unwrap_err(),
                FrontmatterError::YamlReference { .. }
            ));
        }
        let fm = parse("---\nname: demo\ndescription: |-\n  Markdown *emphasis* and R&D!\n---\n")
            .unwrap();
        assert_eq!(fm.fields["description"], "Markdown *emphasis* and R&D!");
    }
}
