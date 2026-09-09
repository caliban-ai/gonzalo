//! Leading-`~` expansion for store roots (#211).
//!
//! A store root reaches this process **verbatim** whenever no shell is involved
//! — an MCP client's `env` block, a systemd unit, a container spec, a `.desktop`
//! entry. `GONZALO_ROOT=~/.gonzalo` then arrives as a literal `~` component and
//! creates a directory named `~` in whatever the working directory happens to
//! be, while every query quietly answers from the wrong (empty) store.
//!
//! That makes the failure worse than a plain mistake: `~/.gonzalo` is the
//! natural thing to write, it works when you try it in a shell (the shell
//! expanded it before `exec`), and it silently does not work in the deployment
//! that matters.

use std::path::{Path, PathBuf};

/// Expand a leading `~` or `~/…` to the user's home directory.
///
/// Only the leading component is expanded. `~user` is left alone — resolving it
/// needs a passwd lookup, and nobody points a per-user store at another user's
/// home. Anything else, absolute or relative, is returned unchanged.
///
/// With no home directory to expand against, the path is returned unchanged:
/// a literal `~` is bad, but inventing a different root would be worse.
pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    expand_tilde_with(path, std::env::var_os("HOME").map(PathBuf::from))
}

/// [`expand_tilde`], against an explicit home directory.
///
/// Split out because a test cannot set `HOME`: `std::env::set_var` is `unsafe`
/// under edition 2024, and this workspace sets `unsafe_code = "forbid"`.
fn expand_tilde_with(path: impl AsRef<Path>, home: Option<PathBuf>) -> PathBuf {
    let path = path.as_ref();
    let Some(home) = home.filter(|h| !h.as_os_str().is_empty()) else {
        return path.to_path_buf();
    };

    // Operate on the raw string, not components: `Path` normalizes `~/` to the
    // component `~`, and rebuilding from components would also silently rewrite
    // unrelated parts of the path.
    let Some(raw) = path.to_str() else {
        return path.to_path_buf(); // non-UTF-8 cannot start with our `~/` form
    };

    match raw {
        "~" => home,
        _ => match raw.strip_prefix("~/") {
            // `~/.gonzalo` → `$HOME/.gonzalo`. Joining a relative remainder
            // keeps the separator handling to `Path`.
            Some(rest) => home.join(rest),
            // `~user/...`, `./~`, `/tmp/~`, and every ordinary path.
            None => path.to_path_buf(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<PathBuf> {
        Some(PathBuf::from("/home/tester"))
    }

    #[test]
    fn a_bare_tilde_is_the_home_directory() {
        assert_eq!(
            expand_tilde_with("~", home()),
            PathBuf::from("/home/tester")
        );
    }

    #[test]
    fn a_leading_tilde_slash_expands() {
        // The case from the ticket: the value an MCP client config carries.
        assert_eq!(
            expand_tilde_with("~/.gonzalo", home()),
            PathBuf::from("/home/tester/.gonzalo")
        );
    }

    #[test]
    fn a_deeper_path_keeps_the_rest_intact() {
        assert_eq!(
            expand_tilde_with("~/.gonzalo/graphs/main.db", home()),
            PathBuf::from("/home/tester/.gonzalo/graphs/main.db")
        );
    }

    #[test]
    fn absolute_and_relative_paths_are_untouched() {
        for raw in ["/var/lib/gonzalo", "./gonzalo-data", "gonzalo-data", "."] {
            assert_eq!(expand_tilde_with(raw, home()), PathBuf::from(raw));
        }
    }

    #[test]
    fn a_tilde_that_is_not_leading_is_untouched() {
        // Expanding these would corrupt a legitimate path — `~` is a perfectly
        // ordinary directory name once it is not in front.
        for raw in ["/tmp/~/store", "./~", "store/~/data"] {
            assert_eq!(expand_tilde_with(raw, home()), PathBuf::from(raw));
        }
    }

    #[test]
    fn a_user_relative_tilde_is_left_alone() {
        // `~other/store` needs a passwd lookup. Left verbatim rather than
        // guessed at — and never mistaken for the current user's home.
        assert_eq!(
            expand_tilde_with("~other/store", home()),
            PathBuf::from("~other/store")
        );
    }

    #[test]
    fn without_a_home_the_path_is_returned_unchanged() {
        // A literal `~` is bad; silently choosing some other root is worse.
        assert_eq!(
            expand_tilde_with("~/.gonzalo", None),
            PathBuf::from("~/.gonzalo")
        );
        assert_eq!(
            expand_tilde_with("~/.gonzalo", Some(PathBuf::new())),
            PathBuf::from("~/.gonzalo")
        );
    }
}
