//! A reusable conformance suite every [`GraphStore`] impl must pass, so a
//! persistent backend (e.g. SQLite, ticket B) is provably equivalent to the
//! in-memory reference. Backend crates call [`run_graph_store_conformance`]
//! from their tests with a factory that returns a fresh, empty store.

use crate::{FromScope, GraphStore, Language, RefKind, build, build_rust};
use std::collections::BTreeSet;

/// Run the full suite against stores produced by `make` (a fresh, empty
/// [`GraphStore`] per call).
pub fn run_graph_store_conformance<S: GraphStore>(make: impl Fn() -> S) {
    definitions_locate_by_path(&mut seeded(&make));
    symbols_in_file_filters_by_path(&mut seeded(&make));
    callers_callees_and_impact(&mut seeded(&make));
    references_to_reports_paths(&mut seeded(&make));
    reinsert_replaces_a_path(&mut seeded(&make));
    enumerates_all_symbols_and_references(&mut seeded(&make));
    qualifier_and_owner_survive_a_round_trip(&mut seeded(&make));
    a_value_reference_is_stored_but_kept_out_of_the_call_graph(&mut seeded(&make));
    imports_survive_a_round_trip_and_disambiguate(&mut seeded(&make));
    module_scope_survives_a_round_trip(&mut seeded(&make));
    empty_store_answers_are_empty(&mut make());
}

/// A store seeded with a small two-file call chain:
/// `leaf` (lib.rs) ← `mid` (lib.rs) ← `top` (main.rs).
fn seeded<S: GraphStore>(make: &impl Fn() -> S) -> S {
    let mut s = make();
    s.insert("lib.rs", build_rust("fn leaf() {}\nfn mid() { leaf(); }"));
    s.insert("main.rs", build_rust("fn top() { mid(); }"));
    s
}

fn definitions_locate_by_path<S: GraphStore>(s: &mut S) {
    let defs = s.definitions("leaf");
    assert_eq!(defs.len(), 1, "one definition of leaf");
    assert_eq!(defs[0].path, "lib.rs");
    assert_eq!(defs[0].item.name, "leaf");
    assert_eq!(s.definitions("top")[0].path, "main.rs");
    assert!(s.definitions("nonexistent").is_empty());
}

fn symbols_in_file_filters_by_path<S: GraphStore>(s: &mut S) {
    let lib: Vec<String> = s
        .symbols_in_file("lib.rs")
        .into_iter()
        .map(|sy| sy.name)
        .collect();
    assert!(lib.contains(&"leaf".to_string()));
    assert!(lib.contains(&"mid".to_string()));
    assert!(!lib.contains(&"top".to_string()));
    assert!(s.symbols_in_file("absent.rs").is_empty());
}

fn callers_callees_and_impact<S: GraphStore>(s: &mut S) {
    assert_eq!(s.callers_of("leaf"), vec!["mid".to_string()]);
    assert_eq!(s.callers_of("mid"), vec!["top".to_string()]);
    assert_eq!(s.callees("mid"), vec!["leaf".to_string()]);
    assert_eq!(s.callees("top"), vec!["mid".to_string()]);
    assert!(s.callees("leaf").is_empty());
    // Transitive caller closure, seed-excluded and sorted.
    assert_eq!(s.impact("leaf"), vec!["mid".to_string(), "top".to_string()]);
    assert_eq!(s.impact("mid"), vec!["top".to_string()]);
    assert!(s.impact("top").is_empty());
}

fn references_to_reports_paths<S: GraphStore>(s: &mut S) {
    let refs = s.references_to("mid");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].path, "main.rs");
    assert_eq!(refs[0].item.from.as_deref(), Some("top"));
}

fn reinsert_replaces_a_path<S: GraphStore>(s: &mut S) {
    // Re-assembling the same path must not duplicate its symbols.
    s.insert("lib.rs", build_rust("fn leaf() {}\nfn mid() { leaf(); }"));
    assert_eq!(s.definitions("leaf").len(), 1);
    assert_eq!(s.callers_of("leaf"), vec!["mid".to_string()]);
}

fn enumerates_all_symbols_and_references<S: GraphStore>(s: &mut S) {
    let symbol_names: BTreeSet<String> = s.all_symbols().into_iter().map(|l| l.item.name).collect();
    assert_eq!(
        symbol_names,
        BTreeSet::from(["leaf".to_string(), "mid".to_string(), "top".to_string()])
    );
    // Every reference to `leaf` and `mid` appears exactly once in the full set.
    let refs = s.all_references();
    assert_eq!(refs.iter().filter(|r| r.item.name == "leaf").count(), 1);
    assert_eq!(refs.iter().filter(|r| r.item.name == "mid").count(), 1);
    assert!(
        s.all_symbols()
            .iter()
            .any(|l| l.item.name == "top" && l.path == "main.rs")
    );
}

fn empty_store_answers_are_empty<S: GraphStore>(s: &mut S) {
    assert!(s.definitions("anything").is_empty());
    assert!(s.callers_of("anything").is_empty());
    assert!(s.callees("anything").is_empty());
    assert!(s.impact("anything").is_empty());
    assert!(s.symbols_in_file("anything").is_empty());
    assert!(s.references_to("anything").is_empty());
    assert!(s.all_symbols().is_empty());
    assert!(s.all_references().is_empty());
}

