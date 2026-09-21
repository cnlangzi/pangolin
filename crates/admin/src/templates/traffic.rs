//! Templates for `/traffic` and its HTMX fragments.

use askama::Template;
use pangolin_core::TrafficSnapshot;

/// Full page — KPI cards + tables, HTMX-refreshed after first paint.
#[derive(Template)]
#[template(path = "pages/traffic.html")]
pub struct TrafficPageTemplate<'a> {
    pub snap: TrafficSnapshot,
    pub spark_bars: Vec<u8>,
    pub csrf_token: String,
    pub active_nav: &'a str,
}

/// KPI strip swapped every 2s (`GET /api/traffic/kpis`).
#[derive(Template)]
#[template(path = "views/traffic/_kpis.html")]
pub struct TrafficKpisView {
    pub snap: TrafficSnapshot,
    pub spark_bars: Vec<u8>,
}

/// Host / path / status tables swapped every 8s (`GET /api/traffic/tables`).
#[derive(Template)]
#[template(path = "views/traffic/_tables.html")]
pub struct TrafficTablesView {
    pub snap: TrafficSnapshot,
}
