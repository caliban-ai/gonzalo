//! The configuration records: people, account bindings, role grants and
//! channel configs. All four merge `Structured` under sync (ADR 0022).

use super::keys::{self, FleetKeyError};
use super::{
    Authenticator, CHANNELS_COLLECTION, FLEET_NAMESPACE, FleetActor, FleetRole, GrantScope,
    IDENTITY_BINDINGS_COLLECTION, PEOPLE_COLLECTION, ROLE_GRANTS_COLLECTION,
};
use crate::codec::RecordCodec;
use gonzalo_core::{RecordKey, RecordKind};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeSet;
use std::fmt;

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
    /// Stored as a one-element array; merges atomically (ADR 0022 §"Merge
    /// classes in sync terms") so a concurrent change doesn't mix variant
    /// tags with `bound_by`'s other variant.
    #[serde(with = "super::atomic")]
    pub authenticator: Authenticator,
    /// The platform's stable user id, or the OIDC `sub`. The only lookup key.
    pub subject: String,
    /// The person id this account belongs to.
    pub person: String,
    /// A snapshot of the platform username, for display only.
    pub handle: Option<String>,
    /// Stored as a one-element array; merges atomically so a concurrent
    /// change can't pair one side's `address` with the other's `verified`.
    #[serde(with = "super::atomic")]
    pub email: Option<VerifiedEmail>,
    pub bound_at: i64,
    /// Stored as a one-element array; merges atomically, since this is an
    /// externally-tagged enum and object-recursive merging of two different
    /// variants would produce an undecodable body.
    #[serde(with = "super::atomic")]
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
    /// Stored as a one-element array; merges atomically, matching `role`'s
    /// own conflict-on-divergence behaviour (ADR 0022).
    #[serde(with = "super::atomic")]
    pub scope: GrantScope,
    pub role: FleetRole,
    /// Stored as a one-element array; merges atomically, since this is an
    /// externally-tagged enum and object-recursive merging of two different
    /// variants would produce an undecodable body.
    #[serde(with = "super::atomic")]
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

/// Which workspaces a channel follows (ariel ADR 0009; gonzalo ADR 0023).
///
/// Stored tagged: `{"kind":"fleet"}` or
/// `{"kind":"workspaces","names":[…]}` with at least one name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Follows {
    /// Every workspace, including ones created later.
    Fleet,
    /// A non-empty set of workspace names, as prospero reports them. Build it
    /// with [`Follows::workspaces`], which rejects an empty set.
    Workspaces { names: BTreeSet<String> },
}

impl Follows {
    /// `Workspaces` over a non-empty set of names.
    pub fn workspaces<I, S>(names: I) -> Result<Self, EmptyFollowSet>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let names: BTreeSet<String> = names.into_iter().map(Into::into).collect();
        if names.is_empty() {
            return Err(EmptyFollowSet);
        }
        Ok(Follows::Workspaces { names })
    }
}

impl<'de> Deserialize<'de> for Follows {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A shadow type carries the wire shape; the invariant (a non-empty
        // name set) is enforced here so no decoded `Follows` can break it.
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Wire {
            Fleet,
            Workspaces { names: BTreeSet<String> },
        }
        match Wire::deserialize(deserializer)? {
            Wire::Fleet => Ok(Follows::Fleet),
            Wire::Workspaces { names } if names.is_empty() => {
                Err(serde::de::Error::custom(EmptyFollowSet))
            }
            Wire::Workspaces { names } => Ok(Follows::Workspaces { names }),
        }
    }
}

/// `Follows::Workspaces` was given no workspace names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyFollowSet;

impl fmt::Display for EmptyFollowSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a channel following workspaces must name at least one")
    }
}

impl std::error::Error for EmptyFollowSet {}

/// How much a channel hears. A preset only narrows the consumer's default
/// pacing; it can never add event kinds (ariel ADR 0007/0009).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyPreset {
    /// Everything the consumer's pacing allows.
    #[default]
    All,
    /// Only when an agent ends.
    Terminal,
    /// Only when an agent ends failed, crashed or gone.
    Failures,
}

