# Fleet Access-Control Records Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Register six fleet access-control `RecordKind`s in core and ship their typed views in `gonzalo-domain`, re-exported by the `gonzalo` facade (gonzalo#278).

**Architecture:** Core gains only six enum variants and their `merge_class` arms (principle 5). A new `gonzalo-domain/src/fleet/` module holds pure, synchronous views: shared enums, injective key helpers, the four configuration records, audit entries, and link tokens with hashing and redemption. Store behaviour (OCC redemption, sync fast-forward, sync conflict) is proven by integration tests against a real `FsStore`.

**Tech Stack:** Rust 2024 workspace, serde / serde_json, blake3 via `gonzalo_core::ContentHash`, tokio + tempfile + gonzalo-store-fs (dev only).

**Spec:** `docs/superpowers/specs/2026-09-14-fleet-access-control-records-design.md` (ADR 0022: `docs/adr/0022-fleet-access-control-records.md`)

## Global Constraints

- Core change is exactly: six `RecordKind` variants `Person`, `IdentityBinding`, `RoleGrant`, `ChannelConfig`, `LinkToken`, `AuditEntry`; their `merge_class` arms; test assertions. No new core traits, types or functions.
- Merge classes: `Person`, `IdentityBinding`, `RoleGrant`, `ChannelConfig` → `MergeClass::Structured`; `LinkToken`, `AuditEntry` → `MergeClass::Opaque`.
- Type names: `FleetRole`, `GrantScope`, `Authenticator`, `FleetActor`, `FleetKeyError`, `Person`, `VerifiedEmail`, `BindingOrigin`, `IdentityBinding`, `RoleGrant`, `ChannelConfig`, `AuditEntry`, `AuditResult`, `LinkSecret`, `LinkSecretError`, `LinkToken`, `Consumption`, `RedeemError`.
- Namespaces `"fleet"` and `"fleet-audit"`; collections `"people"`, `"identity-bindings"`, `"role-grants"`, `"channels"`, `"link-tokens"`, `"entries"`.
- All timestamps are `i64` milliseconds since the Unix epoch.
- Link token hash: `ContentHash::of(b"gonzalo:link-token:v1" ‖ secret_bytes).0` (64 lowercase hex). A `LinkToken` never holds the secret; `LinkSecret` is not `Serialize` and its `Debug` is redacted.
- `gonzalo-domain` gains **no new `[dependencies]`**; dev-dependencies `gonzalo-store-fs`, `tokio`, `tempfile` (all `workspace = true`) are allowed. No `Store` calls in `gonzalo-domain/src`.
- No lookup of a person by email or handle anywhere.
- Local gate before every commit that ends a task: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace --all-targets --all-features`, `cargo test --workspace --all-features`. Run cargo in the foreground only.
- Commit messages end with the line `Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu`. Reference the ticket as `(#278)`; never write a closing keyword (`close`, `closes`, `fix`, `fixes`, `resolve`, `resolves` followed by `#N`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/gonzalo-core/src/record.rs` (modify) | six variants, merge arms, tests |
| `crates/gonzalo-knowledge/src/lib.rs` (modify) | its exhaustive `RecordKind` match: fleet kinds are not indexed |
| `crates/gonzalo-domain/src/fleet/mod.rs` (create) | module docs, namespace/collection constants, shared enums, re-exports |
| `crates/gonzalo-domain/src/fleet/keys.rs` (create) | `FleetKeyError`, escaping, id validation, composite-id builders |
| `crates/gonzalo-domain/src/fleet/records.rs` (create) | `Person`, `VerifiedEmail`, `BindingOrigin`, `IdentityBinding`, `RoleGrant`, `ChannelConfig` |
| `crates/gonzalo-domain/src/fleet/audit.rs` (create) | `AuditEntry`, `AuditResult` |
| `crates/gonzalo-domain/src/fleet/link.rs` (create) | `LinkSecret`, `LinkSecretError`, `LinkToken`, `Consumption`, `RedeemError` |
| `crates/gonzalo-domain/tests/fleet_store.rs` (create) | OCC and sync behaviour against `FsStore` |
| `crates/gonzalo-domain/src/lib.rs` (modify) | `pub mod fleet;` and root re-exports |
| `crates/gonzalo-domain/Cargo.toml` (modify) | description, dev-dependencies |
| `crates/gonzalo/src/lib.rs` (modify) | facade re-exports and a re-export test |
| `CHANGELOG.md` (modify) | `[Unreleased]` entry and upgrade note |

---

### Task 1: Register the six kinds in core

**Files:**
- Modify: `crates/gonzalo-core/src/record.rs` (enum at lines 8-25, `merge_class` at 44-58, tests at 144-156)
- Modify: `crates/gonzalo-knowledge/src/lib.rs:268-279`

**Interfaces:**
- Produces: `RecordKind::{Person, IdentityBinding, RoleGrant, ChannelConfig, LinkToken, AuditEntry}`; `merge_class()` returns the classes in Global Constraints.

- [ ] **Step 1: Write the failing test.** In `record.rs` tests, append these assertions to the end of `merge_class_is_assigned_per_kind`, and add a new test after it:

```rust
        assert_eq!(RecordKind::Person.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::IdentityBinding.merge_class(),
            MergeClass::Structured
        );
        assert_eq!(RecordKind::RoleGrant.merge_class(), MergeClass::Structured);
        assert_eq!(
            RecordKind::ChannelConfig.merge_class(),
            MergeClass::Structured
        );
        assert_eq!(RecordKind::LinkToken.merge_class(), MergeClass::Opaque);
        assert_eq!(RecordKind::AuditEntry.merge_class(), MergeClass::Opaque);
```

```rust
    #[test]
    fn fleet_kinds_serialize_as_their_names() {
        for (kind, name) in [
            (RecordKind::Person, "\"Person\""),
            (RecordKind::IdentityBinding, "\"IdentityBinding\""),
            (RecordKind::RoleGrant, "\"RoleGrant\""),
            (RecordKind::ChannelConfig, "\"ChannelConfig\""),
            (RecordKind::LinkToken, "\"LinkToken\""),
            (RecordKind::AuditEntry, "\"AuditEntry\""),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), name);
            let back: RecordKind = serde_json::from_str(name).unwrap();
            assert_eq!(back, kind);
        }
    }
```

- [ ] **Step 2: Run to verify it fails.** `cargo test -p gonzalo-core --lib record::tests` → compile error, no variant `Person`.

- [ ] **Step 3: Add the variants.** In `pub enum RecordKind`, insert after `GraphManifest,` and before the `Tombstone` doc comment:

```rust
    /// A human with fleet roles. Not ADR 0015's `Principal`, which is a gonzalo
    /// bearer token. See ADR 0022.
    Person,
    /// Binds one external account (chat user id, OIDC subject) to a `Person`.
    /// See ADR 0022.
    IdentityBinding,
    /// A person's fleet role, fleet-wide or for one repo. See ADR 0022.
    RoleGrant,
    /// A chat channel's role ceiling, followed repos and filters. See ADR 0022.
    ChannelConfig,
    /// A one-time, expiring token that links an account to a person. Stores only
    /// the token's hash, and is marked consumed rather than deleted. See ADR 0022.
    LinkToken,
    /// A write-once record of who did what, to what, and with what result.
    /// See ADR 0022.
    AuditEntry,
```

- [ ] **Step 4: Add the merge arms.** Replace the body of `merge_class` with:

```rust
        match self {
            RecordKind::Topic | RecordKind::Session | RecordKind::TicketEvent => {
                MergeClass::AppendOnly
            }
            RecordKind::MemoryTier
            | RecordKind::Ticket
            | RecordKind::Person
            | RecordKind::IdentityBinding
            | RecordKind::RoleGrant
            | RecordKind::ChannelConfig => MergeClass::Structured,
            RecordKind::Checkpoint => MergeClass::Opaque,
            // Both diverge only through a double redemption or an audit-key
            // collision, which must surface rather than merge (ADR 0022).
            RecordKind::LinkToken | RecordKind::AuditEntry => MergeClass::Opaque,
            RecordKind::GraphManifest => MergeClass::Derived,
            // Sync and pull reconcile tombstones before any body merge runs;
            // the most conservative class guards a path that forgets to.
            RecordKind::Tombstone => MergeClass::Opaque,
        }
```

- [ ] **Step 5: Update the knowledge match.** In `crates/gonzalo-knowledge/src/lib.rs`, replace the comment block and arm at lines 268-279 with:

```rust
        // Not knowledge-bearing: a checkpoint is opaque state; a graph manifest
        // is a path -> content-hash map, not natural-language text (ADR 0011/0012).
        // Checkpoint and GraphManifest are the ONLY definitionally-opaque
        // kinds — a parse failure on a knowledge-bearing kind above
        // propagates as an error rather than being silently indistinguishable
        // from "not indexable" (#139).
        // Tombstone is listed here defensively, not definitionally: consumer
        // reads never surface one, so indexing never actually sees this kind
        // (ADR 0021).
        // Fleet access-control records are configuration and audit data, and
        // hold personal data (emails, handles), so they are never indexed for
        // semantic search (ADR 0022).
        RecordKind::Checkpoint
        | RecordKind::GraphManifest
        | RecordKind::Tombstone
        | RecordKind::Person
        | RecordKind::IdentityBinding
        | RecordKind::RoleGrant
        | RecordKind::ChannelConfig
        | RecordKind::LinkToken
        | RecordKind::AuditEntry => {
            return Ok(None);
        }
```

If `cargo build --workspace --all-targets --all-features` reports any other non-exhaustive `match` on `RecordKind`, add the six variants to that match's arm for kinds it does not handle specially, with the same one-line ADR 0022 reason, and mention the file in your report.

- [ ] **Step 6: Run the tests.** `cargo test -p gonzalo-core --lib record::tests` → PASS. `cargo test -p gonzalo-knowledge --all-features` → PASS.

- [ ] **Step 7: Full gate, then commit.**

```bash
git add crates/gonzalo-core/src/record.rs crates/gonzalo-knowledge/src/lib.rs
git commit -F - <<'EOF'
feat(core): register fleet access-control record kinds (#278)

Person, IdentityBinding, RoleGrant and ChannelConfig merge Structured;
LinkToken and AuditEntry merge Opaque (ADR 0022). Knowledge indexing
skips them.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
```

---

### Task 2: Fleet module, key helpers, configuration records and audit entries

**Files:**
- Create: `crates/gonzalo-domain/src/fleet/mod.rs`, `crates/gonzalo-domain/src/fleet/keys.rs`, `crates/gonzalo-domain/src/fleet/records.rs`, `crates/gonzalo-domain/src/fleet/audit.rs`
- Modify: `crates/gonzalo-domain/src/lib.rs` (add `pub mod fleet;` after `pub mod codec;`)

**Interfaces:**
- Consumes: Task 1's `RecordKind` variants.
- Produces (all under `gonzalo_domain::fleet`):
  - constants `FLEET_NAMESPACE`, `FLEET_AUDIT_NAMESPACE`, `PEOPLE_COLLECTION`, `IDENTITY_BINDINGS_COLLECTION`, `ROLE_GRANTS_COLLECTION`, `CHANNELS_COLLECTION`, `LINK_TOKENS_COLLECTION`, `AUDIT_ENTRIES_COLLECTION` (all `&str`)
  - `FleetRole`, `GrantScope`, `Authenticator`, `FleetActor`, `FleetKeyError`
  - `Person::key(person_id: &str) -> Result<RecordKey, FleetKeyError>`
  - `IdentityBinding::key_for(&Authenticator, subject: &str) -> Result<RecordKey, FleetKeyError>`, `IdentityBinding::key(&self)`
  - `RoleGrant::key_for(person: &str, &GrantScope) -> Result<RecordKey, FleetKeyError>`, `RoleGrant::key(&self)`
  - `ChannelConfig::key_for(provider: &str, channel_id: &str) -> Result<RecordKey, FleetKeyError>`, `ChannelConfig::key(&self)`
  - `AuditEntry::key(&self, nonce: &str) -> Result<RecordKey, FleetKeyError>`
  - every view: `impl RecordCodec`, `pub const KIND: RecordKind`
  - Task 3 adds `mod link;` to `fleet/mod.rs` and uses `FleetRole`, `GrantScope`, `FleetActor`, `FLEET_NAMESPACE`, `LINK_TOKENS_COLLECTION`.

- [ ] **Step 1: Create `fleet/mod.rs`.**

```rust
//! Fleet access-control records: people, the external accounts bound to them,
//! role grants, per-channel configuration, and the audit trail. See ADR 0022.
//!
//! These are typed views only. gonzalo stores the records but never evaluates
//! the roles in them; consumers such as Ariel read them and decide, including
//! the two-key rule `effective = min(person_role, channel_ceiling)`. A person is
//! never resolved by email or handle, so nothing here offers that lookup.

mod audit;
mod keys;
mod records;

pub use audit::{AuditEntry, AuditResult};
pub use keys::FleetKeyError;
pub use records::{
    BindingOrigin, ChannelConfig, IdentityBinding, Person, RoleGrant, VerifiedEmail,
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

/// A fleet role, ordered `Viewer < Operator < Admin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FleetRole {
    Viewer,
    Operator,
    Admin,
}

/// Where a role grant or link token applies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GrantScope {
    Fleet,
    /// One repository, as `owner/name`.
    Repo(String),
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
```

- [ ] **Step 2: Create `fleet/keys.rs` with its tests first.** The tests below are the contract; the helpers follow in the same file.

```rust
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
        GrantScope::Repo(repo) => {
            non_empty(repo, "repo")?;
            format!("repo:{}", escape(repo))
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

/// `<provider>:<channel_id>`.
pub(crate) fn channel_id(provider: &str, channel: &str) -> Result<String, FleetKeyError> {
    non_empty(provider, "provider")?;
    non_empty(channel, "channel id")?;
    Ok(format!("{}:{}", escape(provider), escape(channel)))
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
        assert_eq!(binding_id(&Authenticator::Slack, "U01").unwrap(), "slack:U01");
        assert_eq!(binding_id(&Authenticator::Teams, "t-9").unwrap(), "teams:t-9");
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
            channel_id("a:b", "c").unwrap(),
            channel_id("a", "b:c").unwrap()
        );
    }

    #[test]
    fn grant_ids_name_person_and_scope() {
        assert_eq!(grant_id("p1", &GrantScope::Fleet).unwrap(), "p1:fleet");
        assert_eq!(
            grant_id("p1", &GrantScope::Repo("caliban-ai/gonzalo".into())).unwrap(),
            "p1:repo:caliban-ai/gonzalo"
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
            channel_id("", "c"),
            Err(FleetKeyError::EmptyComponent("provider"))
        );
        assert_eq!(
            channel_id("discord", ""),
            Err(FleetKeyError::EmptyComponent("channel id"))
        );
        assert_eq!(
            grant_id("p1", &GrantScope::Repo(String::new())),
            Err(FleetKeyError::EmptyComponent("repo"))
        );
    }
}
```

- [ ] **Step 3: Create `fleet/records.rs`.**

```rust
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
        Ok(RecordKey::new(FLEET_NAMESPACE, PEOPLE_COLLECTION, person_id))
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
```

- [ ] **Step 4: Create `fleet/audit.rs`.**

```rust
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
```

- [ ] **Step 5: Register the module.** In `crates/gonzalo-domain/src/lib.rs`, add `pub mod fleet;` between `pub mod codec;` and `pub mod memory;`.

- [ ] **Step 6: Run the tests.** `cargo test -p gonzalo-domain --lib fleet` → all PASS.

- [ ] **Step 7: Full gate, then commit.**

```bash
git add crates/gonzalo-domain/src/fleet crates/gonzalo-domain/src/lib.rs
git commit -F - <<'EOF'
feat(domain): fleet record views, key helpers and audit entries (#278)

Person, IdentityBinding, RoleGrant, ChannelConfig and AuditEntry views
with injective, escaped composite keys (ADR 0022).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
```

---

### Task 3: Link tokens

**Files:**
- Create: `crates/gonzalo-domain/src/fleet/link.rs`
- Modify: `crates/gonzalo-domain/src/fleet/mod.rs` (add `mod link;` after `mod keys;`, and `pub use link::{Consumption, LinkSecret, LinkSecretError, LinkToken, RedeemError};` after `pub use keys::FleetKeyError;`)

**Interfaces:**
- Consumes: `FleetRole`, `GrantScope`, `FleetActor`, `FLEET_NAMESPACE`, `LINK_TOKENS_COLLECTION` from Task 2.
- Produces:
  - `LinkSecret::from_bytes([u8; 32]) -> LinkSecret`, `LinkSecret::parse(&str) -> Result<LinkSecret, LinkSecretError>`, `LinkSecret::to_hex(&self) -> String`, `LinkSecret::hash(&self) -> String`
  - `LinkToken::new(secret: &LinkSecret, role: FleetRole, scope: GrantScope, person: Option<String>, minted_by: FleetActor, minted_at: i64, expires_at: i64) -> LinkToken`
  - `LinkToken::key(&self) -> RecordKey`, `LinkToken::key_for_secret(&LinkSecret) -> RecordKey`
  - `LinkToken::redeem(&self, secret: &LinkSecret, person: &str, binding: RecordKey, now_ms: i64) -> Result<LinkToken, RedeemError>`
  - `RedeemError::{WrongSecret, Expired, AlreadyConsumed, PersonMismatch}`; `Consumption { person: String, binding: RecordKey, at: i64 }`

- [ ] **Step 1: Create `fleet/link.rs`.**

```rust
//! One-time link tokens. The record stores only a hash of the secret, and
//! redemption marks the token consumed instead of deleting it (ADR 0022).

use super::{FLEET_NAMESPACE, FleetActor, FleetRole, GrantScope, LINK_TOKENS_COLLECTION};
use crate::codec::RecordCodec;
use gonzalo_core::{ContentHash, RecordKey, RecordKind};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fmt::Write as _;

/// Domain separation for link-token hashes, so a token hash can never equal a
/// record body's content hash.
const LINK_TOKEN_HASH_DOMAIN: &[u8] = b"gonzalo:link-token:v1";

/// The secret half of a link token: 32 random bytes the caller generates.
///
/// Shown to the operator once, as [`to_hex`](Self::to_hex). It is not
/// `Serialize`, and its `Debug` output is redacted, so it can't end up in a
/// record body or a log by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkSecret([u8; 32]);

impl LinkSecret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parse the 64 lowercase hex characters produced by [`to_hex`](Self::to_hex).
    pub fn parse(hex: &str) -> Result<Self, LinkSecretError> {
        let digits = hex.as_bytes();
        if digits.len() != 64 || !digits.iter().all(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(LinkSecretError);
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| LinkSecretError)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| LinkSecretError)?;
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for byte in &self.0 {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// The hash stored in a [`LinkToken`] and used as its key id.
    pub fn hash(&self) -> String {
        let mut input = Vec::with_capacity(LINK_TOKEN_HASH_DOMAIN.len() + self.0.len());
        input.extend_from_slice(LINK_TOKEN_HASH_DOMAIN);
        input.extend_from_slice(&self.0);
        ContentHash::of(&input).0
    }
}

impl fmt::Debug for LinkSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkSecret(<redacted>)")
    }
}

