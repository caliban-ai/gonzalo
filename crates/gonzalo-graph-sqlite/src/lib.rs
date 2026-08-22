//! A persistent, SQLite-backed [`GraphStore`] (ticket B).
//!
//! Resolves the scalable-backend spike in favour of SQLite (`rusqlite`,
//! bundled): FOSS/local, a natural fit for the sync [`GraphStore`] trait, and
//! able to persist a view built once at index time rather than re-assembling it
//! into memory on every query. Symbols and references live in two path-keyed
//! tables; `insert` replaces a path's rows transactionally, so re-indexing a
//! file never duplicates its symbols. `callees`/`impact` use the trait defaults.
//!
//! The `GraphStore` trait is infallible, so query methods `expect` on the
//! embedded database — a failure there is corruption/programmer error, not a
//! recoverable condition. The connection is held behind a `Mutex` (rusqlite's
//! `Connection` is `Send` but not `Sync`, and `GraphStore` requires `Sync`);
//! read concurrency via a connection pool is a follow-on.

use gonzalo_graph::{CodeGraph, GraphStore, Located, RefKind, Reference, Symbol};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS symbols (
    path       TEXT    NOT NULL,
    name       TEXT    NOT NULL,
    kind       TEXT    NOT NULL,
    start_line INTEGER NOT NULL,
    end_line   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS refs (
    path    TEXT    NOT NULL,
    name    TEXT    NOT NULL,
    from_fn TEXT,
    line    INTEGER NOT NULL,
    kind    TEXT    NOT NULL DEFAULT 'free'
);
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_path ON symbols(path);
CREATE INDEX IF NOT EXISTS idx_refs_name    ON refs(name);
CREATE INDEX IF NOT EXISTS idx_refs_from    ON refs(from_fn);
CREATE INDEX IF NOT EXISTS idx_refs_path    ON refs(path);
";

/// A [`GraphStore`] backed by a SQLite database.
pub struct SqliteGraphStore {
    conn: Mutex<Connection>,
}

impl SqliteGraphStore {
    /// Open (creating if absent, including parent directories) a file-backed
    /// store at `path`.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            // Best-effort: if this fails, `Connection::open` reports the real error.
            let _ = std::fs::create_dir_all(parent);
        }
        Self::init(Connection::open(path)?)
    }

    /// Open an ephemeral in-memory store.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Bring an existing database up to the current schema.
    ///
    /// `CREATE TABLE IF NOT EXISTS` leaves an older `refs` table untouched, so a
    /// database written before `refs.kind` existed needs the column added
    /// explicitly. Rows already there default to `free`, which is exactly the
    /// shape they were assumed to have (#223).
    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        let has_kind = conn
            .prepare("SELECT 1 FROM pragma_table_info('refs') WHERE name = 'kind'")?
            .exists([])?;
        if !has_kind {
            conn.execute_batch("ALTER TABLE refs ADD COLUMN kind TEXT NOT NULL DEFAULT 'free'")?;
        }
        Ok(())
    }

    /// Remove all rows for `path` (a file dropped from the view). Complements
    /// [`GraphStore::insert`], which replaces a path's rows.
    pub fn remove_path(&mut self, path: &str) {
        let guard = self.conn.lock().expect("connection poisoned");
        guard
            .execute("DELETE FROM symbols WHERE path = ?1", params![path])
            .expect("delete symbols for path");
        guard
            .execute("DELETE FROM refs WHERE path = ?1", params![path])
            .expect("delete refs for path");
    }
}

/// The database file for a view's graph under `graph_root`:
/// `<graph_root>/<repo>/<view_id>.db`, with `repo`/`view_id` made
/// filesystem-safe. Writer (indexer) and reader (server) must agree on this.
pub fn view_db_path(graph_root: &Path, repo: &str, view_id: &str) -> std::path::PathBuf {
    graph_root
        .join(fs_safe(repo))
        .join(format!("{}.db", fs_safe(view_id)))
}

/// Map an identifier to a single filesystem-safe path segment via the shared,
/// **injective** percent-style encoder ([`gonzalo_core::segment`]): the
/// unreserved set `[A-Za-z0-9_-]` survives verbatim and every other byte
/// (including `/` and `.`) becomes `%XX`. Because the map is injective,
/// distinct `(repo, view_id)` pairs can never collide onto one `.db` file (the
/// old lossy `_`-collapse let `org/repo` and `org_repo` share a path). Escaping
/// `.` and `/` also keeps a component from escaping `graph_root`, and since the
/// encoded `view_id` contains no literal `.`, the only `.` in the filename is
/// the trailing `.db` suffix. The mapping is a pure function so the writer
/// (indexer) and reader (server) always agree; the path is never parsed back
/// into `repo`/`view_id`, so no decode is needed here.
fn fs_safe(s: &str) -> String {
    gonzalo_core::segment(s)
}

/// A [`SymbolKind`](gonzalo_graph::SymbolKind) as stored TEXT (its serde form).
fn kind_to_text(sym: &Symbol) -> String {
    serde_json::to_string(&sym.kind).expect("SymbolKind serializes")
}

fn symbol_from_row(row: &rusqlite::Row, base: usize) -> rusqlite::Result<Symbol> {
    let name: String = row.get(base)?;
    let kind_text: String = row.get(base + 1)?;
    let start: i64 = row.get(base + 2)?;
    let end: i64 = row.get(base + 3)?;
    Ok(Symbol {
        name,
        kind: serde_json::from_str(&kind_text).expect("stored SymbolKind is valid"),
        start_line: start as usize,
        end_line: end as usize,
    })
}

