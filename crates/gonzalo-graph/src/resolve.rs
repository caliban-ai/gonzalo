//! Best-effort name resolution over an assembled view (ticket G).
//!
//! The base graph is a **heuristic** call graph: references match a target by
//! name, so `callers_of("foo")` returns callers of *any* `foo`. This layer
//! resolves each reference to the specific defining path it most likely means,
//! disambiguating same-named symbols across files:
//!
//! 1. **Qualified** — the call site named an owner (`Beta::get()`) and exactly
//!    one definition in the view is owned by it (#248).
//! 2. **Local** — a definition of the name in the reference's own file wins.
//! 3. **Unique global** — otherwise, the sole definition across the view.
//! 4. **Ambiguous** — multiple definitions and none local: left unresolved.
//! 5. **Unresolved** — no definition in the view (honest dangling, ADR 0012).
//!
//! The qualifier rule only ever *narrows*. A Rust qualifier is as often a
//! module as a type, and most modules are files rather than inline `mod`
//! blocks, so nothing in the view is owned by them; declining those edges would
//! lose attributions the view makes correctly today. When a qualifier matches
//! no owner it is ignored and the remaining rules apply unchanged.
//!
//! Resolution is file-scoped (not yet import-aware); it is a pure function of a
//! [`GraphStore`]'s query methods, so it works over any backend and adds no
//! trait surface. Import-following resolution is a further step, and is also
//! what would let a module path be told from a type — at which point an
//! unmatched qualifier could decline rather than fall through (#252).

