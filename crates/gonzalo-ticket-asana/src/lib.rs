//! Asana task connector for gonzalo's ticket layer.
//!
//! Implements [`gonzalo_ticket::TicketSource`] over the Asana REST API, mapping
//! tasks to the canonical [`gonzalo_domain::Ticket`] (ADR 0010). The mapping
//! ([`mapping`]) is pure and fixture-tested — Asana has no intrinsic state, so
//! it exercises the `Completed` / `Section` / `CustomField` state signals and
//! multi-home containers. [`AsanaSource`] is the thin HTTP layer. Read-only in
//! phase 1.

mod mapping;
mod source;

pub use source::AsanaSource;
