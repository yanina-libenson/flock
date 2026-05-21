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
}

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
              id         INTEGER PRIMARY KEY,
              repo_id    INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
              branch     TEXT NOT NULL,
              path       TEXT NOT NULL UNIQUE,
              title      TEXT,
              created_at INTEGER NOT NULL,
              last_used  INTEGER
            );

            CREATE INDEX IF NOT EXISTS idx_worktrees_repo ON worktrees(repo_id);

            -- Legacy: earlier versions of Flock persisted PTY scrollback blobs
            -- here. Sessions are now owned by tmux, so this table is dead.
            DROP TABLE IF EXISTS sessions;
            "#,
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

    pub fn insert_worktree(
        &self,
        repo_id: i64,
        branch: &str,
        path: &str,
        title: Option<&str>,
    ) -> AppResult<Worktree> {
        let c = self.c()?;
        c.execute(
            "INSERT INTO worktrees (repo_id, branch, path, title, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET branch=excluded.branch, title=excluded.title",
            params![repo_id, branch, path, title, now()],
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
            "SELECT id, repo_id, branch, path, title, created_at, last_used
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
                })
            },
        )?;
        Ok(w)
    }

    pub fn list_worktrees(&self, repo_id: i64) -> AppResult<Vec<Worktree>> {
        let c = self.c()?;
        let mut stmt = c.prepare(
            "SELECT id, repo_id, branch, path, title, created_at, last_used
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
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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
}
