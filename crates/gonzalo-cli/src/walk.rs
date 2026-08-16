//! Which files of a source tree enter a view.
//!
//! The indexer has two drivers — a full tree walk and a git-diff-driven
//! incremental pass — and they must agree on membership, or the same commit
//! yields different graphs depending on which driver ran. [`IndexFilter`] is the
//! single place that decides, so both call it.
//!
//! Three rules, in order of precedence:
//!
//! 1. An explicit `--include` prefix re-admits a path the built-in rules would
//!    drop (for repos that vendor code they genuinely want indexed).
//! 2. Dependency and build-output directories ([`SKIP_DIRS`]), hidden
//!    directories, and generated or minified files ([`SKIP_SUFFIXES`]) are
//!    dropped. These are path-only rules, so they apply to both drivers.
//! 3. `.gitignore` is honoured when the tree is a git repository. This is what
//!    makes a view reproducible: indexing build output means the graph depends
//!    on whether someone happened to run a build, so the same commit produces
//!    different graphs on different machines (#209). `--include` deliberately
//!    does *not* override it — see [`IndexFilter::is_indexable`].

use crate::Language;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Directory names that conventionally hold dependencies or build output rather
/// than source. Matched as whole path components, so `contests/` is unaffected
/// by `tests`-style entries and `rebuild/` is unaffected by `build`.
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "site-packages",
    "third_party",
];

/// Filename suffixes marking generated or minified artifacts. Minified bundles
/// are the worst case for a name-matched graph: they are single-letter
/// identifiers defined hundreds of times, which manufactures ambiguity that does
/// not exist in the real source (#207).
const SKIP_SUFFIXES: &[&str] = &[".min.js", ".min.css", ".bundle.js", "-lock.json"];

/// Decides which repo-relative paths are eligible for indexing.
#[derive(Debug, Clone, Default)]
pub struct IndexFilter {
    include: Vec<String>,
}

impl IndexFilter {
    /// Build a filter whose `include` entries are repo-relative path prefixes
    /// that override the built-in skip rules.
    pub fn new(include: &[String]) -> Self {
        Self {
            include: include
                .iter()
                .map(|p| p.trim_end_matches('/').replace('\\', "/"))
                .filter(|p| !p.is_empty())
                .collect(),
        }
    }

    /// Whether an explicit `--include` prefix re-admits `rel`. Matched on whole
    /// path components so `--include src` does not re-admit `srcgen/`.
    fn is_included(&self, rel: &str) -> bool {
        self.include.iter().any(|prefix| {
            rel == prefix
                || (rel.len() > prefix.len()
                    && rel.starts_with(prefix.as_str())
                    && rel.as_bytes()[prefix.len()] == b'/')
        })
    }

    /// Whether `rel` passes the path-only rules: dependency/output directories,
    /// hidden directories, and generated-file suffixes.
    ///
    /// This deliberately says nothing about `.gitignore`. The incremental driver
    /// does not need it — `git2`'s diff already omits ignored files — and the
    /// full walk applies it separately in [`source_files`], where a repository
    /// is open. Keeping gitignore out of the `--include` override also keeps the
    /// reproducibility guarantee intact: no flag can pull build output into a
    /// view.
    pub fn is_indexable(&self, rel: &str) -> bool {
        if self.is_included(rel) {
            return true;
        }
        if rel
            .split('/')
            .any(|part| part.starts_with('.') || SKIP_DIRS.contains(&part))
        {
            return false;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        !SKIP_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
    }

    /// Whether the walk must descend into directory `rel`.
    ///
    /// Wider than [`is_indexable`](Self::is_indexable): a directory that is
    /// itself skipped must still be entered when an `--include` path lives
    /// underneath it, or the override can never take effect —
    /// `--include vendor/mylib` is useless if `vendor/` is pruned first.
    pub fn should_descend(&self, rel: &str) -> bool {
        self.is_indexable(rel)
            || self.include.iter().any(|p| {
                p.len() > rel.len() && p.starts_with(rel) && p.as_bytes()[rel.len()] == b'/'
            })
    }
}

/// How many paths a walk declined to index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IgnoredCounts {
    /// Source files dropped by a rule (vendored, generated, or gitignored).
    pub files: usize,
    /// Directories not descended into at all. Their contents are *not* counted
    /// in `files`, so a pruned `node_modules` costs one entry here rather than
    /// a walk of everything inside it.
    pub dirs: usize,
}

