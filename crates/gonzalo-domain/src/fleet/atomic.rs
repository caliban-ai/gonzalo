//! Serde helper that stores a value wrapped in a one-element JSON array, so
//! it merges as a single atomic unit under `gonzalo-core`'s `Structured`
//! merge instead of being merged key by key.
//!
//! `structured_merge` (`crates/gonzalo-core/src/merge.rs`, `merge_value`)
//! recurses into JSON *objects*, merging them field by field, but compares
//! JSON *arrays* by equality and never recurses into them. For a composite
//! field whose type is an externally-tagged enum (`FleetActor`,
//! `BindingOrigin`) or a small object (`VerifiedEmail`), that object recursion
//! is a hazard: two sides that changed the field to *different* variants (or
//! different halves of the same struct) merge key by key into a value that
//! either fails to decode (two enum variant tags at once) or asserts something
//! nobody wrote (an address from one side paired with a verified flag from the
//! other) — the same family of hazard as gonzalo#204. Wrapping the field as
//! `#[serde(with = "super::atomic")]` makes core's array-is-atomic rule apply:
//! a field changed on only one side is taken whole, and a field changed
//! differently on both sides is a genuine conflict (ADR 0022).
//!
//! Used by the five fields listed in ADR 0022 / spec §6: `IdentityBinding`'s
//! `authenticator`, `email` and `bound_by`, and `RoleGrant`'s `scope` and
//! `granted_by`.

use serde::de::{Error as DeError, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::marker::PhantomData;

/// Write `value` as a JSON array with exactly one element.
pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(1))?;
    seq.serialize_element(value)?;
    seq.end()
}

/// Read a JSON array with exactly one element and return it. Any other length
/// is a serde error.
pub fn deserialize<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    struct OneVisitor<T>(PhantomData<T>);

    impl<'de, T: Deserialize<'de>> Visitor<'de> for OneVisitor<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a sequence of exactly one element")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<T, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let value: T = seq
                .next_element()?
                .ok_or_else(|| DeError::invalid_length(0, &self))?;
            if seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(DeError::invalid_length(2, &self));
            }
            Ok(value)
        }
    }

    deserializer.deserialize_seq(OneVisitor(PhantomData))
}
