//! Multi-connection ticket config and the provider registry (ADR 0010).
//!
//! A `tickets.toml` holds an array of `[[connection]]` tables. This crate parses
//! it and is the **registry** that turns each connection into a
//! `Box<dyn TicketSource>` — it sits *above* the connector crates, which would
//! otherwise be a dependency cycle (they depend on `gonzalo-ticket` for the
//! trait). Secrets are referenced by env-var name, never stored in the file.

use gonzalo_domain::StateCategory;
use gonzalo_ticket::{StateMapping, StateSignal, TicketSource};
use gonzalo_ticket_github::GitHubProjectSource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A named, live ticket source built from a connection: `(connection name, source)`.
///
/// The first tuple element (`.0`) is the connection's name; the second is its
/// live source.
pub type NamedSource = (String, Box<dyn TicketSource>);

/// Top-level config: a list of connections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(rename = "connection", default)]
    pub connections: Vec<Connection>,
}

/// One ticket connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub name: String,
    /// Provider key in the registry, e.g. `"github-projects"`.
    pub provider: String,
    pub org: String,
    pub project: u32,
    /// Name of the env var holding the access token (never the token itself).
    pub token_env: String,
    /// Status-name → category map. The reserved key `"default"` sets the
    /// fallback category; all other keys are board column names.
    #[serde(default)]
    pub state_map: BTreeMap<String, String>,
}

/// Config / registry failures.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading config {0}: {1}")]
    Read(String, String),
    #[error("parsing config: {0}")]
    Parse(String),
    #[error("connection {conn}: env var {var} is not set")]
    MissingEnv { conn: String, var: String },
    #[error("connection {conn}: unknown provider {provider}")]
    UnknownProvider { conn: String, provider: String },
    #[error("connection {conn}: unknown state category {value:?}")]
    BadCategory { conn: String, value: String },
    #[error("building source: {0}")]
    Source(String),
}

/// Load and parse a `tickets.toml` from disk.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Read(path.display().to_string(), e.to_string()))?;
    parse(&text)
}

/// Parse config from a TOML string.
pub fn parse(text: &str) -> Result<Config, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))
}

impl Config {
    /// Convenience: load and parse from a path.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        load(path)
    }

    /// Build a live `TicketSource` for each connection.
    pub fn sources(&self) -> Result<Vec<NamedSource>, ConfigError> {
        self.connections
            .iter()
            .map(|c| Ok((c.name.clone(), build_source(c)?)))
            .collect()
    }
}

/// The registry: map a connection's `provider` to a constructed source.
pub fn build_source(conn: &Connection) -> Result<Box<dyn TicketSource>, ConfigError> {
    let token = std::env::var(&conn.token_env).map_err(|_| ConfigError::MissingEnv {
        conn: conn.name.clone(),
        var: conn.token_env.clone(),
    })?;
    match conn.provider.as_str() {
        "github-projects" => {
            let mapping = state_mapping(conn)?;
            let src = GitHubProjectSource::new(&conn.org, conn.project, token, mapping)
                .map_err(|e| ConfigError::Source(e.to_string()))?;
            Ok(Box::new(src))
        }
        other => Err(ConfigError::UnknownProvider {
            conn: conn.name.clone(),
            provider: other.to_string(),
        }),
    }
}

fn state_mapping(conn: &Connection) -> Result<StateMapping, ConfigError> {
    let mut by_value = BTreeMap::new();
    let mut default = StateCategory::Open;
    for (k, v) in &conn.state_map {
        let cat = parse_category(v).ok_or_else(|| ConfigError::BadCategory {
            conn: conn.name.clone(),
            value: v.clone(),
        })?;
        if k == "default" {
            default = cat;
        } else {
            by_value.insert(k.clone(), cat);
        }
    }
    Ok(StateMapping {
        signal: StateSignal::NativeStatus,
        by_value,
        default,
    })
}