impl GraphStore for SqliteGraphStore {
    fn insert(&mut self, path: &str, graph: CodeGraph) {
        let mut guard = self.conn.lock().expect("connection poisoned");
        let tx = guard.transaction().expect("begin transaction");
        tx.execute("DELETE FROM symbols WHERE path = ?1", params![path])
            .expect("clear symbols for path");
        tx.execute("DELETE FROM refs WHERE path = ?1", params![path])
            .expect("clear refs for path");
        for s in &graph.symbols {
            tx.execute(
                "INSERT INTO symbols (path, name, kind, start_line, end_line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    path,
                    s.name,
                    kind_to_text(s),
                    s.start_line as i64,
                    s.end_line as i64
                ],
            )
            .expect("insert symbol");
        }
        for r in &graph.references {
            tx.execute(
                "INSERT INTO refs (path, name, from_fn, line, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![path, r.name, r.from, r.line as i64, r.kind.as_str()],
            )
            .expect("insert reference");
        }
        tx.commit().expect("commit transaction");
    }

    fn symbols_in_file(&self, path: &str) -> Vec<Symbol> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare("SELECT name, kind, start_line, end_line FROM symbols WHERE path = ?1")
            .expect("prepare symbols_in_file");
        let rows = stmt
            .query_map(params![path], |row| symbol_from_row(row, 0))
            .expect("query symbols_in_file");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect symbols_in_file")
    }

    fn definitions(&self, name: &str) -> Vec<Located<Symbol>> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare(
                "SELECT path, name, kind, start_line, end_line FROM symbols
                 WHERE name = ?1 ORDER BY path",
            )
            .expect("prepare definitions");
        let rows = stmt
            .query_map(params![name], |row| {
                Ok(Located {
                    path: row.get(0)?,
                    item: symbol_from_row(row, 1)?,
                })
            })
            .expect("query definitions");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect definitions")
    }

    fn references_to(&self, name: &str) -> Vec<Located<Reference>> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare(
                "SELECT path, name, from_fn, line, kind FROM refs
                 WHERE name = ?1 ORDER BY path, line",
            )
            .expect("prepare references_to");
        let rows = stmt
            .query_map(params![name], |row| {
                Ok(Located {
                    path: row.get(0)?,
                    item: Reference {
                        name: row.get(1)?,
                        from: row.get::<_, Option<String>>(2)?,
                        line: row.get::<_, i64>(3)? as usize,
                        kind: RefKind::from_str_or_free(&row.get::<_, String>(4)?),
                    },
                })
            })
            .expect("query references_to");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect references_to")
    }

    fn callers_of(&self, name: &str) -> Vec<String> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare(
                "SELECT DISTINCT from_fn FROM refs
                 WHERE name = ?1 AND from_fn IS NOT NULL ORDER BY from_fn",
            )
            .expect("prepare callers_of");
        let rows = stmt
            .query_map(params![name], |row| row.get::<_, String>(0))
            .expect("query callers_of");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect callers_of")
    }

    fn callees(&self, name: &str) -> Vec<String> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare("SELECT DISTINCT name FROM refs WHERE from_fn = ?1 ORDER BY name")
            .expect("prepare callees");
        let rows = stmt
            .query_map(params![name], |row| row.get::<_, String>(0))
            .expect("query callees");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect callees")
    }

    fn all_symbols(&self) -> Vec<Located<Symbol>> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare("SELECT path, name, kind, start_line, end_line FROM symbols ORDER BY path")
            .expect("prepare all_symbols");
        let rows = stmt
            .query_map([], |row| {
                Ok(Located {
                    path: row.get(0)?,
                    item: symbol_from_row(row, 1)?,
                })
            })
            .expect("query all_symbols");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect all_symbols")
    }

    fn all_references(&self) -> Vec<Located<Reference>> {
        let guard = self.conn.lock().expect("connection poisoned");
        let mut stmt = guard
            .prepare("SELECT path, name, from_fn, line, kind FROM refs ORDER BY path, line")
            .expect("prepare all_references");
        let rows = stmt
            .query_map([], |row| {
                Ok(Located {
                    path: row.get(0)?,
                    item: Reference {
                        name: row.get(1)?,
                        from: row.get::<_, Option<String>>(2)?,
                        line: row.get::<_, i64>(3)? as usize,
                        kind: RefKind::from_str_or_free(&row.get::<_, String>(4)?),
                    },
                })
            })
            .expect("query all_references");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("collect all_references")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for #132: distinct `(repo, view)` pairs that collided under
    /// the old lossy `_`-collapse must now map to different db files. Under the
    /// old scheme both `("org/repo","main")` and `("org_repo","main")` produced
    /// `<root>/org_repo/main.db`; one view's graph would serve/overwrite the
    /// other. The injective encoder keeps them apart.
    #[test]
    fn view_db_path_is_injective_across_colliding_pairs() {
        let root = Path::new("/graphs");
        let a = view_db_path(root, "org/repo", "main");
        let b = view_db_path(root, "org_repo", "main");
        assert_ne!(a, b, "distinct repos must not collide onto one db file");

        // The `/` is percent-escaped, so `repo` stays a single non-escaping
        // segment and the filename's only `.` is the `.db` suffix.
        assert_eq!(a, Path::new("/graphs/org%2Frepo/main.db"));
        assert_eq!(b, Path::new("/graphs/org_repo/main.db"));
        assert!(a.extension().is_some_and(|e| e == "db"));

        // Collisions in the view_id component are likewise avoided.
        let c = view_db_path(root, "org/repo", "v1.0");
        let d = view_db_path(root, "org/repo", "v1_0");
        assert_ne!(c, d, "distinct views must not collide onto one db file");
    }
}
