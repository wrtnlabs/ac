//! The context architecture — [docs/ac-context.md]. A runtime does not only
//! relay what the user typed; it *injects* — a skills catalog, standing
//! instructions, mention-selected documents, ambient state. This crate is the
//! kit-owned machinery for doing that without the four naive-injection failures:
//!
//! - **Recognition (R1).** Machine-injected text carries in-band markers, so
//!   `injected(t)` is decidable from an item's text alone — under persistence,
//!   replay, and forking, with no side table ([`fragment`], [`registry`]).
//! - **Change-detected state (R3).** A reactive section emits only when what the
//!   model would be told differs from what it was last told ([`state`]).
//! - **Budgeted catalogs (R4).** A catalog is rendered to fit a budget,
//!   degrading in a fixed lawful order and never silently ([`catalog`]).
//!
//! The crate is pure: no loop, no provider, no I/O. It knows `ac_types::Role`
//! and nothing else. The cadence *drivers* that decide *when* to inject (window
//! establishment, per-turn mentions, per-turn reactive evaluation) are the
//! integration layer a host wires over this machinery — §8 of the RFC.

mod catalog;
mod fragment;
mod registry;
mod state;

pub use catalog::{CatalogEntry, CatalogRender, CatalogReport, DegradationLevel, render_catalog};
pub use fragment::{Cadence, FragmentClass, Rendered};
pub use registry::FragmentRegistry;
pub use state::{Decision, Prior, decide};