/// Supported source files under `dir` with their [`Language`], sorted by path,
/// alongside a count of what was skipped.
///
/// Applies `filter` plus, when `dir` is a git repository, `.gitignore`.
pub fn source_files(
    dir: &Path,
    filter: &IndexFilter,
) -> Result<(Vec<(PathBuf, Language)>, IgnoredCounts)> {
    // Opened once for the whole walk; `is_path_ignored` is a pure query so the
    // repository is never mutated. A non-git tree simply gets no gitignore rules.
    let repo = git2::Repository::open(dir).ok();
    let mut out = Vec::new();
    let mut ignored = IgnoredCounts::default();
    source_files_inner(dir, dir, repo.as_ref(), filter, &mut out, &mut ignored)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((out, ignored))
}

/// Whether git considers `rel` ignored. Errors are treated as "not ignored" so a
/// malformed ignore file cannot silently empty a view.
fn git_ignores(repo: Option<&git2::Repository>, rel: &str) -> bool {
    repo.is_some_and(|r| r.is_path_ignored(Path::new(rel)).unwrap_or(false))
}

fn source_files_inner(
    root: &Path,
    dir: &Path,
    repo: Option<&git2::Repository>,
    filter: &IndexFilter,
    out: &mut Vec<(PathBuf, Language)>,
    ignored: &mut IgnoredCounts,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if ft.is_dir() {
            if !filter.should_descend(&rel) || git_ignores(repo, &rel) {
                ignored.dirs += 1;
                continue;
            }
            source_files_inner(root, &path, repo, filter, out, ignored)?;
        } else if ft.is_file() {
            let Some(language) = path
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Language::from_extension)
            else {
                continue; // not a source file at all — not "ignored", just irrelevant
            };
            if !filter.is_indexable(&rel) || git_ignores(repo, &rel) {
                ignored.files += 1;
                continue;
            }
            out.push((path, language));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn filter() -> IndexFilter {
        IndexFilter::default()
    }

    // ---- path-only rules (shared by both drivers) -------------------------

    #[test]
    fn indexes_ordinary_source_paths() {
        assert!(filter().is_indexable("crates/gonzalo-cli/src/lib.rs"));
        assert!(filter().is_indexable("main.rs"));
    }

    #[test]
    fn skips_build_and_dependency_directories() {
        for rel in [
            "target/debug/foo.rs",
            "node_modules/left-pad/index.js",
            "vendor/github.com/x/y.go",
            "dist/app.js",
            "build/generated.rs",
            "third_party/lib/a.c",
            "x/site-packages/pkg/mod.py",
        ] {
            assert!(!filter().is_indexable(rel), "{rel} should be skipped");
        }
    }

    #[test]
    fn skips_hidden_directories() {
        assert!(!filter().is_indexable(".git/config.rs"));
        assert!(!filter().is_indexable(".github/scripts/x.py"));
    }

    #[test]
    fn matches_directory_names_as_whole_components() {
        // Substring matches must not trigger: these are real source paths.
        assert!(filter().is_indexable("rebuild/main.rs"));
        assert!(filter().is_indexable("distribution/mod.rs"));
        assert!(filter().is_indexable("vendored_docs/notes.py"));
    }

    #[test]
    fn skips_minified_and_generated_files() {
        for rel in [
            "docs/guide/mermaid.min.js",
            "assets/site.min.css",
            "static/app.bundle.js",
            "package-lock.json",
        ] {
            assert!(!filter().is_indexable(rel), "{rel} should be skipped");
        }
    }

    #[test]
    fn does_not_confuse_a_minified_suffix_with_a_normal_name() {
        assert!(filter().is_indexable("src/mermaid.js"));
        assert!(filter().is_indexable("src/bundle.js"));
    }

    // ---- the --include override -------------------------------------------

    #[test]
    fn an_include_prefix_readmits_a_vendored_path() {
        let f = IndexFilter::new(&["vendor/mylib".to_string()]);
        assert!(f.is_indexable("vendor/mylib/core.go"));
        // Everything else under vendor/ stays out.
        assert!(!f.is_indexable("vendor/other/core.go"));
    }

    #[test]
    fn an_include_prefix_readmits_a_single_file() {
        let f = IndexFilter::new(&["docs/guide/mermaid.min.js".to_string()]);
        assert!(f.is_indexable("docs/guide/mermaid.min.js"));
        assert!(!f.is_indexable("docs/guide/other.min.js"));
    }

    #[test]
    fn an_include_prefix_matches_whole_components_only() {
        let f = IndexFilter::new(&["build/keep".to_string()]);
        assert!(f.is_indexable("build/keep/a.rs"));
        // `build/keepsake` merely starts with the prefix string.
        assert!(!f.is_indexable("build/keepsake/a.rs"));
    }

    #[test]
    fn a_trailing_slash_in_an_include_is_tolerated() {
        let f = IndexFilter::new(&["vendor/mylib/".to_string()]);
        assert!(f.is_indexable("vendor/mylib/core.go"));
    }

    // ---- the full walk ----------------------------------------------------

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    fn rels(files: &[(PathBuf, Language)], root: &Path) -> Vec<String> {
        files
            .iter()
            .map(|(p, _)| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_skips_vendored_files_in_a_plain_directory_tree() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "src/lib.rs", "fn a() {}");
        write(dir.path(), "docs/mermaid.min.js", "var a=1;");
        write(dir.path(), "node_modules/x/index.js", "var b=2;");

        let (files, ignored) = source_files(dir.path(), &filter()).unwrap();
        assert_eq!(rels(&files, dir.path()), vec!["src/lib.rs"]);
        assert_eq!(ignored.files, 1, "the .min.js");
        assert_eq!(ignored.dirs, 1, "node_modules pruned without descending");
    }

    #[test]
    fn walk_honours_gitignore_in_a_git_repository() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        write(dir.path(), ".gitignore", "docs/guide/book/\n");
        write(dir.path(), "src/lib.rs", "fn a() {}");
        // mdbook output: present on a machine that ran the build, absent otherwise.
        write(dir.path(), "docs/guide/book/highlight.js", "var a=1;");
        drop(repo);

        let (files, ignored) = source_files(dir.path(), &filter()).unwrap();
        assert_eq!(rels(&files, dir.path()), vec!["src/lib.rs"]);
        // Two pruned directories: the gitignored `docs/guide/book/`, and `.git`
        // itself via the hidden-directory rule.
        assert_eq!(ignored.dirs, 2);
    }

    #[test]
    fn walk_descends_into_a_skipped_directory_to_reach_an_include() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "vendor/mylib/core.js", "var a=1;");
        write(dir.path(), "vendor/other/core.js", "var b=2;");

        let f = IndexFilter::new(&["vendor/mylib".to_string()]);
        let (files, _) = source_files(dir.path(), &f).unwrap();
        // `vendor/` is entered only because the include lives under it; its
        // other children stay excluded.
        assert_eq!(rels(&files, dir.path()), vec!["vendor/mylib/core.js"]);
    }

    #[test]
    fn walk_produces_the_same_view_whether_or_not_build_output_exists() {
        // The reproducibility criterion from #209: same commit, same graph,
        // regardless of whether anyone ran the build.
        let build = |with_output: bool| {
            let dir = TempDir::new().unwrap();
            git2::Repository::init(dir.path()).unwrap();
            write(dir.path(), ".gitignore", "book/\n");
            write(dir.path(), "src/lib.rs", "fn a() {}");
            if with_output {
                write(dir.path(), "book/gen.js", "var a=1;");
                write(dir.path(), "book/other.js", "var b=2;");
            }
            let (files, _) = source_files(dir.path(), &filter()).unwrap();
            rels(&files, dir.path())
        };
        assert_eq!(build(true), build(false));
    }

    #[test]
    fn walk_still_works_without_a_repository() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.rs", "fn a() {}");
        write(dir.path(), "target/b.rs", "fn b() {}");
        let (files, _) = source_files(dir.path(), &filter()).unwrap();
        assert_eq!(rels(&files, dir.path()), vec!["a.rs"]);
    }

    #[test]
    fn walk_include_overrides_a_builtin_rule_but_not_gitignore() {
        let dir = TempDir::new().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        write(dir.path(), ".gitignore", "generated/\n");
        write(dir.path(), "vendor/mylib/core.js", "var a=1;");
        write(dir.path(), "generated/out.js", "var b=2;");

        let f = IndexFilter::new(&["vendor/mylib".to_string(), "generated".to_string()]);
        let (files, _) = source_files(dir.path(), &f).unwrap();
        // The vendored path is re-admitted; the gitignored one is not, because
        // reproducibility must not be defeatable by a flag.
        assert_eq!(rels(&files, dir.path()), vec!["vendor/mylib/core.js"]);
    }

    #[test]
    fn walk_ignores_files_with_no_known_language_without_counting_them() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "README.md", "# hi");
        write(dir.path(), "a.rs", "fn a() {}");
        let (files, ignored) = source_files(dir.path(), &filter()).unwrap();
        assert_eq!(rels(&files, dir.path()), vec!["a.rs"]);
        assert_eq!(ignored.files, 0, "a non-source file is not a skip");
    }
}