fn parse_category(s: &str) -> Option<StateCategory> {
    Some(match s {
        "triage" => StateCategory::Triage,
        "backlog" => StateCategory::Backlog,
        "open" => StateCategory::Open,
        "in_progress" => StateCategory::InProgress,
        "pending" => StateCategory::Pending,
        "done" => StateCategory::Done,
        "canceled" => StateCategory::Canceled,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the tests that mutate the shared `TEST_TICKET_TOKEN` env var,
    /// which would otherwise race under `cargo test`'s default parallelism.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const SAMPLE: &str = r#"
[[connection]]
name      = "caliban-ai-board"
provider  = "github-projects"
org       = "caliban-ai"
project   = 1
token_env = "TEST_TICKET_TOKEN"

[connection.state_map]
default       = "open"
"Todo"        = "open"
"In Progress" = "in_progress"
"Done"        = "done"
"#;

    #[test]
    fn parses_a_connection() {
        let cfg = parse(SAMPLE).unwrap();
        assert_eq!(cfg.connections.len(), 1);
        let c = &cfg.connections[0];
        assert_eq!(c.provider, "github-projects");
        assert_eq!(c.org, "caliban-ai");
        assert_eq!(c.project, 1);
        assert_eq!(
            c.state_map.get("In Progress").map(String::as_str),
            Some("in_progress")
        );
    }

    #[test]
    fn state_mapping_pulls_out_default_and_entries() {
        let cfg = parse(SAMPLE).unwrap();
        let m = state_mapping(&cfg.connections[0]).unwrap();
        assert_eq!(m.signal, StateSignal::NativeStatus);
        assert_eq!(m.default, StateCategory::Open);
        assert_eq!(m.category_of("In Progress"), StateCategory::InProgress);
        assert_eq!(m.category_of("Done"), StateCategory::Done);
        assert_eq!(m.category_of("Nonexistent"), StateCategory::Open);
    }

    #[test]
    fn missing_env_var_is_reported() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_TICKET_TOKEN")
        };
        let cfg = parse(SAMPLE).unwrap();
        let err = build_source(&cfg.connections[0]).err().unwrap();
        assert!(matches!(err, ConfigError::MissingEnv { .. }));
    }

    #[test]
    fn unknown_provider_is_reported() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let text = SAMPLE.replace("github-projects", "bogus-tracker");
        let cfg = parse(&text).unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("TEST_TICKET_TOKEN", "x")
        };
        let err = build_source(&cfg.connections[0]).err().unwrap();
        assert!(matches!(err, ConfigError::UnknownProvider { .. }));
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("TEST_TICKET_TOKEN")
        };
    }

    #[test]
    fn parses_and_builds_multiple_connections() {
        const TWO: &str = r#"
[[connection]]
name      = "board-a"
provider  = "github-projects"
org       = "org-a"
project   = 1
token_env = "MULTI_TEST_TOKEN_A"
[connection.state_map]
default = "open"

[[connection]]
name      = "board-b"
provider  = "github-projects"
org       = "org-b"
project   = 2
token_env = "MULTI_TEST_TOKEN_B"
[connection.state_map]
default = "open"
"#;
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = parse(TWO).unwrap();
        assert_eq!(cfg.connections.len(), 2);
        assert_eq!(cfg.connections[0].name, "board-a");
        assert_eq!(cfg.connections[1].name, "board-b");

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("MULTI_TEST_TOKEN_A", "x");
            std::env::set_var("MULTI_TEST_TOKEN_B", "y");
        }
        let sources = cfg.sources().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].0, "board-a");
        assert_eq!(sources[1].0, "board-b");
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("MULTI_TEST_TOKEN_A");
            std::env::remove_var("MULTI_TEST_TOKEN_B");
        }
    }

    #[test]
    fn bad_category_is_reported() {
        let text = SAMPLE.replace(r#""Done"        = "done""#, r#""Done"        = "finished""#);
        let cfg = parse(&text).unwrap();
        let err = state_mapping(&cfg.connections[0]).unwrap_err();
        assert!(matches!(err, ConfigError::BadCategory { .. }));
    }
}
