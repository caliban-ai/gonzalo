//! The provider boundary: [`TicketSource`] (ADR 0010).
//!
//! `TicketSource` is the ticket analogue of `gonzalo_vector::Embedder` — it
//! keeps gonzalo provider-agnostic about *where* tickets come from. Phase 1 is
//! read-only (`fetch_changed` / `get`); write-back (`set_state`, `comment`) is
//! capability-gated and defaults to `Unsupported`, so a read-only mirror need
//! implement only the two readers.

use async_trait::async_trait;
use gonzalo_domain::{StateCategory, Ticket};
use thiserror::Error;

/// An opaque, per-source incremental-sync cursor — a timestamp, a JQL bound, a
/// GraphQL page cursor, or an event sync token, depending on the provider.
/// Deliberately **not** gonzalo's `Revision`: the external system owns its own
/// change watermark.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cursor(pub Option<String>);

/// What a source supports, negotiated up front rather than discovered at
/// runtime — this is what keeps the trait free of `if provider == …` branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub push: bool,
    pub transitions_required: bool,
    pub custom_fields: bool,
    pub single_assignee: bool,
    pub hierarchy: bool,
    pub relations: bool,
    pub comments: bool,
}

/// A page of changed tickets plus the cursor to resume from. Does not derive
/// `Eq` because [`Ticket`] does not (its `fields` hold `serde_json::Value`).
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    pub tickets: Vec<Ticket>,
    pub next: Cursor,
}

/// Errors a source can surface.
#[derive(Debug, Error)]
pub enum SourceError {
    /// A capability the source does not provide was requested.
    #[error("operation not supported by this source: {0}")]
    Unsupported(&'static str),
    /// A transport / backend failure, carrying the provider's message.
    #[error("ticket source backend error: {0}")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, SourceError>;

/// A source of tickets from an external platform.
#[async_trait]
pub trait TicketSource {
    /// What this source supports. Callers consult this before attempting writes.
    fn capabilities(&self) -> Capabilities;

    /// Tickets changed since `cursor` (or all tickets, if the cursor is empty),
    /// plus the cursor to resume incremental sync from.
    async fn fetch_changed(&self, cursor: &Cursor) -> Result<Page>;

    /// Fetch a single ticket by its stable provider `uid`.
    async fn get(&self, uid: &str) -> Result<Ticket>;

    /// Move a ticket to a normalized [`StateCategory`]. The source resolves this
    /// to its native mechanism (a Jira transition, a GitLab label swap, an Asana
    /// section move). Capability-gated: defaults to `Unsupported`.
    async fn set_state(&self, _uid: &str, _target: StateCategory) -> Result<()> {
        Err(SourceError::Unsupported("set_state"))
    }

    /// Append a comment to a ticket. Capability-gated: defaults to `Unsupported`.
    async fn comment(&self, _uid: &str, _body: &str) -> Result<()> {
        Err(SourceError::Unsupported("comment"))
    }
}
