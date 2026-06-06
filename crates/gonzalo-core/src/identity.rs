//! Contributor identity attached to every write.

use serde::{Deserialize, Serialize};

/// Who made a change. In local mode this is a configured local identity;
/// in daemon mode the server authenticates it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub display: Option<String>,
}

impl Identity {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_id_and_no_display() {
        let i = Identity::new("john");
        assert_eq!(i.id, "john");
        assert_eq!(i.display, None);
    }
}
