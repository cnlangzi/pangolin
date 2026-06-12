//! Domains HTMX partials.
//!
//! The only HTMX endpoint under `/api/...` for domains is
//! `GET /api/site/{name}/domains`, which returns the per-site table
//! rows. The original implementation hand-built the HTML to match the
//! minimal styling of the per-site delete form. We preserve that
//! byte-for-byte here to keep UI behavior identical.

use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use crate::App;

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// Render the per-site domains table rows for HTMX swap.
/// Endpoint: `GET /api/site/{name}/domains`.
///
/// Output is hand-built HTML (preserved from the pre-refactor
/// implementation) — NOT the `views/domains/_table.html` template that
/// the list page uses. They render the same logical data, but the
/// per-site view uses simpler styling.
pub async fn render_table_for_site(
    app: &Arc<App>,
    site_name: &str,
    csrf: &str,
) -> http::Result<Resp> {
    let db = app.db.lock().await;
    let all_domains = pangolin_core::db::list_domains(&db).unwrap_or_default();
    drop(db);
    let site_name_owned = site_name.to_owned();
    let domains: Vec<_> = all_domains
        .into_iter()
        .filter(|d| d.site_name == site_name_owned)
        .collect();

    let rows: Vec<String> = domains
        .iter()
        .map(|d| {
            format!(
                r##"<tr id="domain-{}" class="border-b border-slate-100 dark:border-slate-700 hover:bg-slate-50 dark:hover:bg-slate-700/30 transition-colors">
  <td class="py-3 px-3"><span class="font-mono text-sm text-slate-800 dark:text-slate-100">{}</span></td>
  <td class="py-3 px-3">
    <span class="inline-flex items-center gap-1 text-xs font-medium px-2 py-0.5 rounded-full {} {} {}">{}</span>
  </td>
  <td class="py-3 px-3">
    <div class="flex items-center gap-1">
      <form method="POST" action="/domains/delete" onsubmit="return confirm('Delete domain {}?');" class="inline">
        <input type="hidden" name="domain" value="{}">
        <input type="hidden" name="_csrf" value="__CSRF__">
        <button type="submit"
          class="text-slate-400 dark:text-slate-400 hover:text-red-500 dark:hover:text-red-400 p-1.5 rounded hover:bg-red-50 dark:hover:bg-red-500/10 transition-colors"
          title="Delete">
          <svg class="w-4 h-4" fill="none" stroke="currentColor" stroke-width="2" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" d="M14.74 9l-.346 9m-4.788 0L9.26 9m9.968-3.21c.342.052.682.107 1.022.166m-1.022-.165L18.16 19.673a2.25 2.25 0 01-2.244 2.077H8.084a2.25 2.25 0 01-2.244-2.077L4.772 5.79m14.456 0a48.108 48.108 0 00-3.478-.397m-12 .562c.34-.059.68-.114 1.022-.165m0 0a48.11 48.11 0 013.478-.397m7.5 0v-.916c0-1.18-.91-2.164-2.09-2.201a51.964 51.964 0 00-3.32 0c-1.18.037-2.09 1.022-2.09 2.201v.916m7.5 0a48.667 48.667 0 00-7.5 0"/></svg>
        </button>
      </form>
    </div>
  </td>
</tr>"##,
                d.domain,
                d.domain,
                if d.enabled {
                    "bg-green-100 text-green-700"
                } else {
                    "bg-slate-100 text-slate-400"
                },
                if d.enabled {
                    "dark:bg-green-900/30 dark:text-green-300"
                } else {
                    "dark:bg-slate-700 dark:text-slate-500"
                },
                if d.enabled { "" } else { "line-through" },
                if d.enabled { "enabled" } else { "disabled" },
                d.domain,
                d.domain
            )
        })
        .collect();

    ok_html(crate::render_with_assets_and_csrf(rows.join(""), csrf))
}
