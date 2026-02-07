//! ClipboardProvider trait — system clipboard read/write abstraction.
//!
//! See CONTRACT_RESOLVER.md §ClipboardProvider.

use super::ResolverError;

/// Reads and writes the system clipboard.
///
/// Platform adapters implement this trait to abstract clipboard access.
/// The broker's clipboard sink delegates to `write()` instead of
/// manipulating platform-specific clipboard mechanisms directly.
///
/// `Send + Sync` is required because the broker may invoke clipboard
/// operations from async task contexts.
// Wired into broker sink in PR 4.
#[allow(dead_code)]
pub trait ClipboardProvider: Send + Sync {
    /// Set the system clipboard content to the given bytes.
    fn write(&self, content: &[u8]) -> Result<(), ResolverError>;

    /// Read the current system clipboard content.
    fn read(&self) -> Result<Vec<u8>, ResolverError>;
}
