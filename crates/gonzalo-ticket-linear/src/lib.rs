//! Linear issue connector for gonzalo's ticket layer.
//!
//! Implements [`gonzalo_ticket::TicketSource`] over the Linear GraphQL API,
//! mapping issues to the canonical [`gonzalo_domain::Ticket`] (ADR 0010). The
//! mapping ([`mapping`]) is pure and fixture-tested — it normalizes via Linear's
//! state `type` with optional per-connection `StateMapping` overrides.
//! [`LinearSource`] is the thin GraphQL layer. Read-only in phase 1.

mod mapping;
mod source;

pub use source::LinearSource;
