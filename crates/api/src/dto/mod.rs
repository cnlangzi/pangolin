//! The data-transfer objects of the API.
//!
//! * [`write`] — the request bodies, and their adapters to the admin
//!   UI's form parameters.
//! * [`views`] — the response shapes, with the credentials removed.

pub mod views;
pub mod write;

pub use views::{
    BadbotsSettingsView, DashboardAddress, DnsProviderView, InstanceView, ProviderStatusView,
    ProvidersView, SiteOriginAllowView, SiteProviderView, SiteRateLimitBotsView, SiteRateLimitView,
    TunView, TunnelNodeView,
};
pub use write::{
    BadbotsConfigDto, Credential, DnsProviderDto, DomainDto, IpRuleDto, Params, RateLimitDto,
    SiteDto, SiteProviderDto, SystemConfigDto, TunDto, encode_form,
};
