use crate::error::ForgeError;
use crate::events::Event;

/// Append-only store for run/session events; the basis for resume, replay,
/// inspection, and server streaming.
pub trait SessionStore: Send + Sync {
    fn append(&self, event: &Event) -> Result<(), ForgeError>;

    fn events(&self) -> Result<Vec<Event>, ForgeError>;
}
