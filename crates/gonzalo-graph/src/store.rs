//! Storage and structural queries over an assembled view's slices.
//!
//! Slices are path-agnostic (ADR 0012); the store keys each slice by the path
//! it was assembled under, so location-bearing queries return [`Located`]
//! results while the stored [`Symbol`]/[`Reference`] stay path-free.

use crate::builder::Language;
use crate::model::{
    CodeGraph, FileSummary, Located, Page, RankedSymbol, Ranking, Reference, Symbol, SymbolFilter,
    SymbolKind, ViewOverview,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The language bucket for `path`, by extension. Unrecognized or extensionless
/// paths bucket under `"unknown"` so counts always sum to the symbol total.
fn language_of(path: &str) -> &'static str {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Language::from_extension)
        .map_or("unknown", Language::as_str)
}

/// Structural queries over an assembled view (a set of path-keyed slices).
pub trait GraphStore: Send + Sync {
    /// Insert (or replace) the slice assembled at `path`. Re-inserting the same
    /// path overwrites — slices are content-addressed and write-if-absent, never
    /// appended, so this cannot duplicate a file's symbols.
    fn insert(&mut self, path: &str, graph: CodeGraph);
    /// Symbols defined in the slice at `path`.
    fn symbols_in_file(&self, path: &str) -> Vec<Symbol>;
    /// Definitions matching `name`, each with the path it was found under (there
    /// may be several — names are unresolved).
    fn definitions(&self, name: &str) -> Vec<Located<Symbol>>;
    /// References whose target name is `name`, each with its path.
    fn references_to(&self, name: &str) -> Vec<Located<Reference>>;
    /// Distinct enclosing-function names that reference `name`.
    fn callers_of(&self, name: &str) -> Vec<String>;
    /// Distinct names referenced from within `name` (the inverse of
    /// [`callers_of`](Self::callers_of)), sorted.
    fn callees(&self, name: &str) -> Vec<String>;
    /// Every symbol in the view, each with its path (used for whole-graph
    /// operations like diffing).
    fn all_symbols(&self) -> Vec<Located<Symbol>>;
    /// Every reference in the view, each with its path.
    fn all_references(&self) -> Vec<Located<Reference>>;

    /// The transitive closure of callers: every symbol that could be affected if
    /// `name` changes, reached by walking [`callers_of`](Self::callers_of)
    /// breadth-first. Runs server-side (never ships the whole graph); the seed
    /// `name` is excluded, and cycles terminate via a visited set. Returned
    /// sorted.
    fn impact(&self, name: &str) -> Vec<String> {
        let mut visited = BTreeSet::from([name.to_string()]);
        let mut queue: VecDeque<String> = VecDeque::new();
        for caller in self.callers_of(name) {
            if visited.insert(caller.clone()) {
                queue.push_back(caller);
            }
        }
        while let Some(current) = queue.pop_front() {
            for caller in self.callers_of(&current) {
                if visited.insert(caller.clone()) {
                    queue.push_back(caller);
                }
            }
        }
        visited.remove(name);
        visited.into_iter().collect()
    }

    /// The aggregate shape of the whole view: counts, a breakdown by kind and
    /// by language, and the `largest` files by symbol count.
    ///
    /// Answers "what is in this view" without the caller having to know a
    /// symbol name first. Like [`impact`](Self::impact) this runs server-side
    /// over the assembled graph; only the summary is returned.
    fn overview(&self, largest: usize) -> ViewOverview {
        let symbols = self.all_symbols();
        let references = self.all_references();

        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
        let mut per_file: BTreeMap<&str, usize> = BTreeMap::new();
        for located in &symbols {
            *by_kind
                .entry(located.item.kind.as_str().to_string())
                .or_default() += 1;
            *by_language
                .entry(language_of(&located.path).to_string())
                .or_default() += 1;
            *per_file.entry(located.path.as_str()).or_default() += 1;
        }

        // A file contributes to the view if it holds symbols *or* references.
        let files: BTreeSet<&str> = symbols
            .iter()
            .map(|l| l.path.as_str())
            .chain(references.iter().map(|l| l.path.as_str()))
            .collect();

        // Descending by symbol count, then by path so ties are deterministic.
        let mut largest_files: Vec<FileSummary> = per_file
            .into_iter()
            .map(|(path, symbols)| FileSummary {
                path: path.to_string(),
                symbols,
            })
            .collect();
        largest_files.sort_by(|a, b| b.symbols.cmp(&a.symbols).then_with(|| a.path.cmp(&b.path)));
        largest_files.truncate(largest);

        ViewOverview {
            files: files.len(),
            symbols: symbols.len(),
            references: references.len(),
            by_kind,
            by_language,
            largest_files,
        }
    }