use crate::{GraphStore, Import, Located, RefKind, Reference};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// How a reference was resolved to a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The call site named a qualifier (`Beta::get()`) and exactly one
    /// definition in the view is owned by it.
    ///
    /// Outranks [`Local`](Resolution::Local): a call that says which type it
    /// means says so whatever else sits in the same file (#248).
    Qualified,
    /// A definition of the name exists in the reference's own file.
    Local,
    /// The name is defined exactly once across the view.
    UniqueGlobal,
    /// Several definitions, none local, and the referencing file imports the
    /// name from a module that exactly one of them lives under (#252).
    Imported,
    /// Several definitions and none in the reference's file — not resolved.
    Ambiguous,
    /// A method call (`x.foo()`) whose receiver type is unknown, so no
    /// definition in the view can be claimed even if exactly one exists.
    ///
    /// Distinct from [`Unresolved`](Resolution::Unresolved): definitions of the
    /// name may well be present, but attributing the call to one of them would
    /// assert a receiver type the graph does not know (#223).
    ReceiverUnknown,
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
    let defs = store.definitions(name);
    let def_paths: BTreeSet<String> = defs.iter().map(|d| d.path.clone()).collect();
    let references = store.references_to(name);

    // Imports only matter where the name is ambiguous, and only for the files
    // that actually reference it — so fetch once per such file rather than once
    // per call site (#252).
    let mut imports_by_path: BTreeMap<String, Vec<Import>> = BTreeMap::new();
    if def_paths.len() > 1 {
        for located in &references {
            if !imports_by_path.contains_key(&located.path) {
                imports_by_path.insert(located.path.clone(), store.imports_in_file(&located.path));
            }
        }
    }

    references
        .into_iter()
        .map(|located| {
            // A call that names its owner (`Beta::get()`) says which definition
            // it means, whatever else happens to share the name — so this is
            // consulted before the local rule.
            //
            // Only when the qualifier actually matches an owner in the view. A
            // file module (`some_module::helper()`) owns nothing here, and
            // declining that edge would throw away an attribution the view can
            // make perfectly well. So the qualifier only ever *narrows*: it can
            // turn ambiguity into an answer, never an answer into ambiguity
            // (#248). Telling a module path from a type is what #252 is for.
            let qualified: BTreeSet<&str> = match located.item.qualifier.as_deref() {
                Some(q) => defs
                    .iter()
                    .filter(|d| d.item.owner.as_deref() == Some(q))
                    .map(|d| d.path.as_str())
                    .collect(),
                None => BTreeSet::new(),
            };

            let (target, resolution) = if qualified.len() == 1 {
                (
                    qualified.iter().next().map(|p| (*p).to_string()),
                    Resolution::Qualified,
                )
            } else if !qualified.is_empty() {
                // Same owner name defined in several files: the qualifier
                // narrowed the candidates but did not settle them.
                (None, Resolution::Ambiguous)
            } else if def_paths.contains(&located.path) {
                // A definition in the caller's own file wins for either shape:
                // `self.foo()` next to `fn foo` is the one thing about a
                // receiver we can reasonably assume.
                (Some(located.path.clone()), Resolution::Local)
            } else if def_paths.is_empty() {
                // Nothing of this name anywhere: "not in the view" is the more
                // informative answer than "receiver unknown", and it is true
                // whatever the call shape.
                (None, Resolution::Unresolved)
            } else if located.item.kind == RefKind::Method {
                // `x.foo()` belongs to whatever `x` is. Measured on gonzalo,
                // 294 of the 388 cross-file method calls that used to resolve
                // `UniqueGlobal` pointed at a *different crate* — `push`,
                // `filter`, `send`, `next` and friends, i.e. std methods
                // attributed to a same-named project function (#223).
                (None, Resolution::ReceiverUnknown)
            } else if def_paths.len() == 1 {
                (def_paths.iter().next().cloned(), Resolution::UniqueGlobal)
            } else if let Some(path) = imports_by_path
                .get(&located.path)
                .and_then(|imports| imported_target(imports, name, &located.path, &def_paths))
            {
                (Some(path), Resolution::Imported)
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

/// The single candidate an import in the referencing file points at, or `None`
/// when the file imports the name from nowhere or the import fits several
/// candidates.
///
/// Narrowing only: an import that does not settle the question is ignored and
/// the reference stays ambiguous, which is the honest answer and keeps this from
/// ever turning a resolved edge into an unresolved one (#248, #252).
fn imported_target(
    imports: &[Import],
    name: &str,
    from_path: &str,
    candidates: &BTreeSet<String>,
) -> Option<String> {
    let Some(import) = imports.iter().find(|i| i.name == name) else {
        // No import names this symbol. A C/C++ `#include` names a whole file
        // instead, so match by path: any candidate under an included path is
        // what this file meant by the name (#267).
        return included_target(imports, from_path, candidates);
    };

    let matched: Vec<&str> = if import.depth > 0 {
        // A relative import needs no project root: count dots up from the
        // referencing file's own package and the answer is a path (#261).
        let anchor = relative_anchor(from_path, import.depth, &import.path)?;
        candidates
            .iter()
            .filter(|path| path_under(path, &anchor))
            .map(String::as_str)
            .collect()
    } else {
        if import.path.is_empty() {
            return None;
        }
        candidates
            .iter()
            .filter(|path| path_ends_with_module(path, &import.path))
            .map(String::as_str)
            .collect()
    };

    match matched.as_slice() {
        [] => None,
        [only] => Some((*only).to_string()),
        // Several identical copies of one package tree — one per assignment,
        // a vendored reference beside your own code. The nearest is meant.
        several => nearest_to(several, from_path),
    }
}

/// The single candidate covered by one of this file's whole-file imports.
///
/// An include names a path, and dropping the extension is what makes it useful:
/// a prototype in a header is not a symbol, the definition in the translation
/// unit is, and both sit at the same path stem. Narrowing only, like every
/// other rule here — several matches leave the reference ambiguous unless one
/// is unambiguously nearer.
fn included_target(
    imports: &[Import],
    from_path: &str,
    candidates: &BTreeSet<String>,
) -> Option<String> {
    let included: Vec<&Import> = imports
        .iter()
        .filter(|i| i.brings_whole_file() && !i.path.is_empty())
        .collect();
    if included.is_empty() {
        return None;
    }
    let matched: Vec<&str> = candidates
        .iter()
        .filter(|path| {
            included
                .iter()
                .any(|import| path_ends_with_module(path, &import.path))
        })
        .map(String::as_str)
        .collect();
    match matched.as_slice() {
        [] => None,
        [only] => Some((*only).to_string()),
        several => nearest_to(several, from_path),
    }
}

/// The directory a relative import points at: the referencing file's own
/// package, one level up per dot beyond the first, then the module segments.
///
/// `None` when the path has fewer levels than the import claims, which leaves
/// the older rules to run unchanged rather than dropping the edge.
fn relative_anchor(from_path: &str, depth: usize, module: &[String]) -> Option<Vec<String>> {
    let mut components: Vec<&str> = from_path.split('/').filter(|c| !c.is_empty()).collect();
    components.pop()?; // the file itself; what remains is its package
    for _ in 1..depth {
        components.pop()?;
    }
    let mut anchor: Vec<String> = components.into_iter().map(str::to_string).collect();
    anchor.extend(module.iter().cloned());
    Some(anchor)
}

/// Whether `file` sits at or under the package directory `anchor`.
fn path_under(file: &str, anchor: &[String]) -> bool {
    let mut components: Vec<&str> = file.split('/').filter(|c| !c.is_empty()).collect();
    if let Some(last) = components.pop() {
        let stem = last.rsplit_once('.').map_or(last, |(stem, _)| stem);
        // `pkg/__init__.py` *is* `pkg`, so it adds no segment of its own.
        if stem != "__init__" {
            components.push(stem);
        }
    }
    components.len() >= anchor.len()
        && components
            .iter()
            .zip(anchor)
            .all(|(component, segment)| same_segment(component, segment))
}

/// The one candidate sharing the longest path prefix with `from_path`.
///
/// `None` on a tie, and `None` when nothing shares a prefix at all: two copies
/// equally far away say nothing about which was meant, and guessing there is
/// the coin flip this whole layer exists to avoid.
fn nearest_to(candidates: &[&str], from_path: &str) -> Option<String> {
    let from: Vec<&str> = from_path.split('/').filter(|c| !c.is_empty()).collect();
    let shared = |path: &str| {
        path.split('/')
            .filter(|c| !c.is_empty())
            .zip(from.iter())
            .take_while(|(component, own)| same_segment(component, own))
            .count()
    };
    let best = candidates.iter().map(|path| shared(path)).max()?;
    if best == 0 {
        return None;
    }
    let mut winners = candidates.iter().filter(|path| shared(path) == best);
    let only = winners.next()?;
    winners.next().is_none().then(|| (*only).to_string())
}

/// Whether `file`'s path components end with the module segments `module`.
///
/// `src/model.rs` ends with `["model"]`; `a/b/c.rs` ends with `["b", "c"]`.
///
/// Three conventions are folded in because they are near-universal and the rule
/// is close to useless without them. An entry file names its container rather
/// than itself, so `mod.rs`, `lib.rs`, `main.rs`, `__init__.py` and `index.ts`
/// contribute no segment of their own. A `src` directory is build layout rather
/// than a module, so it is skipped — `my-pkg/src/model.rs` is `my_pkg::model`.
/// And segments compare with `-` and `_` alike, because a Rust crate is written
/// `gonzalo_cli` in code and lives in `gonzalo-cli` on disk.
fn path_ends_with_module(file: &str, module: &[String]) -> bool {
    let mut components: Vec<&str> = file
        .split('/')
        .filter(|c| !c.is_empty() && *c != "src")
        .collect();
    if let Some(last) = components.pop() {
        let stem = last.rsplit_once('.').map_or(last, |(stem, _)| stem);
        if !matches!(stem, "mod" | "lib" | "main" | "__init__" | "index") {
            components.push(stem);
        }
    }
    components.len() >= module.len()
        && components[components.len() - module.len()..]
            .iter()
            .zip(module)
            .all(|(component, segment)| same_segment(component, segment))
}

/// Whether two path segments name the same thing, treating `-` and `_` alike.
fn same_segment(component: &str, segment: &str) -> bool {
    component.len() == segment.len()
        && component
            .bytes()
            .zip(segment.bytes())
            .all(|(a, b)| a == b || (a == b'-' || a == b'_') && (b == b'-' || b == b'_'))
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
    /// Call edges dropped because the name has several definitions and none is
    /// local. Reported rather than hidden: silently truncating a closure is its
    /// own kind of lie, and a non-zero count here means the true impact set may
    /// be larger than `reached`.
    pub ambiguous_edges: usize,
    /// Call edges dropped because they are method calls on a receiver of unknown
    /// type ([`Resolution::ReceiverUnknown`]). Counted separately from
    /// `ambiguous_edges` because the cause differs: not "too many candidates"
    /// but "cannot claim any candidate" (#223).
    pub receiver_unknown_edges: usize,
    /// Call-graph edges skipped because the reference names a function as a
    /// *value* rather than calling it — `register(helper)`.
    ///
    /// Passing a function is a real dependency, so dropping these silently
    /// would under-report. But a value reference is over-inclusive by
    /// construction: extraction is per-file, so a local named like a function is
    /// indistinguishable from the function, and traversing them would put false
    /// edges back into the walk #207 and #223 cleaned up. Counted instead, so
    /// non-zero means "go look" rather than nothing at all (#250).
    pub value_edges: usize,
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
                // Naming a function is not calling it. Real dependency, but too
                // over-inclusive to traverse — report it instead (#250).
                if resolved.reference.item.kind == RefKind::Value {
                    report.value_edges += 1;
                    continue;
                }
                match resolved.resolution {
                    // Unattributable: report it, do not traverse it.
                    Resolution::Ambiguous => report.ambiguous_edges += 1,
                    Resolution::ReceiverUnknown => report.receiver_unknown_edges += 1,
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

    // ---- method calls do not claim a same-named free function (#223) ------

    /// The exact shape found in the wild: a std method (`Iterator::chain`)
    /// called in one crate, and a same-named test fixture that is the view's
    /// only definition of `chain`.
    fn std_method_collision() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "crates/core/src/merge.rs",
            build_rust("fn merge() { let _ = ours.keys().chain(theirs.keys()); }"),
        );
        s.insert(
            "crates/graph/src/store.rs",
            build_rust("fn chain() -> u8 { 0 }"),
        );
        s
    }

    #[test]
    fn a_method_call_does_not_resolve_to_a_same_named_free_function() {
        let s = std_method_collision();
        let r = resolve_references_to(&s, "chain");
        let call = r
            .iter()
            .find(|r| r.reference.path.contains("merge.rs"))
            .expect("the .chain() call is recorded");
        assert_eq!(call.resolution, Resolution::ReceiverUnknown);
        assert_eq!(call.target, None, "must not claim the test fixture");
    }

    #[test]
    fn the_collision_no_longer_bridges_the_impact_closure() {
        // Before: `chain` resolved UniqueGlobal, so seeding at the fixture
        // dragged `merge` — in a crate that cannot depend on this one — in.
        let report = resolved_impact(&std_method_collision(), "chain", None);
        assert!(
            !names_of(&report).contains(&"merge"),
            "must not cross into the other crate: {report:?}"
        );
        assert_eq!(report.receiver_unknown_edges, 1, "and must say so");
    }

    #[test]
    fn a_free_call_still_resolves_unique_global() {
        // The fix must not disarm ordinary resolution.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn only() {}"));
        s.insert("b.rs", build_rust("fn cb() { only(); }"));
        let r = resolve_references_to(&s, "only");
        assert_eq!(r[0].resolution, Resolution::UniqueGlobal);
        assert_eq!(r[0].target.as_deref(), Some("a.rs"));
    }

    #[test]
    fn a_method_call_still_resolves_locally() {
        // `self.helper()` beside `fn helper` is the one receiver assumption
        // worth making, so same-file method calls keep resolving.
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "a.rs",
            build_rust("fn helper() {}\nfn caller() { self.helper(); }"),
        );
        let r = resolve_references_to(&s, "helper");
        let call = r.iter().find(|r| r.reference.item.from.is_some()).unwrap();
        assert_eq!(call.resolution, Resolution::Local);
        assert_eq!(call.target.as_deref(), Some("a.rs"));
    }

    #[test]
    fn a_path_call_is_not_treated_as_a_method_call() {
        // `a::b::foo()` is a path, not a receiver — it stays resolvable.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn parse() {}"));
        s.insert("b.rs", build_rust("fn cb() { util::parse(); }"));
        let r = resolve_references_to(&s, "parse");
        let call = r.iter().find(|r| r.reference.path == "b.rs").unwrap();
        assert_eq!(call.resolution, Resolution::UniqueGlobal);
    }

    #[test]
    fn receiver_unknown_is_distinct_from_unresolved() {
        // A name with no definition at all is still Unresolved — the two mean
        // different things and must stay distinguishable.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn c() { x.ghost(); }"));
        let r = resolve_references_to(&s, "ghost");
        assert_eq!(r[0].resolution, Resolution::Unresolved);
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

    // ---- qualified references (#248) --------------------------------------

    /// Two types, in separate files, each with a `get` method; a third file
    /// calls one of them by name.
    fn two_gets() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "a.rs",
            build_rust("struct Alpha; impl Alpha { fn get() {} }"),
        );
        s.insert("b.rs", build_rust("struct Beta; impl Beta { fn get() {} }"));
        s
    }

    #[test]
    fn a_qualified_reference_resolves_to_the_matching_owner() {
        let mut s = two_gets();
        s.insert("c.rs", build_rust("fn caller() { Beta::get(); }"));
        let r = resolve_references_to(&s, "get")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Qualified);
        assert_eq!(r.target.as_deref(), Some("b.rs"));
    }

    #[test]
    fn an_unqualified_reference_to_an_overloaded_name_stays_ambiguous() {
        let mut s = two_gets();
        s.insert("c.rs", build_rust("fn plain() { get(); }"));
        let r = resolve_references_to(&s, "get")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("plain"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Ambiguous);
        assert_eq!(r.target, None);
    }

    #[test]
    fn a_qualified_reference_beats_a_local_definition_of_the_same_name() {
        // The call says `Beta`, so the free `get` sitting in the same file is
        // not what it means — the qualifier has to outrank the local rule.
        let mut s = two_gets();
        s.insert(
            "c.rs",
            build_rust("fn get() {}\nfn caller() { Beta::get(); }"),
        );
        let r = resolve_references_to(&s, "get")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Qualified);
        assert_eq!(r.target.as_deref(), Some("b.rs"));
    }

    #[test]
    fn a_qualifier_that_matches_no_owner_falls_back_to_the_existing_ladder() {
        // `some_module` is a file module, so nothing in the view is *owned* by
        // it. The qualifier must then be ignored rather than declining an edge
        // that resolves perfectly well today: this pass only ever narrows.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn helper() {}"));
        s.insert("b.rs", build_rust("fn caller() { some_module::helper(); }"));
        let r = resolve_references_to(&s, "helper")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::UniqueGlobal);
        assert_eq!(r.target.as_deref(), Some("a.rs"));
    }

    #[test]
    fn resolved_impact_follows_a_qualified_edge_it_used_to_decline() {
        let mut s = two_gets();
        s.insert("c.rs", build_rust("fn caller() { Beta::get(); }"));
        let report = resolved_impact(&s, "get", None);
        assert!(
            names_of(&report).contains(&"caller"),
            "qualified edge must be traversed: {report:?}"
        );
        assert_eq!(
            report.ambiguous_edges, 0,
            "and must not be counted as declined: {report:?}"
        );
    }

    // ---- functions passed as values (#250) --------------------------------

    #[test]
    fn resolved_impact_counts_a_value_reference_instead_of_traversing_it() {
        // Passing a function *is* a real dependency, so dropping it silently
        // would under-report exactly the way #207 and #223 were about not doing.
        // But a value reference is over-inclusive by construction — a local
        // named like a function is indistinguishable — so traversing it would
        // put false edges back into the one tool those tickets cleaned up.
        // Count it, and let the caller decide whether to go look.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn helper() {}"));
        s.insert("b.rs", build_rust("fn g() { register(helper); }"));

        let report = resolved_impact(&s, "helper", None);
        assert!(
            !names_of(&report).contains(&"g"),
            "must not be traversed: {report:?}"
        );
        assert_eq!(report.value_edges, 1, "must be reported: {report:?}");
        assert_eq!(report.ambiguous_edges, 0, "{report:?}");
    }

    #[test]
    fn a_plain_call_is_not_counted_as_a_value_edge() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn helper() {}"));
        s.insert("b.rs", build_rust("fn g() { helper(); }"));
        let report = resolved_impact(&s, "helper", None);
        assert!(names_of(&report).contains(&"g"), "{report:?}");
        assert_eq!(report.value_edges, 0, "{report:?}");
    }

    // ---- syntactic receiver typing (#251) ---------------------------------

    #[test]
    fn a_typed_receiver_resolves_to_its_own_type() {
        // The payoff: `b.get()` used to decline because the receiver's type was
        // unknown. The body says what `b` is, so it resolves like a written
        // `Beta::get()` would.
        let mut s = two_gets();
        s.insert(
            "c.rs",
            build_rust("fn caller() { let b = Beta::new(); b.get(); }"),
        );
        let r = resolve_references_to(&s, "get")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Qualified);
        assert_eq!(r.target.as_deref(), Some("b.rs"));
        assert_eq!(
            r.reference.item.kind,
            RefKind::Method,
            "still a method call: the shape is what guards #223"
        );
    }

    #[test]
    fn an_untyped_receiver_still_declines_and_is_counted() {
        let mut s = two_gets();
        s.insert(
            "c.rs",
            build_rust("fn caller() { let b = make(); b.get(); }"),
        );
        let r = resolve_references_to(&s, "get")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::ReceiverUnknown);

        let report = resolved_impact(&s, "get", None);
        assert!(
            report.receiver_unknown_edges > 0,
            "still reported: {report:?}"
        );
    }

    #[test]
    fn a_receiver_typed_as_something_outside_the_view_falls_through() {
        // `String::new()` types `s`, but nothing in the view is owned by
        // `String`, so the qualifier is ignored rather than declining an edge
        // the older rules resolve — the monotonicity #248 established.
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn len() -> usize { 0 }"));
        s.insert(
            "b.rs",
            build_rust("fn caller() { let s = String::new(); s.len(); }"),
        );
        let r = resolve_references_to(&s, "len")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        // A method call on a receiver typed outside the view is exactly the
        // #223 case: one same-named project function must not be credited.
        assert_eq!(r.resolution, Resolution::ReceiverUnknown);
    }

    // ---- import-aware resolution (#252) -----------------------------------

    /// Two files define `make`; a third calls it, importing one of them.
    fn two_makes(caller_file: &str, caller_src: &str) -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert("src/model.rs", build_rust("fn make() {}"));
        s.insert("src/other.rs", build_rust("fn make() {}"));
        s.insert(caller_file, build_rust(caller_src));
        s
    }

    #[test]
    fn an_import_picks_the_module_it_names() {
        let s = two_makes(
            "src/app.rs",
            "use crate::model::make;\nfn caller() { make(); }",
        );
        let r = resolve_references_to(&s, "make")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Imported);
        assert_eq!(r.target.as_deref(), Some("src/model.rs"));
    }

    #[test]
    fn without_the_import_the_same_call_stays_ambiguous() {
        // The control: nothing else about this view changed.
        let s = two_makes("src/app.rs", "fn caller() { make(); }");
        let r = resolve_references_to(&s, "make")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Ambiguous);
    }

    #[test]
    fn an_import_that_does_not_narrow_falls_through() {
        // Two `model.rs` under different crates: the import names a module both
        // could be, so the honest answer is still ambiguous rather than a coin
        // flip. The rule only ever narrows (#248).
        let mut s = InMemoryGraphStore::new();
        s.insert("a/src/model.rs", build_rust("fn make() {}"));
        s.insert("b/src/model.rs", build_rust("fn make() {}"));
        s.insert(
            "c/src/app.rs",
            build_rust("use crate::model::make;\nfn caller() { make(); }"),
        );
        let r = resolve_references_to(&s, "make")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Ambiguous);
        assert_eq!(r.target, None);
    }

    #[test]
    fn an_import_matches_a_module_directory_through_mod_rs() {
        // `model/mod.rs` *is* the module `model`; the filename is not a segment.
        let mut s = InMemoryGraphStore::new();
        s.insert("src/model/mod.rs", build_rust("fn make() {}"));
        s.insert("src/other.rs", build_rust("fn make() {}"));
        s.insert(
            "src/app.rs",
            build_rust("use crate::model::make;\nfn caller() { make(); }"),
        );
        let r = resolve_references_to(&s, "make")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.target.as_deref(), Some("src/model/mod.rs"));
    }

    #[test]
    fn a_unique_global_still_resolves_without_consulting_imports() {
        // Imports are only consulted where the ladder would otherwise give up,
        // so nothing that resolves today changes.
        let mut s = InMemoryGraphStore::new();
        s.insert("src/model.rs", build_rust("fn only() {}"));
        s.insert("src/app.rs", build_rust("fn caller() { only(); }"));
        let r = resolve_references_to(&s, "only")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::UniqueGlobal);
    }

    #[test]
    fn resolved_impact_follows_an_imported_edge() {
        let s = two_makes(
            "src/app.rs",
            "use crate::model::make;\nfn caller() { make(); }",
        );
        let report = resolved_impact(&s, "make", None);
        assert!(names_of(&report).contains(&"caller"), "{report:?}");
    }

    #[test]
    fn an_import_of_a_crate_root_matches_the_crate_directory() {
        // Measured on gonzalo itself: `use gonzalo_cli::list` in
        // `crates/gonzalo-cli/src/main.rs` names the *crate*, whose root is
        // `crates/gonzalo-cli/src/lib.rs`. Neither `lib` nor `src` is a module,
        // and Rust writes the crate name with `_` where the directory has `-`.
        let mut s = InMemoryGraphStore::new();
        s.insert("crates/gonzalo-cli/src/lib.rs", build_rust("fn list() {}"));
        s.insert(
            "crates/gonzalo-graph/src/store.rs",
            build_rust("fn list() {}"),
        );
        s.insert(
            "crates/gonzalo-cli/src/main.rs",
            build_rust("use gonzalo_cli::list;\nfn caller() { list(); }"),
        );
        let r = resolve_references_to(&s, "list")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.resolution, Resolution::Imported);
        assert_eq!(r.target.as_deref(), Some("crates/gonzalo-cli/src/lib.rs"));
    }

    #[test]
    fn a_hyphenated_directory_matches_an_underscored_module() {
        let mut s = InMemoryGraphStore::new();
        s.insert("my-pkg/src/model.rs", build_rust("fn make() {}"));
        s.insert("other/src/model.rs", build_rust("fn make() {}"));
        s.insert(
            "app.rs",
            build_rust("use my_pkg::model::make;\nfn caller() { make(); }"),
        );
        let r = resolve_references_to(&s, "make")
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some("caller"))
            .expect("call recorded");
        assert_eq!(r.target.as_deref(), Some("my-pkg/src/model.rs"));
    }

    // ---- anchored and local imports (#261) --------------------------------

    fn py(src: &str) -> crate::CodeGraph {
        crate::build(crate::Language::Python, src)
    }

    fn java(src: &str) -> crate::CodeGraph {
        crate::build(crate::Language::Java, src)
    }

    fn resolved_from(s: &InMemoryGraphStore, name: &str, caller: &str) -> ResolvedReference {
        resolve_references_to(s, name)
            .into_iter()
            .find(|r| r.reference.item.from.as_deref() == Some(caller))
            .expect("call recorded")
    }

    #[test]
    fn a_relative_import_anchors_to_the_referencing_package() {
        // The CS-5260 shape: a vendored reference copy beside your own tree, so
        // the same module path exists twice and a bare suffix match hits both.
        // `from ..DataTypes import Action` means *my* parent package's copy.
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "ReferenceCode/src/cs5260/DataTypes/Action.py",
            py("def Action():\n    pass\n"),
        );
        s.insert(
            "WorldTraderSim/src/WorldTraderSim/DataTypes/Action.py",
            py("def Action():\n    pass\n"),
        );
        s.insert(
            "ReferenceCode/src/cs5260/Examples/HW2_1.py",
            py("from ..DataTypes import Action\ndef run():\n    Action()\n"),
        );

        let r = resolved_from(&s, "Action", "run");
        assert_eq!(r.resolution, Resolution::Imported);
        assert_eq!(
            r.target.as_deref(),
            Some("ReferenceCode/src/cs5260/DataTypes/Action.py")
        );
    }

    #[test]
    fn a_single_dot_import_anchors_to_the_files_own_package() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a/pkg/util.py", py("def helper():\n    pass\n"));
        s.insert("b/pkg/util.py", py("def helper():\n    pass\n"));
        s.insert(
            "a/pkg/main.py",
            py("from .util import helper\ndef run():\n    helper()\n"),
        );

        let r = resolved_from(&s, "helper", "run");
        assert_eq!(r.target.as_deref(), Some("a/pkg/util.py"));
    }

    #[test]
    fn an_absolute_import_matching_several_copies_prefers_the_nearest() {
        // The Vanderbilt course shape: one package tree per assignment, so an
        // absolute import matches every copy. The one in the referencing file's
        // own tree is what it means.
        let mut s = InMemoryGraphStore::new();
        for tree in ["assignment1", "assignment2"] {
            s.insert(
                &format!("{tree}/src/main/java/edu/vandy/util/Helper.java"),
                java("class Helper { static void go() {} }"),
            );
        }
        s.insert(
            "assignment2/src/main/java/edu/vandy/app/Main.java",
            java("import static edu.vandy.util.Helper.go;\nclass Main { void run() { go(); } }"),
        );

        let r = resolved_from(&s, "go", "run");
        assert_eq!(r.resolution, Resolution::Imported);
        assert_eq!(
            r.target.as_deref(),
            Some("assignment2/src/main/java/edu/vandy/util/Helper.java")
        );
    }

    #[test]
    fn two_equally_near_copies_stay_ambiguous() {
        // Nothing distinguishes them, so picking one would be a coin flip.
        let mut s = InMemoryGraphStore::new();
        for tree in ["one", "two"] {
            s.insert(
                &format!("{tree}/src/main/java/edu/vandy/util/Helper.java"),
                java("class Helper { static void go() {} }"),
            );
        }
        s.insert(
            "apps/src/main/java/edu/vandy/app/Main.java",
            java("import static edu.vandy.util.Helper.go;\nclass Main { void run() { go(); } }"),
        );

        let r = resolved_from(&s, "go", "run");
        assert_eq!(r.resolution, Resolution::Ambiguous);
        assert_eq!(r.target, None);
    }

    #[test]
    fn a_relative_import_that_anchors_nowhere_falls_through() {
        // More dots than the path has levels: the anchor cannot be computed, so
        // the older rules run unchanged rather than the edge being dropped.
        let mut s = InMemoryGraphStore::new();
        s.insert("util.py", py("def helper():\n    pass\n"));
        s.insert(
            "main.py",
            py("from ...deep import helper\ndef run():\n    helper()\n"),
        );
        let r = resolved_from(&s, "helper", "run");
        assert_eq!(r.resolution, Resolution::UniqueGlobal);
    }

    // ---- C/C++ includes (#267) ---------------------------------------------

    fn c(src: &str) -> crate::CodeGraph {
        crate::build(crate::Language::C, src)
    }

    #[test]
    fn an_include_narrows_an_ambiguous_reference() {
        // Including a header means calling into what that translation unit
        // defines. The include names the file directly (#267).
        let mut s = InMemoryGraphStore::new();
        s.insert("engine/render/pipeline.c", c("void draw(void) {}"));
        s.insert("tools/preview/pipeline.c", c("void draw(void) {}"));
        s.insert(
            "app/main.c",
            c("#include \"engine/render/pipeline.h\"\nvoid run(void) { draw(); }"),
        );

        let r = resolved_from(&s, "draw", "run");
        assert_eq!(r.resolution, Resolution::Imported);
        assert_eq!(r.target.as_deref(), Some("engine/render/pipeline.c"));
    }

    #[test]
    fn without_the_include_the_same_c_call_stays_ambiguous() {
        let mut s = InMemoryGraphStore::new();
        s.insert("engine/render/pipeline.c", c("void draw(void) {}"));
        s.insert("tools/preview/pipeline.c", c("void draw(void) {}"));
        s.insert("app/main.c", c("void run(void) { draw(); }"));

        assert_eq!(
            resolved_from(&s, "draw", "run").resolution,
            Resolution::Ambiguous
        );
    }

    #[test]
    fn an_include_fitting_several_candidates_falls_through() {
        // Two files at the same path stem: the include cannot say which.
        let mut s = InMemoryGraphStore::new();
        s.insert("a/pipeline.c", c("void draw(void) {}"));
        s.insert("b/pipeline.c", c("void draw(void) {}"));
        s.insert(
            "app/main.c",
            c("#include \"pipeline.h\"\nvoid run(void) { draw(); }"),
        );

        assert_eq!(
            resolved_from(&s, "draw", "run").resolution,
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_system_include_matches_nothing_in_the_view() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a/x.c", c("void printf_impl(void) {}"));
        s.insert("b/x.c", c("void printf_impl(void) {}"));
        s.insert(
            "app/main.c",
            c("#include <stdio.h>\nvoid run(void) { printf_impl(); }"),
        );

        assert_eq!(
            resolved_from(&s, "printf_impl", "run").resolution,
            Resolution::Ambiguous,
            "stdio is not in the view"
        );
    }

    #[test]
    fn a_named_import_still_wins_where_a_language_has_one() {
        // Guard: the include path must not displace the named lookup every
        // other language depends on.
        let s = two_makes(
            "src/app.rs",
            "use crate::model::make;\nfn caller() { make(); }",
        );
        assert_eq!(
            resolved_from(&s, "make", "caller").target.as_deref(),
            Some("src/model.rs")
        );
    }
}
