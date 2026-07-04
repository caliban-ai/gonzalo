//! Structural diff between two assembled views (ticket K).
//!
//! Given two [`GraphStore`]s — e.g. two competing worktrees over the same slice
//! store — [`diff`] reports which symbols and references were **added** (in `b`,
//! not `a`) or **removed** (in `a`, not `b`). Identity is structural, not
//! positional: a symbol is `(path, name, kind)` and a reference is
//! `(path, from, name)`, so line shifts do not register as changes.

use crate::{GraphStore, Located, Reference, Symbol, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The structural difference between two views (`a` → `b`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphDiff {
    /// Symbols in `b` but not `a`.
    pub added_symbols: Vec<Located<Symbol>>,
    /// Symbols in `a` but not `b`.
    pub removed_symbols: Vec<Located<Symbol>>,
    /// References in `b` but not `a`.
    pub added_references: Vec<Located<Reference>>,
    /// References in `a` but not `b`.
    pub removed_references: Vec<Located<Reference>>,
}

impl GraphDiff {
    /// Whether the two views are structurally identical.
    pub fn is_empty(&self) -> bool {
        self.added_symbols.is_empty()
            && self.removed_symbols.is_empty()
            && self.added_references.is_empty()
            && self.removed_references.is_empty()
    }
}

type SymbolKey = (String, String, SymbolKind);
type ReferenceKey = (String, Option<String>, String);

fn symbol_key(l: &Located<Symbol>) -> SymbolKey {
    (l.path.clone(), l.item.name.clone(), l.item.kind)
}

fn reference_key(l: &Located<Reference>) -> ReferenceKey {
    (l.path.clone(), l.item.from.clone(), l.item.name.clone())
}

/// Diff view `a` against view `b`: what `b` adds and what it removes.
pub fn diff(a: &dyn GraphStore, b: &dyn GraphStore) -> GraphDiff {
    let (a_syms, b_syms) = (a.all_symbols(), b.all_symbols());
    let (a_refs, b_refs) = (a.all_references(), b.all_references());

    let a_sym_keys: HashSet<SymbolKey> = a_syms.iter().map(symbol_key).collect();
    let b_sym_keys: HashSet<SymbolKey> = b_syms.iter().map(symbol_key).collect();
    let a_ref_keys: HashSet<ReferenceKey> = a_refs.iter().map(reference_key).collect();
    let b_ref_keys: HashSet<ReferenceKey> = b_refs.iter().map(reference_key).collect();

    GraphDiff {
        added_symbols: dedup_by_key(b_syms, |l| !a_sym_keys.contains(&symbol_key(l)), symbol_key),
        removed_symbols: dedup_by_key(a_syms, |l| !b_sym_keys.contains(&symbol_key(l)), symbol_key),
        added_references: dedup_by_key(
            b_refs,
            |l| !a_ref_keys.contains(&reference_key(l)),
            reference_key,
        ),
        removed_references: dedup_by_key(
            a_refs,
            |l| !b_ref_keys.contains(&reference_key(l)),
            reference_key,
        ),
    }
}

/// Keep items matching `keep`, deduplicated by `key` (first occurrence wins).
fn dedup_by_key<T, K: std::hash::Hash + Eq>(
    items: Vec<T>,
    keep: impl Fn(&T) -> bool,
    key: impl Fn(&T) -> K,
) -> Vec<T> {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|it| keep(it) && seen.insert(key(it)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InMemoryGraphStore, build_rust};

    fn store(files: &[(&str, &str)]) -> InMemoryGraphStore {
        let mut s = InMemoryGraphStore::new();
        for (path, src) in files {
            s.insert(path, build_rust(src));
        }
        s
    }

    #[test]
    fn identical_views_have_no_diff() {
        let a = store(&[("lib.rs", "fn a() {}\nfn b() { a(); }")]);
        let b = store(&[("lib.rs", "fn a() {}\nfn b() { a(); }")]);
        assert!(diff(&a, &b).is_empty());
    }

    #[test]
    fn reports_added_and_removed_symbols() {
        let a = store(&[("lib.rs", "fn keep() {}\nfn gone() {}")]);
        let b = store(&[("lib.rs", "fn keep() {}\nfn fresh() {}")]);
        let d = diff(&a, &b);

        let added: Vec<&str> = d
            .added_symbols
            .iter()
            .map(|l| l.item.name.as_str())
            .collect();
        let removed: Vec<&str> = d
            .removed_symbols
            .iter()
            .map(|l| l.item.name.as_str())
            .collect();
        assert_eq!(added, vec!["fresh"]);
        assert_eq!(removed, vec!["gone"]);
    }

    #[test]
    fn reports_added_and_removed_references() {
        let a = store(&[("lib.rs", "fn f() {}\nfn caller() { f(); }")]);
        // caller now calls g() instead of f().
        let b = store(&[("lib.rs", "fn f() {}\nfn g() {}\nfn caller() { g(); }")]);
        let d = diff(&a, &b);

        assert!(d.added_references.iter().any(|l| l.item.name == "g"));
        assert!(d.removed_references.iter().any(|l| l.item.name == "f"));
        // `g` is an added symbol; `f` still exists (not removed).
        assert!(d.added_symbols.iter().any(|l| l.item.name == "g"));
        assert!(!d.removed_symbols.iter().any(|l| l.item.name == "f"));
    }

    #[test]
    fn moving_a_symbol_across_files_is_add_plus_remove() {
        let a = store(&[("a.rs", "fn moved() {}"), ("b.rs", "")]);
        let b = store(&[("a.rs", ""), ("b.rs", "fn moved() {}")]);
        let d = diff(&a, &b);
        assert_eq!(d.added_symbols.len(), 1);
        assert_eq!(d.added_symbols[0].path, "b.rs");
        assert_eq!(d.removed_symbols.len(), 1);
        assert_eq!(d.removed_symbols[0].path, "a.rs");
    }

    #[test]
    fn line_shift_is_not_a_change() {
        let a = store(&[("lib.rs", "fn a() {}")]);
        // Same symbol, different line.
        let b = store(&[("lib.rs", "\n\nfn a() {}")]);
        assert!(diff(&a, &b).is_empty());
    }
}
