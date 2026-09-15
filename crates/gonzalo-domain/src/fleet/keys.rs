//! Record ids for fleet records. Composite ids are built only here: each
//! component is escaped (`%` → `%25`, then `:` → `%3A`) before joining with
//! `:`, so an id stays unambiguous when a component, such as an OIDC issuer
//! URL, contains `:`.

use super::{Authenticator, GrantScope};
use std::fmt;

/// Why a fleet record key could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetKeyError {
    /// A person id outside `[A-Za-z0-9_-]{1,64}`.
    InvalidPersonId(String),
    /// An audit nonce outside `[A-Za-z0-9_-]{1,32}`.
    InvalidNonce(String),
    /// An audit timestamp before the Unix epoch.
    NegativeTimestamp(i64),
    /// A required key component was empty; names the component.
    EmptyComponent(&'static str),
}

impl fmt::Display for FleetKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPersonId(id) => {
                write!(f, "person id {id:?} must match [A-Za-z0-9_-]{{1,64}}")
            }
            Self::InvalidNonce(nonce) => {
                write!(f, "audit nonce {nonce:?} must match [A-Za-z0-9_-]{{1,32}}")
            }
            Self::NegativeTimestamp(at) => {
                write!(f, "audit timestamp {at} is before the Unix epoch")
            }
            Self::EmptyComponent(component) => write!(f, "{component} must not be empty"),
        }
    }
}

impl std::error::Error for FleetKeyError {}