/// A link secret that isn't 64 lowercase hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkSecretError;

impl fmt::Display for LinkSecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a link secret must be 64 lowercase hex characters")
    }
}

impl std::error::Error for LinkSecretError {}

/// Who redeemed a token, into which binding, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumption {
    pub person: String,
    pub binding: RecordKey,
    pub at: i64,
}

/// A one-time, expiring token that lets a person claim an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkToken {
    /// [`LinkSecret::hash`] of the secret. The secret itself is never stored.
    pub token_hash: String,
    pub role: FleetRole,
    pub scope: GrantScope,
    /// `Some` when the token adds an account to an existing person.
    pub person: Option<String>,
    pub minted_by: FleetActor,
    pub minted_at: i64,
    /// Redemption fails at or after this time.
    pub expires_at: i64,
    pub consumed: Option<Consumption>,
}
impl RecordCodec for LinkToken {}

impl LinkToken {
    pub const KIND: RecordKind = RecordKind::LinkToken;

    /// An unconsumed token for `secret`, storing only the secret's hash.
    pub fn new(
        secret: &LinkSecret,
        role: FleetRole,
        scope: GrantScope,
        person: Option<String>,
        minted_by: FleetActor,
        minted_at: i64,
        expires_at: i64,
    ) -> Self {
        Self {
            token_hash: secret.hash(),
            role,
            scope,
            person,
            minted_by,
            minted_at,
            expires_at,
            consumed: None,
        }
    }

