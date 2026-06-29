use crate::error::{AppError, AppResult};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worktree {
    pub id: i64,
    pub repo_id: i64,
    pub branch: String,
    pub path: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub last_used: Option<i64>,
    /// Claude `--permission-mode` value passed at session start.
    /// `"bypassPermissions"` (default) → auto-approve everything;
    /// `"default"` → Claude prompts as usual. Any other value Claude accepts
    /// (acceptEdits, plan, …) is also stored verbatim and forwarded.
    pub permission_mode: String,
    /// `"worktree"` (default) for a normal git worktree, or `"orchestrator"`
    /// for a repo-less orchestrator session that lives in a Flock scratch dir
    /// and directs a fleet of agents across repos.
    pub kind: String,
    /// The orchestrator worktree that spawned this one, if any. NULL for
    /// user-created worktrees and for orchestrators themselves. ON DELETE SET
    /// NULL so removing an orchestrator orphans (but never kills) its fleet.
    pub parent_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: i64,
    pub repo_id: i64,
    pub prompt: String,
    /// Schedule spec: `@every <N>{m,h,d}` or `HH:MM` (daily, local time).
    pub spec: String,
    pub title: Option<String>,
    pub enabled: bool,
    pub last_run: Option<i64>,
    pub next_run: i64,
    pub created_at: i64,
}

/// A knowledge-base search hit (ranked FTS5 match with a highlighted snippet).
#[derive(Debug, Clone, Serialize)]
pub struct KbHit {
    pub path: String,
    pub title: String,
    pub snippet: String,
}

/// A full knowledge-base document, as returned by `kb_read`.
#[derive(Debug, Clone, Serialize)]
pub struct KbDoc {
    pub path: String,
    pub title: String,
    pub body: String,
    pub tags: Vec<String>,
    pub word_count: i64,
    pub modified_at: i64,
}

/// A directory-listing entry (no body), as returned by `kb_list`.
#[derive(Debug, Clone, Serialize)]
pub struct KbListItem {
    pub path: String,
    pub title: String,
    pub word_count: i64,
}

