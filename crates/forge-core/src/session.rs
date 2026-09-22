use crate::error::ForgeError;
use crate::events::Event;

/// Append-only store for run/session events; the basis for resume, replay,
/// inspection, and server streaming.
///
/// The store owns sequence-number assignment: `append` stamps `event.seq`
/// with the next monotonic per-run number and returns the stored event.
pub trait SessionStore: Send + Sync {
    fn append(&self, event: Event) -> Result<Event, ForgeError>;

    fn events(&self) -> Result<Vec<Event>, ForgeError>;
}
