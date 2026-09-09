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

/// Longest provider error body echoed into a [`SourceError::Backend`] message.
/// A provider error is normally a short JSON object, but nothing guarantees it
/// and an error string is a poor place for a megabyte.
const MAX_ERROR_BODY: usize = 512;

/// Build the error for a non-success HTTP response from a ticket provider.
///
/// [`SourceError::Backend`] is documented as "carrying the provider's message",
/// but every connector reached it through reqwest's `error_for_status()`, which
/// **discards the response body** — so the message was reqwest's generic
/// "HTTP status client error … for url …" and the provider's actual reason was
/// thrown away. That reason is the whole value: an expired token, a missing
/// scope, a malformed query, a rate-limit window with a retry hint (#240).
///
/// Shared here, rather than per connector, so five providers cannot drift into
/// five ways of describing the same failure.
pub fn provider_error(status: u16, body: &str) -> SourceError {
    let body = body.trim();
    if body.is_empty() {
        return SourceError::Backend(format!("provider returned {status}"));
    }
    // Truncate on a char boundary; a provider may answer in any encoding.
    let shown: String = match body.char_indices().nth(MAX_ERROR_BODY) {
        Some((cut, _)) => format!("{}…", &body[..cut]),
        None => body.to_string(),
    };
    SourceError::Backend(format!("provider returned {status}: {shown}"))
}

/// A source of tickets from an external platform.
///
/// Requires `Send + Sync` (like [`gonzalo_core::Store`]) so a
/// `Box<dyn TicketSource>` can be driven across threads — the daemon ingests
/// over `Send` futures on the gRPC and HTTP transports.
#[async_trait]
pub trait TicketSource: Send + Sync {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of #240: the provider's reason survives, where
    /// `error_for_status()` threw it away and left reqwest's generic message.
    #[test]
    fn the_provider_reason_survives() {
        let err = provider_error(
            403,
            r#"{"message":"Resource not accessible by integration"}"#,
        );
        let SourceError::Backend(msg) = err else {
            panic!("provider failures are Backend errors");
        };
        assert!(msg.contains("403"), "{msg}");
        assert!(
            msg.contains("Resource not accessible by integration"),
            "{msg}"
        );
    }

    #[test]
    fn an_empty_body_still_names_the_status() {
        // Some providers answer 5xx with nothing at all; the status is then the
        // whole signal and the message must not trail a bare colon.
        let msg = provider_error(502, "   ").to_string();
        assert!(msg.contains("502"), "{msg}");
        assert!(
            !msg.contains("502: "),
            "no dangling separator after the status: {msg}"
        );
    }

    #[test]
    fn a_long_body_is_truncated() {
        // An error string is a poor place for an unbounded response.
        let msg = provider_error(400, &"x".repeat(MAX_ERROR_BODY * 4)).to_string();
        assert!(
            msg.len() < MAX_ERROR_BODY * 2,
            "still bounded: {}",
            msg.len()
        );
        assert!(msg.ends_with('…'), "truncation is visible: {msg}");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // A provider may answer in any encoding; slicing mid-codepoint panics.
        let body = "é".repeat(MAX_ERROR_BODY * 2);
        let msg = provider_error(400, &body).to_string();
        assert!(msg.contains("400"), "{msg}");
    }
}
