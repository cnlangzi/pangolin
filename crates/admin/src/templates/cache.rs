//! The edge cache's statistics card (issue #260).
//!
//! The card is a fragment rather than a page: it lives on the
//! dashboard and is refreshed through HTMX, so the operator watches
//! the hit rate move without reloading.

use askama::Template;

use crate::i18n::Lang;
use pangolin_core::CacheStats;

/// The cache card — the counters and the "clear the cache" button.
///
/// The counters come straight from the cache's atomics: there is no
/// publishing thread and no snapshot to wait for, so the numbers are
/// the numbers as of the moment the card was rendered.
#[derive(Template)]
#[template(path = "views/dashboard/_cache.html")]
pub struct CacheCardView {
    /// The cache's counters.
    ///
    /// The name matches the dashboard page's field, so the same
    /// fragment serves both surfaces.
    pub cache_stats: CacheStats,
    /// The interface language this card is rendered in.
    pub lang: Lang,
}
