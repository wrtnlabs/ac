//! The tool system: typed [`Tool`] trait, type-erased registry, JSON-schema
//! spec generation (schemars), and the run context tools receive — including
//! the [`PathPolicy`] seam (hosts decide *where* tools may act), typed
//! [`Extensions`], and per-run read-before-write [`FileTimes`].

mod ctx;
mod policy;
mod registry;
mod tool;

pub use ctx::{Extensions, FileTimes, PathLocks, ToolCtx, WriteCheck};
pub use policy::{PathPolicy, PolicyError, SubtreePolicy};
pub use registry::ToolRegistry;
pub use tool::{Capability, Tool, ToolOutput};
