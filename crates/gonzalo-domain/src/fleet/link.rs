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
        if digits.len() != 64
            || !digits
                .iter()
                .all(|&b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        {
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
        RecordKey::new(
            FLEET_NAMESPACE,
            LINK_TOKENS_COLLECTION,
            self.token_hash.clone(),
        )
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
        let redeemed = token(None)
            .redeem(&secret(), "p1", binding(), NOW + 1)
            .unwrap();
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
        assert!(
            token(Some("p1"))
                .redeem(&secret(), "p1", binding(), NOW)
                .is_ok()
        );
    }

    #[test]
    fn redeem_checks_run_in_order() {
        let consumed = token(Some("p1"))
            .redeem(&secret(), "p1", binding(), NOW)
            .unwrap();
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