/// Per-chat-channel configuration, keyed by provider, tenant and channel
/// (ariel ADR 0009; gonzalo ADR 0023). It belongs to no person: who changed it
/// is recorded in the audit trail and in gonzalo's author stamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    /// The chat provider, such as `"discord"`.
    pub provider: String,
    /// The guild, workspace or team id the channel belongs to. Part of the key,
    /// because a channel id is only unique within its tenant on some platforms.
    pub tenant: String,
    /// The platform's channel id.
    pub channel: String,
    /// Stored as a one-element array; merges atomically (ADR 0022), so two
    /// concurrent follow changes conflict instead of one silently winning.
    #[serde(with = "super::atomic")]
    pub follows: Follows,
    /// Defaults to [`NotifyPreset::All`].
    #[serde(default)]
    pub notify: NotifyPreset,
    /// The highest role any command in this channel runs with. Defaults to
    /// [`FleetRole::Viewer`]; it governs commands only, never notifications.
    #[serde(default)]
    pub ceiling: FleetRole,
}
impl RecordCodec for ChannelConfig {}
impl ChannelConfig {
    pub const KIND: RecordKind = RecordKind::ChannelConfig;

    /// `fleet/channels/<provider>:<tenant>:<channel>`.
    pub fn key_for(
        provider: &str,
        tenant: &str,
        channel: &str,
    ) -> Result<RecordKey, FleetKeyError> {
        Ok(RecordKey::new(
            FLEET_NAMESPACE,
            CHANNELS_COLLECTION,
            keys::channel_id(provider, tenant, channel)?,
        ))
    }

    pub fn key(&self) -> Result<RecordKey, FleetKeyError> {
        Self::key_for(&self.provider, &self.tenant, &self.channel)
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
            scope: GrantScope::Workspace("caliban".into()),
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        assert_eq!(RoleGrant::from_body(&g.to_body().unwrap()).unwrap(), g);
        assert_eq!(RoleGrant::KIND, RecordKind::RoleGrant);
        assert_eq!(
            g.key().unwrap(),
            RecordKey::new("fleet", "role-grants", "p1:workspace:caliban")
        );
    }

    fn ops_channel() -> ChannelConfig {
        // ariel ADR 0009's `#ops` example: the whole fleet, failures only,
        // read-only commands.
        ChannelConfig {
            provider: "discord".into(),
            tenant: "guild-1".into(),
            channel: "42".into(),
            follows: Follows::Fleet,
            notify: NotifyPreset::Failures,
            ceiling: FleetRole::Viewer,
        }
    }

    #[test]
    fn channel_config_roundtrips_and_keys() {
        let c = ops_channel();
        assert_eq!(ChannelConfig::from_body(&c.to_body().unwrap()).unwrap(), c);
        assert_eq!(ChannelConfig::KIND, RecordKind::ChannelConfig);
        assert_eq!(
            c.key().unwrap(),
            RecordKey::new("fleet", "channels", "discord:guild-1:42")
        );
    }

    #[test]
    fn channel_config_keys_include_the_tenant() {
        // The same channel id under two tenants is two records, not one
        // (ariel ADR 0006).
        let a = ops_channel();
        let mut b = ops_channel();
        b.tenant = "guild-2".into();
        assert_ne!(a.key().unwrap(), b.key().unwrap());
    }

    #[test]
    fn follows_stores_ariel_adr_0009_wire_shape() {
        // ariel ADR 0009: {"kind":"fleet"} or
        // {"kind":"workspaces","names":[…]}, inside gonzalo's atomic wrapper.
        let mut c = ops_channel();
        c.follows = Follows::workspaces(["caliban"]).unwrap();
        let body = c.to_body().unwrap();
        let v: serde_json::Value = serde_json::from_slice(body.bytes()).unwrap();
        assert_eq!(
            v["follows"],
            serde_json::json!([{"kind": "workspaces", "names": ["caliban"]}])
        );
        assert_eq!(v["notify"], serde_json::json!("failures"));

        let fleet_body = ops_channel().to_body().unwrap();
        let v: serde_json::Value = serde_json::from_slice(fleet_body.bytes()).unwrap();
        assert_eq!(v["follows"], serde_json::json!([{"kind": "fleet"}]));
        assert_eq!(ChannelConfig::from_body(&body).unwrap(), c);
    }

    #[test]
    fn follows_workspaces_must_name_at_least_one() {
        assert_eq!(
            Follows::workspaces(Vec::<String>::new()),
            Err(EmptyFollowSet)
        );
        assert!(Follows::workspaces(["caliban", "gonzalo"]).is_ok());

        // The invariant also holds for a body written by hand.
        let mut v: serde_json::Value =
            serde_json::from_slice(ops_channel().to_body().unwrap().bytes()).unwrap();
        v["follows"] = serde_json::json!([{"kind": "workspaces", "names": []}]);
        let bytes = serde_json::to_vec(&v).unwrap();
        assert!(
            ChannelConfig::from_body(&gonzalo_core::Body::Inline(bytes)).is_err(),
            "an empty workspace set must be rejected"
        );
    }