pub const DEFAULT_PERMISSION_MODE: &str = "bypassPermissions";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    pub fn open() -> AppResult<Self> {
        let dir = dirs::data_local_dir()
            .ok_or_else(|| AppError::msg("no data local dir"))?
            .join("Flock");
        std::fs::create_dir_all(&dir)?;
        Self::open_at(&dir.join("flock.db"))
    }

    /// Open (and migrate) the DB at an explicit path. `open()` is this against
    /// the data dir; tests use it with a temp file.
    pub fn open_at(path: &std::path::Path) -> AppResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;

            CREATE TABLE IF NOT EXISTS repos (
              id         INTEGER PRIMARY KEY,
              name       TEXT NOT NULL,
              path       TEXT NOT NULL UNIQUE,
              created_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS worktrees (
              id              INTEGER PRIMARY KEY,
              repo_id         INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
              branch          TEXT NOT NULL,
              path            TEXT NOT NULL UNIQUE,
              title           TEXT,
              created_at      INTEGER NOT NULL,
              last_used       INTEGER,
              permission_mode TEXT NOT NULL DEFAULT 'bypassPermissions',
              kind            TEXT NOT NULL DEFAULT 'worktree',
              parent_id       INTEGER REFERENCES worktrees(id) ON DELETE SET NULL
            );

            CREATE INDEX IF NOT EXISTS idx_worktrees_repo ON worktrees(repo_id);

            CREATE TABLE IF NOT EXISTS schedules (
              id         INTEGER PRIMARY KEY,
              repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
              prompt     TEXT NOT NULL,
              spec       TEXT NOT NULL,
              title      TEXT,
              enabled    INTEGER NOT NULL DEFAULT 1,
              last_run   INTEGER,
              next_run   INTEGER NOT NULL,
              created_at INTEGER NOT NULL
            );

            -- Knowledge base: an FTS5 index over an Obsidian vault (the vault on
            -- disk is the source of truth; this index is rebuilt from it). The
            -- UNINDEXED `path` is the vault-relative file path / identity.
            CREATE VIRTUAL TABLE IF NOT EXISTS kb_documents USING fts5(
              path UNINDEXED, title, body, tags, tokenize = 'porter unicode61'
            );
            CREATE TABLE IF NOT EXISTS kb_metadata (
              path        TEXT PRIMARY KEY,
              modified_at INTEGER NOT NULL,
              ingested_at INTEGER NOT NULL,
              word_count  INTEGER NOT NULL DEFAULT 0
            );

            -- Legacy: earlier versions of Flock persisted PTY scrollback blobs
            -- here. Sessions are now owned by tmux, so this table is dead.
            DROP TABLE IF EXISTS sessions;
            "#,
        )?;
        // Defensive ALTERs for DBs that predate a column. SQLite has no IF NOT
        // EXISTS for ADD COLUMN; the error is fine to swallow — it only fires
        // when the column is already there.
        let _ = conn.execute(
            "ALTER TABLE worktrees ADD COLUMN permission_mode TEXT NOT NULL DEFAULT 'bypassPermissions'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE worktrees ADD COLUMN kind TEXT NOT NULL DEFAULT 'worktree'",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE worktrees ADD COLUMN parent_id INTEGER REFERENCES worktrees(id) ON DELETE SET NULL",
            [],
        );
        // Index on parent_id must come AFTER the defensive ALTER above: on a DB
        // that predates the column, creating it inside the schema batch fails
        // (no such column) before the ALTER ever runs.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_worktrees_parent ON worktrees(parent_id);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn c(&self) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| AppError::msg("db mutex poisoned"))
    }

    // --- Repos ---

    pub fn insert_repo(&self, name: &str, path: &str) -> AppResult<Repo> {
        let c = self.c()?;
        c.execute(
            "INSERT INTO repos (name, path, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET name=excluded.name",
            params![name, path, now()],
        )?;
        let id = c.query_row(
            "SELECT id FROM repos WHERE path = ?1",
            params![path],
            |r| r.get::<_, i64>(0),
        )?;
        drop(c);
        self.get_repo(id)
    }

    pub fn get_repo(&self, id: i64) -> AppResult<Repo> {
        let c = self.c()?;
        let r = c.query_row(
            "SELECT id, name, path, created_at FROM repos WHERE id = ?1",
            params![id],
            |row| {
                Ok(Repo {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    created_at: row.get(3)?,
                })
            },
        )?;
        Ok(r)
    }

    pub fn list_repos(&self) -> AppResult<Vec<Repo>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, name, path, created_at FROM repos ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(Repo {
                id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete_repo(&self, id: i64) -> AppResult<()> {
        self.c()?
            .execute("DELETE FROM repos WHERE id = ?1", params![id])?;
        Ok(())
    }

    // --- Worktrees ---

    #[allow(clippy::too_many_arguments)]
    pub fn insert_worktree(
        &self,
        repo_id: i64,
        branch: &str,
        path: &str,
        title: Option<&str>,
        permission_mode: &str,
        kind: &str,
        parent_id: Option<i64>,
    ) -> AppResult<Worktree> {
        let c = self.c()?;
        c.execute(
            "INSERT INTO worktrees (repo_id, branch, path, title, created_at, permission_mode, kind, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET branch=excluded.branch, title=excluded.title",
            params![repo_id, branch, path, title, now(), permission_mode, kind, parent_id],
        )?;
        let id = c.query_row(
            "SELECT id FROM worktrees WHERE path = ?1",
            params![path],
            |r| r.get::<_, i64>(0),
        )?;
        drop(c);
        self.get_worktree(id)
    }

    fn row_to_worktree(row: &rusqlite::Row<'_>) -> rusqlite::Result<Worktree> {
        Ok(Worktree {
            id: row.get(0)?,
            repo_id: row.get(1)?,
            branch: row.get(2)?,
            path: row.get(3)?,
            title: row.get(4)?,
            created_at: row.get(5)?,
            last_used: row.get(6)?,
            permission_mode: row.get(7)?,
            kind: row.get(8)?,
            parent_id: row.get(9)?,
        })
    }

    pub fn get_worktree(&self, id: i64) -> AppResult<Worktree> {
        let c = self.c()?;
        let w = c.query_row(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode, kind, parent_id
             FROM worktrees WHERE id = ?1",
            params![id],
            Self::row_to_worktree,
        )?;
        Ok(w)
    }

    pub fn list_worktrees(&self, repo_id: i64) -> AppResult<Vec<Worktree>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode, kind, parent_id
             FROM worktrees WHERE repo_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![repo_id], Self::row_to_worktree)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every worktree across all repos. Used to render orchestrators (filter
    /// kind='orchestrator') and their fleets (filter by parent_id) without one
    /// query per repo.
    pub fn list_all_worktrees(&self) -> AppResult<Vec<Worktree>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode, kind, parent_id
             FROM worktrees ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_worktree)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// The fleet of an orchestrator: every worktree whose parent_id points at
    /// it. Used to cascade a teardown when the orchestrator is removed.
    pub fn list_children(&self, parent_id: i64) -> AppResult<Vec<Worktree>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode, kind, parent_id
             FROM worktrees WHERE parent_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![parent_id], Self::row_to_worktree)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_worktree_permission_mode(&self, id: i64, mode: &str) -> AppResult<()> {
        self.c()?.execute(
            "UPDATE worktrees SET permission_mode = ?1 WHERE id = ?2",
            params![mode, id],
        )?;
        Ok(())
    }

    pub fn update_worktree_title(&self, id: i64, title: &str) -> AppResult<()> {
        self.c()?.execute(
            "UPDATE worktrees SET title = ?1 WHERE id = ?2",
            params![title, id],
        )?;
        Ok(())
    }

    pub fn delete_worktree(&self, id: i64) -> AppResult<()> {
        self.c()?
            .execute("DELETE FROM worktrees WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn touch_worktree(&self, id: i64) -> AppResult<()> {
        self.c()?.execute(
            "UPDATE worktrees SET last_used = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
        Ok(())
    }

    // --- Schedules ---

    pub fn insert_schedule(
        &self,
        repo_id: i64,
        prompt: &str,
        spec: &str,
        title: Option<&str>,
        next_run: i64,
    ) -> AppResult<Schedule> {
        let c = self.c()?;
        c.execute(
            "INSERT INTO schedules (repo_id, prompt, spec, title, enabled, next_run, created_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            params![repo_id, prompt, spec, title, next_run, now()],
        )?;
        let id = c.last_insert_rowid();
        drop(c);
        self.get_schedule(id)
    }

    fn row_to_schedule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Schedule> {
        Ok(Schedule {
            id: row.get(0)?,
            repo_id: row.get(1)?,
            prompt: row.get(2)?,
            spec: row.get(3)?,
            title: row.get(4)?,
            enabled: row.get::<_, i64>(5)? != 0,
            last_run: row.get(6)?,
            next_run: row.get(7)?,
            created_at: row.get(8)?,
        })
    }

    pub fn get_schedule(&self, id: i64) -> AppResult<Schedule> {
        let c = self.c()?;
        let s = c.query_row(
            "SELECT id, repo_id, prompt, spec, title, enabled, last_run, next_run, created_at
             FROM schedules WHERE id = ?1",
            params![id],
            Self::row_to_schedule,
        )?;
        Ok(s)
    }

    pub fn list_schedules(&self) -> AppResult<Vec<Schedule>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, prompt, spec, title, enabled, last_run, next_run, created_at
             FROM schedules ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_schedule)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn set_schedule_enabled(&self, id: i64, enabled: bool) -> AppResult<()> {
        self.c()?.execute(
            "UPDATE schedules SET enabled = ?1 WHERE id = ?2",
            params![enabled as i64, id],
        )?;
        Ok(())
    }

    /// Stamp a fire: record when it ran and when it should next run.
    pub fn mark_schedule_run(&self, id: i64, last_run: i64, next_run: i64) -> AppResult<()> {
        self.c()?.execute(
            "UPDATE schedules SET last_run = ?1, next_run = ?2 WHERE id = ?3",
            params![last_run, next_run, id],
        )?;
        Ok(())
    }

    pub fn delete_schedule(&self, id: i64) -> AppResult<()> {
        self.c()?
            .execute("DELETE FROM schedules WHERE id = ?1", params![id])?;
        Ok(())
    }

    // --- Knowledge base ---

    /// Insert or replace a document, keeping the FTS index and metadata in sync.
    /// `path` is the vault-relative identity; `tags` is a space-separated string.
    pub fn kb_upsert(
        &self,
        path: &str,
        title: &str,
        body: &str,
        tags: &str,
        modified_at: i64,
        word_count: i64,
    ) -> AppResult<()> {
        let mut c = self.c()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM kb_documents WHERE path = ?1", params![path])?;
        tx.execute(
            "INSERT INTO kb_documents (path, title, body, tags) VALUES (?1, ?2, ?3, ?4)",
            params![path, title, body, tags],
        )?;
        tx.execute(
            "INSERT INTO kb_metadata (path, modified_at, ingested_at, word_count)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET
               modified_at=excluded.modified_at,
               ingested_at=excluded.ingested_at,
               word_count=excluded.word_count",
            params![path, modified_at, now(), word_count],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn kb_delete(&self, path: &str) -> AppResult<()> {
        let mut c = self.c()?;
        let tx = c.transaction()?;
        tx.execute("DELETE FROM kb_documents WHERE path = ?1", params![path])?;
        tx.execute("DELETE FROM kb_metadata WHERE path = ?1", params![path])?;
        tx.commit()?;
        Ok(())
    }

    /// FTS5 search. `query` must already be sanitized into a valid MATCH
    /// expression (see `kb::sanitize_query`).
    pub fn kb_search(&self, query: &str, limit: i64) -> AppResult<Vec<KbHit>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT path, title, snippet(kb_documents, 2, '[', ']', '…', 12)
             FROM kb_documents WHERE kb_documents MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |row| {
            Ok(KbHit {
                path: row.get(0)?,
                title: row.get(1)?,
                snippet: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn kb_get(&self, path: &str) -> AppResult<KbDoc> {
        let c = self.c()?;
        let doc = c.query_row(
            "SELECT d.path, d.title, d.body, d.tags,
                    COALESCE(m.word_count, 0), COALESCE(m.modified_at, 0)
             FROM kb_documents d LEFT JOIN kb_metadata m ON m.path = d.path
             WHERE d.path = ?1",
            params![path],
            |row| {
                let tags: String = row.get(3)?;
                Ok(KbDoc {
                    path: row.get(0)?,
                    title: row.get(1)?,
                    body: row.get(2)?,
                    tags: tags.split_whitespace().map(|s| s.to_string()).collect(),
                    word_count: row.get(4)?,
                    modified_at: row.get(5)?,
                })
            },
        )?;
        Ok(doc)
    }

    pub fn kb_list(&self, prefix: Option<&str>, limit: i64) -> AppResult<Vec<KbListItem>> {
        let c = self.c()?;
        let like = format!("{}%", prefix.unwrap_or(""));
        let mut stmt = c.prepare(
            "SELECT d.path, d.title, COALESCE(m.word_count, 0)
             FROM kb_documents d LEFT JOIN kb_metadata m ON m.path = d.path
             WHERE d.path LIKE ?1 ORDER BY d.path LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like, limit], |row| {
            Ok(KbListItem {
                path: row.get(0)?,
                title: row.get(1)?,
                word_count: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// path → mtime for every indexed doc — drives the incremental re-scan
    /// (skip files whose mtime is unchanged) and prune of deleted files.
    pub fn kb_metadata_map(&self) -> AppResult<std::collections::HashMap<String, i64>> {
        let c = self.c()?;
        let mut stmt = c.prepare("SELECT path, modified_at FROM kb_metadata")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        let mut map = std::collections::HashMap::new();
        for r in rows {
            let (p, m) = r?;
            map.insert(p, m);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::Db;
    use rusqlite::{params, Connection};

    fn temp_db() -> Db {
        // A process-wide counter guarantees a distinct path per call: two tests
        // on different threads can read the same nanosecond clock, and a shared
        // path means the second `open_at` hits "database is locked".
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut p = std::env::temp_dir();
        let uniq = format!(
            "flock-test-{}-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        p.push(uniq);
        Db::open_at(&p).expect("open temp db")
    }

    #[test]
    fn worktree_kind_and_parent_roundtrip() {
        let db = temp_db();
        let repo = db.insert_repo("acme", "/tmp/acme").unwrap();
        let orch = db
            .insert_worktree(repo.id, "kyoto", "/tmp/orch", None, "bypassPermissions", "orchestrator", None)
            .unwrap();
        assert_eq!(orch.kind, "orchestrator");
        assert_eq!(orch.parent_id, None);

        let child = db
            .insert_worktree(
                repo.id,
                "flock/petra",
                "/tmp/child",
                Some("a child"),
                "bypassPermissions",
                "worktree",
                Some(orch.id),
            )
            .unwrap();
        assert_eq!(child.kind, "worktree");
        assert_eq!(child.parent_id, Some(orch.id));

        // Reads see the persisted columns.
        let got = db.get_worktree(child.id).unwrap();
        assert_eq!(got.parent_id, Some(orch.id));

        let all = db.list_all_worktrees().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|w| w.id == orch.id && w.kind == "orchestrator"));
    }

    #[test]
    fn deleting_orchestrator_orphans_its_fleet() {
        let db = temp_db();
        let repo = db.insert_repo("acme", "/tmp/acme2").unwrap();
        let orch = db
            .insert_worktree(repo.id, "lima", "/tmp/orch2", None, "bypassPermissions", "orchestrator", None)
            .unwrap();
        let child = db
            .insert_worktree(repo.id, "flock/oslo", "/tmp/child2", None, "bypassPermissions", "worktree", Some(orch.id))
            .unwrap();

        // ON DELETE SET NULL: the child survives, just loses the link.
        db.delete_worktree(orch.id).unwrap();
        let got = db.get_worktree(child.id).unwrap();
        assert_eq!(got.parent_id, None);
        assert!(db.get_worktree(orch.id).is_err());
    }

    #[test]
    fn list_children_returns_only_the_fleet() {
        let db = temp_db();
        let repo = db.insert_repo("acme", "/tmp/acme3").unwrap();
        let orch = db
            .insert_worktree(repo.id, "cairo", "/tmp/orch3", None, "bypassPermissions", "orchestrator", None)
            .unwrap();
        let c1 = db
            .insert_worktree(repo.id, "flock/a", "/tmp/c1", None, "bypassPermissions", "worktree", Some(orch.id))
            .unwrap();
        let c2 = db
            .insert_worktree(repo.id, "flock/b", "/tmp/c2", None, "bypassPermissions", "worktree", Some(orch.id))
            .unwrap();
        // An unrelated standalone worktree must not show up in the fleet.
        db.insert_worktree(repo.id, "flock/loose", "/tmp/loose", None, "bypassPermissions", "worktree", None)
            .unwrap();

        let fleet = db.list_children(orch.id).unwrap();
        assert_eq!(fleet.len(), 2);
        assert!(fleet.iter().any(|w| w.id == c1.id));
        assert!(fleet.iter().any(|w| w.id == c2.id));

        // A childless orchestrator has an empty fleet.
        let lonely = db
            .insert_worktree(repo.id, "tokyo", "/tmp/orch_lonely", None, "bypassPermissions", "orchestrator", None)
            .unwrap();
        assert!(db.list_children(lonely.id).unwrap().is_empty());
    }

    /// Linchpin: the bundled SQLite must ship FTS5, and our search shape
    /// (MATCH + snippet + rank ordering + DELETE-by-column) must run.
    #[test]
    fn fts5_available_and_search_shape_works() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE kb_documents USING fts5(\
             path UNINDEXED, title, body, tags, tokenize='porter unicode61');",
        )
        .expect("FTS5 must be enabled in the bundled SQLite");
        c.execute(
            "INSERT INTO kb_documents (path, title, body, tags) VALUES (?1, ?2, ?3, ?4)",
            params![
                "memory/deploy.md",
                "Deploy process",
                "We deploy via Render to production each morning",
                "ops"
            ],
        )
        .unwrap();
        let (title, snippet): (String, String) = c
            .query_row(
                "SELECT title, snippet(kb_documents, 2, '[', ']', '…', 12) \
                 FROM kb_documents WHERE kb_documents MATCH ?1 ORDER BY rank LIMIT 1",
                params!["\"deploy\""],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "Deploy process");
        assert!(snippet.to_lowercase().contains("deploy"));

        c.execute(
            "DELETE FROM kb_documents WHERE path = ?1",
            params!["memory/deploy.md"],
        )
        .unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM kb_documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
