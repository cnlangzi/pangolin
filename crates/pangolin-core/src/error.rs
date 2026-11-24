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

    /// The background task that ran the query died. Only
    /// [`crate::App::with_db`] produces this: the query closure
    /// panicked on the blocking pool, so the result is lost.
    #[error("database task: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, PangolinError>;

/// The failure of a query that ran on the blocking pool
/// ([`crate::App::with_db`]).
///
/// The query's own error is carried verbatim: `Display` renders the
/// database's message unchanged, so the text the operator — or the
/// dashboard — reads is the text the database produced.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The query failed.
    #[error("{0}")]
    Query(#[from] rusqlite::Error),

    /// The query panicked, so its result is lost.
    #[error("the database task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

impl From<DbError> for PangolinError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::Query(e) => PangolinError::Database(e),
            DbError::Task(e) => PangolinError::Task(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query's error keeps its text (issue #254).
    ///
    /// The message reaches the operator — and the dashboard, which
    /// renders it in the error body — so the wrapper must not add a
    /// prefix to the database's own message.
    #[test]
    fn a_query_error_keeps_its_text() {
        let wrapped = DbError::from(rusqlite::Error::InvalidQuery);
        assert_eq!(
            wrapped.to_string(),
            rusqlite::Error::InvalidQuery.to_string(),
            "the wrapper must not change the database's message"
        );
    }
}
