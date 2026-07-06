//! Generic versioned Record/Store core for gonzalo.

pub mod identity;
pub mod key;

pub use identity::Identity;
pub use key::{KeyPrefix, RecordKey};

pub mod revision;
pub use revision::{ContentHash, Revision};

pub mod record;
pub use record::{Body, MergeClass, Meta, Record, RecordKind};

pub mod manifest;
pub use manifest::{Manifest, Reconciliation, desired_set};

pub mod gc;
pub use gc::{GcReport, gc_blobs, live_slice_hashes, unreferenced_slices};

pub mod error;
pub use error::{CoreError, Result};

pub mod store;
pub use store::{BlobStore, Conflict, PutResult, Store};

pub mod merge;
pub use merge::{MergeOutcome, merge};

pub mod paths;
pub use paths::{object_key, record_components, segment};

pub mod ancestry;
pub use ancestry::AncestryStore;

pub mod sync;
pub use sync::{SyncConflict, SyncReport, sync, sync_with_ancestry};

#[cfg(feature = "conformance")]
pub mod conformance;
