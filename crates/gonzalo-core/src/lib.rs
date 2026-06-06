//! Generic versioned Record/Store core for gonzalo.

pub mod identity;
pub mod key;

pub use identity::Identity;
pub use key::{KeyPrefix, RecordKey};

pub mod revision;
pub use revision::{ContentHash, Revision};
