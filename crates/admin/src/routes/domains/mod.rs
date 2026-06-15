//! Domains route — list / new / edit / delete / site-specific sub-page.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/domains`, `/domains/new`,
//!   `/domains/{domain}/edit`, `/site/{name}/domains`)
//! - `views.rs`  — GET HTMX partials (`/api/site/{name}/domains`,
//!   `/api/site/{name}/domains/new`)
//! - `mutate.rs` — POST / DELETE handlers (incl. `handle_update` for
//!   `POST /api/domains/{domain}/edit` — issue #57)
//!
//! Issue #57 lifts the deliberate post-creation immutability of
//! domains: `handle_update` writes site_name, enabled, auto_issue, and
//! dns_provider via the existing `upsert_domain` ON CONFLICT UPDATE
//! path, then fires `reload_indexes().await` so `AcmeState` reacts
//! immediately. The PK (`domain` itself) is preserved.

pub mod mutate;
pub mod pages;
pub mod views;

// Re-exports for the dispatch table in `lib.rs::handle()`.
pub use mutate::{api_handle_delete, handle_create, handle_delete, handle_update};
pub use pages::{
    api_render_form_new, render, render_create_page, render_edit_page, render_for_site,
};
pub use views::render_table_for_site;