/// A backend that drops [`Symbol::owner`] or [`Reference::qualifier`] silently
/// resolves everything by bare name again, and would still pass every other
/// case in this suite. Both fields are optional, so a lossy backend looks
/// exactly like a file that simply had none (#248).
fn qualifier_and_owner_survive_a_round_trip<S: GraphStore>(s: &mut S) {
    s.insert(
        "beta.rs",
        build_rust("struct Beta; impl Beta { fn get() {} }"),
    );
    s.insert("call.rs", build_rust("fn caller() { Beta::get(); }"));

    let owned = s
        .definitions("get")
        .into_iter()
        .find(|d| d.path == "beta.rs")
        .expect("method definition stored");
    assert_eq!(owned.item.owner.as_deref(), Some("Beta"));

    let call = s
        .references_to("get")
        .into_iter()
        .find(|r| r.path == "call.rs")
        .expect("qualified call stored");
    assert_eq!(call.item.qualifier.as_deref(), Some("Beta"));

    // The whole-view enumerations read through their own queries.
    let all_owned = s
        .all_symbols()
        .into_iter()
        .find(|d| d.path == "beta.rs" && d.item.name == "get")
        .expect("method in all_symbols");
    assert_eq!(all_owned.item.owner.as_deref(), Some("Beta"));
    let all_call = s
        .all_references()
        .into_iter()
        .find(|r| r.path == "call.rs" && r.item.name == "get")
        .expect("call in all_references");
    assert_eq!(all_call.item.qualifier.as_deref(), Some("Beta"));

    // A free function and a plain call carry neither.
    assert!(s.definitions("caller")[0].item.owner.is_none());
    assert!(s.references_to("mid")[0].item.qualifier.is_none());
}

/// A function passed as a value must reach the store (or `unreferenced` calls a
/// live callback dead again) while staying out of `callers`/`callees` (or
/// passing a function reads as calling it). A backend that persists the row but
/// forgets the filter looks correct on `unreferenced` and wrong on the call
/// graph, so both halves are checked here rather than only in the in-memory
/// reference (#250).
fn a_value_reference_is_stored_but_kept_out_of_the_call_graph<S: GraphStore>(s: &mut S) {
    s.insert(
        "cb.rs",
        build_rust("fn handler() {}\nfn wire() { register(handler); }"),
    );

    // Stored, and marked as a value.
    let refs = s.references_to("handler");
    assert_eq!(refs.len(), 1, "value reference persisted");
    assert_eq!(refs[0].item.kind, RefKind::Value);

    // But not a call edge in either direction.
    assert!(
        s.callers_of("handler").is_empty(),
        "passing is not calling: {:?}",
        s.callers_of("handler")
    );
    assert_eq!(
        s.callees("wire"),
        vec!["register".to_string()],
        "only the real call"
    );

    // And it still counts as a reference, so `handler` is not reported dead.
    let dead = s.unreferenced(&crate::model::SymbolFilter::default(), false, 100);
    assert!(
        !dead.items.iter().any(|l| l.item.name == "handler"),
        "handler is referenced: {dead:?}"
    );
}

/// A backend that drops imports leaves import-aware resolution quietly doing
/// nothing while every other query still looks right — the exact shape of
/// failure this suite exists to catch (#252).
fn imports_survive_a_round_trip_and_disambiguate<S: GraphStore>(s: &mut S) {
    s.insert("src/model.rs", build_rust("fn make() {}"));
    s.insert("src/other.rs", build_rust("fn make() {}"));
    s.insert(
        "src/app.rs",
        build_rust("use crate::model::make;\nfn caller() { make(); }"),
    );

    let imports = s.imports_in_file("src/app.rs");
    assert_eq!(imports.len(), 1, "import persisted: {imports:?}");
    assert_eq!(imports[0].name, "make");
    assert_eq!(imports[0].path, vec!["model".to_string()]);
    assert!(
        s.imports_in_file("src/model.rs").is_empty(),
        "a file with no imports has none"
    );

    // A relative import's depth is what lets it be anchored at all; a backend
    // that drops it turns the import back into a free-floating suffix (#261).
    s.insert(
        "pkg/app.py",
        build(Language::Python, "from ..other import thing\n"),
    );
    let relative = s.imports_in_file("pkg/app.py");
    assert_eq!(relative.len(), 1, "{relative:?}");
    assert_eq!(relative[0].depth, 2, "{relative:?}");
    assert_eq!(
        imports[0].depth, 0,
        "an absolute import stays at zero: {imports:?}"
    );

    // And the resolver can then tell the two `make`s apart.
    let resolved = crate::resolve::resolve_references_to(s, "make");
    let call = resolved
        .iter()
        .find(|r| r.reference.item.from.as_deref() == Some("caller"))
        .expect("call recorded");
    assert_eq!(
        call.target.as_deref(),
        Some("src/model.rs"),
        "import must narrow the candidates: {call:?}"
    );
}

/// A backend that drops [`Reference::from_scope`] silently widens what `from`
/// means: a module-level binding starts reading as a function, which is exactly
/// the distinction #268 added the field to preserve.
fn module_scope_survives_a_round_trip<S: GraphStore>(s: &mut S) {
    s.insert(
        "schema.ts",
        build(
            Language::TypeScript,
            "const allEnv = z.object({});\nconst run = () => { go(); };",
        ),
    );

    let module = s
        .references_to("object")
        .into_iter()
        .next()
        .expect("module-level call stored");
    assert_eq!(module.item.from.as_deref(), Some("allEnv"));
    assert_eq!(module.item.from_scope, FromScope::Module);

    let function = s
        .references_to("go")
        .into_iter()
        .next()
        .expect("call in a function stored");
    assert_eq!(function.item.from.as_deref(), Some("run"));
    assert_eq!(
        function.item.from_scope,
        FromScope::Function,
        "an enclosing function still reports itself as one"
    );
}
