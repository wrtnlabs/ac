//! The tool system: typed [`Tool`] trait, type-erased registry, JSON-schema
//! spec generation (schemars), and the run context tools receive — including
//! the [`PathPolicy`] seam (hosts decide *where* tools may act), typed
//! [`Extensions`], and per-run read-before-write [`FileTimes`].

mod agent;
mod ctx;
mod observer;
mod policy;
mod registry;
mod sandbox;
mod spec_override;
mod tool;

pub use ac_types::Effort;
pub use agent::{
    AgentDefinition, AgentSpawner, RefusingSpawner, SpawnRequest, SpawnResult, SpawnStatus,
    ToolScope, as_dyn,
};
pub use ctx::{
    Extensions, FileSnapshot, FileTimeError, FileTimes, PathLocks, ToolCtx, WriteCheck,
    file_time_key, iso8601_ms, lexical_normalize,
};
pub use observer::WriteObserver;
pub use policy::{
    AuthorizedPath, DenyPolicy, GrantedReadPolicy, PathPolicy, PolicyError, PrefixRemapPolicy,
    ReadGrants, ReadOnlyPolicy, SplitPolicy, SubtreePolicy, SwapPolicy,
};
pub use registry::{ToolDispatchGate, ToolRegistry};
pub use sandbox::{
    CommandSpec, NetworkMode, Prepared, ResourceLimits, SandboxError, SandboxLauncher, SandboxMode,
    SandboxPolicy, WriteDenyRule, default_deny_paths,
};
pub use spec_override::RawToolSpecOverride;
pub use tool::{Capability, RawTool, Tool, ToolOutput, ToolOutputPart};
