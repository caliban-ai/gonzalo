//! Best-effort name resolution over an assembled view (ticket G).
//!
//! The base graph is a **heuristic** call graph: references match a target by
//! name, so `callers_of("foo")` returns callers of *any* `foo`. This layer
//! resolves each reference to the specific defining path it most likely means,
//! disambiguating same-named symbols across files:
//!
//! 1. **Local** — a definition of the name in the reference's own file wins.
//! 2. **Unique global** — otherwise, the sole definition across the view.
//! 3. **Ambiguous** — multiple definitions and none local: left unresolved.
//! 4. **Unresolved** — no definition in the view (honest dangling, ADR 0012).
//!
//! Resolution is file-scoped (not yet import-aware); it is a pure function of a
//! [`GraphStore`]'s query methods, so it works over any backend and adds no
//! trait surface. Import-following resolution is a further step.

use crate::{GraphStore, Located, Reference};
use std::collections::BTreeSet;

/// How a reference was resolved to a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// A definition of the name exists in the reference's own file.
    Local,
    /// The name is defined exactly once across the view.
    UniqueGlobal,
    /// Several definitions and none in the reference's file — not resolved.
    Ambiguous,
    /// No definition of the name in the view — a dangling reference.
    Unresolved,
}

/// A reference resolved (best-effort) to the path of the symbol it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReference {
    /// The reference and the path it was found in.
    pub reference: Located<Reference>,
    /// The defining path this reference resolves to, or `None` when ambiguous
    /// or unresolved.
    pub target: Option<String>,
    /// Why it resolved (or didn't).
    pub resolution: Resolution,
}

/// Resolve every reference to `name` to a defining path (see the module docs
/// for the strategy).
pub fn resolve_references_to(store: &dyn GraphStore, name: &str) -> Vec<ResolvedReference> {
    let def_paths: BTreeSet<String> = store
        .definitions(name)
        .into_iter()
        .map(|d| d.path)
        .collect();

    store
        .references_to(name)
        .into_iter()
        .map(|located| {
            let (target, resolution) = if def_paths.contains(&located.path) {
                (Some(located.path.clone()), Resolution::Local)
            } else if def_paths.len() == 1 {
                (def_paths.iter().next().cloned(), Resolution::UniqueGlobal)
            } else if def_paths.is_empty() {
                (None, Resolution::Unresolved)
            } else {
                (None, Resolution::Ambiguous)
            };
            ResolvedReference {
                reference: located,
                target,
                resolution,
            }
        })
        .collect()
}

/// Enclosing functions that call the `name` **defined at `defining_path`** — the
/// precision refinement of [`GraphStore::callers_of`], which returns callers of
/// any same-named symbol. Sorted and deduped.
pub fn resolved_callers_of(store: &dyn GraphStore, defining_path: &str, name: &str) -> Vec<String> {
    let mut callers: Vec<String> = resolve_references_to(store, name)
        .into_iter()
        .filter(|r| r.target.as_deref() == Some(defining_path))
        .filter_map(|r| r.reference.item.from)
        .collect();
    callers.sort();
    callers.dedup();
    callers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryGraphStore, build_rust};

    fn view() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        // Two files each define `foo` and call it locally.
        s.insert(
            "a.rs",
            build_rust("fn foo() {}\nfn ca() { foo(); }\nfn only() {}"),
        );
        s.insert("b.rs", build_rust("fn foo() {}\nfn cb() { foo(); }"));
        // A cross-file call to `only` (defined once, in a.rs).
        s.insert("c.rs", build_rust("fn cc() { only(); }"));
        // A call to `foo` from a file that does not define it (ambiguous).
        s.insert("d.rs", build_rust("fn cd() { foo(); }"));
        s
    }

    #[test]
    fn local_definition_wins() {
        let s = view();
        let resolved = resolve_references_to(&s, "foo");
        // ca -> a.rs's foo, cb -> b.rs's foo (each local).
        let ca = resolved
            .iter()
            .find(|r| r.reference.item.from.as_deref() == Some("ca"))
            .unwrap();
        assert_eq!(ca.resolution, Resolution::Local);
        assert_eq!(ca.target.as_deref(), Some("a.rs"));
        let cb = resolved
            .iter()
            .find(|r| r.reference.item.from.as_deref() == Some("cb"))
            .unwrap();
        assert_eq!(cb.target.as_deref(), Some("b.rs"));
    }

    #[test]
    fn unique_global_resolves() {
        let s = view();
        let resolved = resolve_references_to(&s, "only");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].resolution, Resolution::UniqueGlobal);
        assert_eq!(resolved[0].target.as_deref(), Some("a.rs"));
    }

    #[test]
    fn multiple_defs_without_a_local_are_ambiguous() {
        let s = view();
        let cd = resolve_references_to(&s, "foo")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("cd"))
            .unwrap();
        assert_eq!(cd.resolution, Resolution::Ambiguous);
        assert_eq!(cd.target, None);
    }

    #[test]
    fn missing_definition_is_unresolved() {
        let s = view();
        let mut s = s;
        s.insert("e.rs", build_rust("fn ce() { ghost(); }"));
        let resolved = resolve_references_to(&s, "ghost");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].resolution, Resolution::Unresolved);
        assert_eq!(resolved[0].target, None);
    }

    #[test]
    fn resolved_callers_disambiguates_by_defining_path() {
        let s = view();
        // The heuristic callers_of("foo") returns ca, cb (and cd via the ref).
        assert!(s.callers_of("foo").contains(&"ca".to_string()));
        assert!(s.callers_of("foo").contains(&"cb".to_string()));
        // Resolution narrows to callers of the *specific* foo.
        assert_eq!(
            resolved_callers_of(&s, "a.rs", "foo"),
            vec!["ca".to_string()]
        );
        assert_eq!(
            resolved_callers_of(&s, "b.rs", "foo"),
            vec!["cb".to_string()]
        );
    }
}
