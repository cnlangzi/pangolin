//! Sites route — list / new / edit / delete.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/sites`, `/sites/new`, `/sites/edit`)
//! - `views.rs`  — GET HTMX partials (none for sites at the moment)
//! - `mutate.rs` — POST/PUT/DELETE handlers
//! - `helpers.rs` — internal `parse_form` / `assemble_backend_from_form`

pub mod helpers;
pub mod mutate;
pub mod pages;
pub mod views;

// Re-exports for ergonomic call-site paths:
//   `routes::sites::render`
//   `routes::sites::handle_create`
//   `routes::sites::handle_update`
//   `routes::sites::handle_delete`
pub use mutate::{api_handle_delete, handle_create, handle_delete, handle_update};
pub use pages::{render, render_create_page, render_edit_page};