fn escape(component: &str) -> String {
    component.replace('%', "%25").replace(':', "%3A")
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn non_empty(value: &str, component: &'static str) -> Result<(), FleetKeyError> {
    if value.is_empty() {
        Err(FleetKeyError::EmptyComponent(component))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_person_id(id: &str) -> Result<(), FleetKeyError> {
    if (1..=64).contains(&id.len()) && id.chars().all(is_id_char) {
        Ok(())
    } else {
        Err(FleetKeyError::InvalidPersonId(id.to_string()))
    }
}

fn validate_nonce(nonce: &str) -> Result<(), FleetKeyError> {
    if (1..=32).contains(&nonce.len()) && nonce.chars().all(is_id_char) {
        Ok(())
    } else {
        Err(FleetKeyError::InvalidNonce(nonce.to_string()))
    }
}

fn authenticator_segment(authenticator: &Authenticator) -> Result<String, FleetKeyError> {
    Ok(match authenticator {
        Authenticator::Discord => "discord".to_string(),
        Authenticator::Slack => "slack".to_string(),
        Authenticator::Teams => "teams".to_string(),
        Authenticator::Oidc { issuer } => {
            non_empty(issuer, "issuer")?;
            format!("oidc:{}", escape(issuer))
        }
        Authenticator::Other(name) => {
            non_empty(name, "authenticator name")?;
            format!("other:{}", escape(name))
        }
    })
}

fn scope_segment(scope: &GrantScope) -> Result<String, FleetKeyError> {
    Ok(match scope {
        GrantScope::Fleet => "fleet".to_string(),
        GrantScope::Workspace(workspace) => {
            non_empty(workspace, "workspace")?;
            format!("workspace:{}", escape(workspace))
        }
    })
}

/// `<authenticator>:<subject>`.
pub(crate) fn binding_id(
    authenticator: &Authenticator,
    subject: &str,
) -> Result<String, FleetKeyError> {
    non_empty(subject, "subject")?;
    Ok(format!(
        "{}:{}",
        authenticator_segment(authenticator)?,
        escape(subject)
    ))
}

/// `<person_id>:<scope>`.
pub(crate) fn grant_id(person: &str, scope: &GrantScope) -> Result<String, FleetKeyError> {
    validate_person_id(person)?;
    Ok(format!("{person}:{}", scope_segment(scope)?))
}

/// `<provider>:<tenant>:<channel>`. The tenant (guild, workspace or team id) is
/// part of the key because a platform channel id is only unique within its
/// tenant on Slack and Teams (ariel ADR 0006; gonzalo ADR 0023).
pub(crate) fn channel_id(
    provider: &str,
    tenant: &str,
    channel: &str,
) -> Result<String, FleetKeyError> {
    non_empty(provider, "provider")?;
    non_empty(tenant, "tenant")?;
    non_empty(channel, "channel id")?;
    Ok(format!(
        "{}:{}:{}",
        escape(provider),
        escape(tenant),
        escape(channel)
    ))
}

/// `<at_ms zero-padded to 13 digits>-<nonce>`.
pub(crate) fn audit_id(at: i64, nonce: &str) -> Result<String, FleetKeyError> {
    if at < 0 {
        return Err(FleetKeyError::NegativeTimestamp(at));
    }
    validate_nonce(nonce)?;
    Ok(format!("{at:013}-{nonce}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oidc(issuer: &str) -> Authenticator {
        Authenticator::Oidc {
            issuer: issuer.to_string(),
        }
    }

    #[test]
    fn binding_ids_are_readable_for_chat_accounts() {
        assert_eq!(
            binding_id(&Authenticator::Discord, "1234").unwrap(),
            "discord:1234"
        );
        assert_eq!(
            binding_id(&Authenticator::Slack, "U01").unwrap(),
            "slack:U01"
        );
        assert_eq!(
            binding_id(&Authenticator::Teams, "t-9").unwrap(),
            "teams:t-9"
        );
        assert_eq!(
            binding_id(&Authenticator::Other("matrix".into()), "@a").unwrap(),
            "other:matrix:@a"
        );
    }

    #[test]
    fn oidc_components_are_escaped() {
        assert_eq!(
            binding_id(&oidc("https://id.example.com"), "u:1").unwrap(),
            "oidc:https%3A//id.example.com:u%3A1"
        );
    }

    #[test]
    fn escaping_is_injective_across_colons_and_percents() {
        assert_ne!(
            binding_id(&oidc("https://a"), "b:c").unwrap(),
            binding_id(&oidc("https://a:b"), "c").unwrap()
        );
        assert_ne!(
            binding_id(&Authenticator::Discord, "%3A").unwrap(),
            binding_id(&Authenticator::Discord, ":").unwrap()
        );
        assert_ne!(
            channel_id("a:b", "t", "c").unwrap(),
            channel_id("a", "b:t", "c").unwrap()
        );
        assert_ne!(
            channel_id("discord", "t1", "c").unwrap(),
            channel_id("discord", "t2", "c").unwrap(),
            "the same channel id in two tenants keys differently"
        );
    }

    #[test]
    fn channel_ids_name_provider_tenant_and_channel() {
        assert_eq!(
            channel_id("discord", "guild-1", "42").unwrap(),
            "discord:guild-1:42"
        );
    }

    #[test]
    fn grant_ids_name_person_and_scope() {
        assert_eq!(grant_id("p1", &GrantScope::Fleet).unwrap(), "p1:fleet");
        assert_eq!(
            grant_id("p1", &GrantScope::Workspace("caliban".into())).unwrap(),
            "p1:workspace:caliban"
        );
    }

    #[test]
    fn person_ids_are_validated() {
        assert!(validate_person_id("abc_D-9").is_ok());
        assert!(validate_person_id(&"a".repeat(64)).is_ok());
        for bad in ["", "a:b", "a b", "é"] {
            assert_eq!(
                validate_person_id(bad),
                Err(FleetKeyError::InvalidPersonId(bad.to_string()))
            );
        }
        assert!(validate_person_id(&"a".repeat(65)).is_err());
        assert!(grant_id("a:b", &GrantScope::Fleet).is_err());
    }

    #[test]
    fn audit_ids_are_zero_padded_and_validated() {
        assert_eq!(
            audit_id(1_700_000_000_000, "n1").unwrap(),
            "1700000000000-n1"
        );
        assert_eq!(audit_id(5, "x").unwrap(), "0000000000005-x");
        assert_eq!(audit_id(-1, "x"), Err(FleetKeyError::NegativeTimestamp(-1)));
        assert!(audit_id(0, "").is_err());
        assert!(audit_id(0, "a b").is_err());
        assert!(audit_id(0, &"n".repeat(32)).is_ok());
        assert!(audit_id(0, &"n".repeat(33)).is_err());
    }

    #[test]
    fn empty_components_are_rejected() {
        assert_eq!(
            binding_id(&Authenticator::Discord, ""),
            Err(FleetKeyError::EmptyComponent("subject"))
        );
        assert_eq!(
            binding_id(&oidc(""), "s"),
            Err(FleetKeyError::EmptyComponent("issuer"))
        );
        assert_eq!(
            channel_id("", "t", "c"),
            Err(FleetKeyError::EmptyComponent("provider"))
        );
        assert_eq!(
            channel_id("discord", "", "c"),
            Err(FleetKeyError::EmptyComponent("tenant"))
        );
        assert_eq!(
            channel_id("discord", "t", ""),
            Err(FleetKeyError::EmptyComponent("channel id"))
        );
        assert_eq!(
            grant_id("p1", &GrantScope::Workspace(String::new())),
            Err(FleetKeyError::EmptyComponent("workspace"))
        );
    }
}
