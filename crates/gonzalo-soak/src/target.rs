//! The S3 target for the soak, resolved from the environment.
//!
//! Mirrors the existing `gonzalo-store-s3/tests/integration.rs` convention: the
//! backend is provisioned *externally* (see `scripts/rustfs-up.sh`) and its
//! coordinates arrive via env vars. When they are unset the soak **skips**
//! (returns `None`) rather than fails — so `cargo test --workspace` on a machine
//! without an S3 backend / docker stays green.

/// S3 backend coordinates for spawning S3-backed `gonzalod` replicas.
#[derive(Debug, Clone)]
pub struct S3Target {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub region: Option<String>,
}

impl S3Target {
    /// Resolve from an env accessor. Returns `None` (→ skip) unless the endpoint,
    /// bucket, and AWS credentials are all present. Region is optional.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        Some(Self {
            endpoint: non_empty(get("GONZALO_S3_TEST_ENDPOINT"))?,
            bucket: non_empty(get("GONZALO_S3_TEST_BUCKET"))?,
            access_key: non_empty(get("AWS_ACCESS_KEY_ID"))?,
            secret_key: non_empty(get("AWS_SECRET_ACCESS_KEY"))?,
            region: non_empty(get("GONZALO_S3_TEST_REGION")),
        })
    }

    /// Resolve from the process environment, or `None` to skip.
    pub fn from_process_env() -> Option<Self> {
        Self::from_env(|k| std::env::var(k).ok())
    }
}

fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn resolves_when_all_required_present() {
        let t = S3Target::from_env(env(&[
            ("GONZALO_S3_TEST_ENDPOINT", "http://127.0.0.1:3900"),
            ("GONZALO_S3_TEST_BUCKET", "soak"),
            ("AWS_ACCESS_KEY_ID", "AK"),
            ("AWS_SECRET_ACCESS_KEY", "SK"),
        ]))
        .expect("all required present");
        assert_eq!(t.bucket, "soak");
        assert_eq!(t.region, None);
    }

    #[test]
    fn skips_when_endpoint_missing() {
        assert!(
            S3Target::from_env(env(&[
                ("GONZALO_S3_TEST_BUCKET", "soak"),
                ("AWS_ACCESS_KEY_ID", "AK"),
                ("AWS_SECRET_ACCESS_KEY", "SK"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn skips_when_credentials_missing() {
        assert!(
            S3Target::from_env(env(&[
                ("GONZALO_S3_TEST_ENDPOINT", "http://127.0.0.1:3900"),
                ("GONZALO_S3_TEST_BUCKET", "soak"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn treats_blank_as_unset() {
        assert!(
            S3Target::from_env(env(&[
                ("GONZALO_S3_TEST_ENDPOINT", "  "),
                ("GONZALO_S3_TEST_BUCKET", "soak"),
                ("AWS_ACCESS_KEY_ID", "AK"),
                ("AWS_SECRET_ACCESS_KEY", "SK"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn region_is_optional_but_carried() {
        let t = S3Target::from_env(env(&[
            ("GONZALO_S3_TEST_ENDPOINT", "http://127.0.0.1:3900"),
            ("GONZALO_S3_TEST_BUCKET", "soak"),
            ("AWS_ACCESS_KEY_ID", "AK"),
            ("AWS_SECRET_ACCESS_KEY", "SK"),
            ("GONZALO_S3_TEST_REGION", "garage"),
        ]))
        .unwrap();
        assert_eq!(t.region.as_deref(), Some("garage"));
    }
}
