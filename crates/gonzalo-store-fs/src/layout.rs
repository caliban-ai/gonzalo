//! On-disk path mapping. Each record is one JSON file at
//! `<root>/<namespace>/<collection>/<id>.json`. Components are percent-ish
//! sanitized so arbitrary ids cannot escape the root.

use gonzalo_core::{RecordKey, record_components};
use std::path::{Path, PathBuf};

/// The file path for a record's JSON under `root`.
pub fn record_path(root: &Path, key: &RecordKey) -> PathBuf {
    let (ns, col, file) = record_components(key);
    root.join(ns).join(col).join(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_path_is_nested_json() {
        let root = Path::new("/tmp/g");
        let key = RecordKey::new("caliban", "topics", "rust");
        assert_eq!(
            record_path(root, &key),
            Path::new("/tmp/g/caliban/topics/rust.json")
        );
    }

    #[test]
    fn unsafe_chars_are_neutralized() {
        let root = Path::new("/tmp/g");
        let key = RecordKey::new("..", "../etc", "../../passwd");
        let p = record_path(root, &key);
        assert!(p.starts_with("/tmp/g"));
        assert!(!p.to_string_lossy().contains(".."));
    }
}