    /// The top `limit` symbol names by `ranking`, descending.
    ///
    /// [`Ranking::Definitions`] is the ambiguity report: any name scoring above
    /// 1 is defined in several places, so every name-matched traversal through
    /// it merges unrelated subgraphs.
    fn top(&self, ranking: Ranking, limit: usize) -> Page<RankedSymbol> {
        let mut definitions: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let symbols = self.all_symbols();
        for located in &symbols {
            definitions
                .entry(located.item.name.as_str())
                .or_default()
                .insert(located.path.as_str());
        }

        let references = self.all_references();
        let mut scores: BTreeMap<&str, usize> = BTreeMap::new();
        match ranking {
            Ranking::Definitions => {
                for (name, paths) in &definitions {
                    scores.insert(name, paths.len());
                }
            }
            Ranking::FanIn => {
                for located in &references {
                    *scores.entry(located.item.name.as_str()).or_default() += 1;
                }
            }
            Ranking::FanOut => {
                let mut callees: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
                for located in &references {
                    if let Some(from) = located.item.from.as_deref() {
                        callees.entry(from).or_default().insert(&located.item.name);
                    }
                }
                for (name, called) in &callees {
                    scores.insert(name, called.len());
                }
            }
        }

        // Descending by score, then by name so ties are deterministic.
        let mut ranked: Vec<RankedSymbol> = scores
            .into_iter()
            .map(|(name, score)| RankedSymbol {
                name: name.to_string(),
                score,
                paths: definitions
                    .get(name)
                    .map(|paths| paths.iter().map(|p| p.to_string()).collect())
                    .unwrap_or_default(),
            })
            .collect();
        ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        Page::new(ranked, limit)
    }

    /// Symbols matching `filter`, in path order, bounded by `limit`.
    ///
    /// The enumeration counterpart to [`definitions`](Self::definitions):
    /// answers "what is in this crate" rather than "where is this name".
    fn list(&self, filter: &SymbolFilter, limit: usize) -> Page<Located<Symbol>> {
        let matched: Vec<Located<Symbol>> = self
            .all_symbols()
            .into_iter()
            .filter(|located| filter.matches(located))
            .collect();
        Page::new(matched, limit)
    }

