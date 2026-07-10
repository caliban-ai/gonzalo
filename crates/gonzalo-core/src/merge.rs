//! Merge strategies keyed by `MergeClass`. Used by `Sync` (M2) and by
//! callers resolving a `PutResult::Conflict`.

use crate::record::{Body, MergeClass};

/// The result of attempting an automatic merge of two divergent bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
#[must_use = "a MergeOutcome may need caller resolution and must not be ignored"]
pub enum MergeOutcome {
    /// A merged body was produced automatically.
    Merged(Body),
    /// No safe automatic merge; the caller must resolve.
    NeedsResolution,
}

/// Attempt to merge `ours` and `theirs` given their common `base`,
/// according to `class`.
///
/// - `AppendOnly`: `ours` verbatim, followed by the tail of `theirs` past the
///   shared line-prefix of the two sides. A strict append-only union that
///   preserves committed content byte-exact — it never drops blank lines and
///   never dedups repeated lines (doing so is silent data loss, ADR 0005).
///   Suits append-only topics and session transcripts.
/// - `Structured`: field-level 3-way merge of JSON object bodies (see
///   [`structured_merge`]). Disjoint field edits auto-merge; the same field
///   changed differently on both sides is a genuine conflict.
/// - `Opaque`: always `NeedsResolution`.
/// - `Derived`: never `NeedsResolution`. The body is regenerable and views are
///   single-writer (ADR 0012), so a divergence is a rare race. `merge` receives
///   only bodies (no `Meta`), so it cannot compare `updated` timestamps; it
///   resolves the race deterministically in favor of side A (`ours`) — the
///   discarded side can be re-derived from source.
pub fn merge(class: MergeClass, base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    match class {
        MergeClass::AppendOnly => append_only_merge(base, ours, theirs),
        MergeClass::Structured => structured_merge(base, ours, theirs),
        MergeClass::Opaque => MergeOutcome::NeedsResolution,
        MergeClass::Derived => MergeOutcome::Merged(ours.clone()),
    }
}

/// Field-level 3-way merge of JSON bodies.
///
/// Each body is parsed as JSON. For every key, the standard 3-way rule applies:
/// if only one side changed it from `base`, take that side; if both changed it
/// the same way, take it; if both changed it differently, recurse when both are
/// objects, otherwise surface a conflict. Non-object roots, or bodies that
/// aren't valid JSON, fall back to `NeedsResolution` (the safe default — a
/// caller resolves rather than risk a wrong merge).
fn structured_merge(base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    let (Ok(base), Ok(ours), Ok(theirs)) = (
        serde_json::from_slice::<serde_json::Value>(base.bytes()),
        serde_json::from_slice::<serde_json::Value>(ours.bytes()),
        serde_json::from_slice::<serde_json::Value>(theirs.bytes()),
    ) else {
        return MergeOutcome::NeedsResolution;
    };
    match merge_value(&base, &ours, &theirs) {
        Some(merged) => match serde_json::to_vec(&merged) {
            Ok(bytes) => MergeOutcome::Merged(Body::Inline(bytes)),
            Err(_) => MergeOutcome::NeedsResolution,
        },
        None => MergeOutcome::NeedsResolution,
    }
}

/// 3-way merge of a single JSON value. Returns `None` on a genuine conflict.
///
/// Scalars and arrays are merged atomically (by equality); objects are merged
/// key-by-key via [`merge_field`], which models presence so a key deleted on one
/// side is honored rather than turned into `null`.
fn merge_value(
    base: &serde_json::Value,
    ours: &serde_json::Value,
    theirs: &serde_json::Value,
) -> Option<serde_json::Value> {
    // Both sides agree (covers "neither changed" and "both changed identically").
    if ours == theirs {
        return Some(ours.clone());
    }
    // Only one side diverged from base — take the side that changed.
    if ours == base {
        return Some(theirs.clone());
    }
    if theirs == base {
        return Some(ours.clone());
    }
    // Both changed differently. Recurse only when all three are objects;
    // anything else (scalars, arrays, type changes) is a genuine conflict.
    match (base.as_object(), ours.as_object(), theirs.as_object()) {
        (Some(base_obj), Some(ours_obj), Some(theirs_obj)) => {
            let mut out = serde_json::Map::new();
            let mut keys: Vec<&String> = base_obj
                .keys()
                .chain(ours_obj.keys())
                .chain(theirs_obj.keys())
                .collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                // `?` propagates a conflict; `Some(v)` keeps the key, `None`
                // drops it (deleted on the winning side).
                if let Some(v) =
                    merge_field(base_obj.get(key), ours_obj.get(key), theirs_obj.get(key))?
                {
                    out.insert(key.clone(), v);
                }
            }
            Some(serde_json::Value::Object(out))
        }
        _ => None,
    }
}

