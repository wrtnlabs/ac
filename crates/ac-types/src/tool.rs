use serde::{Deserialize, Serialize};

/// What the model sees for one tool: name, description, and a JSON Schema for
/// its input. Produced by the tool layer (later via schemars), consumed by
/// wire crates when encoding requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}