    /// Symbols with no inbound reference anywhere in the view — dead-code
    /// *candidates*, in path then line order.
    ///
    /// With `exclude_tests` (the useful default) symbols inside a test scope are
    /// dropped: members of a `mod tests` / `mod test` block, by line range, and
    /// anything under a `tests/` directory. On gonzalo itself that filter is the
    /// difference between 515 hits and 40 — without it the result is ~92% noise.
    ///
    /// **This is a heuristic, and its false positives are real.** It inherits
    /// every limit of the name-matched extractor underneath:
    ///
    /// - A function used only as a value (`map_err(be)`, `and_then(f)`) is a
    ///   path expression, not a call, so higher-order usage is invisible.
    /// - Calls inside Rust macro arguments *are* now recorded (#216), but by a
    ///   token-level heuristic: an identifier followed by a parenthesised token
    ///   tree. A tuple-struct pattern such as `Some(_)` reads the same way, so
    ///   macro-derived edges are slightly over-inclusive rather than missing.
    /// - Names are unresolved: an unused `foo` is hidden by any other `foo` that
    ///   is used.
    /// - Conversely a reference from anywhere counts, including from tests and
    ///   from the symbol itself, so recursive-only and test-only functions are
    ///   *not* reported even though they may be dead.
    ///
    /// Treat every result as a lead to confirm, never as proof.
    fn unreferenced(
        &self,
        filter: &SymbolFilter,
        exclude_tests: bool,
        limit: usize,
    ) -> Page<Located<Symbol>> {
        let referenced: BTreeSet<String> = self
            .all_references()
            .into_iter()
            .map(|located| located.item.name)
            .collect();

        let symbols = self.all_symbols();
        // Line ranges of `mod tests` blocks, per path, so members can be
        // excluded by containment rather than by name.
        let test_scopes: Vec<(String, usize, usize)> = symbols
            .iter()
            .filter(|l| l.item.kind == SymbolKind::Module && is_test_scope_name(&l.item.name))
            .map(|l| (l.path.clone(), l.item.start_line, l.item.end_line))
            .collect();

        let in_test_scope = |located: &Located<Symbol>| {
            in_tests_dir(&located.path)
                || test_scopes.iter().any(|(path, start, end)| {
                    *path == located.path
                        && located.item.start_line >= *start
                        && located.item.end_line <= *end
                })
        };

        let mut matched: Vec<Located<Symbol>> = symbols
            .into_iter()
            .filter(|located| !referenced.contains(&located.item.name))
            .filter(|located| filter.matches(located))
            .filter(|located| !exclude_tests || !in_test_scope(located))
            .collect();

        matched.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.item.start_line.cmp(&b.item.start_line))
        });
        Page::new(matched, limit)
    }
}

/// Whether a module name marks a test scope (Rust's `mod tests` convention).
fn is_test_scope_name(name: &str) -> bool {
    name == "tests" || name == "test"
}

/// Whether `path` lies under a `tests/` directory. Compares whole path
/// components, so `contests/entry.rs` does not match.
fn in_tests_dir(path: &str) -> bool {
    path.split('/').any(|part| part == "tests")
}

/// An in-memory [`GraphStore`], one slice per path.
#[derive(Debug, Default)]
pub struct InMemoryGraphStore {
    slices: BTreeMap<String, CodeGraph>,
}

impl InMemoryGraphStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// The assembled slices, keyed by path.
    pub fn slices(&self) -> &BTreeMap<String, CodeGraph> {
        &self.slices
    }
}

impl GraphStore for InMemoryGraphStore {
    fn insert(&mut self, path: &str, graph: CodeGraph) {
        self.slices.insert(path.to_string(), graph);
    }

    fn symbols_in_file(&self, path: &str) -> Vec<Symbol> {
        self.slices
            .get(path)
            .map(|g| g.symbols.clone())
            .unwrap_or_default()
    }