    /// `fleet/link-tokens/<token_hash>`.
    pub fn key(&self) -> RecordKey {
        RecordKey::new(FLEET_NAMESPACE, LINK_TOKENS_COLLECTION, self.token_hash.clone())
    }

    /// The key a presented secret's token lives at.
    pub fn key_for_secret(secret: &LinkSecret) -> RecordKey {
        RecordKey::new(FLEET_NAMESPACE, LINK_TOKENS_COLLECTION, secret.hash())
    }

    /// This token marked consumed by `person` into `binding` at `now_ms`.
    ///
    /// Write the result with `Store::put(record, Some(read_revision))`, so a
    /// concurrent redemption of the same revision is a `Conflict`. Checks run
    /// in order: wrong secret, expired, already consumed, person mismatch.
    pub fn redeem(
        &self,
        secret: &LinkSecret,
        person: &str,
        binding: RecordKey,
        now_ms: i64,
    ) -> Result<LinkToken, RedeemError> {
        if secret.hash() != self.token_hash {
            return Err(RedeemError::WrongSecret);
        }
        if now_ms >= self.expires_at {
            return Err(RedeemError::Expired);
        }
        if self.consumed.is_some() {
            return Err(RedeemError::AlreadyConsumed);
        }
        if let Some(intended) = &self.person
            && intended != person
        {
            return Err(RedeemError::PersonMismatch);
        }
        let mut redeemed = self.clone();
        redeemed.consumed = Some(Consumption {
            person: person.to_string(),
            binding,
            at: now_ms,
        });
        Ok(redeemed)
    }
}

