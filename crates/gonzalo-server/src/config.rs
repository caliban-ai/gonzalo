//! Runtime substrate selection for `gonzalod` (gonzalo#62).
//!
//! The daemon can back its record + blob store with the local filesystem
//! (default) or an S3-compatible object store (MinIO/Garage), chosen by
//! environment variables. Parsing is a pure function over an env accessor so it
//! is unit-tested without touching the process environment; the binary does the
//! (impure, async) store construction from the parsed [`StoreConfig`].

/// The selected storage substrate and its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreConfig {
    /// Filesystem-backed store rooted at `root`.
    Fs { root: String },
    /// S3-compatible store. `endpoint`/`region` are `None` when the ambient AWS
    /// configuration should supply them (real AWS S3); `endpoint` is set for
    /// MinIO/Garage.
    S3 {
        bucket: String,
        endpoint: Option<String>,
        region: Option<String>,
    },
}

impl StoreConfig {
    /// Parse the substrate selection from an environment accessor `get`.
    ///
    /// - `GONZALO_STORE` = `fs` (default) or `s3`.
    /// - `fs`: `GONZALO_ROOT` (default `./gonzalo-data`).
    /// - `s3`: `GONZALO_S3_BUCKET` (required), `GONZALO_S3_ENDPOINT`,
    ///   `GONZALO_S3_REGION` (both optional); credentials come from the standard
    ///   `AWS_*` environment as usual.
    ///
    /// Returns `Err` for an unknown `GONZALO_STORE` or a missing required S3
    /// variable, so the daemon fails fast with a clear message instead of
    /// silently falling back.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<StoreConfig, String> {
        match get("GONZALO_STORE").as_deref() {
            None | Some("") | Some("fs") => Ok(StoreConfig::Fs {
                root: get("GONZALO_ROOT").unwrap_or_else(|| "./gonzalo-data".into()),
            }),
            Some("s3") => {
                let bucket = get("GONZALO_S3_BUCKET")
                    .filter(|b| !b.is_empty())
                    .ok_or("GONZALO_STORE=s3 requires GONZALO_S3_BUCKET")?;
                Ok(StoreConfig::S3 {
                    bucket,
                    endpoint: get("GONZALO_S3_ENDPOINT").filter(|s| !s.is_empty()),
                    region: get("GONZALO_S3_REGION").filter(|s| !s.is_empty()),
                })
            }
            Some(other) => Err(format!(
                "unknown GONZALO_STORE={other:?} (expected \"fs\" or \"s3\")"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build an accessor over a fixed map (owns its data — no borrow of `pairs`).
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn default_is_fs_with_default_root() {
        let cfg = StoreConfig::from_env(env(&[])).unwrap();
        assert_eq!(
            cfg,
            StoreConfig::Fs {
                root: "./gonzalo-data".into()
            }
        );
    }

    #[test]
    fn fs_honors_root() {
        let cfg = StoreConfig::from_env(env(&[("GONZALO_STORE", "fs"), ("GONZALO_ROOT", "/data")]))
            .unwrap();
        assert_eq!(
            cfg,
            StoreConfig::Fs {
                root: "/data".into()
            }
        );
    }

    #[test]
    fn s3_requires_bucket() {
        let err = StoreConfig::from_env(env(&[("GONZALO_STORE", "s3")])).unwrap_err();
        assert!(err.contains("GONZALO_S3_BUCKET"), "got {err}");
    }

    #[test]
    fn s3_reads_bucket_endpoint_region() {
        let cfg = StoreConfig::from_env(env(&[
            ("GONZALO_STORE", "s3"),
            ("GONZALO_S3_BUCKET", "gonzalo"),
            ("GONZALO_S3_ENDPOINT", "http://garage:3900"),
            ("GONZALO_S3_REGION", "garage"),
        ]))
        .unwrap();
        assert_eq!(
            cfg,
            StoreConfig::S3 {
                bucket: "gonzalo".into(),
                endpoint: Some("http://garage:3900".into()),
                region: Some("garage".into()),
            }
        );
    }

    #[test]
    fn s3_endpoint_and_region_optional() {
        let cfg =
            StoreConfig::from_env(env(&[("GONZALO_STORE", "s3"), ("GONZALO_S3_BUCKET", "b")]))
                .unwrap();
        assert_eq!(
            cfg,
            StoreConfig::S3 {
                bucket: "b".into(),
                endpoint: None,
                region: None,
            }
        );
    }

    #[test]
    fn unknown_store_is_an_error() {
        let err = StoreConfig::from_env(env(&[("GONZALO_STORE", "cassandra")])).unwrap_err();
        assert!(err.contains("cassandra"), "got {err}");
    }
}
