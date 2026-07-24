//! The pre-overwrite snapshot seam. Hosts that keep history (undo rings,
//! backups) need the prior contents of a file *before* a mutating tool
//! replaces them; this trait is where the kit hands those bytes over.

use std::path::Path;

/// Called by mutating file tools just before an EXISTING file is overwritten,
/// with that file's current bytes. New-file creation never invokes it.
///
/// Install one by inserting an `Arc<dyn WriteObserver>` into
/// `ToolCtx::extensions`; with no observer registered the write tools behave
/// exactly as before.
///
/// An `Err` ABORTS the write and reaches the model as tool-error data: a host
/// that injects a snapshot hook is promising history durability, and a failed
/// snapshot must not silently lose the prior content.
///
/// The method is sync and called inline on the tool's async path — snapshot
/// hooks are expected to be quick local persistence; a host needing heavier
/// work should enqueue it internally and return.
pub trait WriteObserver: Send + Sync {
    fn before_overwrite(&self, path: &Path, prior: &[u8]) -> Result<(), String>;
}
