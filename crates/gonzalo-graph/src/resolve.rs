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
use serde::{Deserialize, Serialize};
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

/// One symbol reached by an impact closure, identified by the path that defines
/// it rather than by name alone.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ImpactNode {
    pub name: String,
    /// The file defining this symbol. Two same-named symbols in different files
    /// are different nodes — that distinction is the whole point (#207).
    pub path: String,
}

/// The result of a resolution-gated impact walk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactReport {
    /// Symbols transitively affected, sorted, excluding the seed.
    pub reached: Vec<ImpactNode>,
    /// Call edges that could not be attributed to a specific definition and so
    /// were **not** followed. Reported rather than hidden: silently truncating a
    /// closure is its own kind of lie, and a non-zero count here means the true
    /// impact set may be larger than `reached`.
    pub ambiguous_edges: usize,
    /// Whether the walk stopped at `max_depth` with unexplored frontier left.
    ///
    /// This means "the set may be incomplete", not "more definitely exists":
    /// a node reached on the last permitted level was never asked for its own
    /// callers, so completeness cannot be claimed either way.
    pub truncated: bool,
}

/// The transitive closure of callers of `name`, following only edges that
/// resolve to a specific definition.
///
/// [`GraphStore::impact`](crate::GraphStore::impact) walks the name-matched
/// graph, so one hop into a name with several unrelated definitions absorbs
/// every subgraph sharing that identifier — on gonzalo itself a single seed
/// reached a quarter of the repository (#207). This walk keys nodes on
/// `(name, defining path)` and consults [`resolve_references_to`] for every
/// edge, so an [`Ambiguous`](Resolution::Ambiguous) reference is counted and
/// dropped instead of merging two graphs.
///
/// A caller's own path needs no resolution: the enclosing function of a call is
/// by definition in the file containing that call, so each reached node gets an
/// exact path.
///
/// This gates [`Ambiguous`](Resolution::Ambiguous) edges only. A name defined
/// exactly once still resolves [`UniqueGlobal`](Resolution::UniqueGlobal) even
/// when the call really meant a std or dependency method of the same name, which
/// remains a source of false edges (#223).
///
/// `max_depth` bounds the walk (`None` = unbounded); an ambiguous seed is walked
/// from each of its definitions, since the caller asked about all of them.
pub fn resolved_impact(
    store: &dyn GraphStore,
    name: &str,
    max_depth: Option<usize>,
) -> ImpactReport {
    let seeds: Vec<ImpactNode> = store
        .definitions(name)
        .into_iter()
        .map(|d| ImpactNode {
            name: name.to_string(),
            path: d.path,
        })
        .collect();

    let mut visited: BTreeSet<ImpactNode> = seeds.iter().cloned().collect();
    let mut frontier: Vec<ImpactNode> = seeds.clone();
    let mut report = ImpactReport::default();
    let mut depth = 0usize;

    while !frontier.is_empty() {
        if max_depth.is_some_and(|max| depth >= max) {
            report.truncated = true;
            break;
        }
        depth += 1;

        let mut next: Vec<ImpactNode> = Vec::new();
        for node in &frontier {
            for resolved in resolve_references_to(store, &node.name) {
                match resolved.resolution {
                    // Unattributable: report it, do not traverse it.
                    Resolution::Ambiguous => report.ambiguous_edges += 1,
                    // Resolves elsewhere, or nowhere — not an edge into `node`.
                    _ if resolved.target.as_deref() != Some(node.path.as_str()) => {}
                    _ => {
                        let Some(from) = resolved.reference.item.from else {
                            continue; // a top-level reference has no caller
                        };
                        let caller = ImpactNode {
                            name: from,
                            path: resolved.reference.path,
                        };
                        if visited.insert(caller.clone()) {
                            next.push(caller);
                        }
                    }
                }
            }
        }
        frontier = next;
    }

    for seed in &seeds {
        visited.remove(seed);
    }
    report.reached = visited.into_iter().collect();
    report
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

    // ---- resolution-gated impact closure (#207) ---------------------------

    /// Two unrelated subgraphs joined only by a shared name. `helper` is
    /// defined in both crates; nothing else is shared. A name-matched closure
    /// merges them, a resolved one must not.
    fn bridged() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "a.rs",
            build_rust(
                "fn leaf_a() {}\n\
                 fn helper() { leaf_a(); }\n\
                 fn top_a() { helper(); }",
            ),
        );
        s.insert(
            "b.rs",
            build_rust(
                "fn leaf_b() {}\n\
                 fn helper() { leaf_b(); }\n\
                 fn top_b() { helper(); }",
            ),
        );
        s
    }

    fn names_of(report: &ImpactReport) -> Vec<&str> {
        report.reached.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn name_matched_impact_merges_the_two_subgraphs() {
        // The defect, pinned: the heuristic closure from `leaf_a` reaches
        // b.rs's `top_b`, which cannot call it.
        let s = bridged();
        assert!(s.impact("leaf_a").contains(&"top_b".to_string()));
    }

    #[test]
    fn resolved_impact_does_not_cross_an_ambiguous_name() {
        let s = bridged();
        let report = resolved_impact(&s, "leaf_a", None);
        assert!(names_of(&report).contains(&"helper"), "{report:?}");
        assert!(names_of(&report).contains(&"top_a"), "{report:?}");
        assert!(
            !names_of(&report).contains(&"top_b"),
            "must not reach the other subgraph: {report:?}"
        );
        assert!(!names_of(&report).contains(&"leaf_b"), "{report:?}");
    }

    #[test]
    fn resolved_impact_carries_a_defining_path_for_every_node() {
        let report = resolved_impact(&bridged(), "leaf_a", None);
        assert!(!report.reached.is_empty());
        assert!(
            report.reached.iter().all(|n| n.path == "a.rs"),
            "{report:?}"
        );
    }

    #[test]
    fn resolved_impact_excludes_the_seed() {
        let report = resolved_impact(&bridged(), "leaf_a", None);
        assert!(!names_of(&report).contains(&"leaf_a"));
    }

    #[test]
    fn resolved_impact_counts_ambiguous_edges_it_declined_to_follow() {
        // `helper` is called from top_a and top_b, each local to its own file,
        // so those resolve. Add a third file calling `helper` with no local
        // definition: that edge is genuinely ambiguous and must be reported,
        // not silently dropped.
        let mut s = bridged();
        s.insert("c.rs", build_rust("fn outsider() { helper(); }"));
        let report = resolved_impact(&s, "leaf_a", None);
        assert!(
            report.ambiguous_edges > 0,
            "an unattributable edge must be reported: {report:?}"
        );
        assert!(
            !names_of(&report).contains(&"outsider"),
            "and must not be traversed: {report:?}"
        );
    }

    #[test]
    fn resolved_impact_reports_no_ambiguity_when_every_name_is_unique() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn leaf() {}\nfn mid() { leaf(); }"));
        let report = resolved_impact(&s, "leaf", None);
        assert_eq!(report.ambiguous_edges, 0);
        assert_eq!(names_of(&report), vec!["mid"]);
        assert!(!report.truncated);
    }

    #[test]
    fn resolved_impact_survives_cycles() {
        let mut s = InMemoryGraphStore::new();
        s.insert("cyc.rs", build_rust("fn a() { b(); }\nfn b() { a(); }"));
        let report = resolved_impact(&s, "a", None);
        assert_eq!(names_of(&report), vec!["b"], "terminates, seed excluded");
    }

    #[test]
    fn resolved_impact_respects_max_depth() {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "a.rs",
            build_rust("fn l() {}\nfn m() { l(); }\nfn t() { m(); }"),
        );
        let one = resolved_impact(&s, "l", Some(1));
        assert_eq!(names_of(&one), vec!["m"], "one hop only");
        assert!(one.truncated, "a capped walk must say so");

        // `truncated` means "stopped with frontier left", not "more existed":
        // at depth 2 the walk has reached `t` but never asked who calls it, so
        // it cannot claim the set is complete.
        let two = resolved_impact(&s, "l", Some(2));
        assert_eq!(names_of(&two), vec!["m", "t"]);
        assert!(two.truncated, "t was reached but never explored");

        // Only an uncapped walk (or one that exhausts the graph inside the cap)
        // can honestly report completeness.
        let deep = resolved_impact(&s, "l", Some(9));
        assert_eq!(names_of(&deep), vec!["m", "t"]);
        assert!(!deep.truncated);
        assert!(!resolved_impact(&s, "l", None).truncated);
    }

    #[test]
    fn resolved_impact_on_an_undefined_name_is_empty() {
        let report = resolved_impact(&bridged(), "ghost", None);
        assert!(report.reached.is_empty());
        assert_eq!(report.ambiguous_edges, 0);
    }

    #[test]
    fn resolved_impact_walks_every_definition_of_an_ambiguous_seed() {
        // Seeding on an ambiguous name is legitimate: the caller asked about
        // "helper", and both are real. Each is walked from its own path.
        let report = resolved_impact(&bridged(), "helper", None);
        let mut pairs: Vec<(&str, &str)> = report
            .reached
            .iter()
            .map(|n| (n.name.as_str(), n.path.as_str()))
            .collect();
        pairs.sort();
        assert_eq!(pairs, vec![("top_a", "a.rs"), ("top_b", "b.rs")]);
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
