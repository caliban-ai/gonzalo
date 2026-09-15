//! Audit entries: one write-once record per action, in the `fleet-audit`
//! namespace. Opaque under sync, so a key collision surfaces instead of
//! merging (ADR 0022).

use super::keys::{self, FleetKeyError};
use super::{AUDIT_ENTRIES_COLLECTION, FLEET_AUDIT_NAMESPACE, FleetActor};
use crate::codec::RecordCodec;
use gonzalo_core::{RecordKey, RecordKind};
use serde::{Deserialize, Serialize};

/// The outcome an audit entry records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuditResult {
    Succeeded,
    Denied,
    Failed(String),
}

/// Who did what, to what, when, and with what result.
///
/// Create it with `Store::put(record, None)` and never update it. A `Conflict`
/// means the key already exists: build a new key with a different nonce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub actor: FleetActor,
    /// What was done, such as `"spawn"` or `"link"`.
    pub action: String,
    /// What it was done to.
    pub target: String,
    pub at: i64,
    /// A reference to where it happened, such as a chat message link.
    pub surface_ref: Option<String>,
    pub result: AuditResult,
}
impl RecordCodec for AuditEntry {}
impl AuditEntry {
    pub const KIND: RecordKind = RecordKind::AuditEntry;

    /// `fleet-audit/entries/<at_ms>-<nonce>`, with `at` zero-padded to 13 digits.
    pub fn key(&self, nonce: &str) -> Result<RecordKey, FleetKeyError> {
        Ok(RecordKey::new(
            FLEET_AUDIT_NAMESPACE,
            AUDIT_ENTRIES_COLLECTION,
            keys::audit_id(self.at, nonce)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::Authenticator;

    #[test]
    fn audit_entry_roundtrips_and_keys() {
        let e = AuditEntry {
            actor: FleetActor::Unlinked {
                authenticator: Authenticator::Discord,
                subject: "1234".into(),
            },
            action: "spawn".into(),
            target: "caliban-ai/gonzalo".into(),
            at: 1_700_000_000_000,
            surface_ref: Some("https://discord.com/channels/1/2/3".into()),
            result: AuditResult::Denied,
        };
        assert_eq!(AuditEntry::from_body(&e.to_body().unwrap()).unwrap(), e);
        assert_eq!(AuditEntry::KIND, RecordKind::AuditEntry);
        assert_eq!(
            e.key("n1").unwrap(),
            RecordKey::new("fleet-audit", "entries", "1700000000000-n1")
        );
        assert!(e.key("bad nonce").is_err());
    }
}
