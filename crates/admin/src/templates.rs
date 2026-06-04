//! Askama templates for admin pages.

use askama::Template;
use chrono::{DateTime, Utc};
use pangolin_core::types::{Cert, Domain, Site, Token, Tun};

// ─── Dashboard ──────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub site_count: usize,
    pub domain_count: usize,
    pub online_tun_count: usize,
    pub total_tun_count: usize,
    pub token_count: usize,
    pub cert_count: usize,
}

// ─── Sites ─────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "sites.html")]
pub struct SitesTemplate<'a> {
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "sites_table.html")]
pub struct SitesTableTemplate {
    pub sites: Vec<Site>,
}

#[derive(Template)]
#[template(path = "site_form.html")]
pub struct SiteFormTemplate<'a> {
    pub site: Option<Site>,
    pub action: &'a str,
    pub error: Option<&'a str>,
}

// ─── Domains ───────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "domains.html")]
pub struct DomainsTemplate<'a> {
    pub domains: Vec<Domain>,
    pub sites: Vec<Site>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "domains_form.html")]
pub struct DomainFormTemplate<'a> {
    pub sites: Vec<Site>,
    pub error: Option<&'a str>,
}

// ─── Tunnels ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tunnels.html")]
pub struct TunnelsTemplate<'a> {
    pub tuns: Vec<Tun>,
    pub active_nav: &'a str,
}

// ─── Tokens ────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "tokens.html")]
pub struct TokensTemplate<'a> {
    pub tokens: Vec<Token>,
    pub active_nav: &'a str,
}

#[derive(Template)]
#[template(path = "tokens_form.html")]
pub struct TokenFormTemplate<'a> {
    pub token: Option<Token>,
    pub error: Option<&'a str>,
}

// ─── Certs ─────────────────────────────────────────────────────────────────────

#[derive(Template)]
#[template(path = "certs.html")]
pub struct CertsTemplate<'a> {
    pub certs: Vec<Cert>,
    pub active_nav: &'a str,
    pub now: &'a DateTime<Utc>,
}

#[derive(Template)]
#[template(path = "certs_form.html")]
pub struct CertFormTemplate<'a> {
    pub error: Option<&'a str>,
}