//! Typed domain views over gonzalo records.

pub mod checkpoint;
pub mod codec;
pub mod fleet;
pub mod memory;
pub mod session;
pub mod ticket;

pub use checkpoint::Checkpoint;
pub use codec::RecordCodec;
pub use fleet::{
    AuditEntry, AuditResult, Authenticator, BindingOrigin, ChannelConfig, Consumption,
    EmptyFollowSet, FleetActor, FleetKeyError, FleetRole, Follows, GrantScope, IdentityBinding,
    LinkSecret, LinkSecretError, LinkToken, NotifyPreset, Person, RedeemError, RoleGrant,
    VerifiedEmail,
};
pub use memory::{MemoryTier, Topic};
pub use session::{Session, Turn};
pub use ticket::{
    Actor, ActorRole, BodyFormat, Container, Link, LinkKind, LinkTarget, Priority, PriorityLevel,
    Provider, Resolution, State, StateCategory, Ticket, TicketBody, TicketEvent,
};