/// Why a link token could not be redeemed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    WrongSecret,
    Expired,
    AlreadyConsumed,
    /// The token was minted for a different person.
    PersonMismatch,
}

impl fmt::Display for RedeemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongSecret => "the secret does not match this link token",
            Self::Expired => "the link token has expired",
            Self::AlreadyConsumed => "the link token has already been redeemed",
            Self::PersonMismatch => "the link token was minted for a different person",
        })
    }
}

impl std::error::Error for RedeemError {}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn secret() -> LinkSecret {
        LinkSecret::from_bytes([0xab; 32])
    }

    fn token(person: Option<&str>) -> LinkToken {
        LinkToken::new(
            &secret(),
            FleetRole::Operator,
            GrantScope::Fleet,
            person.map(str::to_string),
            FleetActor::Service("ariel-cli".into()),
            NOW,
            NOW + 600_000,
        )
    }

    fn binding() -> RecordKey {
        RecordKey::new("fleet", "identity-bindings", "discord:1234")
    }

    #[test]
    fn secret_hex_roundtrips_and_rejects_bad_input() {
        let s = LinkSecret::from_bytes(std::array::from_fn(|i| i as u8));
        let hex = s.to_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(LinkSecret::parse(&hex).unwrap(), s);
        assert_eq!(LinkSecret::parse(&hex[..63]), Err(LinkSecretError));
        assert_eq!(LinkSecret::parse(&hex.to_uppercase()), Err(LinkSecretError));
        assert_eq!(LinkSecret::parse(&"g".repeat(64)), Err(LinkSecretError));
    }

    #[test]
    fn hash_is_domain_separated() {
        let s = secret();
        let mut input = b"gonzalo:link-token:v1".to_vec();
        input.extend_from_slice(&[0xab; 32]);
        assert_eq!(s.hash(), ContentHash::of(&input).0);
        assert_ne!(s.hash(), ContentHash::of(&[0xab; 32]).0);
        assert_ne!(s.hash(), LinkSecret::from_bytes([0xac; 32]).hash());
        assert_eq!(s.hash().len(), 64);
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = secret();
        assert!(!format!("{s:?}").contains(&s.to_hex()));
    }

    #[test]
    fn token_body_never_contains_the_secret() {
        let t = token(None);
        let body = t.to_body().unwrap();
        let text = String::from_utf8(body.bytes().to_vec()).unwrap();
        assert!(!text.contains(&secret().to_hex()));
        assert!(text.contains(&secret().hash()));
        assert_eq!(LinkToken::from_body(&body).unwrap(), t);
        assert_eq!(LinkToken::KIND, RecordKind::LinkToken);
    }

    #[test]
    fn token_is_keyed_by_hash() {
        let t = token(None);
        assert_eq!(
            t.key(),
            RecordKey::new("fleet", "link-tokens", secret().hash())
        );
        assert_eq!(LinkToken::key_for_secret(&secret()), t.key());
    }

    #[test]
    fn redeem_marks_consumed() {
        let redeemed = token(None).redeem(&secret(), "p1", binding(), NOW + 1).unwrap();
        assert_eq!(
            redeemed.consumed,
            Some(Consumption {
                person: "p1".into(),
                binding: binding(),
                at: NOW + 1,
            })
        );
        let mut expected = token(None);
        expected.consumed = redeemed.consumed.clone();
        assert_eq!(redeemed, expected, "redeem changes only `consumed`");
    }

    #[test]
    fn redeem_rejects_wrong_secret_expired_consumed_and_mismatch() {
        let other = LinkSecret::from_bytes([1; 32]);
        assert_eq!(
            token(None).redeem(&other, "p1", binding(), NOW),
            Err(RedeemError::WrongSecret)
        );
        assert_eq!(
            token(None).redeem(&secret(), "p1", binding(), NOW + 600_000),
            Err(RedeemError::Expired),
            "expiry is exclusive"
        );
        let consumed = token(None).redeem(&secret(), "p1", binding(), NOW).unwrap();
        assert_eq!(
            consumed.redeem(&secret(), "p2", binding(), NOW + 1),
            Err(RedeemError::AlreadyConsumed)
        );
        assert_eq!(
            token(Some("p1")).redeem(&secret(), "p2", binding(), NOW),
            Err(RedeemError::PersonMismatch)
        );
        assert!(token(Some("p1")).redeem(&secret(), "p1", binding(), NOW).is_ok());
    }

    #[test]
    fn redeem_checks_run_in_order() {
        let consumed = token(Some("p1")).redeem(&secret(), "p1", binding(), NOW).unwrap();
        let other = LinkSecret::from_bytes([1; 32]);
        assert_eq!(
            consumed.redeem(&other, "p2", binding(), NOW + 600_000),
            Err(RedeemError::WrongSecret)
        );
        assert_eq!(
            consumed.redeem(&secret(), "p2", binding(), NOW + 600_000),
            Err(RedeemError::Expired)
        );
        assert_eq!(
            consumed.redeem(&secret(), "p2", binding(), NOW + 1),
            Err(RedeemError::AlreadyConsumed)
        );
    }
}
```

- [ ] **Step 2: Register it.** Apply the two `fleet/mod.rs` edits named under **Files**. Also append ` One-time link tokens store only a hash of their secret.` to the end of the first paragraph of the `fleet/mod.rs` module doc (after "See ADR 0022."), and change its first line to list "one-time link tokens" after "per-channel configuration,".

- [ ] **Step 3: Run the tests.** `cargo test -p gonzalo-domain --lib fleet::link` → all PASS.

- [ ] **Step 4: Full gate, then commit.**

```bash
git add crates/gonzalo-domain/src/fleet
git commit -F - <<'EOF'
feat(domain): link tokens with hashed secrets and redemption (#278)

LinkSecret hashes with a domain-separated blake3; LinkToken stores only
the hash and redeem marks it consumed after checking secret, expiry,
prior consumption and intended person (ADR 0022).

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
```

---

### Task 4: Store and sync behaviour tests

**Files:**
- Create: `crates/gonzalo-domain/tests/fleet_store.rs`
- Modify: `crates/gonzalo-domain/Cargo.toml` (add a `[dev-dependencies]` section before `[lints]`)

**Interfaces:**
- Consumes: everything from Tasks 2-3; `gonzalo_core::{sync, Store, PutResult, Record, Revision, Meta, Identity, Body, RecordKey}`; `gonzalo_store_fs::FsStore::new(path)`.
- Produces: tests only.

- [ ] **Step 1: Add dev-dependencies** to `crates/gonzalo-domain/Cargo.toml`:

```toml
[dev-dependencies]
gonzalo-store-fs = { workspace = true }
tokio            = { workspace = true }
tempfile         = { workspace = true }
```

- [ ] **Step 2: Create `tests/fleet_store.rs`.**

```rust
//! Fleet records against a real store: OCC redemption, sync fast-forward of a
//! redeemed token, sync conflict on independent redemptions, and write-once
//! audit entries and bindings (ADR 0022).

use gonzalo_core::{
    Body, Identity, Meta, PutResult, Record, RecordKey, RecordKind, Revision, Store, sync,
};
use gonzalo_domain::RecordCodec;
use gonzalo_domain::fleet::{
    AuditEntry, AuditResult, Authenticator, BindingOrigin, FleetActor, FleetRole, GrantScope,
    IdentityBinding, LinkSecret, LinkToken,
};
use gonzalo_store_fs::FsStore;
use std::collections::BTreeMap;

const NOW: i64 = 1_700_000_000_000;

fn record(key: RecordKey, kind: RecordKind, body: Body, parent: Option<Revision>) -> Record {
    let revision = match &parent {
        Some(prev) => prev.next(body.bytes()),
        None => Revision::initial(body.bytes()),
    };
    Record {
        key,
        kind,
        revision,
        parent,
        body,
        meta: Meta {
            author: Identity::new("fleet-test"),
            origin_system: "fleet-test".into(),
            created: 0,
            updated: 0,
            labels: BTreeMap::new(),
        },
        links: Vec::new(),
        ancestors: Vec::new(),
        deleted_at: None,
    }
}

fn secret() -> LinkSecret {
    LinkSecret::from_bytes([7; 32])
}

fn token() -> LinkToken {
    LinkToken::new(
        &secret(),
        FleetRole::Operator,
        GrantScope::Fleet,
        None,
        FleetActor::Service("ariel-cli".into()),
        NOW,
        NOW + 600_000,
    )
}

fn binding_key() -> RecordKey {
    IdentityBinding::key_for(&Authenticator::Discord, "1234").unwrap()
}

async fn put_unredeemed(store: &dyn Store) {
    let t = token();
    let result = store
        .put(record(t.key(), LinkToken::KIND, t.to_body().unwrap(), None), None)
        .await
        .unwrap();
    assert!(matches!(result, PutResult::Committed(_)));
}

/// The stored token, redeemed by `person` at `at`, as a record descending from it.
fn redeemed(stored: &Record, person: &str, at: i64) -> Record {
    let t = LinkToken::from_body(&stored.body)
        .unwrap()
        .redeem(&secret(), person, binding_key(), at)
        .unwrap();
    record(
        stored.key.clone(),
        LinkToken::KIND,
        t.to_body().unwrap(),
        Some(stored.revision.clone()),
    )
}

async fn consumed_by(store: &dyn Store) -> Option<String> {
    let stored = store.get(&token().key()).await.unwrap().unwrap();
    LinkToken::from_body(&stored.body)
        .unwrap()
        .consumed
        .map(|c| c.person)
}

#[tokio::test]
async fn concurrent_redemptions_commit_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    put_unredeemed(&store).await;
    let stored = store.get(&token().key()).await.unwrap().unwrap();
    let expected = Some(stored.revision.clone());

    let (first, second) = tokio::join!(
        store.put(redeemed(&stored, "p1", NOW + 1), expected.clone()),
        store.put(redeemed(&stored, "p2", NOW + 2), expected),
    );
    let results = [first.unwrap(), second.unwrap()];
    let committed = results
        .iter()
        .filter(|r| matches!(r, PutResult::Committed(_)))
        .count();
    let conflicts = results
        .iter()
        .filter(|r| matches!(r, PutResult::Conflict(_)))
        .count();
    assert_eq!((committed, conflicts), (1, 1));

    let winner = consumed_by(&store).await;
    assert!(
        winner.as_deref() == Some("p1") || winner.as_deref() == Some("p2"),
        "exactly one redemption is stored, got {winner:?}"
    );
}