/// 3-way merge of one object field, modeling presence as `Option` (inner
/// `None` = the key is absent/deleted). The outer `Option` is the merge result:
/// `None` = genuine conflict; `Some(Some(v))` = keep `v`; `Some(None)` = drop
/// the key.
fn merge_field(
    base: Option<&serde_json::Value>,
    ours: Option<&serde_json::Value>,
    theirs: Option<&serde_json::Value>,
) -> Option<Option<serde_json::Value>> {
    // Both sides agree on presence + value.
    if ours == theirs {
        return Some(ours.cloned());
    }
    // Only one side changed relative to base — take the changed side.
    if ours == base {
        return Some(theirs.cloned());
    }
    if theirs == base {
        return Some(ours.cloned());
    }
    // Both changed differently: recurse if both are still present objects,
    // else it's a genuine conflict (incl. modify/delete).
    match (ours, theirs) {
        (Some(o), Some(t)) => {
            let b = base.unwrap_or(&serde_json::Value::Null);
            merge_value(b, o, t).map(Some)
        }
        _ => None,
    }
}

/// Append-only 3-way merge that never loses committed content.
///
/// By the append-only invariant, `base` is a line-prefix of both sides, so the
/// shared committed content is exactly the longest common line-prefix of `ours`
/// and `theirs` (computing it from the two sides rather than `base` also makes
/// the merge correct under the empty base that [`crate::sync`] passes). The
/// result is `ours` byte-for-byte, followed by the lines of `theirs` beyond that
/// shared prefix — its divergent appended tail.
///
/// Crucially this does NOT split-and-filter or dedup: blank lines and legitimate
/// repeated lines inside committed content survive verbatim on both sides. `base`
/// is unused because emitting `ours` verbatim already preserves it.
fn append_only_merge(_base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    let ours_lines = split_lines(ours.bytes());
    let theirs_lines = split_lines(theirs.bytes());
    // Compare by line *content* (ignoring a trailing `\n`) so a non-newline-
    // terminated final line still counts as shared with a newline-terminated
    // peer — otherwise `"a\nb"` vs `"a\nb\nc\n"` would treat `b` as divergent
    // and fuse it with `theirs`' `b` into `bb`.
    let shared = ours_lines
        .iter()
        .zip(theirs_lines.iter())
        .take_while(|(o, t)| strip_nl(o) == strip_nl(t))
        .count();

    let mut out: Vec<u8> = ours.bytes().to_vec();
    let tail = &theirs_lines[shared..];
    // If `ours` didn't end in a newline, separate its final line from `theirs`'
    // appended tail so the two distinct lines don't fuse into one.
    if !tail.is_empty() && out.last().is_some_and(|&b| b != b'\n') {
        out.push(b'\n');
    }
    for line in tail {
        out.extend_from_slice(line);
    }
    MergeOutcome::Merged(Body::Inline(out))
}

