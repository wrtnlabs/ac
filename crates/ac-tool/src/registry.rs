use std::collections::BTreeMap;
use std::sync::Arc;

use ac_types::ToolSpec;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde_json::Value;

use crate::ctx::ToolCtx;
use crate::tool::{Capability, RawTool, Tool, ToolOutput};

trait DynTool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn capability(&self) -> Capability;
    fn run_value(&self, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput>;
}

struct Erased<T: Tool> {
    tool: Arc<T>,
    spec: ToolSpec,
}

impl<T: Tool> DynTool for Erased<T> {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn capability(&self) -> Capability {
        self.tool.capability()
    }

    fn run_value(&self, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        match serde_json::from_value::<T::Input>(input) {
            Ok(input) => self.tool.clone().run(input, ctx),
            Err(e) => {
                let message = format!("invalid input for {}: {e}", self.spec.name);
                Box::pin(std::future::ready(ToolOutput::error(message)))
            }
        }
    }
}

struct ErasedRaw<T: RawTool> {
    tool: Arc<T>,
    spec: ToolSpec,
}

impl<T: RawTool> DynTool for ErasedRaw<T> {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn capability(&self) -> Capability {
        self.tool.capability()
    }

    fn run_value(&self, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        self.tool.clone().run(input, ctx)
    }
}

/// All tools a run can see, regardless of source (built-in, host, MCP).
/// BTreeMap so spec order — what the model sees — is deterministic.
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn DynTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool, replacing any previous tool with the same name.
    pub fn register<T: Tool>(&mut self, tool: T) {
        let spec = ToolSpec {
            name: tool.name().to_string(),
            description: tool.description(),
            input_schema: input_schema::<T::Input>(),
        };
        self.tools.insert(
            spec.name.clone(),
            Arc::new(Erased {
                tool: Arc::new(tool),
                spec,
            }),
        );
    }

    /// Registers a runtime-described tool ([`RawTool`]), replacing any previous
    /// tool with the same name. The spec — including the input schema — is
    /// taken verbatim from the tool; nothing is derived.
    pub fn register_raw<T: RawTool>(&mut self, tool: T) {
        let spec = tool.spec();
        self.tools.insert(
            spec.name.clone(),
            Arc::new(ErasedRaw {
                tool: Arc::new(tool),
                spec,
            }),
        );
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.values().map(|t| t.spec().clone()).collect()
    }

    pub fn capability(&self, name: &str) -> Option<Capability> {
        self.tools.get(name).map(|t| t.capability())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Dispatches by name. An unknown tool is a model-visible error output,
    /// not a runtime failure.
    pub fn run(
        &self,
        name: &str,
        input: Value,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        match self.tools.get(name) {
            Some(tool) => tool.run_value(input, ctx),
            None => {
                let message = format!("unknown tool: {name}");
                Box::pin(std::future::ready(ToolOutput::error(message)))
            }
        }
    }
}

fn input_schema<T: JsonSchema>() -> Value {
    let mut settings = schemars::generate::SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let schema = settings.into_generator().into_root_schema_for::<T>();
    let mut value = serde_json::to_value(schema).unwrap_or_else(|_| serde_json::json!({}));
    if let Value::Object(map) = &mut value {
        map.remove("$schema");
        map.remove("title");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SubtreePolicy;
    use schemars::JsonSchema;
    use serde::Deserialize;

    #[derive(Deserialize, JsonSchema)]
    struct EchoInput {
        text: String,
    }

    struct Echo;

    impl Tool for Echo {
        type Input = EchoInput;

        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> String {
            "Echoes the input text.".into()
        }

        fn capability(&self) -> Capability {
            Capability::ReadOnly
        }

        fn run(
            self: Arc<Self>,
            input: Self::Input,
            _ctx: Arc<ToolCtx>,
        ) -> BoxFuture<'static, ToolOutput> {
            Box::pin(std::future::ready(ToolOutput::ok(input.text)))
        }
    }

    fn ctx() -> Arc<ToolCtx> {
        let dir = tempfile::tempdir().unwrap();
        let policy = SubtreePolicy::new(dir.path()).unwrap();
        Arc::new(ToolCtx::new(Arc::new(policy)))
    }

    #[tokio::test]
    async fn dispatch_and_specs() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);

        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].input_schema["type"], "object");
        assert!(specs[0].input_schema["properties"]["text"].is_object());

        let out = registry
            .run("echo", serde_json::json!({ "text": "hi" }), ctx())
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "hi");
    }

    struct RawEcho;

    impl RawTool for RawEcho {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "raw_echo".into(),
                description: "Echoes the raw input value.".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"],
                }),
            }
        }

        fn capability(&self) -> Capability {
            Capability::Mutating
        }

        fn run(
            self: Arc<Self>,
            input: Value,
            _ctx: Arc<ToolCtx>,
        ) -> BoxFuture<'static, ToolOutput> {
            Box::pin(std::future::ready(match input.get("text") {
                Some(Value::String(s)) => ToolOutput::ok(s.clone()),
                _ => ToolOutput::error("raw_echo: missing text"),
            }))
        }
    }

    #[tokio::test]
    async fn raw_tool_spec_is_verbatim_and_input_passes_through() {
        let mut registry = ToolRegistry::new();
        registry.register_raw(RawEcho);

        let specs = registry.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "raw_echo");
        assert_eq!(specs[0].input_schema["required"][0], "text");
        assert_eq!(registry.capability("raw_echo"), Some(Capability::Mutating));

        let out = registry
            .run("raw_echo", serde_json::json!({ "text": "hi" }), ctx())
            .await;
        assert!(!out.is_error);
        assert_eq!(out.content, "hi");

        // No serde validation layer on the raw path: the tool sees the value
        // verbatim and reports bad input as error data itself.
        let out = registry.run("raw_echo", serde_json::json!({}), ctx()).await;
        assert!(out.is_error);
        assert!(out.content.contains("missing text"));
    }

    #[tokio::test]
    async fn bad_input_and_unknown_tool_are_error_data() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);

        let out = registry
            .run("echo", serde_json::json!({ "nope": 1 }), ctx())
            .await;
        assert!(out.is_error);

        let out = registry.run("missing", serde_json::json!({}), ctx()).await;
        assert!(out.is_error);
        assert!(out.content.contains("unknown tool"));
    }
}
