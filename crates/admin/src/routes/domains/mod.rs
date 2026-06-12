//! Domains route — list / new / delete / site-specific sub-page.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/domains`, `/domains/new`, `/site/{name}/domains`)
//! - `views.rs`  — GET HTMX partials (`/api/site/{name}/domains`,
//!   `/api/site/{name}/domains/new`)
//! - `mutate.rs` — POST / DELETE handlers
//!
//! Note: the previous flat `routes/domains.rs` had no edit page (domain
//! was treated as immutable post-creation; updates were a delete +
//! recreate). The "edit" template (pages/domains/edit.html) exists
//! for forward compatibility — the domains form is used for both new
//! and edit by the new resource split.

pub mod mutate;
pub mod pages;
pub mod views;

// Re-exports for the dispatch table in `lib.rs::handle()`.
pub use mutate::{api_handle_delete, handle_create, handle_delete};
pub use pages::{api_render_form_new, render, render_create_page, render_for_site};
pub use views::render_table_for_site;
