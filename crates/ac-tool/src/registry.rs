use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ac_types::ToolSpec;
use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde_json::Value;

use crate::ctx::ToolCtx;
use crate::tool::{Capability, RawTool, Tool, ToolOutput};

/// Optional per-run barrier for tools that publish shared host state.
///
/// Ordinary calls take a shared lease and retain their existing concurrency.
/// A host-declared exclusive tool takes the write lease for its complete tool
/// future, so no sibling tool can observe a partially published transition.
/// Names are host values: the mechanism has no application-specific tools.
pub struct ToolDispatchGate {
    lock: tokio::sync::RwLock<()>,
    exclusive: BTreeSet<String>,
}

impl ToolDispatchGate {
    pub fn new(exclusive: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            lock: tokio::sync::RwLock::new(()),
            exclusive: exclusive.into_iter().map(Into::into).collect(),
        }
    }

    fn is_exclusive(&self, name: &str) -> bool {
        self.exclusive.contains(name)
    }
}

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

    /// Retain tools selected by their declared capability.
    ///
    /// Hosts should compose name-based [`ToolScope`](crate::ToolScope) while
    /// registering a child surface, then use this filter for effect modes.
    /// This keeps both typed and [`RawTool`] registrations on the same
    /// authority path and makes unknown future tools fail closed naturally.
    pub fn retain_capabilities(&mut self, mut keep: impl FnMut(&str, Capability) -> bool) {
        self.tools
            .retain(|name, tool| keep(name, tool.capability()));
    }

    /// Drop mutating tools while retaining read-only and policy-guarded tools.
    pub fn retain_for_read_only(&mut self) {
        self.retain_capabilities(|_, capability| capability.allowed_in_read_only());
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
            Some(tool) => {
                let tool = tool.clone();
                let name = name.to_string();
                Box::pin(async move {
                    let Some(gate) = ctx.extensions.get::<ToolDispatchGate>() else {
                        return tool.run_value(input, ctx).await;
                    };
                    if gate.is_exclusive(&name) {
                        let _lease = gate.lock.write().await;
                        tool.run_value(input, ctx).await
                    } else {
                        let _lease = gate.lock.read().await;
                        tool.run_value(input, ctx).await
                    }
                })
            }
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
    use tokio::sync::Notify;

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

    struct GuardedRawEcho;

    impl RawTool for GuardedRawEcho {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "guarded_echo".into(),
                description: "A policy-guarded raw tool.".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }
        }

        fn capability(&self) -> Capability {
            Capability::Guarded
        }

        fn run(
            self: Arc<Self>,
            input: Value,
            _ctx: Arc<ToolCtx>,
        ) -> BoxFuture<'static, ToolOutput> {
            Box::pin(std::future::ready(ToolOutput::ok(input.to_string())))
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

    #[test]
    fn read_only_filter_uses_typed_and_raw_capabilities() {
        let mut registry = ToolRegistry::new();
        registry.register(Echo);
        registry.register_raw(RawEcho);
        registry.register_raw(GuardedRawEcho);

        registry.retain_for_read_only();

        assert!(registry.contains("echo"));
        assert!(registry.contains("guarded_echo"));
        assert!(!registry.contains("raw_echo"));
    }

    struct DispatchProbe {
        name: &'static str,
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl RawTool for DispatchProbe {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: "Dispatch-gate test probe.".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }
        }

        fn capability(&self) -> Capability {
            Capability::ReadOnly
        }

        fn run(
            self: Arc<Self>,
            _input: Value,
            _ctx: Arc<ToolCtx>,
        ) -> BoxFuture<'static, ToolOutput> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                ToolOutput::ok(self.name)
            })
        }
    }

    #[tokio::test]
    async fn an_exclusive_tool_waits_for_siblings_and_blocks_new_siblings() {
        let shared_entered = Arc::new(Notify::new());
        let shared_release = Arc::new(Notify::new());
        let exclusive_entered = Arc::new(Notify::new());
        let exclusive_release = Arc::new(Notify::new());
        let late_entered = Arc::new(Notify::new());
        let late_release = Arc::new(Notify::new());

        let mut registry = ToolRegistry::new();
        registry.register_raw(DispatchProbe {
            name: "shared",
            entered: shared_entered.clone(),
            release: shared_release.clone(),
        });
        registry.register_raw(DispatchProbe {
            name: "publish",
            entered: exclusive_entered.clone(),
            release: exclusive_release.clone(),
        });
        registry.register_raw(DispatchProbe {
            name: "late",
            entered: late_entered.clone(),
            release: late_release.clone(),
        });
        let registry = Arc::new(registry);
        let ctx = ctx();
        ctx.extensions.insert(ToolDispatchGate::new(["publish"]));

        let shared = {
            let registry = registry.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { registry.run("shared", serde_json::json!({}), ctx).await })
        };
        shared_entered.notified().await;

        let exclusive = {
            let registry = registry.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { registry.run("publish", serde_json::json!({}), ctx).await })
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                exclusive_entered.notified()
            )
            .await
            .is_err(),
            "exclusive publication entered while a sibling held a shared lease"
        );
        shared_release.notify_one();
        assert!(!shared.await.unwrap().is_error);
        exclusive_entered.notified().await;

        let late = {
            let registry = registry.clone();
            let ctx = ctx.clone();
            tokio::spawn(async move { registry.run("late", serde_json::json!({}), ctx).await })
        };
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                late_entered.notified()
            )
            .await
            .is_err(),
            "a new sibling entered during exclusive publication"
        );
        exclusive_release.notify_one();
        assert!(!exclusive.await.unwrap().is_error);
        late_entered.notified().await;
        late_release.notify_one();
        assert!(!late.await.unwrap().is_error);
    }
}
