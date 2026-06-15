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
/// - `AppendOnly`: union of lines from base→ours and base→theirs, in a
///   stable order (base lines, then new ours lines, then new theirs lines),
///   de-duplicated. Suits append-only topics and transcripts.
/// - `Structured`: field-level 3-way merge of JSON object bodies (see
///   [`structured_merge`]). Disjoint field edits auto-merge; the same field
///   changed differently on both sides is a genuine conflict.
/// - `Opaque`: always `NeedsResolution`.
pub fn merge(class: MergeClass, base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    match class {
        MergeClass::AppendOnly => append_only_merge(base, ours, theirs),
        MergeClass::Structured => structured_merge(base, ours, theirs),
        MergeClass::Opaque => MergeOutcome::NeedsResolution,
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

fn append_only_merge(base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    let base_lines: Vec<&[u8]> = split_lines(base.bytes());
    let ours_new = new_lines(&base_lines, ours.bytes());
    // Pass base_lines + ours_new as already-seen so theirs deduplicates against both
    let mut seen_for_theirs = Vec::with_capacity(base_lines.len() + ours_new.len());
    seen_for_theirs.extend_from_slice(&base_lines);
    seen_for_theirs.extend_from_slice(&ours_new);
    let theirs_new = new_lines(&seen_for_theirs, theirs.bytes());

    let mut out: Vec<u8> = Vec::new();
    for line in &base_lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for line in ours_new.iter().chain(theirs_new.iter()) {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    MergeOutcome::Merged(Body::Inline(out))
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .collect()
}

/// Lines present in `bytes` but not in `base_lines`, preserving order and
/// dropping duplicates.
fn new_lines<'a>(base_lines: &[&[u8]], bytes: &'a [u8]) -> Vec<&'a [u8]> {
    // TODO(m2): for large bodies (externalized sessions), switch this O(n^2)
    // linear scan to a HashSet-based membership check.
    let mut seen: Vec<&[u8]> = base_lines.to_vec();
    let mut out = Vec::new();
    for line in split_lines(bytes) {
        if !seen.contains(&line) {
            seen.push(line);
            out.push(line);
        }
    }
    out
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
    fn append_only_dedups_same_addition() {
        let base = body("a\n");
        let ours = body("a\nb\n");
        let theirs = body("a\nb\n");
        let MergeOutcome::Merged(m) = merge(MergeClass::AppendOnly, &base, &ours, &theirs) else {
            panic!("expected merge");
        };
        assert_eq!(m, body("a\nb\n"));
    }

    #[test]
    fn opaque_needs_resolution() {
        let b = body("x\n");
        assert_eq!(
            merge(MergeClass::Opaque, &b, &b, &b),
            MergeOutcome::NeedsResolution
        );
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
