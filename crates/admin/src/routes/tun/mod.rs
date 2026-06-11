//! Tunnels route — list / new / edit / delete.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/tun`, `/tun/new`, `/tun/edit`)
//! - `views.rs`  — GET HTMX partials (none for tun at the moment)
//! - `mutate.rs` — POST / PUT / DELETE handlers

pub mod helpers;
pub mod mutate;
pub mod pages;
pub mod views;

// Re-exports for the dispatch table.
pub use mutate::{handle_create, handle_delete, handle_update};
pub use pages::{render, render_create_page, render_edit_page};
