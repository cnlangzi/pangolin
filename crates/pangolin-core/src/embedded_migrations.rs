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

/// Runs the migrations up to (and including) `version`.
///
/// Used by the tests to build a database that predates a migration,
/// so the in-place upgrade path can be exercised. Production code
/// always wants [`run_migrations`].
pub fn run_migrations_up_to(conn: &mut rusqlite::Connection, version: i32) -> crate::Result<()> {
    embedded::migrations::runner()
        .set_target(refinery::Target::Version(version))
        .run(conn)?;
    Ok(())
}

/// Runs all pending migrations on the given connection.
///
/// This is called once at application startup. Refinery will create a
/// `schema_version` table to track which migrations have been applied.
/// Safe to call on every startup — already-applied migrations are skipped.
pub fn run_migrations(conn: &mut rusqlite::Connection) -> crate::Result<()> {
    embedded::migrations::runner().run(conn)?;
    Ok(())
}
