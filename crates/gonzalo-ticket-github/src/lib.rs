//! GitHub issue connector for gonzalo's ticket layer.
//!
//! Implements [`gonzalo_ticket::TicketSource`] over the GitHub REST API,
//! mapping issues to the canonical [`gonzalo_domain::Ticket`] (ADR 0010). The
//! mapping ([`mapping`]) is pure and fixture-tested; [`GitHubSource`] is the
//! thin HTTP layer. Read-only in phase 1.

mod mapping;
mod project_mapping;
mod source;

pub use source::GitHubSource;
