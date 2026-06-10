//! Embedded migrations for pangolin-core.
//!
//! Uses the `refinery` embedded migrations pattern:
//! - Migrations live in `migrations/` directory at crate root
//! - Named `V{version}__{name}.sql`
//! - `embed_migrations!()` collects them at compile time
//! - `runner().run(&mut conn)` runs pending migrations on startup
//!
//! Version tracking: refinery maintains a `schema_version` table.

pub mod embedded {
    use refinery::embed_migrations;
    embed_migrations!("migrations");
}

/// Runs all pending migrations on the given connection.
///
/// This is called once at application startup. Refinery will create a
/// `schema_version` table to track which migrations have been applied.
/// Safe to call on every startup — already-applied migrations are skipped.
pub fn run_migrations(conn: &mut rusqlite::Connection) -> Result<(), refinery::Error> {
    embedded::migrations::runner().run(conn)
}