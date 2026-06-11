//! DNS provider full-page renders.

use askama::Template;
use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http::Response;
use http_body_util::Full;

use pangolin_core::DnsProviderKind;

use crate::templates::{
    DnsProvidersEditTemplate, DnsProvidersListTemplate, DnsProvidersNewTemplate,
};
use crate::App;

type Resp = Response<Full<Bytes>>;

fn ok_html(body: String) -> http::Result<Resp> {
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("response builder for 200 OK should not fail"))
}

pub async fn render(app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let providers = {
        let db = app.db.lock().await;
        pangolin_core::db::list_dns_providers(&db).unwrap_or_default()
    };
    let mut domain_counts: HashMap<String, usize> = HashMap::new();
    {
        let db = app.db.lock().await;
        for d in pangolin_core::db::list_domains(&db).unwrap_or_default() {
            if let Some(p) = d.dns_provider {
                *domain_counts.entry(p).or_insert(0) += 1;
            }
        }
    }
    let html = DnsProvidersListTemplate {
        providers,
        domain_counts,
        active_nav: "dns",
    }
    .render()
    .unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_create_page(_app: &Arc<App>, csrf: &str) -> http::Result<Resp> {
    let html = build_new_form(None).render().unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

pub async fn render_edit_page(
    app: &Arc<App>,
    name: Option<String>,
    csrf: &str,
) -> http::Result<Resp> {
    let Some(name) = name else {
        return Ok(crate::not_found());
    };
    let provider = {
        let db = app.db.lock().await;
        pangolin_core::db::get_dns_provider(&db, &name).unwrap_or(None)
    };
    let Some(provider) = provider else {
        return Ok(crate::not_found());
    };
    let html = build_edit_form(&provider).render().unwrap();
    ok_html(crate::render_with_assets_and_csrf(html, csrf))
}

/// Build a DnsProvidersNewTemplate populated with no provider (new mode).
/// Optionally carries an error message to display at the top of the form.
pub(super) fn build_new_form<'a>(error: Option<&'a str>) -> DnsProvidersNewTemplate<'a> {
    DnsProvidersNewTemplate {
        provider: None,
        action: "/dns/new",
        form_title: "New DNS Provider",
        submit_label: "Create",
        is_edit: false,
        error,
        active_nav: "dns",
        cf_token: None,
        cf_token_set: false,
        aliyun_ak_id: None,
        aliyun_ak_secret: None,
        aliyun_ak_secret_set: false,
        aliyun_region: None,
        tencent_secret_id: None,
        tencent_secret_key: None,
        tencent_secret_key_set: false,
    }
}

/// Build a DnsProvidersEditTemplate populated with the existing provider
/// and the per-kind credential fields. Behaviour-preserving port of the
/// `empty_form(Some(p), None, "dns")` from the pre-refactor
/// `routes/dns.rs`.
pub(super) fn build_edit_form(
    provider: &pangolin_core::types::DnsProvider,
) -> DnsProvidersEditTemplate<'_> {
    let v: serde_json::Value = serde_json::from_str(&provider.config).unwrap_or_default();
    let (
        cf_token,
        cf_token_set,
        aliyun_ak_id,
        aliyun_ak_secret,
        aliyun_ak_secret_set,
        aliyun_region,
        tencent_secret_id,
        tencent_secret_key,
        tencent_secret_key_set,
    ) = match provider.kind {
        DnsProviderKind::Cloudflare => {
            let t = v
                .get("api_token")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            (
                Some("••••••••••••".into()),
                !t.is_empty(),
                None,
                None,
                false,
                None,
                None,
                None,
                false,
            )
        }
        DnsProviderKind::Aliyun => {
            let ak = v
                .get("access_key_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let sk = v
                .get("access_key_secret")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let r = v
                .get("region")
                .and_then(|x| x.as_str())
                .unwrap_or("cn-hangzhou")
                .to_string();
            (
                None,
                false,
                Some(ak),
                Some("••••••••••••".into()),
                !sk.is_empty(),
                Some(r),
                None,
                None,
                false,
            )
        }
        DnsProviderKind::Tencent => {
            let id = v
                .get("secret_id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let key = v
                .get("secret_key")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            (
                None,
                false,
                None,
                None,
                false,
                None,
                Some(id),
                Some("••••••••••••".into()),
                !key.is_empty(),
            )
        }
    };
    DnsProvidersEditTemplate {
        provider: Some(provider.clone()),
        action: "/dns/edit",
        form_title: "Edit DNS Provider",
        submit_label: "Save",
        is_edit: true,
        error: None,
        active_nav: "dns",
        cf_token,
        cf_token_set,
        aliyun_ak_id,
        aliyun_ak_secret,
        aliyun_ak_secret_set,
        aliyun_region,
        tencent_secret_id,
        tencent_secret_key,
        tencent_secret_key_set,
    }
}
