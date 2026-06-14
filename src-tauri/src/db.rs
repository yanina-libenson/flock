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
        let path = dir.join("flock.db");
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
              permission_mode TEXT NOT NULL DEFAULT 'bypassPermissions'
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

            -- Legacy: earlier versions of Flock persisted PTY scrollback blobs
            -- here. Sessions are now owned by tmux, so this table is dead.
            DROP TABLE IF EXISTS sessions;
            "#,
        )?;
        // Defensive ALTER for DBs that predate the permission_mode column.
        // SQLite has no IF NOT EXISTS for ADD COLUMN; the error is fine to
        // swallow — it only fires when the column is already there.
        let _ = conn.execute(
            "ALTER TABLE worktrees ADD COLUMN permission_mode TEXT NOT NULL DEFAULT 'bypassPermissions'",
            [],
        );
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

    pub fn insert_worktree(
        &self,
        repo_id: i64,
        branch: &str,
        path: &str,
        title: Option<&str>,
        permission_mode: &str,
    ) -> AppResult<Worktree> {
        let c = self.c()?;
        c.execute(
            "INSERT INTO worktrees (repo_id, branch, path, title, created_at, permission_mode)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET branch=excluded.branch, title=excluded.title",
            params![repo_id, branch, path, title, now(), permission_mode],
        )?;
        let id = c.query_row(
            "SELECT id FROM worktrees WHERE path = ?1",
            params![path],
            |r| r.get::<_, i64>(0),
        )?;
        drop(c);
        self.get_worktree(id)
    }

    pub fn get_worktree(&self, id: i64) -> AppResult<Worktree> {
        let c = self.c()?;
        let w = c.query_row(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode
             FROM worktrees WHERE id = ?1",
            params![id],
            |row| {
                Ok(Worktree {
                    id: row.get(0)?,
                    repo_id: row.get(1)?,
                    branch: row.get(2)?,
                    path: row.get(3)?,
                    title: row.get(4)?,
                    created_at: row.get(5)?,
                    last_used: row.get(6)?,
                    permission_mode: row.get(7)?,
                })
            },
        )?;
        Ok(w)
    }

    pub fn list_worktrees(&self, repo_id: i64) -> AppResult<Vec<Worktree>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, branch, path, title, created_at, last_used, permission_mode
             FROM worktrees WHERE repo_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![repo_id], |row| {
            Ok(Worktree {
                id: row.get(0)?,
                repo_id: row.get(1)?,
                branch: row.get(2)?,
                path: row.get(3)?,
                title: row.get(4)?,
                created_at: row.get(5)?,
                last_used: row.get(6)?,
                permission_mode: row.get(7)?,
            })
        })?;
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
}
