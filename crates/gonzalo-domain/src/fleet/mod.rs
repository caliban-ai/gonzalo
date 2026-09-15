//! Fleet access-control records: people, the external accounts bound to them,
//! role grants, per-channel configuration, one-time link tokens, and the audit
//! trail. See ADR 0022, amended for channel configuration by ADR 0023. One-time
//! link tokens store only a hash of their secret.
//!
//! These are typed views only. gonzalo stores the records but never evaluates
//! the roles in them; consumers such as Ariel read them and decide, including
//! the two-key rule `effective = min(person_role, channel_ceiling)`. A person is
//! never resolved by email or handle, so nothing here offers that lookup.

mod atomic;
mod audit;
mod keys;
mod link;
mod records;

pub use audit::{AuditEntry, AuditResult};
pub use keys::FleetKeyError;
pub use link::{Consumption, LinkSecret, LinkSecretError, LinkToken, RedeemError};
pub use records::{
    BindingOrigin, ChannelConfig, EmptyFollowSet, Follows, IdentityBinding, NotifyPreset, Person,
    RoleGrant, VerifiedEmail,
};

use serde::{Deserialize, Serialize};

/// Namespace for people, bindings, grants, channel configs and link tokens.
pub const FLEET_NAMESPACE: &str = "fleet";
/// Namespace for audit entries, kept apart so daemon auth (ADR 0015) can scope
/// audit access separately.
pub const FLEET_AUDIT_NAMESPACE: &str = "fleet-audit";
pub const PEOPLE_COLLECTION: &str = "people";
pub const IDENTITY_BINDINGS_COLLECTION: &str = "identity-bindings";
pub const ROLE_GRANTS_COLLECTION: &str = "role-grants";
pub const CHANNELS_COLLECTION: &str = "channels";
pub const LINK_TOKENS_COLLECTION: &str = "link-tokens";
pub const AUDIT_ENTRIES_COLLECTION: &str = "entries";

/// A fleet role, ordered `Viewer < Operator < Admin`. Defaults to `Viewer`, the
/// least privileged, so an omitted ceiling grants nothing extra (ADR 0023).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum FleetRole {
    #[default]
    Viewer,
    Operator,
    Admin,
}

/// Where a role grant or link token applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantScope {
    Fleet,
    /// One workspace, by the name prospero reports for it (ADR 0023).
    Workspace(String),
}

/// The system that authenticated an external account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Authenticator {
    Discord,
    Slack,
    Teams,
    Oidc { issuer: String },
    Other(String),
}

/// Who did something: a person, an account not yet linked to one, or a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FleetActor {
    /// A person id.
    Person(String),
    Unlinked {
        authenticator: Authenticator,
        subject: String,
    },
    /// A service name, such as `"ariel"` or `"ariel-cli"`.
    Service(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_ordered_viewer_operator_admin() {
        assert!(FleetRole::Viewer < FleetRole::Operator);
        assert!(FleetRole::Operator < FleetRole::Admin);
        assert_eq!(
            FleetRole::Admin.min(FleetRole::Viewer),
            FleetRole::Viewer,
            "a viewer-ceiling channel caps an admin"
        );
    }
}
