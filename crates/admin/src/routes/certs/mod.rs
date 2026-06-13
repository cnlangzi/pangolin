//! Certs route — list / new / delete / retry / summary.
//!
//! Split per xun-style resource layout:
//! - `pages.rs`  — GET full pages (`/certs`, `/certs/new`)
//! - `views.rs`  — GET HTMX partials (none for certs at the moment)
//! - `mutate.rs` — POST / DELETE handlers (create, delete, retry)
//! - `summary.rs` — GET `/api/certs/summary` (dashboard badge JSON)

pub mod helpers;
pub mod mutate;
pub mod pages;
pub mod summary;
pub mod views;

pub use mutate::{api_handle_delete, handle_create, handle_delete, handle_retry};
pub use pages::{render, render_create_page};
pub use summary::handle_summary;
