use crate::db::Db;
use crate::pty::PtyManager;

pub struct AppState {
    pub db: Db,
    pub pty: PtyManager,
}

impl AppState {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            db: Db::open()?,
            pty: PtyManager::new(),
        })
    }
}
