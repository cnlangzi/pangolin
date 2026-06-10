//! Pangolin-wide error type. Wraps lower-level errors with context.

#[derive(Debug, thiserror::Error)]
pub enum PangolinError {
    #[error("config: {0}")]
    Config(String),

    #[error("database: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("backend parse: {0}")]
    Parse(#[from] crate::parse::ParseError),

    #[error("migration: {0}")]
    Migration(#[from] refinery::Error),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, PangolinError>;
