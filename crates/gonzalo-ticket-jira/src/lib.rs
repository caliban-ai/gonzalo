//! Jira issue connector for gonzalo's ticket layer.
//!
//! Implements [`gonzalo_ticket::TicketSource`] over the Jira Cloud REST v3 API,
//! mapping issues to the canonical [`gonzalo_domain::Ticket`] (ADR 0010). The
//! mapping ([`mapping`]) is pure and fixture-tested — it normalizes via Jira's
//! `statusCategory` with optional per-connection `StateMapping` overrides, and
//! extracts text from ADF bodies. [`JiraSource`] is the thin HTTP layer.
//! Read-only in phase 1.

mod mapping;
mod source;

pub use source::JiraSource;
