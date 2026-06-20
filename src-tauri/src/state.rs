use crate::db::Db;
use crate::monitor::WorktreeStatus;
use crate::pty::PtyManager;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// Latest agent status per worktree id, written by the monitor and read by the
/// REST API. Shared so the PWA can report status without the desktop frontend.
pub type StatusMap = Arc<Mutex<HashMap<i64, WorktreeStatus>>>;

pub struct AppState {
    pub db: Db,
    pub pty: PtyManager,
    pub statuses: StatusMap,
    /// Cancellation handle for the running remote API server, or None when the
    /// server is stopped. Firing it gracefully shuts down every bound listener.
    pub remote: Mutex<Option<CancellationToken>>,
    /// Live watcher on the knowledge-base vault (re-indexes notes edited in
    /// Obsidian). Kept alive here; dropping it stops watching. None when no
    /// vault is configured.
    pub kb_watcher: Mutex<Option<notify::RecommendedWatcher>>,
}

impl AppState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            db: Db::open()?,
            pty: PtyManager::new(),
            statuses: Arc::new(Mutex::new(HashMap::new())),
            remote: Mutex::new(None),
            kb_watcher: Mutex::new(None),
        })
    }
}
