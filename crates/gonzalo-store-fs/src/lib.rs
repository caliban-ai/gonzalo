//! Filesystem storage substrate for gonzalo.

mod layout;

use async_trait::async_trait;
use gonzalo_core::{
    CoreError, KeyPrefix, PutResult, Record, RecordKey, Result, Store, store::Conflict,
};
use std::path::PathBuf;

/// A `Store` backed by JSON files under a root directory.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    async fn read_record(&self, key: &RecordKey) -> Result<Option<Record>> {
        let path = layout::record_path(&self.root, key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let rec: Record =
                    serde_json::from_slice(&bytes).map_err(|e| CoreError::Serde(e.to_string()))?;
                Ok(Some(rec))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(CoreError::Backend(e.to_string())),
        }
    }
}

#[async_trait]
impl Store for FsStore {
    async fn get(&self, key: &RecordKey) -> Result<Option<Record>> {
        self.read_record(key).await
    }

    async fn put(&self, record: Record, expected: Option<gonzalo_core::Revision>) -> Result<PutResult> {
        // Optimistic concurrency: the stored revision must equal `expected`.
        let current = self.read_record(&record.key).await?;
        let current_rev = current.as_ref().map(|r| r.revision.clone());
        if current_rev != expected {
            if let Some(current) = current {
                return Ok(PutResult::Conflict(Box::new(Conflict {
                    key: record.key.clone(),
                    expected,
                    current,
                })));
            }
            // expected referenced a revision but nothing exists: treat as conflict
            return Err(CoreError::NotFound(record.key.clone()));
        }

        let path = layout::record_path(&self.root, &record.key);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| CoreError::Backend(e.to_string()))?;
        }
        let bytes =
            serde_json::to_vec_pretty(&record).map_err(|e| CoreError::Serde(e.to_string()))?;
        // Atomic write: temp file + rename.
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, &bytes).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        tokio::fs::rename(&tmp, &path).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        Ok(PutResult::Committed(record.revision))
    }

    async fn list(&self, prefix: &KeyPrefix) -> Result<Vec<RecordKey>> {
        let mut out = Vec::new();
        collect_keys(&self.root, prefix, &mut out).await?;
        Ok(out)
    }
}

/// Walk `<root>/<ns>/<col>/<id>.json` and collect keys matching `prefix`.
async fn collect_keys(root: &std::path::Path, prefix: &KeyPrefix, out: &mut Vec<RecordKey>) -> Result<()> {
    let mut namespaces = match tokio::fs::read_dir(root).await {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(CoreError::Backend(e.to_string())),
    };
    while let Some(ns) = namespaces.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
        let ns_name = ns.file_name().to_string_lossy().to_string();
        let mut cols = tokio::fs::read_dir(ns.path()).await.map_err(|e| CoreError::Backend(e.to_string()))?;
        while let Some(col) = cols.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
            let col_name = col.file_name().to_string_lossy().to_string();
            let mut files = tokio::fs::read_dir(col.path()).await.map_err(|e| CoreError::Backend(e.to_string()))?;
            while let Some(f) = files.next_entry().await.map_err(|e| CoreError::Backend(e.to_string()))? {
                let fname = f.file_name().to_string_lossy().to_string();
                if let Some(id) = fname.strip_suffix(".json") {
                    let key = RecordKey::new(ns_name.clone(), col_name.clone(), id.to_string());
                    if prefix.matches(&key) {
                        out.push(key);
                    }
                }
            }
        }
    }
    Ok(())
}