/// A line slice without its trailing `\n`, for newline-insensitive comparison.
fn strip_nl(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// Split `bytes` into lines, each slice keeping its trailing `\n` (the final
/// line has none iff `bytes` doesn't end in `\n`). Concatenating the result
/// reproduces `bytes` exactly, so blank and repeated lines round-trip verbatim.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            lines.push(&bytes[start..=i]);
            start = i + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(s: &str) -> Body {
        Body::Inline(s.as_bytes().to_vec())
    }

    #[test]
    fn append_only_unions_disjoint_additions() {
        let base = body("a\n");
        let ours = body("a\nb\n");
        let theirs = body("a\nc\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\nc\n"));
    }

    #[test]
    fn append_only_identical_sides_yield_ours() {
        // When both sides hold the same content, the divergent tail of `theirs`
        // is empty, so the merge is exactly `ours` — no duplication.
        let base = body("a\n");
        let ours = body("a\nb\n");
        let theirs = body("a\nb\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\n"));
    }

    #[test]
    fn append_only_appends_theirs_tail_and_preserves_repeats() {
        // Corrected semantics (was `append_only_dedups_new_lines_...`): the merge
        // is `ours` verbatim followed by the tail of `theirs` past the shared
        // line-prefix. Repeated lines that are genuinely part of committed
        // content are NOT deduped — losing them would be silent data loss.
        // Shared prefix here is "a\nb\n"; ours keeps its "c\nc\n" repeat and
        // theirs contributes only its divergent tail "e\nf\n".
        let base = body("a\nb\n");
        let ours = body("a\nb\nc\nc\nd\n");
        let theirs = body("a\nb\ne\nf\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\nc\nc\nd\ne\nf\n"));
    }

    #[test]
    fn append_only_preserves_blank_and_duplicate_lines() {
        // Regression for #133: with an empty base (what `sync` passes), a body
        // whose committed content legitimately contains a blank line and a
        // repeated line must round-trip WITHOUT losing the blank line or
        // collapsing the repeat — on both the shared prefix and the appended
        // tails. Shared prefix: "a\n\nyes\nyes\n"; ours appends "from_a\n",
        // theirs appends "from_b\n".
        let base = body("");
        let ours = body("a\n\nyes\nyes\nfrom_a\n");
        let theirs = body("a\n\nyes\nyes\nfrom_b\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\n\nyes\nyes\nfrom_a\nfrom_b\n"));
    }

    #[test]
    fn append_only_handles_non_newline_terminated_ours() {
        // `ours` has no trailing newline on its final line `b`. The last line
        // must be recognized as shared with `theirs`' `b\n` (not fused into
        // `bb`), and `theirs`' divergent tail appended cleanly on its own line.
        let base = body("");
        let ours = body("a\nb");
        let theirs = body("a\nb\nc\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\nc\n"));

        // And when the final line genuinely diverges, it is preserved (not
        // fused) with a separating newline before theirs' tail.
        let ours2 = body("a\nx");
        let theirs2 = body("a\ny\nz\n");
        let MergeOutcome::Merged(m2) = merge(MergeClass::AppendOnly, &base, &ours2, &theirs2)
        else {
            panic!("expected merge");
        };
        assert_eq!(m2, body("a\nx\ny\nz\n"));
    }

    #[test]
    fn opaque_needs_resolution() {
        let b = body("x\n");
        assert_eq!(
            merge(MergeClass::Opaque, &b, &b, &b),
            MergeOutcome::NeedsResolution
        );
    }

    #[test]
    fn derived_takes_side_a_deterministically() {
        // Derived bodies are regenerable (e.g. per-view code-graph manifests),
        // so reconciliation never surfaces a conflict. `merge` has no access to
        // `Meta`, so it cannot honor last-writer-wins by timestamp; it resolves
        // deterministically in favor of side A (`ours`). The choice is purely
        // positional — swapping the arguments picks the other side — and the
        // discarded side can be re-derived from source.
        let base = body("base");
        let ours = body("ours-version");
        let theirs = body("theirs-version");
        let MergeOutcome::Merged(m) = merge(MergeClass::Derived, &base, &ours, &theirs) else {
            panic!("Derived must merge, never NeedsResolution");
        };
        assert_eq!(m, ours);
        // Purely positional: whichever body is side A wins.
        let MergeOutcome::Merged(swapped) = merge(MergeClass::Derived, &base, &theirs, &ours)
        else {
            panic!("Derived must merge, never NeedsResolution");
        };
        assert_eq!(swapped, theirs);
    }

    /// Run a structured merge over JSON string inputs and return the parsed
    /// merged value, panicking if the merge needed resolution.
    fn structured(base: &str, ours: &str, theirs: &str) -> serde_json::Value {
        match merge(
            MergeClass::Structured,
            &body(base),
            &body(ours),
            &body(theirs),
        ) {
            MergeOutcome::Merged(b) => serde_json::from_slice(b.bytes()).unwrap(),
            MergeOutcome::NeedsResolution => panic!("expected a merge, got NeedsResolution"),
        }
    }

    fn structured_conflicts(base: &str, ours: &str, theirs: &str) -> bool {
        matches!(
            merge(
                MergeClass::Structured,
                &body(base),
                &body(ours),
                &body(theirs),
            ),
            MergeOutcome::NeedsResolution
        )
    }

    #[test]
    fn structured_merges_disjoint_field_edits() {
        // ours changes `name`, theirs changes `content` — both survive.
        let merged = structured(
            r#"{"name":"a","content":"x"}"#,
            r#"{"name":"b","content":"x"}"#,
            r#"{"name":"a","content":"y"}"#,
        );
        assert_eq!(merged, serde_json::json!({"name":"b","content":"y"}));
    }

    #[test]
    fn structured_takes_the_only_changed_side() {
        // Only theirs changed a field; ours is identical to base.
        let merged = structured(r#"{"k":1,"j":2}"#, r#"{"k":1,"j":2}"#, r#"{"k":9,"j":2}"#);
        assert_eq!(merged, serde_json::json!({"k":9,"j":2}));
    }

    #[test]
    fn structured_adds_new_keys_from_both_sides() {
        let merged = structured(r#"{"a":1}"#, r#"{"a":1,"b":2}"#, r#"{"a":1,"c":3}"#);
        assert_eq!(merged, serde_json::json!({"a":1,"b":2,"c":3}));
    }

    #[test]
    fn structured_conflicts_on_same_field_changed_differently() {
        assert!(structured_conflicts(
            r#"{"k":1}"#,
            r#"{"k":2}"#,
            r#"{"k":3}"#,
        ));
    }

    #[test]
    fn structured_merges_nested_objects() {
        // Disjoint edits within a nested object merge field-by-field.
        let merged = structured(
            r#"{"meta":{"a":1,"b":2}}"#,
            r#"{"meta":{"a":9,"b":2}}"#,
            r#"{"meta":{"a":1,"b":8}}"#,
        );
        assert_eq!(merged, serde_json::json!({"meta":{"a":9,"b":8}}));
    }

    #[test]
    fn structured_conflicts_on_nested_same_field() {
        assert!(structured_conflicts(
            r#"{"meta":{"a":1}}"#,
            r#"{"meta":{"a":2}}"#,
            r#"{"meta":{"a":3}}"#,
        ));
    }

    #[test]
    fn structured_honors_one_sided_deletion() {
        // ours deletes `b`, theirs leaves it untouched → deleted.
        let merged = structured(r#"{"a":1,"b":2}"#, r#"{"a":1}"#, r#"{"a":1,"b":2}"#);
        assert_eq!(merged, serde_json::json!({"a":1}));
    }

    #[test]
    fn structured_conflicts_on_modify_delete() {
        // ours deletes `b`, theirs modifies it → genuine conflict.
        assert!(structured_conflicts(
            r#"{"a":1,"b":2}"#,
            r#"{"a":1}"#,
            r#"{"a":1,"b":9}"#,
        ));
    }

    #[test]
    fn structured_arrays_merge_atomically() {
        // Identical array edit on both sides → fine.
        let merged = structured(r#"{"xs":[1]}"#, r#"{"xs":[1,2]}"#, r#"{"xs":[1,2]}"#);
        assert_eq!(merged, serde_json::json!({"xs":[1,2]}));
        // Divergent array edits → conflict (atomic).
        assert!(structured_conflicts(
            r#"{"xs":[1]}"#,
            r#"{"xs":[1,2]}"#,
            r#"{"xs":[1,3]}"#,
        ));
    }

    #[test]
    fn structured_non_json_falls_back_to_needs_resolution() {
        assert!(structured_conflicts("not json", "also not", "nope"));
    }

    #[test]
    fn structured_non_object_root_conflict_needs_resolution() {
        // Scalar roots changed differently → NeedsResolution (no field structure).
        assert!(structured_conflicts("1", "2", "3"));
    }
}
