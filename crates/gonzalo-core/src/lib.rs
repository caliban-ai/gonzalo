//! Generic versioned Record/Store core for gonzalo.

pub mod identity;
pub mod key;

pub use identity::Identity;
pub use key::{KeyPrefix, RecordKey};

pub mod revision;
pub use revision::{ContentHash, Revision};

pub mod record;
pub use record::{Body, MergeClass, Meta, Record, RecordKind};

pub mod tombstone;
pub use tombstone::{
    CONSUMER_TOMBSTONE_REJECTED, DEFAULT_ANCESTOR_CAP, DeletePlan, PurgePlan, PutPlan,
    TOMBSTONE_DOMAIN, fold_ancestors, now_ms, plan_delete, plan_purge, plan_put, plan_put_raw,
    reconciled_ancestors, reconciled_record, tombstone_hash, tombstone_of, tombstone_winner,
    validate_ancestor_cap,
};

pub mod manifest;
pub use manifest::{Manifest, Reconciliation, desired_set};

pub mod vector_manifest;
pub use vector_manifest::VectorManifest;

pub mod gc;
pub use gc::{
    DEFAULT_MIN_AGE, GcReport, SweepPolicy, gc_blobs, gc_blobs_with, live_blob_hashes, sweep_blobs,
    sweep_blobs_with,
};

pub mod error;
pub use error::{CoreError, Result};

pub mod store;
pub use store::{BlobEntry, BlobStore, Conflict, DeleteResult, PutResult, Store};

pub mod merge;
pub use merge::{MergeOutcome, merge};

pub mod paths;
pub use paths::{decode_segment, object_key, record_components, segment};

pub mod ancestry;
pub use ancestry::AncestryStore;

pub mod sync;
pub use sync::{SyncConflict, SyncReport, sync, sync_with_ancestry};

pub mod reset;
pub use reset::{ResetReport, reset, reset_as};

pub mod collect;
pub use collect::{CollectReport, collect};

pub mod undelete;
pub use undelete::undelete;

#[cfg(any(test, feature = "conformance"))]
pub mod memstore;

#[cfg(test)]
mod test_support;

#[cfg(feature = "conformance")]
pub mod conformance;