    #[test]
    fn channel_config_defaults_notify_all_and_ceiling_viewer() {
        // `follows` is required; the other two are optional (ariel ADR 0009).
        let json = serde_json::json!({
            "provider": "discord",
            "tenant": "guild-1",
            "channel": "42",
            "follows": [{"kind": "fleet"}],
        });
        let bytes = serde_json::to_vec(&json).unwrap();
        let c = ChannelConfig::from_body(&gonzalo_core::Body::Inline(bytes)).unwrap();
        assert_eq!(c.notify, NotifyPreset::All);
        assert_eq!(c.ceiling, FleetRole::Viewer);

        let missing_follows = serde_json::json!({
            "provider": "discord",
            "tenant": "guild-1",
            "channel": "42",
        });
        let bytes = serde_json::to_vec(&missing_follows).unwrap();
        assert!(
            ChannelConfig::from_body(&gonzalo_core::Body::Inline(bytes)).is_err(),
            "follows is required"
        );
    }

    // ---- atomic wrapper: shape and error handling (ADR 0022) ----

    #[test]
    fn atomic_wrapped_field_stores_as_one_element_array() {
        let g = RoleGrant {
            person: "p1".into(),
            scope: GrantScope::Fleet,
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        let body = g.to_body().unwrap();
        let text = String::from_utf8(body.bytes().to_vec()).unwrap();
        assert!(
            text.contains(r#""granted_by":[{"#),
            "granted_by must be stored as a one-element array, got: {text}"
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(v["granted_by"].as_array().unwrap().len() == 1);
        // Round-trips.
        assert_eq!(RoleGrant::from_body(&body).unwrap(), g);
    }

    #[test]
    fn atomic_wrapped_field_rejects_empty_and_multi_element_arrays() {
        let g = RoleGrant {
            person: "p1".into(),
            scope: GrantScope::Fleet,
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        let mut v: serde_json::Value =
            serde_json::from_slice(g.to_body().unwrap().bytes()).unwrap();

        v["granted_by"] = serde_json::json!([]);
        let bytes = serde_json::to_vec(&v).unwrap();
        assert!(
            RoleGrant::from_body(&gonzalo_core::Body::Inline(bytes)).is_err(),
            "an empty array must be rejected"
        );

        v["granted_by"] = serde_json::json!([{"Person": "p0"}, {"Person": "p0"}]);
        let bytes = serde_json::to_vec(&v).unwrap();
        assert!(
            RoleGrant::from_body(&gonzalo_core::Body::Inline(bytes)).is_err(),
            "a two-element array must be rejected"
        );
    }

    // ---- merge-level tests: composite fields merge atomically (ADR 0022) ----

    fn merge_bodies(
        kind: RecordKind,
        base: &gonzalo_core::Body,
        ours: &gonzalo_core::Body,
        theirs: &gonzalo_core::Body,
    ) -> gonzalo_core::MergeOutcome {
        gonzalo_core::merge(kind.merge_class(), base, ours, theirs)
    }

    #[test]
    fn role_grant_variant_mix_on_granted_by_needs_resolution() {
        // Base: Person; ours: Service; theirs: Unlinked. Without the atomic
        // wrapper, core's object-recursive Structured merge would delete
        // "Person" on both sides and add "Service" and "Unlinked", producing
        // a two-tag body that fails to decode. Wrapped as an array, this is a
        // genuine conflict instead.
        let base = RoleGrant {
            person: "p1".into(),
            scope: GrantScope::Fleet,
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        let mut ours = base.clone();
        ours.granted_by = FleetActor::Service("ariel".into());
        let mut theirs = base.clone();
        theirs.granted_by = FleetActor::Unlinked {
            authenticator: Authenticator::Discord,
            subject: "1234".into(),
        };

        let outcome = merge_bodies(
            RecordKind::RoleGrant,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        assert_eq!(outcome, gonzalo_core::MergeOutcome::NeedsResolution);
    }

    #[test]
    fn identity_binding_email_half_mix_needs_resolution() {
        // Base: unverified a@x. Ours changes only the address; theirs verifies
        // it. Without the atomic wrapper, merging the object key-by-key would
        // combine ours' `address` with theirs' `verified`, asserting a
        // verification nobody made for that address.
        let base = IdentityBinding {
            authenticator: Authenticator::Discord,
            subject: "sub-1".into(),
            person: "p1".into(),
            handle: None,
            email: Some(VerifiedEmail {
                address: "a@x".into(),
                verified: false,
            }),
            bound_at: 1_700_000_000_000,
            bound_by: BindingOrigin::Operator(FleetActor::Service("ariel".into())),
        };
        let mut ours = base.clone();
        ours.email = Some(VerifiedEmail {
            address: "b@x".into(),
            verified: false,
        });
        let mut theirs = base.clone();
        theirs.email = Some(VerifiedEmail {
            address: "a@x".into(),
            verified: true,
        });

        let outcome = merge_bodies(
            RecordKind::IdentityBinding,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        assert_eq!(outcome, gonzalo_core::MergeOutcome::NeedsResolution);
    }

    #[test]
    fn identity_binding_disjoint_atomic_fields_merge() {
        // Ours changes only `handle`; theirs changes only `email`. Disjoint
        // field edits still merge even though both fields are atomically
        // wrapped.
        let base = IdentityBinding {
            authenticator: Authenticator::Discord,
            subject: "sub-1".into(),
            person: "p1".into(),
            handle: None,
            email: None,
            bound_at: 1_700_000_000_000,
            bound_by: BindingOrigin::Operator(FleetActor::Service("ariel".into())),
        };
        let mut ours = base.clone();
        ours.handle = Some("ada".into());
        let mut theirs = base.clone();
        theirs.email = Some(VerifiedEmail {
            address: "a@x".into(),
            verified: true,
        });

        let outcome = merge_bodies(
            RecordKind::IdentityBinding,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        let gonzalo_core::MergeOutcome::Merged(body) = outcome else {
            panic!("expected a merge, got NeedsResolution");
        };
        let merged = IdentityBinding::from_body(&body).unwrap();
        assert_eq!(merged.handle, Some("ada".into()));
        assert_eq!(
            merged.email,
            Some(VerifiedEmail {
                address: "a@x".into(),
                verified: true,
            })
        );
    }

    #[test]
    fn role_grant_one_sided_atomic_change_merges() {
        // Only `ours` changed `granted_by`; theirs is unchanged from base.
        let base = RoleGrant {
            person: "p1".into(),
            scope: GrantScope::Fleet,
            role: FleetRole::Operator,
            granted_by: FleetActor::Person("p0".into()),
            granted_at: 1_700_000_000_000,
        };
        let mut ours = base.clone();
        ours.granted_by = FleetActor::Service("ariel".into());
        let theirs = base.clone();

        let outcome = merge_bodies(
            RecordKind::RoleGrant,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        let gonzalo_core::MergeOutcome::Merged(body) = outcome else {
            panic!("expected a merge, got NeedsResolution");
        };
        let merged = RoleGrant::from_body(&body).unwrap();
        assert_eq!(merged.granted_by, FleetActor::Service("ariel".into()));
    }

    #[test]
    fn channel_follows_changed_on_both_sides_needs_resolution() {
        // Two concurrent follow edits conflict rather than one silently
        // winning or the two sets being spliced together (ADR 0023).
        let base = ops_channel();
        let mut ours = base.clone();
        ours.follows = Follows::workspaces(["caliban"]).unwrap();
        let mut theirs = base.clone();
        theirs.follows = Follows::workspaces(["gonzalo"]).unwrap();

        let outcome = merge_bodies(
            RecordKind::ChannelConfig,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        assert_eq!(outcome, gonzalo_core::MergeOutcome::NeedsResolution);
    }

    #[test]
    fn channel_disjoint_notify_and_follows_edits_merge() {
        // Ours retargets the channel; theirs only widens what it hears.
        let base = ops_channel();
        let mut ours = base.clone();
        ours.follows = Follows::workspaces(["caliban"]).unwrap();
        let mut theirs = base.clone();
        theirs.notify = NotifyPreset::All;

        let outcome = merge_bodies(
            RecordKind::ChannelConfig,
            &base.to_body().unwrap(),
            &ours.to_body().unwrap(),
            &theirs.to_body().unwrap(),
        );
        let gonzalo_core::MergeOutcome::Merged(body) = outcome else {
            panic!("expected a merge, got NeedsResolution");
        };
        let merged = ChannelConfig::from_body(&body).unwrap();
        assert_eq!(merged.follows, Follows::workspaces(["caliban"]).unwrap());
        assert_eq!(merged.notify, NotifyPreset::All);
    }
}
