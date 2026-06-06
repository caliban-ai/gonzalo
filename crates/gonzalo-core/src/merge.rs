//! Merge strategies keyed by `MergeClass`. Used by `Sync` (M2) and by
//! callers resolving a `PutResult::Conflict`.

use crate::record::{Body, MergeClass};

/// The result of attempting an automatic merge of two divergent bodies.
#[derive(Clone, Debug, PartialEq, Eq)]
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
/// - `Structured`: deferred to M2 (returns `NeedsResolution` for now).
/// - `Opaque`: always `NeedsResolution`.
pub fn merge(class: MergeClass, base: &Body, ours: &Body, theirs: &Body) -> MergeOutcome {
    match class {
        MergeClass::AppendOnly => append_only_merge(base, ours, theirs),
        MergeClass::Structured | MergeClass::Opaque => MergeOutcome::NeedsResolution,
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
    bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect()
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
        assert_eq!(merge(MergeClass::Opaque, &b, &b, &b), MergeOutcome::NeedsResolution);
    }

    #[test]
    fn structured_needs_resolution_in_m1() {
        let b = body("x\n");
        assert_eq!(merge(MergeClass::Structured, &b, &b, &b), MergeOutcome::NeedsResolution);
    }
}
