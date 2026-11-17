/// Why a [`crate::LogStore`] call failed.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The caller passed something the log cannot represent (a seq beyond
    /// `i64::MAX`, a `database_url` with an unknown scheme, ...). Retrying the
    /// same call will not help.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A stored row no longer decodes into the current contract.
    #[error("corrupt row: {0}")]
    Corrupt(String),
    /// The backend failed; usually transient.
    #[error("database: {0}")]
    Database(#[from] sqlx::Error),
    /// The embedded SQLite store failed.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A blocking-pool task failed (panicked or was cancelled).
    #[error("blocking task: {0}")]
    Task(String),
    /// The database is not one Senate made, or was made by an incompatible
    /// version. Nothing was written to it. The text says what was found.
    #[error(
        "this database was not created by Senate (or was created by an incompatible version): \
         {0}. Senate did not change it; use a new, empty database"
    )]
    ForeignDatabase(String),
    /// Applying the embedded migrations failed.
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}
