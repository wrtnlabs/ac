//! The tool system: typed [`Tool`] trait, type-erased registry, JSON-schema
//! spec generation (schemars), and the run context tools receive — including
//! the [`PathPolicy`] seam (hosts decide *where* tools may act), typed
//! [`Extensions`], and per-run read-before-write [`FileTimes`].

mod ctx;
mod policy;
mod registry;
mod sandbox;
mod tool;

pub use ctx::{Extensions, FileTimes, PathLocks, ToolCtx, WriteCheck};
pub use policy::{PathPolicy, PolicyError, ReadOnlyPolicy, SplitPolicy, SubtreePolicy, SwapPolicy};
pub use registry::ToolRegistry;
pub use sandbox::{
    CommandSpec, NetworkMode, Prepared, ResourceLimits, SandboxError, SandboxLauncher, SandboxMode,
    SandboxPolicy, default_deny_paths,
};
pub use tool::{Capability, RawTool, Tool, ToolOutput};