    fn definitions(&self, name: &str) -> Vec<Located<Symbol>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.symbols
                    .iter()
                    .filter(|s| s.name == name)
                    .map(move |s| Located {
                        path: path.clone(),
                        item: s.clone(),
                    })
            })
            .collect()
    }

    fn references_to(&self, name: &str) -> Vec<Located<Reference>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.references
                    .iter()
                    .filter(|r| r.name == name)
                    .map(move |r| Located {
                        path: path.clone(),
                        item: r.clone(),
                    })
            })
            .collect()
    }

    fn callers_of(&self, name: &str) -> Vec<String> {
        let mut callers: Vec<String> = self
            .slices
            .values()
            .flat_map(|g| g.references.iter())
            .filter(|r| r.name == name)
            .filter_map(|r| r.from.clone())
            .collect();
        callers.sort();
        callers.dedup();
        callers
    }

    fn callees(&self, name: &str) -> Vec<String> {
        let mut callees: Vec<String> = self
            .slices
            .values()
            .flat_map(|g| g.references.iter())
            .filter(|r| r.from.as_deref() == Some(name))
            .map(|r| r.name.clone())
            .collect();
        callees.sort();
        callees.dedup();
        callees
    }

    fn all_symbols(&self) -> Vec<Located<Symbol>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.symbols.iter().map(move |s| Located {
                    path: path.clone(),
                    item: s.clone(),
                })
            })
            .collect()
    }

    fn all_references(&self) -> Vec<Located<Reference>> {
        self.slices
            .iter()
            .flat_map(|(path, g)| {
                g.references.iter().map(move |r| Located {
                    path: path.clone(),
                    item: r.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{build, build_rust};
    use crate::model::SymbolKind;

    const SRC: &str = r#"
fn helper() {}
fn a() { helper(); }
fn b() { helper(); }
"#;

    fn store() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert("lib.rs", build_rust(SRC));
        s
    }

    #[test]
    fn definitions_carry_their_path() {
        let s = store();
        let defs = s.definitions("helper");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].path, "lib.rs");
        assert_eq!(defs[0].item.name, "helper");
    }

    #[test]
    fn symbols_in_file_filters_by_path() {
        let s = store();
        assert!(s.symbols_in_file("lib.rs").iter().any(|sy| sy.name == "a"));
        assert!(s.symbols_in_file("other.rs").is_empty());
    }

    #[test]
    fn definitions_span_multiple_paths() {
        let mut s = InMemoryGraphStore::new();
        s.insert("a.rs", build_rust("fn dup() {}"));
        s.insert("b.rs", build_rust("fn dup() {}"));
        let mut paths: Vec<String> = s.definitions("dup").into_iter().map(|l| l.path).collect();
        paths.sort();
        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
    }

    #[test]
    fn reinserting_same_path_replaces_not_appends() {
        let mut s = store();
        // Re-assembling the same path must not duplicate its symbols.
        s.insert("lib.rs", build_rust(SRC));
        assert_eq!(s.definitions("helper").len(), 1);
    }

    #[test]
    fn callers_of_dedups_and_sorts_across_slices() {
        let s = store();
        assert_eq!(
            s.callers_of("helper"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn references_to_counts_all_with_paths() {
        let s = store();
        let refs = s.references_to("helper");
        assert_eq!(refs.len(), 2);
        assert!(refs.iter().all(|r| r.path == "lib.rs"));
    }

    const CHAIN: &str = r#"
fn leaf() {}
fn mid() { leaf(); }
fn top() { mid(); }
fn other() { leaf(); }
"#;

    fn chain() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert("chain.rs", build_rust(CHAIN));
        s
    }

    #[test]
    fn callees_lists_names_called_from_a_function() {
        let s = chain();
        assert_eq!(s.callees("mid"), vec!["leaf".to_string()]);
        assert_eq!(s.callees("top"), vec!["mid".to_string()]);
        assert!(s.callees("leaf").is_empty());
    }

    #[test]
    fn callees_dedups_repeated_calls() {
        let mut s = InMemoryGraphStore::new();
        s.insert("d.rs", build_rust("fn f() { g(); g(); h(); }"));
        assert_eq!(s.callees("f"), vec!["g".to_string(), "h".to_string()]);
    }

    #[test]
    fn impact_is_the_transitive_caller_closure() {
        let s = chain();
        // Everything transitively affected if `leaf` changes: its callers mid &
        // other, and mid's caller top.
        assert_eq!(
            s.impact("leaf"),
            vec!["mid".to_string(), "other".to_string(), "top".to_string()]
        );
        assert_eq!(s.impact("mid"), vec!["top".to_string()]);
        assert!(s.impact("top").is_empty());
    }

    #[test]
    fn impact_excludes_the_seed_and_survives_cycles() {
        let mut s = InMemoryGraphStore::new();
        s.insert("cyc.rs", build_rust("fn a() { b(); } fn b() { a(); }"));
        // a<->b mutually recurse; impact must terminate and not report the seed.
        assert_eq!(s.impact("a"), vec!["b".to_string()]);
        assert_eq!(s.impact("b"), vec!["a".to_string()]);
    }

    // ---- aggregate / structural queries (#214) ----------------------------

    /// A small multi-path, multi-language view: `new` is defined in two crates
    /// (the ambiguity #207 turns on), `helper` is called twice from one site,
    /// and one file is deliberately larger than the others.
    fn view() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "crates/a/src/lib.rs",
            build_rust(
                "struct A; struct A2; fn new() -> A { A } \
                 fn run() { new(); helper(); helper(); }",
            ),
        );
        s.insert("crates/b/src/lib.rs", build_rust("fn new() -> u8 { 0 }"));
        s.insert(
            "scripts/tool.py",
            build(Language::Python, "def main():\n    pass\n"),
        );
        s
    }

    #[test]
    fn overview_counts_files_symbols_and_references() {
        let o = view().overview(10);
        assert_eq!(o.files, 3);
        assert_eq!(o.symbols, view().all_symbols().len());
        assert_eq!(o.references, view().all_references().len());
    }

    #[test]
    fn overview_breaks_down_by_kind() {
        let o = view().overview(10);
        // Two structs in a/lib.rs; functions in every file.
        assert_eq!(o.by_kind.get("struct"), Some(&2));
        assert!(o.by_kind.get("function").is_some_and(|n| *n >= 3));
    }

    #[test]
    fn overview_breaks_down_by_language_from_the_path_extension() {
        let o = view().overview(10);
        assert!(o.by_language.contains_key("rust"));
        assert!(o.by_language.contains_key("python"));
        // Every symbol lands in exactly one language bucket.
        assert_eq!(o.by_language.values().sum::<usize>(), o.symbols);
    }

    #[test]
    fn overview_buckets_unrecognized_extensions_as_unknown() {
        let mut s = InMemoryGraphStore::new();
        s.insert("build.zzz", build_rust("fn f() {}"));
        let o = s.overview(10);
        assert_eq!(o.by_language.get("unknown"), Some(&1));
        assert_eq!(o.by_language.values().sum::<usize>(), o.symbols);
    }

    #[test]
    fn overview_ranks_largest_files_by_symbol_count() {
        let o = view().overview(10);
        assert_eq!(o.largest_files[0].path, "crates/a/src/lib.rs");
        assert!(o.largest_files[0].symbols >= o.largest_files[1].symbols);
    }

    #[test]
    fn overview_bounds_largest_files_to_the_requested_limit() {
        let o = view().overview(1);
        assert_eq!(o.largest_files.len(), 1);
        // The count itself is never truncated, only the per-file listing.
        assert_eq!(o.files, 3);
    }

    #[test]
    fn top_by_definitions_surfaces_names_defined_in_several_paths() {
        let page = view().top(Ranking::Definitions, 10);
        let new = page
            .items
            .iter()
            .find(|r| r.name == "new")
            .expect("`new` is defined twice");
        assert_eq!(new.score, 2);
        assert_eq!(
            new.paths,
            vec![
                "crates/a/src/lib.rs".to_string(),
                "crates/b/src/lib.rs".to_string()
            ]
        );
    }

    #[test]
    fn top_by_fan_in_ranks_by_reference_count() {
        let page = view().top(Ranking::FanIn, 10);
        let helper = page
            .items
            .iter()
            .find(|r| r.name == "helper")
            .expect("`helper` is called twice");
        assert_eq!(helper.score, 2);
    }

    #[test]
    fn top_by_fan_out_ranks_by_distinct_callees() {
        let page = view().top(Ranking::FanOut, 10);
        let run = page
            .items
            .iter()
            .find(|r| r.name == "run")
            .expect("`run` calls new and helper");
        assert_eq!(run.score, 2);
    }

    #[test]
    fn top_is_ordered_by_descending_score() {
        let page = view().top(Ranking::FanIn, 10);
        let scores: Vec<usize> = page.items.iter().map(|r| r.score).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);
    }

    #[test]
    fn top_bounds_results_and_reports_truncation() {
        let page = view().top(Ranking::FanIn, 1);
        assert_eq!(page.items.len(), 1);
        assert!(page.truncated);
        assert!(page.total > 1);
    }

    #[test]
    fn list_filters_by_path_prefix() {
        let page = view().list(&SymbolFilter::default().path_prefix("crates/b"), 100);
        assert!(!page.items.is_empty());
        assert!(page.items.iter().all(|l| l.path == "crates/b/src/lib.rs"));
    }

    #[test]
    fn list_filters_by_kind() {
        let page = view().list(&SymbolFilter::default().kind(SymbolKind::Struct), 100);
        assert_eq!(page.items.len(), 2);
        assert!(page.items.iter().all(|l| l.item.kind == SymbolKind::Struct));
    }

    #[test]
    fn list_filters_by_name_substring() {
        let page = view().list(&SymbolFilter::default().name_contains("ru"), 100);
        assert!(page.items.iter().any(|l| l.item.name == "run"));
        assert!(page.items.iter().all(|l| l.item.name.contains("ru")));
    }

    #[test]
    fn list_combines_filters_conjunctively() {
        let page = view().list(
            &SymbolFilter::default()
                .path_prefix("crates/a")
                .kind(SymbolKind::Struct),
            100,
        );
        assert_eq!(page.items.len(), 2);
        assert!(
            page.items
                .iter()
                .all(|l| l.path.starts_with("crates/a") && l.item.kind == SymbolKind::Struct)
        );
    }

    #[test]
    fn list_bounds_results_and_reports_truncation() {
        let page = view().list(&SymbolFilter::default(), 1);
        assert_eq!(page.items.len(), 1);
        assert!(page.truncated);
        assert_eq!(page.total, view().all_symbols().len());
    }

    #[test]
    fn list_without_filters_returns_everything_untruncated() {
        let page = view().list(&SymbolFilter::default(), 1000);
        assert!(!page.truncated);
        assert_eq!(page.items.len(), view().all_symbols().len());
    }

    // ---- unreferenced / dead-code candidates (#214) -----------------------

    /// A view with one genuinely-unused production function (`orphan`), one
    /// used one (`used`), and a `mod tests` whose members are unused outside
    /// the module — the 92%-noise population the filter exists to remove.
    fn dead() -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "src/lib.rs",
            build_rust(
                "fn used() {}\n\
                 fn caller() { used(); }\n\
                 fn orphan() {}\n\
                 #[cfg(test)]\n\
                 mod tests {\n    \
                     fn t_helper() {}\n    \
                     fn t_only() {}\n\
                 }\n",
            ),
        );
        s
    }

    fn names(page: &Page<Located<Symbol>>) -> Vec<&str> {
        page.items.iter().map(|l| l.item.name.as_str()).collect()
    }

    #[test]
    fn unreferenced_finds_symbols_with_no_inbound_reference() {
        let page = dead().unreferenced(&SymbolFilter::default(), true, 100);
        assert!(names(&page).contains(&"orphan"));
        assert!(!names(&page).contains(&"used"));
    }

    #[test]
    fn unreferenced_excludes_mod_tests_members_by_default() {
        let page = dead().unreferenced(&SymbolFilter::default(), true, 100);
        // Both live inside `mod tests`' line range, so neither is a candidate.
        assert!(!names(&page).contains(&"t_helper"));
        assert!(!names(&page).contains(&"t_only"));
        // The module symbol itself is a test scope, not a candidate.
        assert!(!names(&page).contains(&"tests"));
    }

    #[test]
    fn unreferenced_keeps_test_members_when_not_excluding() {
        let page = dead().unreferenced(&SymbolFilter::default(), false, 100);
        assert!(names(&page).contains(&"t_only"));
        assert!(names(&page).contains(&"orphan"));
    }

    #[test]
    fn unreferenced_excludes_files_under_a_tests_directory() {
        let mut s = InMemoryGraphStore::new();
        s.insert(
            "tests/it.rs",
            build_rust("fn only_in_integration_test() {}"),
        );
        s.insert("src/lib.rs", build_rust("fn orphan() {}"));
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        assert_eq!(names(&page), vec!["orphan"]);
    }

    #[test]
    fn unreferenced_does_not_treat_a_tests_substring_as_a_directory() {
        let mut s = InMemoryGraphStore::new();
        // `contests` merely contains "tests"; it is not a test directory.
        s.insert("contests/entry.rs", build_rust("fn orphan() {}"));
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        assert_eq!(names(&page), vec!["orphan"]);
    }

    #[test]
    fn unreferenced_counts_a_reference_from_anywhere_including_tests() {
        let mut s = InMemoryGraphStore::new();
        // `prod` is called only from a test — conservatively still "referenced",
        // so it is never reported as dead.
        s.insert(
            "src/lib.rs",
            build_rust(
                "fn prod() {}\n\
                 #[cfg(test)]\n\
                 mod tests {\n    \
                     fn t() { prod(); }\n\
                 }\n",
            ),
        );
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        assert!(!names(&page).contains(&"prod"));
    }

    #[test]
    fn unreferenced_does_not_flag_symbols_called_inside_macros() {
        let mut s = InMemoryGraphStore::new();
        // Was a pinned false positive: macro bodies parse as token trees, so
        // the call to `f` went unrecorded and `f` was reported as dead. #216
        // taught the extractor to read calls out of macro arguments, and this
        // test flipped with it.
        s.insert(
            "src/lib.rs",
            build_rust("fn f() -> u8 { 0 }\nfn g() { assert_eq!(f(), 0); }\n"),
        );
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        assert!(!names(&page).contains(&"f"), "f is called from g");
    }

    #[test]
    fn unreferenced_does_not_flag_recursive_functions() {
        let mut s = InMemoryGraphStore::new();
        // A self-reference counts, so a recursive-only function is a false
        // negative rather than a confidently-wrong dead-code report.
        s.insert("src/lib.rs", build_rust("fn recur() { recur(); }"));
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        assert!(!names(&page).contains(&"recur"));
    }

    #[test]
    fn unreferenced_applies_the_symbol_filter() {
        let page =
            dead().unreferenced(&SymbolFilter::default().kind(SymbolKind::Struct), true, 100);
        assert!(page.items.is_empty());
    }

    #[test]
    fn unreferenced_results_carry_their_path() {
        let page = dead().unreferenced(&SymbolFilter::default(), true, 100);
        assert!(page.items.iter().all(|l| l.path == "src/lib.rs"));
    }

    #[test]
    fn unreferenced_bounds_results_and_reports_truncation() {
        let mut s = InMemoryGraphStore::new();
        s.insert("src/lib.rs", build_rust("fn a() {} fn b() {} fn c() {}"));
        let page = s.unreferenced(&SymbolFilter::default(), true, 2);
        assert_eq!(page.items.len(), 2);
        assert!(page.truncated);
        assert_eq!(page.total, 3);
    }

    #[test]
    fn unreferenced_is_ordered_by_path_then_line() {
        let mut s = InMemoryGraphStore::new();
        s.insert("src/b.rs", build_rust("fn z() {}"));
        s.insert("src/a.rs", build_rust("fn y() {}\nfn x() {}"));
        let page = s.unreferenced(&SymbolFilter::default(), true, 100);
        let seen: Vec<(&str, &str)> = page
            .items
            .iter()
            .map(|l| (l.path.as_str(), l.item.name.as_str()))
            .collect();
        assert_eq!(
            seen,
            vec![("src/a.rs", "y"), ("src/a.rs", "x"), ("src/b.rs", "z")]
        );
    }
}
