//! DNS provider templates — list, new, edit, form fields view.
//!
//! Each struct has its own fields + methods so the new.html and edit.html
//! pages can both include `views/dns/_form_fields.html` (askama renders
//! the include with the parent struct's context). The fields and methods
//! are duplicated across `DnsProvidersNewTemplate`,
//! `DnsProvidersEditTemplate`, and `DnsProvidersFormFieldsView`.

use std::collections::HashMap;

use askama::Template;
use pangolin_core::types::DnsProvider;

// ─── List page (GET /dns) ──────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/dns/list.html")]
pub struct DnsProvidersListTemplate<'a> {
    pub providers: Vec<DnsProvider>,
    /// Map of provider name → number of domains currently using it.
    pub domain_counts: HashMap<String, usize>,
    pub active_nav: &'a str,
}

impl<'a> DnsProvidersListTemplate<'a> {
    pub fn count_for(&self, name: &str) -> usize {
        self.domain_counts.get(name).copied().unwrap_or(0)
    }
}

// ─── New page (GET /dns/new) ──────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/dns/new.html")]
pub struct DnsProvidersNewTemplate<'a> {
    /// Always `None` for the "new" page. Present so the shared
    /// `views/dns/_form_fields.html` compiles in the new context.
    pub provider: Option<DnsProvider>,
    pub action: &'a str,
    pub form_title: &'a str,
    pub submit_label: &'a str,
    pub is_edit: bool,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    pub cf_token: Option<String>,
    pub cf_token_set: bool,
    pub aliyun_ak_id: Option<String>,
    pub aliyun_ak_secret: Option<String>,
    pub aliyun_ak_secret_set: bool,
    pub aliyun_region: Option<String>,
    pub tencent_secret_id: Option<String>,
    pub tencent_secret_key: Option<String>,
    pub tencent_secret_key_set: bool,
}

impl<'a> DnsProvidersNewTemplate<'a> {
    pub fn name_value(&self) -> &str {
        self.provider
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("")
    }
    pub fn kind_value(&self) -> &'static str {
        match self.provider.as_ref().map(|p| p.kind) {
            Some(pangolin_core::DnsProviderKind::Cloudflare) => "cloudflare",
            Some(pangolin_core::DnsProviderKind::Aliyun) => "aliyun",
            Some(pangolin_core::DnsProviderKind::Tencent) => "tencent",
            None => "cloudflare",
        }
    }
    pub fn is_kind(&self, kind: &str) -> bool {
        self.kind_value() == kind
    }
    pub fn enabled_checked(&self) -> bool {
        true
    }
    pub fn aliyun_ak_id_value(&self) -> &str {
        self.aliyun_ak_id.as_deref().unwrap_or("")
    }
    pub fn aliyun_region_value(&self) -> &str {
        self.aliyun_region.as_deref().unwrap_or("cn-hangzhou")
    }
    pub fn tencent_secret_id_value(&self) -> &str {
        self.tencent_secret_id.as_deref().unwrap_or("")
    }
}

// ─── Edit page (GET /dns/{name}/edit) ──────────────────────────────────────────

#[derive(Template)]
#[template(path = "pages/dns/edit.html")]
pub struct DnsProvidersEditTemplate<'a> {
    pub provider: Option<DnsProvider>,
    pub action: &'a str,
    pub form_title: &'a str,
    pub submit_label: &'a str,
    pub is_edit: bool,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    pub cf_token: Option<String>,
    pub cf_token_set: bool,
    pub aliyun_ak_id: Option<String>,
    pub aliyun_ak_secret: Option<String>,
    pub aliyun_ak_secret_set: bool,
    pub aliyun_region: Option<String>,
    pub tencent_secret_id: Option<String>,
    pub tencent_secret_key: Option<String>,
    pub tencent_secret_key_set: bool,
}

impl<'a> DnsProvidersEditTemplate<'a> {
    pub fn name_value(&self) -> &str {
        self.provider
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("")
    }
    pub fn kind_value(&self) -> &'static str {
        match self.provider.as_ref().map(|p| p.kind) {
            Some(pangolin_core::DnsProviderKind::Cloudflare) => "cloudflare",
            Some(pangolin_core::DnsProviderKind::Aliyun) => "aliyun",
            Some(pangolin_core::DnsProviderKind::Tencent) => "tencent",
            None => "cloudflare",
        }
    }
    pub fn is_kind(&self, kind: &str) -> bool {
        self.kind_value() == kind
    }
    pub fn enabled_checked(&self) -> bool {
        self.provider.as_ref().map(|p| p.enabled).unwrap_or(true)
    }
    pub fn aliyun_ak_id_value(&self) -> &str {
        self.aliyun_ak_id.as_deref().unwrap_or("")
    }
    pub fn aliyun_region_value(&self) -> &str {
        self.aliyun_region.as_deref().unwrap_or("cn-hangzhou")
    }
    pub fn tencent_secret_id_value(&self) -> &str {
        self.tencent_secret_id.as_deref().unwrap_or("")
    }
}

// ─── Form fields view (shared by new + edit) ──────────────────────────────────

#[derive(Template)]
#[template(path = "views/dns/_form_fields.html")]
pub struct DnsProvidersFormFieldsView<'a> {
    pub provider: Option<DnsProvider>,
    pub action: &'a str,
    pub form_title: &'a str,
    pub submit_label: &'a str,
    pub is_edit: bool,
    pub error: Option<&'a str>,
    pub active_nav: &'a str,
    pub cf_token: Option<String>,
    pub cf_token_set: bool,
    pub aliyun_ak_id: Option<String>,
    pub aliyun_ak_secret: Option<String>,
    pub aliyun_ak_secret_set: bool,
    pub aliyun_region: Option<String>,
    pub tencent_secret_id: Option<String>,
    pub tencent_secret_key: Option<String>,
    pub tencent_secret_key_set: bool,
}

impl<'a> DnsProvidersFormFieldsView<'a> {
    pub fn name_value(&self) -> &str {
        self.provider
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("")
    }
    pub fn kind_value(&self) -> &'static str {
        match self.provider.as_ref().map(|p| p.kind) {
            Some(pangolin_core::DnsProviderKind::Cloudflare) => "cloudflare",
            Some(pangolin_core::DnsProviderKind::Aliyun) => "aliyun",
            Some(pangolin_core::DnsProviderKind::Tencent) => "tencent",
            None => "cloudflare",
        }
    }
    pub fn is_kind(&self, kind: &str) -> bool {
        self.kind_value() == kind
    }
    pub fn enabled_checked(&self) -> bool {
        self.provider.as_ref().map(|p| p.enabled).unwrap_or(true)
    }
    pub fn aliyun_ak_id_value(&self) -> &str {
        self.aliyun_ak_id.as_deref().unwrap_or("")
    }
    pub fn aliyun_region_value(&self) -> &str {
        self.aliyun_region.as_deref().unwrap_or("cn-hangzhou")
    }
    pub fn tencent_secret_id_value(&self) -> &str {
        self.tencent_secret_id.as_deref().unwrap_or("")
    }
}