#[tokio::test]
async fn redeemed_token_stays_redeemed_through_sync() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (FsStore::new(dir_a.path()), FsStore::new(dir_b.path()));
    put_unredeemed(&a).await;
    let report = sync(&a, &b).await.unwrap();
    assert!(report.conflicts.is_empty());
    assert_eq!(consumed_by(&b).await, None, "B holds the unredeemed token");

    let stored = a.get(&token().key()).await.unwrap().unwrap();
    let result = a
        .put(redeemed(&stored, "p1", NOW + 1), Some(stored.revision.clone()))
        .await
        .unwrap();
    assert!(matches!(result, PutResult::Committed(_)));

    let report = sync(&a, &b).await.unwrap();
    assert!(
        report.conflicts.is_empty(),
        "a redemption fast-forwards the unredeemed peer: {report:?}"
    );
    assert_eq!(consumed_by(&a).await.as_deref(), Some("p1"));
    assert_eq!(consumed_by(&b).await.as_deref(), Some("p1"));
}

#[tokio::test]
async fn independent_redemptions_surface_a_sync_conflict() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (a, b) = (FsStore::new(dir_a.path()), FsStore::new(dir_b.path()));
    put_unredeemed(&a).await;
    let report = sync(&a, &b).await.unwrap();
    assert!(report.conflicts.is_empty());

    for (store, person, at) in [(&a, "p1", NOW + 1), (&b, "p2", NOW + 2)] {
        let stored = store.get(&token().key()).await.unwrap().unwrap();
        let result = store
            .put(redeemed(&stored, person, at), Some(stored.revision.clone()))
            .await
            .unwrap();
        assert!(matches!(result, PutResult::Committed(_)));
    }

    let report = sync(&a, &b).await.unwrap();
    assert_eq!(report.conflicts.len(), 1, "{report:?}");
    assert_eq!(report.conflicts[0].key, token().key());
    assert_eq!(consumed_by(&a).await.as_deref(), Some("p1"), "A unchanged");
    assert_eq!(consumed_by(&b).await.as_deref(), Some("p2"), "B unchanged");
}

