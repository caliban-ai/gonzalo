//! GitLab issue connector for gonzalo's ticket layer.
//!
//! Implements [`gonzalo_ticket::TicketSource`] over the GitLab REST v4 API,
//! mapping issues to the canonical [`gonzalo_domain::Ticket`] (ADR 0010). The
//! mapping ([`mapping`]) is pure and fixture-tested — it demonstrates the
//! `ScopedLabel` state signal (`workflow::` labels on GitLab free) with a
//! fallback to intrinsic `opened`/`closed`. [`GitLabSource`] is the thin HTTP
//! layer. Read-only in phase 1.

mod mapping;
mod source;

pub use source::GitLabSource;
