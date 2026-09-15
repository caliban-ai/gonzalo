//! The configuration records: people, account bindings, role grants and
//! channel configs. All four merge `Structured` under sync (ADR 0022).

use super::keys::{self, FleetKeyError};
use super::{
    Authenticator, CHANNELS_COLLECTION, FLEET_NAMESPACE, FleetActor, FleetRole, GrantScope,
    IDENTITY_BINDINGS_COLLECTION, PEOPLE_COLLECTION, ROLE_GRANTS_COLLECTION,
};
use crate::codec::RecordCodec;
use gonzalo_core::{RecordKey, RecordKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A human with fleet roles, keyed by an opaque person id the minting
/// consumer generates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// The one name shown for this person across surfaces.
    pub display_name: String,
    /// Operator-set contact address. Informational: never used to match an
    /// account to a person.
    pub email: Option<String>,
}
impl RecordCodec for Person {}
impl Person {
    pub const KIND: RecordKind = RecordKind::Person;

    /// `fleet/people/<person_id>`.
    pub fn key(person_id: &str) -> Result<RecordKey, FleetKeyError> {
        keys::validate_person_id(person_id)?;
        Ok(RecordKey::new(
            FLEET_NAMESPACE,
            PEOPLE_COLLECTION,
            person_id,
        ))
    }
}

/// An email address as asserted by an authenticator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedEmail {
    pub address: String,
    pub verified: bool,
}

/// How an account came to be bound to a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BindingOrigin {
    /// Redeemed the link token with this hash.
    LinkToken { token_hash: String },
    /// Bound directly by an operator.
    Operator(FleetActor),
}

/// Ties one external account to a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityBinding {
    pub authenticator: Authenticator,
    /// The platform's stable user id, or the OIDC `sub`. The only lookup key.
    pub subject: String,
    /// The person id this account belongs to.
    pub person: String,
    /// A snapshot of the platform username, for display only.
    pub handle: Option<String>,
    pub email: Option<VerifiedEmail>,
    pub bound_at: i64,
    pub bound_by: BindingOrigin,
}
impl RecordCodec for IdentityBinding {}
impl IdentityBinding {
    pub const KIND: RecordKind = RecordKind::IdentityBinding;

    /// `fleet/identity-bindings/<authenticator>:<subject>`.
    pub fn key_for(
        authenticator: &Authenticator,
        subject: &str,
    ) -> Result<RecordKey, FleetKeyError> {
        Ok(RecordKey::new(
            FLEET_NAMESPACE,
            IDENTITY_BINDINGS_COLLECTION,
            keys::binding_id(authenticator, subject)?,
        ))
    }

    pub fn key(&self) -> Result<RecordKey, FleetKeyError> {
        Self::key_for(&self.authenticator, &self.subject)
    }
}

/// A person's role for one scope. One record per `(person, scope)`; revoking
/// is a delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleGrant {
    pub person: String,
    pub scope: GrantScope,
    pub role: FleetRole,
    pub granted_by: FleetActor,
    pub granted_at: i64,
}
impl RecordCodec for RoleGrant {}
impl RoleGrant {
    pub const KIND: RecordKind = RecordKind::RoleGrant;

    /// `fleet/role-grants/<person_id>:<scope>`.
    pub fn key_for(person: &str, scope: &GrantScope) -> Result<RecordKey, FleetKeyError> {
        Ok(RecordKey::new(
            FLEET_NAMESPACE,
            ROLE_GRANTS_COLLECTION,
            keys::grant_id(person, scope)?,
        ))
    }

    pub fn key(&self) -> Result<RecordKey, FleetKeyError> {
        Self::key_for(&self.person, &self.scope)
    }
}

/// Per-chat-channel configuration.
///
/// Does not derive `Eq`: `filters` holds `serde_json::Value`s, which are not
/// `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// The chat provider, such as `"discord"`.
    pub provider: String,
    pub channel_id: String,
    /// The highest role any command in this channel runs with.
    pub ceiling: FleetRole,
    /// Followed repositories, as `owner/name`.
    pub repos: Vec<String>,
    /// Consumer-defined notification filters.
    pub filters: BTreeMap<String, serde_json::Value>,
}
impl RecordCodec for ChannelConfig {}
impl ChannelConfig {
    pub const KIND: RecordKind = RecordKind::ChannelConfig;

    /// `fleet/channels/<provider>:<channel_id>`.
    pub fn key_for(provider: &str, channel_id: &str) -> Result<RecordKey, FleetKeyError> {
        Ok(RecordKey::new(
            FLEET_NAMESPACE,
            CHANNELS_COLLECTION,
            keys::channel_id(provider, channel_id)?,
        ))
    }

    pub fn key(&self) -> Result<RecordKey, FleetKeyError> {
        Self::key_for(&self.provider, &self.channel_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_roundtrips_and_keys() {
        let p = Person {
            display_name: "Ada".into(),
            email: Some("ada@example.com".into()),
        };
        assert_eq!(Person::from_body(&p.to_body().unwrap()).unwrap(), p);
        assert_eq!(Person::KIND, RecordKind::Person);
        assert_eq!(
            Person::key("p1").unwrap(),
            RecordKey::new("fleet", "people", "p1")
        );
        assert!(Person::key("ada@example.com").is_err());
    }

    #[test]
    fn identity_binding_roundtrips_and_keys() {
        let b = IdentityBinding {
            authenticator: Authenticator::Oidc {
                issuer: "https://id.example.com".into(),
            },
            subject: "sub-1".into(),
            person: "p1".into(),
            handle: Some("ada".into()),
            email: Some(VerifiedEmail {
                address: "ada@example.com".into(),
                verified: true,
            }),
            bound_at: 1_700_000_000_000,
            bound_by: BindingOrigin::Operator(FleetActor::Service("ariel-cli".into())),
        };
        assert_eq!(
            IdentityBinding::from_body(&b.to_body().unwrap()).unwrap(),
            b
        );
        assert_eq!(IdentityBinding::KIND, RecordKind::IdentityBinding);
        assert_eq!(
            b.key().unwrap(),
            RecordKey::new(
                "fleet",
                "identity-bindings",
                "oidc:https%3A//id.example.com:sub-1"
            )
        );
    }

    #[test]
    fn role_grant_roundtrips_and_keys() {
        let g = RoleGrant {
            person: "p1".into(),
            scope: GrantScope::Repo("caliban-ai/gonzalo".into()),
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        assert_eq!(RoleGrant::from_body(&g.to_body().unwrap()).unwrap(), g);
        assert_eq!(RoleGrant::KIND, RecordKind::RoleGrant);
        assert_eq!(
            g.key().unwrap(),
            RecordKey::new("fleet", "role-grants", "p1:repo:caliban-ai/gonzalo")
        );
    }

    #[test]
    fn channel_config_roundtrips_and_keys() {
        let mut filters = BTreeMap::new();
        filters.insert("events".into(), serde_json::json!(["AgentSpawned"]));
        let c = ChannelConfig {
            provider: "discord".into(),
            channel_id: "42".into(),
            ceiling: FleetRole::Viewer,
            repos: vec!["caliban-ai/gonzalo".into()],
            filters,
        };
        assert_eq!(ChannelConfig::from_body(&c.to_body().unwrap()).unwrap(), c);
        assert_eq!(ChannelConfig::KIND, RecordKind::ChannelConfig);
        assert_eq!(
            c.key().unwrap(),
            RecordKey::new("fleet", "channels", "discord:42")
        );
    }
}
