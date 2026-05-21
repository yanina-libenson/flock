use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Msg(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("db: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("git: {0}")]
    Git(String),

    #[error("pty: {0}")]
    Pty(String),

    #[error(transparent)]
    Any(#[from] anyhow::Error),
}

impl AppError {
    pub fn msg(s: impl Into<String>) -> Self {
        Self::Msg(s.into())
    }
}

impl Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
