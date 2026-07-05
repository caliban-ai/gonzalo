//! Namespace-scoped principal auth for the daemon (ADR 0015).
//!
//! A bearer token maps to a [`Principal`] carrying per-namespace `read`/`write`
//! scopes (`"*"` = any namespace). [`Auth`] is the token registry; the
//! transports authenticate (token → principal) at the edge and authorize
//! ([`Principal::allows`]) in each handler where the target namespace is known.
//! This module is pure and transport-agnostic.

use std::collections::HashMap;

use serde::Deserialize;

/// The kind of access an operation needs on a namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

/// An authenticated caller and the namespaces it may read/write. `"*"` in a list
/// grants that access on every namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    name: String,
    read: Vec<String>,
    write: Vec<String>,
    /// Whether this principal was established by real authentication (a
    /// configured token) versus the implicit identity of open/disabled mode.
    /// Only authenticated principals stamp authorship on writes.
    authenticated: bool,
}

impl Principal {
    pub fn new(name: impl Into<String>, read: Vec<String>, write: Vec<String>) -> Self {
        Self {
            name: name.into(),
            read,
            write,
            authenticated: true,
        }
    }

    /// A full-access principal (`read`/`write` on `"*"`).
    pub fn admin(name: impl Into<String>) -> Self {
        Self::new(name, vec!["*".into()], vec!["*".into()])
    }

    /// The implicit full-access identity of open (disabled-auth) mode. Unlike a
    /// configured principal, it is not authenticated, so it does not stamp
    /// authorship — open mode leaves the daemon a transparent store.
    pub fn open() -> Self {
        Self {
            authenticated: false,
            ..Self::admin("local")
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether authorship should be stamped from this principal on writes.
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Whether this principal has `access` on `namespace` (exact match or `"*"`).
    pub fn allows(&self, access: Access, namespace: &str) -> bool {
        let scopes = match access {
            Access::Read => &self.read,
            Access::Write => &self.write,
        };
        scopes.iter().any(|s| s == "*" || s == namespace)
    }
}

/// The daemon's token registry. `Disabled` means no auth was configured — every
/// request is served as an implicit admin (local/library behavior).
pub enum Auth {
    Disabled,
    Enabled(HashMap<String, Principal>),
}

impl Auth {
    /// Resolve a bearer token to its principal. `Disabled` always yields an
    /// implicit admin; `Enabled` yields the matching principal, or `None` when
    /// the token is missing or unknown (the caller maps that to 401).
    pub fn authenticate(&self, token: Option<&str>) -> Option<Principal> {
        match self {
            Auth::Disabled => Some(Principal::open()),
            Auth::Enabled(by_token) => token.and_then(|t| by_token.get(t)).cloned(),
        }
    }

    /// Parse a TOML principals file into an `Enabled` registry. Duplicate tokens
    /// are an error (ambiguous identity).
    pub fn parse_toml(s: &str) -> Result<Auth, String> {
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            principal: Vec<Def>,
        }
        #[derive(Deserialize)]
        struct Def {
            name: String,
            token: String,
            #[serde(default)]
            read: Vec<String>,
            #[serde(default)]
            write: Vec<String>,
        }

        let file: File = toml::from_str(s).map_err(|e| e.to_string())?;
        let mut by_token = HashMap::new();
        for def in file.principal {
            let principal = Principal::new(def.name, def.read, def.write);
            if by_token.insert(def.token, principal).is_some() {
                return Err("duplicate token in auth file".to_string());
            }
        }
        Ok(Auth::Enabled(by_token))
    }

    /// Resolve the daemon's auth from the environment (pure — `read_file` injects
    /// the file IO so this is unit-testable):
    ///
    /// - `GONZALO_AUTH_FILE` → parse that TOML principals file;
    /// - else `GONZALO_TOKEN` → a single admin principal (back-compat);
    /// - else → `Disabled` (open).
    pub fn from_env(
        get: impl Fn(&str) -> Option<String>,
        read_file: impl Fn(&str) -> Result<String, String>,
    ) -> Result<Auth, String> {
        if let Some(path) = get("GONZALO_AUTH_FILE").filter(|s| !s.is_empty()) {
            Auth::parse_toml(&read_file(&path)?)
        } else if let Some(token) = get("GONZALO_TOKEN").filter(|s| !s.is_empty()) {
            Ok(Auth::Enabled(HashMap::from([(
                token,
                Principal::admin("root"),
            )])))
        } else {
            Ok(Auth::Disabled)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_exact_wildcard_and_denies() {
        let p = Principal::new("p", vec!["memory".into()], vec!["*".into()]);
        assert!(p.allows(Access::Read, "memory"));
        assert!(!p.allows(Access::Read, "sessions"));
        // write is wildcard: any namespace.
        assert!(p.allows(Access::Write, "sessions"));
        assert!(p.allows(Access::Write, "anything"));
    }

    #[test]
    fn parse_toml_builds_scoped_principals() {
        let toml = r#"
[[principal]]
name  = "caliban"
token = "s3cret"
read  = ["memory", "sessions"]
write = ["memory"]

[[principal]]
name  = "admin"
token = "root"
read  = ["*"]
write = ["*"]
"#;
        let auth = Auth::parse_toml(toml).unwrap();
        let caliban = auth.authenticate(Some("s3cret")).unwrap();
        assert_eq!(caliban.name(), "caliban");
        assert!(caliban.allows(Access::Read, "sessions"));
        assert!(!caliban.allows(Access::Write, "sessions"));

        let admin = auth.authenticate(Some("root")).unwrap();
        assert!(admin.allows(Access::Write, "whatever"));

        assert!(auth.authenticate(Some("nope")).is_none());
        assert!(auth.authenticate(None).is_none());
    }

    #[test]
    fn parse_toml_rejects_malformed_and_duplicate_tokens() {
        assert!(Auth::parse_toml("not = valid = toml").is_err());
        let dup = r#"
[[principal]]
name = "a"
token = "same"
[[principal]]
name = "b"
token = "same"
"#;
        assert!(Auth::parse_toml(dup).is_err());
    }

    #[test]
    fn disabled_authenticates_everything_as_admin() {
        let auth = Auth::Disabled;
        let p = auth.authenticate(None).unwrap();
        assert!(p.allows(Access::Write, "any"));
    }

    #[test]
    fn from_env_precedence_file_then_token_then_disabled() {
        let file = |_: &str| Ok("[[principal]]\nname='a'\ntoken='t'\nread=['ns']\nwrite=[]".into());

        // File wins when set.
        let by_file = Auth::from_env(
            |k| (k == "GONZALO_AUTH_FILE").then(|| "auth.toml".into()),
            file,
        )
        .unwrap();
        assert!(
            by_file
                .authenticate(Some("t"))
                .unwrap()
                .allows(Access::Read, "ns")
        );

        // Token → single admin.
        let by_token = Auth::from_env(
            |k| (k == "GONZALO_TOKEN").then(|| "root".into()),
            |_| Err("no file".into()),
        )
        .unwrap();
        assert!(
            by_token
                .authenticate(Some("root"))
                .unwrap()
                .allows(Access::Write, "x")
        );

        // Neither → disabled (open).
        let disabled = Auth::from_env(|_| None, |_| Err("no file".into())).unwrap();
        assert!(matches!(disabled, Auth::Disabled));
    }
}
