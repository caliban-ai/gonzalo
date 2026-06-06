//! Session (conversation transcript) view.

use crate::codec::RecordCodec;
use gonzalo_core::RecordKind;
use serde::{Deserialize, Serialize};

/// One transcript turn (role + text). Kept deliberately minimal for M1;
/// richer turn modeling tracks caliban's session schema in a later milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// A conversation session: an ordered, append-only list of turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub turns: Vec<Turn>,
}
impl RecordCodec for Session {}
impl Session {
    pub const KIND: RecordKind = RecordKind::Session;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::RecordCodec;

    #[test]
    fn session_roundtrips() {
        let s = Session {
            name: "research".into(),
            turns: vec![Turn { role: "user".into(), text: "hi".into() }],
        };
        assert_eq!(Session::from_body(&s.to_body().unwrap()).unwrap(), s);
        assert_eq!(Session::KIND, RecordKind::Session);
    }
}
