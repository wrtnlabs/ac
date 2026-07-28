//! Model-facing copy overrides for runtime-described tools.
//!
//! [`RawToolSpecOverride`] is a transparent [`RawTool`] decorator: it can
//! replace the tool description and selected
//! `input_schema.properties.<name>.description` strings while preserving the
//! registration name, capability, and execution behavior. Hosts can therefore
//! supply profile-specific wording without forking a tool implementation.

use std::sync::Arc;

use ac_types::ToolSpec;
use futures::future::BoxFuture;
use serde_json::Value;

use crate::{Capability, RawTool, ToolCtx, ToolOutput};

/// A [`RawTool`] with selected model-facing strings replaced.
///
/// `None` and an empty patch list make this a transparent pass-through.
/// Schema patches only replace an existing property description: an unknown
/// property, or a property without a description, remains structurally
/// unchanged.
pub struct RawToolSpecOverride<T> {
    inner: Arc<T>,
    description: Option<String>,
    schema_description_patches: Vec<(String, String)>,
}

impl<T> RawToolSpecOverride<T> {
    pub fn new(
        inner: T,
        description: Option<&str>,
        schema_description_patches: Vec<(&str, &str)>,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            description: description.map(str::to_owned),
            schema_description_patches: schema_description_patches
                .into_iter()
                .map(|(property, description)| (property.to_owned(), description.to_owned()))
                .collect(),
        }
    }
}

impl<T: RawTool> RawTool for RawToolSpecOverride<T> {
    fn spec(&self) -> ToolSpec {
        let mut spec = self.inner.spec();
        if let Some(description) = &self.description {
            spec.description.clone_from(description);
        }
        for (property, description) in &self.schema_description_patches {
            if let Some(target) = spec
                .input_schema
                .get_mut("properties")
                .and_then(|properties| properties.get_mut(property))
                .and_then(|schema| schema.get_mut("description"))
            {
                *target = Value::String(description.clone());
            }
        }
        spec
    }

    fn capability(&self) -> Capability {
        self.inner.capability()
    }

    fn run(self: Arc<Self>, input: Value, ctx: Arc<ToolCtx>) -> BoxFuture<'static, ToolOutput> {
        self.inner.clone().run(input, ctx)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::SubtreePolicy;

    struct Probe;

    impl RawTool for Probe {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "probe".to_owned(),
                description: "default tool copy".to_owned(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "default path copy"
                        },
                        "other": { "type": "string" }
                    }
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
            Box::pin(async move { ToolOutput::ok(input.to_string()) })
        }
    }

    #[test]
    fn empty_override_is_a_spec_and_capability_passthrough() {
        let wrapped = RawToolSpecOverride::new(Probe, None, vec![]);
        let got = wrapped.spec();
        let expected = Probe.spec();

        assert_eq!(got.name, expected.name);
        assert_eq!(got.description, expected.description);
        assert_eq!(got.input_schema, expected.input_schema);
        assert_eq!(wrapped.capability(), Probe.capability());
    }

    #[test]
    fn overrides_only_existing_model_facing_descriptions() {
        let wrapped = RawToolSpecOverride::new(
            Probe,
            Some("profile tool copy"),
            vec![
                ("path", "profile path copy"),
                ("other", "ignored because no description exists"),
                ("missing", "ignored because the property is absent"),
            ],
        );
        let spec = wrapped.spec();

        assert_eq!(spec.name, "probe", "the registration name never varies");
        assert_eq!(spec.description, "profile tool copy");
        assert_eq!(
            spec.input_schema["properties"]["path"]["description"],
            "profile path copy"
        );
        assert!(spec.input_schema["properties"]["other"]["description"].is_null());
        assert!(spec.input_schema["properties"]["missing"].is_null());
    }

    #[tokio::test]
    async fn delegates_execution_to_the_inner_tool() {
        let wrapped = Arc::new(RawToolSpecOverride::new(
            Probe,
            Some("profile tool copy"),
            vec![],
        ));
        let ctx = Arc::new(ToolCtx::new(Arc::new(
            SubtreePolicy::new(std::env::temp_dir()).unwrap(),
        )));

        let output = wrapped.run(json!({ "path": "x" }), ctx).await;

        assert!(!output.is_error);
        assert!(output.content.contains("\"path\":\"x\""));
    }
}
