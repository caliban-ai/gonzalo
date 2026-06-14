//! An in-memory [`TicketSource`] — the read-only reference implementation,
//! analogous to `gonzalo_vector::MemoryVectorIndex`. Useful for tests and for
//! ingesting hand-built tickets without a network connector.

use crate::source::{Capabilities, Cursor, Page, Result, SourceError, TicketSource};
use async_trait::async_trait;
use gonzalo_domain::Ticket;

/// A fixed set of tickets served from memory. Read-only: write methods inherit
/// the trait's `Unsupported` defaults.
pub struct InMemorySource {
    tickets: Vec<Ticket>,
}

impl InMemorySource {
    pub fn new(tickets: Vec<Ticket>) -> Self {
        Self { tickets }
    }
}

#[async_trait]
impl TicketSource for InMemorySource {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn fetch_changed(&self, _cursor: &Cursor) -> Result<Page> {
        Ok(Page {
            tickets: self.tickets.clone(),
            next: Cursor::default(),
        })
    }

    async fn get(&self, uid: &str) -> Result<Ticket> {
        self.tickets
            .iter()
            .find(|t| t.uid == uid)
            .cloned()
            .ok_or_else(|| SourceError::Backend(format!("no ticket with uid {uid}")))
    }
}
