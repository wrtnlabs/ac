//! `tool_search` — discover a relevant subset of a latent tool catalog.
//!
//! Large dynamic catalogs (MCP is the common case) should not put every schema
//! into every completion request. This tool searches host-supplied metadata and
//! returns names that `ac_runtime::ConditionalToolsHook` may expose on the
//! following step.
//!
//! The tool owns no reveal state. Its JSON result is the durable fact, so the
//! runtime can derive visibility from effective message history across resume,
//! fork, and compaction.

use std::sync::Arc;

use ac_tool::{Capability, Tool, ToolCtx, ToolOutput};
use futures::future::BoxFuture;

pub const TOOL_SEARCH_NAME: &str = "tool_search";
pub const DEFAULT_MAX_RESULTS: u32 = 10;
pub const MAX_RESULTS: u32 = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSearchEntry {
    pub name: String,
    pub description: String,
    /// Extra searchable text, such as a raw remote tool name or server label.
    /// It is never returned to the model.
    pub keywords: String,
}

impl ToolSearchEntry {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            keywords: String::new(),
        }
    }

    pub fn with_keywords(mut self, keywords: impl Into<String>) -> Self {
        self.keywords = keywords.into();
        self
    }
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ToolSearchInput {
    /// What capability you need, for example "create an issue" or "query a
    /// database".
    pub query: String,
    /// Maximum matches to return (default 10, maximum 50).
    #[serde(default)]
    #[schemars(range(min = 1, max = 50))]
    pub max: Option<u32>,
}

pub struct ToolSearch {
    catalog: Arc<Vec<ToolSearchEntry>>,
}

impl ToolSearch {
    pub fn new(catalog: impl Into<Arc<Vec<ToolSearchEntry>>>) -> Self {
        Self {
            catalog: catalog.into(),
        }
    }
}

impl Tool for ToolSearch {
    type Input = ToolSearchInput;

    fn name(&self) -> &'static str {
        TOOL_SEARCH_NAME
    }

    fn description(&self) -> String {
        "Search tools that are hidden by default to keep context small. Call \
         this first with a natural-language description of the capability you \
         need. Matching tools become available on the next step; search again \
         whenever you need a different capability."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        _ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let max = input.max.unwrap_or(DEFAULT_MAX_RESULTS);
            if !(1..=MAX_RESULTS).contains(&max) {
                return ToolOutput::error(format!(
                    "tool_search: invalid arguments: `max` must be between 1 and {MAX_RESULTS}"
                ));
            }

            let tokens: Vec<String> = input
                .query
                .to_lowercase()
                .split_whitespace()
                // Ignore one-scalar fragments in every script. Counting
                // Unicode scalar values keeps the public search contract
                // independent of any host language's string representation.
                .filter(|token| token.chars().count() >= 2)
                .map(str::to_string)
                .collect();

            let mut scored: Vec<(usize, usize, &ToolSearchEntry)> = self
                .catalog
                .iter()
                .enumerate()
                .filter_map(|(index, entry)| {
                    let haystack =
                        format!("{} {} {}", entry.name, entry.description, entry.keywords)
                            .to_lowercase();
                    let score = tokens
                        .iter()
                        .filter(|token| haystack.contains(token.as_str()))
                        .count();
                    (score > 0).then_some((score, index, entry))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

            let matched: Vec<serde_json::Value> = scored
                .into_iter()
                .take(max as usize)
                .map(|(_, _, entry)| {
                    serde_json::json!({
                        "name": entry.name,
                        "description": entry.description,
                    })
                })
                .collect();
            let note = if matched.is_empty() {
                "No matching tools. Try a different query or proceed without one."
            } else {
                "These tools are now available. Call them on your next step."
            };

            ToolOutput::ok(
                serde_json::json!({
                    "matched": matched,
                    "revealed_count": matched.len(),
                    "note": note,
                })
                .to_string(),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::{SubtreePolicy, ToolRegistry};
    use serde_json::{Value, json};

    fn registry() -> (ToolRegistry, Arc<ToolCtx>) {
        let dir = tempfile::tempdir().unwrap().keep();
        let ctx = Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(dir).unwrap())));
        let catalog = vec![
            ToolSearchEntry::new("mcp__tracker__create_item", "Create a tracker item")
                .with_keywords("tracker create_item"),
            ToolSearchEntry::new("mcp__tracker__list_items", "List tracker items")
                .with_keywords("tracker list_items"),
            ToolSearchEntry::new("mcp__pg__query", "Run a SQL query")
                .with_keywords("postgres database"),
        ];
        let mut registry = ToolRegistry::new();
        registry.register(ToolSearch::new(Arc::new(catalog)));
        (registry, ctx)
    }

    #[tokio::test]
    async fn ranks_matches_and_returns_the_durable_reveal_shape() {
        let (registry, ctx) = registry();
        let out = registry
            .run(
                TOOL_SEARCH_NAME,
                json!({"query": "create tracker", "max": 1}),
                ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let value: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["revealed_count"], 1);
        assert_eq!(value["matched"][0]["name"], "mcp__tracker__create_item");
    }

    #[tokio::test]
    async fn unusable_queries_reveal_nothing() {
        let (registry, ctx) = registry();
        let out = registry
            .run(TOOL_SEARCH_NAME, json!({"query": "a x"}), ctx)
            .await;
        let value: Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["matched"], json!([]));
        assert_eq!(value["revealed_count"], 0);
    }

    #[tokio::test]
    async fn token_length_is_unicode_native() {
        let dir = tempfile::tempdir().unwrap().keep();
        let ctx = Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(dir).unwrap())));
        let catalog = vec![
            ToolSearchEntry::new("mcp__ko__report", "표를 생성합니다"),
            ToolSearchEntry::new("mcp__emoji__grin", "😀 grinning tools"),
        ];
        let mut registry = ToolRegistry::new();
        registry.register(ToolSearch::new(Arc::new(catalog)));

        let single_bmp = registry
            .run(TOOL_SEARCH_NAME, json!({"query": "표"}), ctx.clone())
            .await;
        let single_bmp: Value = serde_json::from_str(&single_bmp.content).unwrap();
        assert_eq!(single_bmp["matched"], json!([]));

        let single_astral = registry
            .run(TOOL_SEARCH_NAME, json!({"query": "😀"}), ctx.clone())
            .await;
        let single_astral: Value = serde_json::from_str(&single_astral.content).unwrap();
        assert_eq!(single_astral["matched"], json!([]));

        let two_scalars = registry
            .run(TOOL_SEARCH_NAME, json!({"query": "표를"}), ctx)
            .await;
        let two_scalars: Value = serde_json::from_str(&two_scalars.content).unwrap();
        assert_eq!(two_scalars["revealed_count"], 1);
    }

    #[tokio::test]
    async fn max_is_bounded_even_if_a_caller_bypasses_schema_validation() {
        let (registry, ctx) = registry();
        for max in [0, 51] {
            let out = registry
                .run(
                    TOOL_SEARCH_NAME,
                    json!({"query": "tracker", "max": max}),
                    ctx.clone(),
                )
                .await;
            assert!(out.is_error);
            assert!(out.content.contains("must be between 1 and 50"));
        }
    }
}