#[tokio::test]
async fn audit_entries_are_write_once_per_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let entry = AuditEntry {
        actor: FleetActor::Person("p1".into()),
        action: "spawn".into(),
        target: "caliban-ai/gonzalo".into(),
        at: NOW,
        surface_ref: None,
        result: AuditResult::Succeeded,
    };
    let key = entry.key("n1").unwrap();
    let first = store
        .put(
            record(key.clone(), AuditEntry::KIND, entry.to_body().unwrap(), None),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(first, PutResult::Committed(_)));

    let collision = AuditEntry {
        result: AuditResult::Denied,
        ..entry
    };
    let second = store
        .put(
            record(key, AuditEntry::KIND, collision.to_body().unwrap(), None),
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(second, PutResult::Conflict(_)),
        "a colliding audit key must conflict, never overwrite"
    );
}

#[tokio::test]
async fn second_bind_of_one_account_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsStore::new(dir.path());
    let bind = |person: &str| IdentityBinding {
        authenticator: Authenticator::Discord,
        subject: "1234".into(),
        person: person.into(),
        handle: None,
        email: None,
        bound_at: NOW,
        bound_by: BindingOrigin::Operator(FleetActor::Service("ariel-cli".into())),
    };
    let key = binding_key();
    let first = store
        .put(
            record(key.clone(), IdentityBinding::KIND, bind("p1").to_body().unwrap(), None),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(first, PutResult::Committed(_)));
    let second = store
        .put(
            record(key, IdentityBinding::KIND, bind("p2").to_body().unwrap(), None),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(second, PutResult::Conflict(_)));
}
```

- [ ] **Step 3: Run the tests.** `cargo test -p gonzalo-domain --test fleet_store` → 5 PASS. If `SyncReport` lacks `Debug` the `{report:?}` formats fail to compile; it derives `Debug` (`crates/gonzalo-core/src/sync.rs:32`), so they won't.

- [ ] **Step 4: Full gate, then commit.**

```bash
git add crates/gonzalo-domain/Cargo.toml crates/gonzalo-domain/tests/fleet_store.rs Cargo.lock
git commit -F - <<'EOF'
test(domain): fleet records against FsStore and sync (#278)

One commit and one conflict for concurrent redemptions; a redeemed
token fast-forwards an unredeemed peer; independent redemptions surface
a SyncConflict; audit entries and bindings are write-once per key.

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
```

(`Cargo.lock` may be unchanged; `git add` of an unchanged file is harmless.)

---

### Task 5: Facade, crate root re-exports and CHANGELOG

**Files:**
- Modify: `crates/gonzalo-domain/src/lib.rs`
- Modify: `crates/gonzalo-domain/Cargo.toml` (`description`)
- Modify: `crates/gonzalo/src/lib.rs` (the `pub use gonzalo_domain::{…}` block at lines 12-16, and the `facade_reexports` test module)
- Modify: `CHANGELOG.md` (the `[Unreleased]` upgrade paragraph ending "(#203)" at line 22, and the end of `### Added`, just before `### Changed` at line 97)

**Interfaces:**
- Consumes: all fleet types.
- Produces: `gonzalo_domain::{AuditEntry, AuditResult, Authenticator, BindingOrigin, ChannelConfig, Consumption, FleetActor, FleetKeyError, FleetRole, GrantScope, IdentityBinding, LinkSecret, LinkSecretError, LinkToken, Person, RedeemError, RoleGrant, VerifiedEmail}` and the same names plus the `fleet` module from `gonzalo`.

- [ ] **Step 1: Write the failing facade test.** Add inside `mod facade_reexports` in `crates/gonzalo/src/lib.rs`:

```rust
    #[test]
    fn fleet_records_are_re_exported() {
        let person = Person {
            display_name: "Ada".into(),
            email: None,
        };
        assert_eq!(Person::KIND, RecordKind::Person);
        assert!(person.to_body().is_ok());
        assert_eq!(fleet::FLEET_NAMESPACE, "fleet");
        assert_eq!(fleet::FLEET_AUDIT_NAMESPACE, "fleet-audit");

        let secret = LinkSecret::from_bytes([0; 32]);
        let token = LinkToken::new(
            &secret,
            FleetRole::Viewer,
            GrantScope::Fleet,
            None,
            FleetActor::Service("test".into()),
            0,
            1,
        );
        assert_eq!(token.key(), LinkToken::key_for_secret(&secret));
        let _: Option<(
            AuditEntry,
            AuditResult,
            Authenticator,
            BindingOrigin,
            ChannelConfig,
            Consumption,
            FleetKeyError,
            IdentityBinding,
            LinkSecretError,
            RedeemError,
            RoleGrant,
            VerifiedEmail,
        )> = None;
    }
```

- [ ] **Step 2: Run to verify it fails.** `cargo test -p gonzalo --lib facade_reexports` → compile error, `Person` not found.

- [ ] **Step 3: Re-export from `gonzalo-domain`.** In `crates/gonzalo-domain/src/lib.rs`, after `pub use codec::RecordCodec;` add:

```rust
pub use fleet::{
    AuditEntry, AuditResult, Authenticator, BindingOrigin, ChannelConfig, Consumption, FleetActor,
    FleetKeyError, FleetRole, GrantScope, IdentityBinding, LinkSecret, LinkSecretError, LinkToken,
    Person, RedeemError, RoleGrant, VerifiedEmail,
};
```

And set `crates/gonzalo-domain/Cargo.toml` `description` to:

```toml
description = "Typed domain views over gonzalo records (memory, sessions, checkpoints, tickets, fleet access control)"
```

- [ ] **Step 4: Re-export from the facade.** Replace the `pub use gonzalo_domain::{…};` block in `crates/gonzalo/src/lib.rs` with:

```rust
pub use gonzalo_domain::{
    Actor, ActorRole, AuditEntry, AuditResult, Authenticator, BindingOrigin, BodyFormat,
    ChannelConfig, Checkpoint, Consumption, Container, FleetActor, FleetKeyError, FleetRole,
    GrantScope, IdentityBinding, Link, LinkKind, LinkSecret, LinkSecretError, LinkTarget,
    LinkToken, MemoryTier, Person, Priority, PriorityLevel, Provider, RecordCodec, RedeemError,
    Resolution, RoleGrant, Session, State, StateCategory, Ticket, TicketBody, TicketEvent, Topic,
    Turn, VerifiedEmail, fleet,
};
```

- [ ] **Step 5: CHANGELOG.** In `CHANGELOG.md`, directly after the upgrade paragraph that ends `See the guide's "Deletion, reset & collection" page and ADR 0021. (#203)`, add a blank line and this paragraph:

```markdown
The new fleet access-control record kinds need the same all-at-once upgrade: a
binary built before them fails to decode those records. Upgrade every gonzalod,
CLI and embedded consumer before any writer uses them. (#278)
```

Then add this bullet as the last item of `### Added` (immediately before the blank line preceding `### Changed`):

```markdown
- **Fleet access-control records.** Six record kinds for people and their
  access to the fleet: `Person`, `IdentityBinding`, `RoleGrant`,
  `ChannelConfig`, `LinkToken` and `AuditEntry`. Typed views live in
  `gonzalo-domain`'s `fleet` module and are re-exported by the `gonzalo` facade.
  Records sit in the `fleet` namespace, with audit entries in `fleet-audit`.
  A link token stores only a hash of its secret and is marked consumed when
  redeemed; audit entries are write-once. gonzalo stores these records but
  doesn't evaluate the roles in them. See ADR 0022. (#278)
```

- [ ] **Step 6: Run the tests.** `cargo test -p gonzalo --lib facade_reexports` → PASS. `cargo test -p gonzalo-domain` → PASS.

- [ ] **Step 7: Full gate, then commit.**

```bash
git add crates/gonzalo-domain/src/lib.rs crates/gonzalo-domain/Cargo.toml crates/gonzalo/src/lib.rs CHANGELOG.md
git commit -F - <<'EOF'
feat: re-export fleet access-control records and note the upgrade (#278)

Claude-Session: https://claude.ai/code/session_019C89EVJgoefhAmPcrbP4eu
EOF
```
